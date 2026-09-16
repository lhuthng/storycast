use anyhow::{Context, Result};
use bm_proto::Machine;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

const RSYNC_TIMEOUT_SECS: u64 = 1800;
const RSYNC_IO_TIMEOUT: &str = "--timeout=120";

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
        let mut args = vec![
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
        run_bounded(&mut cmd, timeout_secs, &transport)
    }

    fn rsync_e(&self) -> String {
        let mut e = format!("ssh -o BatchMode=yes -o ConnectTimeout=10 -p {}", self.port);
        if let Some(key) = &self.key {
            e.push_str(&format!(" -i {}", expand_tilde(key).display()));
        }
        e
    }

    /// Push a local path into the machine's worker root.
    pub fn rsync_push(&self, src: &Path, remote_rel: &str, delete: bool) -> Result<()> {
        if self.local {
            return self.rsync_push_local(src, remote_rel);
        }
        let dst = format!("{}:{}/{remote_rel}", self.target, REMOTE_DIR);
        let mut args: Vec<String> =
            vec!["-az".into(), "--no-perms".into(), RSYNC_IO_TIMEOUT.into()];
        if delete {
            args.push("--delete".into());
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
        let (code, _, stderr) = run_bounded(
            Command::new("rsync").args(&args),
            RSYNC_TIMEOUT_SECS,
            &format!("rsync push to {}", self.target),
        )?;
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
        let (code, _, stderr) = run_bounded(
            Command::new("rsync").args([
                "-az",
                "--no-perms",
                RSYNC_IO_TIMEOUT,
                "-e",
                &self.rsync_e(),
                &src,
                &dst.to_string_lossy(),
            ]),
            RSYNC_TIMEOUT_SECS,
            &format!("rsync pull from {}", self.target),
        )?;
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
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs.max(1));
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("{transport} timed out after {timeout_secs}s — the box (or its network) stalled mid-command");
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("waiting on {transport}: {e}");
            }
        }
    };
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
            format!("ssh -o BatchMode=yes -o ConnectTimeout=10 -p 22 -i {home}/.ssh/k")
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
}
