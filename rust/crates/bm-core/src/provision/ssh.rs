use anyhow::{Context, Result};
use bm_proto::Machine;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

const RSYNC_TIMEOUT_SECS: u64 = 1800;
const RSYNC_IO_TIMEOUT: &str = "--timeout=120";

/// How many times one transport call is attempted before a blip becomes an
/// error. Provisioning used to fail a whole box on the first transient ssh
/// timeout — one flap during `ensure_root` and `:prov` died, then the retry
/// died on a different step, so re-running never converged.
const TRANSPORT_ATTEMPTS: u32 = 4;
/// Sleeps between attempts. The flap this rides out is seconds, not minutes;
/// backed-off retries plus rsync's delta resume (a re-run only sends what is
/// still missing) is what lets a 668 MB push converge on a lossy link.
const TRANSPORT_BACKOFF_SECS: [u64; 3] = [2, 5, 10];

/// True when a failed call smells like the network rather than the command:
/// running the identical call again can succeed.
///
/// ssh reports its own transport failures as 255, rsync as 10/12/30 (or 255
/// when its ssh dies first). Auth and host-key failures are deliberately NOT
/// transient — retrying those four times only wastes the backoff.
fn transient_failure(code: i32, stderr: &str) -> bool {
    let t = stderr.to_lowercase();
    if t.contains("permission denied") || t.contains("host key verification failed") {
        return false;
    }
    let net = t.contains("timed out")
        || t.contains("timeout")
        || t.contains("connection reset")
        || t.contains("broken pipe")
        || t.contains("connection closed")
        || t.contains("unexpectedly closed")
        || t.contains("connection refused")
        || t.contains("no route to host")
        || t.contains("network is unreachable")
        || t.contains("unexpected end of file")
        || t.contains("socket io")
        || t.contains("stalled mid-command");
    net && (code == 255 || code == 10 || code == 12 || code == 30)
}

/// Run `call` up to [`TRANSPORT_ATTEMPTS`] times, backing off between
/// attempts while the failure looks transient (see [`transient_failure`).
/// A success or a non-transient failure returns at once; only the last
/// transient failure surfaces, annotated with the attempt count.
fn with_transport_retries<F>(mut call: F) -> Result<(i32, String, String)>
where
    F: FnMut() -> Result<(i32, String, String)>,
{
    // A runner-level `Err` is classified as a 255: `run_bounded` only errors
    // on spawn (a missing local binary — not transient, fails fast) and on
    // its own stall timeout ("timed out ... stalled mid-command" — transient,
    // retries like any other blip).
    for attempt in 1..=TRANSPORT_ATTEMPTS {
        let last_attempt = attempt == TRANSPORT_ATTEMPTS;
        match call() {
            Ok((0, o, e)) => return Ok((0, o, e)),
            Ok((code, _o, e)) if transient_failure(code, &e) && !last_attempt => {
                backoff(attempt);
                continue;
            }
            Ok((code, o, e)) if transient_failure(code, &e) => {
                let e = format!("{e} [after {TRANSPORT_ATTEMPTS} attempts]");
                return Ok((code, o, e));
            }
            Err(e) if transient_failure(255, &e.to_string()) && !last_attempt => {
                backoff(attempt);
                continue;
            }
            Err(e) if transient_failure(255, &e.to_string()) => {
                return Err(e).context(format!("after {TRANSPORT_ATTEMPTS} attempts"));
            }
            other => return other,
        }
    }
    unreachable!("loop always returns on the last attempt");
}

fn backoff(attempt: u32) {
    std::thread::sleep(std::time::Duration::from_secs(
        TRANSPORT_BACKOFF_SECS[(attempt - 1) as usize % TRANSPORT_BACKOFF_SECS.len()],
    ));
}

/// The host-key policy, written once and used by both transports — `ssh` and
/// the `ssh` that `rsync` spawns through `-e`.
///
/// Every box this reaches is either an instance launched minutes ago or a
/// worker linked by hand, and every call is scripted: `BatchMode=yes` forbids
/// the "are you sure you want to continue connecting?" prompt, so a host key
/// that is not already in `known_hosts` is a hard `exit 255 — Host key
/// verification failed`. A freshly launched EC2 instance *always* presents a
/// key nobody has seen before, which is why the first provision of every new
/// box failed with exactly that message.
///
/// So verification is declined and `known_hosts` is neither read nor written
/// (`/dev/null`). That is not only about first contact: AWS hands the same
/// public IP to a different box later, and a *remembered* key for a recycled
/// address is the same failure in a different coat — `StrictHostKeyChecking=no`
/// accepts an unknown host but still refuses a *changed* one. `/dev/null` also
/// keeps the tool out of `~/.ssh` entirely, which is the one directory a
/// sandboxed session may not touch.
///
/// `~/.ssh/config` is still read — an `-o` overrides a single option, it does
/// not replace the file — so Host aliases, `ProxyJump` and `IdentityFile` keep
/// working. `LogLevel=ERROR` removes the "Permanently added … to the list of
/// known hosts" line, which is untrue here because nothing is persisted; it
/// keeps the warnings that carry information, e.g. an identity file that is not
/// readable (verified against a real box, not assumed).
const HOST_KEY_OPTS: [&str; 6] = [
    "-o",
    "StrictHostKeyChecking=no",
    "-o",
    "UserKnownHostsFile=/dev/null",
    "-o",
    "LogLevel=ERROR",
];

use super::REMOTE_DIR;
use crate::util::expand_tilde;

/// A resolved SSH connection to one machine.
pub struct Ssh {
    pub target: String,
    pub port: u16,
    pub key: Option<String>,
    /// The inductor's own machine: run commands directly instead of ssh-ing out.
    pub local: bool,
}

/// Where the winning ssh key came from. Highest wins; `SshDefault` means no
/// key is configured anywhere and ssh decides (agent, `~/.ssh/config`).
/// Shown on the machine overlay so a mispointed key names its source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    Box,
    Settings,
    SshDefault,
}

impl KeySource {
    pub fn label(self) -> &'static str {
        match self {
            KeySource::Box => "machines.json",
            KeySource::Settings => "settings.json",
            KeySource::SshDefault => "ssh default (agent / ~/.ssh/config)",
        }
    }
}

/// One reader for the key chain: the per-machine value, else the app default,
/// else ssh decides. `~` expands here, once, for every transport. Empty
/// strings fall through — clearing the field is how an operator unsets a key.
/// No validation: that belongs at bind time, where a prompt can complain.
pub fn resolve_key(
    box_key: Option<&str>,
    settings_key: Option<&str>,
) -> (Option<PathBuf>, KeySource) {
    fn clean(s: Option<&str>) -> Option<&str> {
        s.map(str::trim).filter(|s| !s.is_empty())
    }
    if let Some(k) = clean(box_key) {
        return (Some(expand_tilde(k)), KeySource::Box);
    }
    if let Some(k) = clean(settings_key) {
        return (Some(expand_tilde(k)), KeySource::Settings);
    }
    (None, KeySource::SshDefault)
}

impl Ssh {
    pub fn for_machine(m: &Machine) -> Self {
        let local = matches!(m.addr.as_str(), "127.0.0.1" | "localhost" | "::1");
        Ssh {
            target: m.ssh_target(),
            port: m.ssh_port,
            key: m.ssh_key.clone(),
            local,
        }
    }

    fn ssh_args(&self) -> Vec<String> {
        let mut args: Vec<String> = vec![
            // Never prompt, never linger: every use is scripted, and a stalled
            // connection must die instead of hanging a TUI job forever.
            "-n".into(),
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            "ConnectTimeout=10".into(),
            "-o".into(),
            "ServerAliveInterval=5".into(),
            "-o".into(),
            "ServerAliveCountMax=2".into(),
        ];
        args.extend(HOST_KEY_OPTS.iter().map(|s| s.to_string()));
        if self.port != 22 {
            args.push("-p".into());
            args.push(self.port.to_string());
        }
        if let Some(key) = &self.key {
            args.push("-i".into());
            args.push(expand_tilde(key).to_string_lossy().to_string());
        }
        args.push(self.target.clone());
        args
    }

    /// The argv for a long-lived **reverse** tunnel to this machine:
    /// `ssh … -N -R {remote_port}:127.0.0.1:{local_port} {target}`.
    ///
    /// This is the one channel back from a worker to the inductor (the
    /// completion hook, `bm-inductor/src/tunnel.rs`), and it exists because
    /// the direction stays inverted: the *inductor* dials the worker to build
    /// the tunnel, so the worker never needs a route — it forwards its own
    /// loopback through the connection the inductor opened. On the worker's
    /// side the remote bind is loopback-only (no `GatewayPorts`), which keeps
    /// the hook port closed to the box's network — only local processes may
    /// use it, and the cluster token gates what it does.
    ///
    /// Everything else mirrors [`Self::ssh_args`] — BatchMode, the declined
    /// host-key verification, the same key expansion — because this is the
    /// third transport and a policy on two of three is the known bug shape.
    /// `-n` stays (no stdin, never a prompt), and the keepalives are the
    /// tunnel's *liveness*, so they matter more than on a command run: a dead
    /// NAT mapping must kill the client quickly so the supervisor respawns it
    /// against the fresh route.
    /// `ExitOnForwardFailure=yes` turns a failed remote bind (a stale tunnel
    /// from a previous, uncleanly-killed inductor still holding the port) into
    /// a dead client — the supervisor's respawn loop then retries, instead of
    /// a zombie client pretending a tunnel that never existed.
    ///
    /// `-N` (no remote command) is what makes this a pure pipe: no shell is
    /// allocated on the box, so there is nothing to escape and nothing to time
    /// out — the client lives exactly as long as the TCP session does.
    pub fn reverse_hook_args(&self, remote_port: u16, local_port: u16) -> Vec<String> {
        let mut args: Vec<String> = vec![
            // No stdin (never a prompt) and no remote command: a pure pipe.
            "-n".into(),
            "-N".into(),
            "-o".into(),
            "BatchMode=yes".into(),
            // A failed remote bind kills the client — the supervisor's signal
            // to retry — instead of a live client around a dead forward.
            "-o".into(),
            "ExitOnForwardFailure=yes".into(),
            "-o".into(),
            "ConnectTimeout=10".into(),
            // The tunnel's liveness: keepalives tight enough that a NAT mapping
            // dying is noticed in seconds, not minutes. Without them a silently
            // dropped connection leaves a client that forwards nowhere while
            // looking alive.
            "-o".into(),
            "ServerAliveInterval=5".into(),
            "-o".into(),
            "ServerAliveCountMax=2".into(),
        ];
        args.extend(HOST_KEY_OPTS.iter().map(|s| s.to_string()));
        if self.port != 22 {
            args.push("-p".into());
            args.push(self.port.to_string());
        }
        if let Some(key) = &self.key {
            args.push("-i".into());
            args.push(expand_tilde(key).to_string_lossy().to_string());
        }
        args.push("-R".into());
        args.push(format!("{remote_port}:127.0.0.1:{local_port}"));
        args.push(self.target.clone());
        args
    }

    /// Run a shell script on the machine. Returns `(exit_code, stdout, stderr)`.
    ///
    /// A transport failure is reported as exit code 255 (ssh's own convention)
    /// so callers can distinguish "box is down" from "the command failed".
    ///
    /// `timeout_secs` bounds the whole run, not just the connect phase: ssh's
    /// own `ConnectTimeout` stops covering us the moment the session is up, and
    /// an un-bounded `Command::output()` once wedged the TUI's serial job queue
    /// behind a never-exiting remote launch — starving every job queued after
    /// it (including the voice roster) forever.
    pub fn run(&self, script: &str, timeout_secs: u64) -> Result<(i32, String, String)> {
        let full = format!("export PATH=$HOME/.local/bin:$HOME/.cargo/bin:$PATH\n{script}");
        let mut cmd = if self.local {
            let mut c = Command::new("sh");
            c.arg("-c").arg(&full);
            c
        } else {
            let mut c = Command::new("ssh");
            c.args(self.ssh_args()).arg(&full);
            c
        };
        let transport = if self.local {
            format!("sh for {}", self.target)
        } else {
            format!("ssh to {}", self.target)
        };
        // Local shells never flap; remote ones do, and one blip must not fail
        // a whole `:prov` run that converging retries would have saved.
        if self.local {
            run_bounded(&mut cmd, timeout_secs, &transport)
        } else {
            with_transport_retries(|| run_bounded(&mut cmd, timeout_secs, &transport))
        }
    }

    /// The `-e` value rsync reaches the box through. It carries the *same*
    /// host-key policy as [`Self::ssh_args`]: rsync spawns its own ssh, so
    /// setting the policy on one transport only would fix the probe and leave
    /// every push failing with the identical message.
    fn rsync_e(&self) -> String {
        let mut e = format!(
            "ssh -o BatchMode=yes -o ConnectTimeout=10 {} -p {}",
            HOST_KEY_OPTS.join(" "),
            self.port
        );
        if let Some(key) = &self.key {
            e.push_str(&format!(" -i {}", expand_tilde(key).display()));
        }
        e
    }

    /// Push a local path into the machine's worker root.
    ///
    /// `progress` streams throttled `[target] {label}: …% … MB/s` lines while
    /// the transfer runs — the difference between watching a 668 MB models
    /// push crawl and wondering whether it stalled. `None` keeps the silent
    /// push (segment collection, local copies).
    pub fn rsync_push(
        &self,
        src: &Path,
        remote_rel: &str,
        delete: bool,
        progress: Option<RsyncProgress<'_>>,
    ) -> Result<()> {
        if self.local {
            return self.rsync_push_local(src, remote_rel);
        }
        let dst = format!("{}:{}/{remote_rel}", self.target, REMOTE_DIR);
        let mut args: Vec<String> =
            vec!["-az".into(), "--no-perms".into(), RSYNC_IO_TIMEOUT.into()];
        if delete {
            args.push("--delete".into());
        }
        if progress.is_some() {
            // Per-file `%` (openrsync knows no `progress2`): the tracker
            // below turns it into throttled file n/N + speed lines.
            args.push("--progress".into());
        }
        args.push("-e".into());
        args.push(self.rsync_e());
        // Directories sync their CONTENTS (trailing slash). Without it rsync
        // nests: bm-worker/assets/assets — the exact bug this comment prevents.
        let mut src_s = src.to_string_lossy().to_string();
        if src.is_dir() && !src_s.ends_with('/') {
            src_s.push('/');
        }
        args.push(src_s);
        args.push(dst);
        let mut tracker = progress.map(|p| {
            ProgressTracker::new(p.tx.clone(), self.target.clone(), p.label.to_string())
        });
        let target = self.target.clone();
        let (code, _, stderr) = with_transport_retries(|| {
            let watch: OutputWatch<'_> = match tracker.as_mut() {
                Some(t) => Some(&mut |out: &str, err: &str| t.on_output(out, err)),
                None => None,
            };
            run_bounded_live(
                Command::new("rsync").args(&args),
                RSYNC_TIMEOUT_SECS,
                &format!("rsync push to {target}"),
                watch,
            )
        })?;
        if code != 0 {
            let hint = if stderr.contains("command not found") {
                " — rsync is not on this box; install it, e.g. sudo apt install -y rsync"
            } else {
                ""
            };
            anyhow::bail!(
                "rsync push failed: {}{hint} (to {}, exit {code})",
                crate::util::head_chars(&stderr, 300),
                self.target
            );
        }
        Ok(())
    }

    /// Local shortcut: copy inside the inductor's own `~/{REMOTE_DIR}`.
    fn rsync_push_local(&self, src: &Path, remote_rel: &str) -> Result<()> {
        let home = std::env::var("HOME").context("HOME not set")?;
        let dst = Path::new(&home).join(REMOTE_DIR).join(remote_rel);
        if src.is_dir() {
            copy_dir(src, &dst)?;
        } else {
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(src, &dst)
                .with_context(|| format!("copying {} -> {}", src.display(), dst.display()))?;
        }
        Ok(())
    }

    /// Pull a path from the machine's worker root into a local destination.
    pub fn rsync_pull(&self, remote_rel: &str, dst: &Path) -> Result<()> {
        if self.local {
            let home = std::env::var("HOME").context("HOME not set")?;
            let src = Path::new(&home).join(REMOTE_DIR).join(remote_rel);
            if src.is_dir() {
                return copy_dir(&src, dst);
            }
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&src, dst)
                .with_context(|| format!("copying {} -> {}", src.display(), dst.display()))?;
            return Ok(());
        }
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let src = format!("{}:{}/{remote_rel}", self.target, REMOTE_DIR);
        let target = self.target.clone();
        let e = self.rsync_e();
        let dst_s = dst.to_string_lossy().to_string();
        let (code, _, stderr) = with_transport_retries(|| {
            run_bounded(
                Command::new("rsync").args([
                    "-az",
                    "--no-perms",
                    RSYNC_IO_TIMEOUT,
                    "-e",
                    &e,
                    &src,
                    &dst_s,
                ]),
                RSYNC_TIMEOUT_SECS,
                &format!("rsync pull from {target}"),
            )
        })?;
        if code != 0 {
            anyhow::bail!(
                "rsync pull failed: {} (from {}, exit {code})",
                crate::util::head_chars(&stderr, 300),
                self.target
            );
        }
        Ok(())
    }
}

/// Live byte-progress for one rsync push: the sender every throttled
/// `[target] {label}: …` line goes to, plus the human label (`models`,
/// `agent`, …) those lines carry.
pub struct RsyncProgress<'a> {
    pub tx: &'a tokio::sync::mpsc::UnboundedSender<String>,
    pub label: &'a str,
}

/// Turns rsync `--progress` snapshots into throttled status lines. A changed
/// file-or-band emits (at most every 2 s, so a hundred tiny files don't
/// flood the pane); an unchanged one re-emits every 30 s as a heartbeat —
/// frozen values with advancing timestamps are exactly how a stall reads.
struct ProgressTracker {
    tx: tokio::sync::mpsc::UnboundedSender<String>,
    target: String,
    label: String,
    last_emit: Option<std::time::Instant>,
    last_key: Option<(String, u8)>,
}

impl ProgressTracker {
    fn new(
        tx: tokio::sync::mpsc::UnboundedSender<String>,
        target: String,
        label: String,
    ) -> Self {
        ProgressTracker {
            tx,
            target,
            label,
            last_emit: None,
            last_key: None,
        }
    }

    fn on_output(&mut self, stdout: &str, stderr: &str) {
        // Progress lands on stdout when piped; stderr is free insurance.
        let snapshot = format!("{stdout}\n{stderr}");
        let Some((file, bytes, pct, speed)) = parse_progress(&snapshot) else {
            return;
        };
        let key = (file.clone(), pct / 10);
        let now = std::time::Instant::now();
        let should = match (self.last_emit, self.last_key.as_ref() != Some(&key)) {
            (None, _) => true,
            (Some(t), true) => now.duration_since(t) >= std::time::Duration::from_secs(2),
            (Some(t), false) => now.duration_since(t) >= std::time::Duration::from_secs(30),
        };
        if !should {
            return;
        }
        let mb = bytes as f64 / 1048576.0;
        let _ = self.tx.send(format!(
            "[{}] {}: {file} {pct}% ({mb:.0} MB) @ {speed}",
            self.target, self.label,
        ));
        self.last_emit = Some(now);
        self.last_key = Some(key);
    }
}

/// The last `--progress` update in a snapshot: `(file, bytes, pct, speed)`.
/// rsync separates live updates with `\r` and files with `\n`, so both split
/// the scan; the current file is the last non-progress line. Only the final
/// line per file carries `(xfer#…)` — intermediate updates are bare
/// `bytes pct speed eta`, which is why the match is on that shape.
fn parse_progress(snapshot: &str) -> Option<(String, u64, u8, String)> {
    let mut file = None;
    let mut prog = None;
    for seg in snapshot.split(['\r', '\n']) {
        let seg = seg.trim();
        if seg.is_empty() {
            continue;
        }
        if let Some(p) = parse_progress_line(seg) {
            prog = Some(p);
        } else {
            file = Some(seg.to_string());
        }
    }
    prog.map(|(bytes, pct, speed)| (file.unwrap_or_default(), bytes, pct, speed))
}

/// One `--progress` update: `12,345 45% 2.10MB/s 0:01:23` (final line per
/// file appends `(xfer#5, to-check=120/400)`, parsed the same way).
/// Four whitespace fields with `%` on the second — a filename matching all
/// of that exactly is absurd enough to ignore.
fn parse_progress_line(seg: &str) -> Option<(u64, u8, String)> {
    let mut parts = seg.split_whitespace();
    let bytes: u64 = parts.next()?.replace(',', "").parse().ok()?;
    let pct: u8 = parts.next()?.strip_suffix('%')?.parse().ok()?;
    let speed = parts.next()?.to_string();
    let _eta = parts.next()?;
    Some((bytes, pct, speed))
}

fn output_file() -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    for _ in 0..128 {
        let path = std::env::temp_dir().join(format!(
            "bm-ssh-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        match std::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => {
                std::fs::remove_file(&path)?;
                return Ok(file);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "no unused output file name",
    ))
}

fn run_bounded(
    cmd: &mut Command,
    timeout_secs: u64,
    transport: &str,
) -> Result<(i32, String, String)> {
    run_bounded_live(cmd, timeout_secs, transport, None)
}

/// Same as [`run_bounded`], plus a watcher that sees stdout/stderr snapshots
/// while the child runs — the rsync push tails its own `--progress` output
/// through it. Called at most twice a second; `None` is today's behavior.
/// Tails a running child's stdout/stderr snapshots into the watcher.
/// `None` is today's fire-and-collect behavior.
type OutputWatch<'a> = Option<&'a mut dyn FnMut(&str, &str)>;

fn run_bounded_live(
    cmd: &mut Command,
    timeout_secs: u64,
    transport: &str,
    mut watch: OutputWatch<'_>,
) -> Result<(i32, String, String)> {
    use std::os::unix::fs::FileExt;
    let stdout = output_file().with_context(|| format!("creating stdout for {transport}"))?;
    let stderr = output_file().with_context(|| format!("creating stderr for {transport}"))?;
    let mut child = cmd
        .stdin(std::process::Stdio::null())
        .stdout(
            stdout
                .try_clone()
                .with_context(|| format!("cloning stdout for {transport}"))?,
        )
        .stderr(
            stderr
                .try_clone()
                .with_context(|| format!("cloning stderr for {transport}"))?,
        )
        .spawn()
        .with_context(|| format!("spawning {transport}"))?;
    // The read closure is defined before the loop so the watcher can reuse
    // it: output files only grow, and a 50 ms `read_exact_at` over megabytes
    // every poll would cost more than the rsync it watches.
    let read = |file: &std::fs::File| -> std::io::Result<String> {
        let mut bytes = vec![
            0;
            file.metadata()?.len().try_into().map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "output too large")
            })?
        ];
        file.read_exact_at(&mut bytes, 0)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs.max(1));
    let mut next_watch = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("{transport} timed out after {timeout_secs}s — the box (or its network) stalled mid-command");
            }
            Ok(None) => {
                std::thread::sleep(std::time::Duration::from_millis(50));
                // One snapshot per half second, not per poll: the files only
                // grow and the watcher parses from scratch each time.
                if let Some(w) = watch.as_mut() {
                    if std::time::Instant::now() >= next_watch {
                        next_watch =
                            std::time::Instant::now() + std::time::Duration::from_millis(500);
                        if let (Ok(o), Ok(e)) = (read(&stdout), read(&stderr)) {
                            w(&o, &e);
                        }
                    }
                }
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("waiting on {transport}: {e}");
            }
        }
    };
    Ok((
        status.code().unwrap_or(255),
        read(&stdout).with_context(|| format!("reading stdout from {transport}"))?,
        read(&stderr).with_context(|| format!("reading stderr from {transport}"))?,
    ))
}

fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            // never copy a virtualenv or build cache between machines
            let name = entry.file_name().to_string_lossy().to_string();
            if matches!(name.as_str(), ".venv" | "__pycache__" | "target") {
                continue;
            }
            copy_dir(&from, &to)?;
        } else {
            // Skip copy if destination file exists and has identical size & mtime
            if to.is_file() {
                if let (Ok(m_from), Ok(m_to)) = (from.metadata(), to.metadata()) {
                    if m_from.len() == m_to.len() {
                        if let (Ok(t_from), Ok(t_to)) = (m_from.modified(), m_to.modified()) {
                            if t_from == t_to {
                                continue;
                            }
                        }
                    }
                }
            }
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_parser_reads_the_last_update_and_its_file() {
        // Real `--progress` bytes (openrsync, piped: `\r` between updates,
        // and only the final line per file carries `(xfer#…)`). The last
        // update wins; the file is the last non-progress line.
        let snap = "weights.bin\r         262144  12%  255.37KB/s   00:00:07\r         655360  31%  192.06KB/s   00:00:07\r        2097152 100%  204.68KB/s   00:00:10 (xfer#1, to-check=0/1)\n";
        assert_eq!(
            parse_progress(snap),
            Some(("weights.bin".into(), 2097152, 100, "204.68KB/s".into()))
        );
        // Multi-file: percent resets per file, and the bare intermediate
        // updates (no xfer suffix) must parse as updates, never as filenames.
        let snap2 = "a.bin\r          100 100%  1.00MB/s   00:00:00 (xfer#1, to-check=1/2)\nb.bin\r           50  25%  1.00MB/s   00:00:01\r";
        assert_eq!(
            parse_progress(snap2),
            Some(("b.bin".into(), 50, 25, "1.00MB/s".into()))
        );
        assert_eq!(parse_progress(""), None);
        assert_eq!(parse_progress("sending incremental file list\n"), None);
    }

    #[test]
    fn rsync_watch_streams_progress_while_the_child_runs() {
        // A fake slow transfer through the real runner: progress-shaped
        // stdout for ~1.5 s. First update emits immediately; same-band
        // repeats inside the 2 s window stay silent.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut tracker = ProgressTracker::new(tx, "t@h".into(), "models".into());
        let mut cmd = std::process::Command::new("sh");
        cmd.args([
            "-c",
            "echo big.bin; for i in 1 2 3 4 5 6; do echo '  100 10% 1.00MB/s 00:00:01'; sleep 0.25; done",
        ]);
        let watch: OutputWatch<'_> =
            Some(&mut |out: &str, err: &str| tracker.on_output(out, err));
        let (code, _, _) = run_bounded_live(&mut cmd, 30, "fake push", watch).unwrap();
        assert_eq!(code, 0);
        let line = rx.try_recv().expect("first update streams immediately");
        assert_eq!(line, "[t@h] models: big.bin 10% (0 MB) @ 1.00MB/s");
        assert!(
            rx.try_recv().is_err(),
            "same-band repeats inside 2 s stay silent"
        );
    }

    #[test]
    fn ssh_run_honours_its_timeout_instead_of_blocking_forever() {
        // The swap-voice hang: a never-exiting remote launch wedged the TUI's
        // serial job queue because `run` ignored `timeout_secs`. `sleep` stands
        // in for the wedged command; the local path runs the same wait loop.
        let ssh = Ssh {
            target: "local".into(),
            port: 22,
            key: None,
            local: true,
        };
        let t = std::time::Instant::now();
        let err = ssh.run("exec sleep 30", 1).unwrap_err();
        assert!(
            t.elapsed() < std::time::Duration::from_secs(10),
            "must die near the deadline, not after the sleep"
        );
        assert!(err.to_string().contains("timed out after 1s"), "got: {err}");

        let (code, out, _) = ssh.run("echo hi", 10).expect("a fast command still runs");
        assert_eq!((code, out.trim()), (0, "hi"));
    }

    #[test]
    fn ssh_run_captures_large_stdout_and_stderr() {
        let ssh = Ssh {
            target: "local".into(),
            port: 22,
            key: None,
            local: true,
        };
        let (code, out, err) = ssh.run(
            "dd if=/dev/zero bs=1048576 count=2 2>/dev/null; { dd if=/dev/zero bs=1048576 count=2 2>/dev/null; } >&2; exit 7",
            5,
        ).unwrap();
        assert_eq!(code, 7);
        assert_eq!(out.as_bytes(), vec![0; 2 * 1048576]);
        assert_eq!(err.as_bytes(), vec![0; 2 * 1048576]);
    }

    #[test]
    fn ssh_run_does_not_wait_for_detached_output_handles() {
        let ssh = Ssh {
            target: "local".into(),
            port: 22,
            key: None,
            local: true,
        };
        let start = std::time::Instant::now();
        let result = ssh
            .run("sleep 5 & echo started; echo warning >&2; exit 7", 1)
            .unwrap();
        assert!(start.elapsed() < std::time::Duration::from_secs(3));
        assert_eq!(result, (7, "started\n".into(), "warning\n".into()));
    }

    #[test]
    fn bounded_runner_preserves_transport_context() {
        let err = run_bounded(
            Command::new("sh").args(["-c", "exec sleep 30"]),
            1,
            "rsync pull from user@host",
        )
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("rsync pull from user@host timed out after 1s"));
        let err = run_bounded(
            &mut Command::new("/nonexistent/bm-rsync"),
            1,
            "rsync push to user@host",
        )
        .unwrap_err();
        assert!(err.to_string().contains("spawning rsync push to user@host"));
    }

    #[test]
    fn ssh_argv_expands_tilde_in_the_key_for_both_transports() {
        let _env = crate::ENV_LOCK.lock().unwrap();
        // The ledger held `~/.ssh/ssh-key-my-wsl` verbatim; ssh (no shell)
        // failed it while rsync (shell) expanded it. Both now go through
        // expand_tilde, so `-i` always names a real path.
        let home = std::env::var("HOME").unwrap();
        let ssh = Ssh {
            target: "thang@192.168.2.2".into(),
            port: 22,
            key: Some("~/.ssh/k".into()),
            local: false,
        };
        let args = ssh.ssh_args();
        let i = args
            .iter()
            .position(|a| a == "-i")
            .expect("key flag present");
        assert_eq!(args[i + 1], format!("{home}/.ssh/k"), "ssh argv: {args:?}");
        assert_eq!(
            ssh.rsync_e(),
            format!(
                "ssh -o BatchMode=yes -o ConnectTimeout=10 {} -p 22 -i {home}/.ssh/k",
                HOST_KEY_OPTS.join(" ")
            )
        );

        let bare = Ssh {
            target: "t@h".into(),
            port: 2222,
            key: None,
            local: false,
        };
        assert!(
            !bare.ssh_args().contains(&"-i".to_string()),
            "no key, no flag"
        );
        assert!(!bare.rsync_e().contains("-i"), "no key, no flag");
    }

    #[test]
    fn resolve_key_prefers_box_then_settings_then_ssh_default() {
        let _env = crate::ENV_LOCK.lock().unwrap();
        let home = std::env::var("HOME").unwrap();
        let (p, src) = resolve_key(Some("~/.ssh/box-k"), Some("~/.ssh/app-k"));
        assert_eq!(
            (p.unwrap(), src),
            (PathBuf::from(format!("{home}/.ssh/box-k")), KeySource::Box)
        );
        let (p, src) = resolve_key(None, Some("/k/app"));
        assert_eq!(
            (p.unwrap(), src),
            (PathBuf::from("/k/app"), KeySource::Settings)
        );
        // Empty strings fall through: clearing the field unsets the key.
        let (p, src) = resolve_key(Some("  "), Some(""));
        assert_eq!((p, src), (None, KeySource::SshDefault));
        let (p, src) = resolve_key(None, None);
        assert_eq!((p, src), (None, KeySource::SshDefault));
        assert_eq!(KeySource::Box.label(), "machines.json");
    }

    #[test]
    fn ssh_never_prompts_never_lingers() {
        // Every ssh use is scripted: no stdin, no password prompts, and a
        // stalled connection must die instead of hanging a TUI job forever.
        let args = Ssh::for_machine(&Machine::new("192.168.2.2", "thang", 22, None, "worker"))
            .ssh_args()
            .join(" ");
        for flag in [
            "-n",
            "BatchMode=yes",
            "ConnectTimeout=10",
            "ServerAliveInterval=5",
            "ServerAliveCountMax=2",
        ] {
            assert!(args.contains(flag), "{args}");
        }
    }

    #[test]
    fn both_transports_decline_host_key_verification() {
        // The bug this pins: a freshly launched EC2 instance presents a host key
        // nobody has seen, and `BatchMode=yes` forbids the prompt, so every
        // first provision died with `exit 255: Host key verification failed`.
        // The fix has to be on *both* transports — rsync spawns its own ssh, so
        // a policy on the direct one alone would fix `probe` and leave every
        // push failing identically.
        let ssh = Ssh::for_machine(&Machine::new("3.121.112.113", "ubuntu", 22, None, "worker"));

        let args = ssh.ssh_args().join(" ");
        assert!(args.contains("StrictHostKeyChecking=no"), "{args}");
        assert!(args.contains("UserKnownHostsFile=/dev/null"), "{args}");
        // `~/.ssh/config` must still be read: an -o overrides one option, it
        // does not replace the file. `-F /dev/null` would silently drop Host
        // aliases, ProxyJump and IdentityFile.
        assert!(!args.contains("-F /dev/null"), "{args}");
        assert!(!args.contains("-F/dev/null"), "{args}");

        let e = ssh.rsync_e();
        assert!(e.contains("StrictHostKeyChecking=no"), "{e}");
        assert!(e.contains("UserKnownHostsFile=/dev/null"), "{e}");
        assert!(!e.contains("-F /dev/null"), "{e}");

        // A local machine never goes through either: it runs `sh` in place.
        let local = Ssh::for_machine(&Machine::new("127.0.0.1", "me", 22, None, "worker"));
        assert!(local.local, "the local node must not be ssh'd to");
    }

    #[test]
    fn localhost_is_detected_as_local() {
        for addr in ["127.0.0.1", "localhost", "::1"] {
            let m = Machine::new(addr, "me", 22, None, "worker");
            assert!(Ssh::for_machine(&m).local, "{addr} should be local");
        }
        let remote = Machine::new("192.168.2.2", "thang", 22, None, "worker");
        assert!(!Ssh::for_machine(&remote).local);
    }

    #[test]
    fn copy_dir_skips_venv_and_caches() {
        let src = std::env::temp_dir().join("bm-provision-src");
        let dst = std::env::temp_dir().join("bm-provision-dst");
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dst);
        std::fs::create_dir_all(src.join(".venv")).unwrap();
        std::fs::create_dir_all(src.join("__pycache__")).unwrap();
        std::fs::write(src.join("tts_server.py"), "x").unwrap();
        std::fs::write(src.join(".venv/marker"), "x").unwrap();
        copy_dir(&src, &dst).unwrap();
        assert!(dst.join("tts_server.py").exists());
        assert!(!dst.join(".venv").exists(), "venv must never be copied");
        assert!(!dst.join("__pycache__").exists());
    }

    #[test]
    fn ssh_local_agrees_with_is_local_node() {
        // One predicate, one place: the provisioner's `Ssh.local` and the
        // offer's `local_node` flag must never disagree.
        for addr in ["127.0.0.1", "localhost", "::1", "192.168.2.2", "10.0.0.5"] {
            let m = Machine::new(addr, "u", 22, None, "worker");
            assert_eq!(
                Ssh::for_machine(&m).local,
                crate::is_local_node(addr),
                "{addr}"
            );
        }
    }

    #[test]
    fn ssh_args_include_port_and_key_only_when_set() {
        let m = Machine::new("10.0.0.5", "pi", 2222, Some("/k/id".into()), "worker");
        let ssh = Ssh::for_machine(&m);
        let args = ssh.ssh_args();
        assert!(args.contains(&"-p".to_string()));
        assert!(args.contains(&"2222".to_string()));
        assert!(args.contains(&"/k/id".to_string()));
        assert_eq!(args.last().unwrap(), "pi@10.0.0.5");

        let m2 = Machine::new("10.0.0.6", "pi", 22, None, "worker");
        let args2 = Ssh::for_machine(&m2).ssh_args();
        assert!(!args2.contains(&"-p".to_string()));
        assert!(!args2.contains(&"-i".to_string()));
    }

    #[test]
    fn copy_dir_leaves_an_unchanged_signature_alone() {
        // The local fast path compares size+mtime, exactly like rsync. To prove
        // the skip (a real identical file cannot be told apart anyway), the
        // destination is given different *content* with the same signature: if
        // it is copied over, the comparison did not happen.
        let src = std::env::temp_dir().join("bm-copy-skip-src");
        let dst = std::env::temp_dir().join("bm-copy-skip-dst");
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dst);
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.txt"), "same").unwrap();
        copy_dir(&src, &dst).unwrap();

        std::fs::write(dst.join("a.txt"), "diff").unwrap();
        let t = std::fs::metadata(src.join("a.txt"))
            .unwrap()
            .modified()
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(dst.join("a.txt"))
            .unwrap()
            .set_modified(t)
            .unwrap();
        copy_dir(&src, &dst).unwrap();
        assert_eq!(
            std::fs::read_to_string(dst.join("a.txt")).unwrap(),
            "diff",
            "an identical size+mtime must not be re-copied"
        );

        // A changed size is a real change and must be copied.
        std::fs::write(src.join("a.txt"), "a longer body").unwrap();
        copy_dir(&src, &dst).unwrap();
        assert_eq!(
            std::fs::read_to_string(dst.join("a.txt")).unwrap(),
            "a longer body"
        );
    }

    #[test]
    fn only_blips_retry_never_auth_or_host_key() {
        // The exact strings a flapping link produces — and the two that must
        // fail fast instead of burning four attempts of backoff.
        for (code, stderr) in [
            (255, "ssh: connect to host h port 22: Operation timed out"),
            (255, "ssh: connect to host h port 22: Connection refused"),
            (255, "Connection reset by peer"),
            (255, "rsync: connection unexpectedly closed"),
            (255, "rsync error: timeout waiting for daemon (30)"),
            (10, "rsync error: error in socket IO (code 10)"),
            (255, "client_loop: send disconnect: Broken pipe"),
        ] {
            assert!(transient_failure(code, stderr), "{stderr}");
        }
        for (code, stderr) in [
            (255, "Permission denied (publickey)"),
            (255, "Host key verification failed."),
            (1, "Operation timed out"),
        ] {
            assert!(!transient_failure(code, stderr), "{stderr}");
        }
    }

    #[test]
    fn transport_retries_a_blip_then_returns_success() {
        // One 2 s backoff, then the identical call succeeds — the flap `:prov`
        // used to die on.
        let mut n = 0;
        let (code, _, _) = with_transport_retries(|| {
            n += 1;
            if n < 2 {
                Ok((
                    255,
                    String::new(),
                    "ssh: connect to host h port 22: Operation timed out".into(),
                ))
            } else {
                Ok((0, "ok".into(), String::new()))
            }
        })
        .unwrap();
        assert_eq!((code, n), (0, 2));
    }
}
