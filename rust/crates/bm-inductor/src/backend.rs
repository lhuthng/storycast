//! Local backend lifecycle: start/stop the inductor + worker from the TUI.
//!
//! The TUI is only a client of the inductor API — with nothing behind it, it
//! is a nicely rendered empty dashboard. These helpers let it bootstrap a
//! solo backend itself: an inductor and a worker as detached background
//! processes with logs under `.bm/`, so the whole pipeline runs from the one
//! terminal the TUI owns.
//!
//! PID files (`.bm/inductor.pid`, `.bm/agent.pid`) record what *this TUI*
//! started locally. `X` goes further: it stops every worker in the cluster —
//! the local ones by PID file plus a sweep for strays, the remote ones over
//! ssh — because a half-stopped cluster silently keeps rendering.
//!
//! Starting works degraded-first: the backend goes up immediately, then each
//! box provisions in the background and joins as it becomes ready. A failing
//! box lands in Error with its reason — it never vetoes the rest, because the
//! scheduler only offers tasks to beating workers anyway.

use bm_core::provision::{Ssh, REMOTE_DIR};
use bm_proto::Machine;
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// Parse the port out of an API base URL (`http://127.0.0.1:8901` → 8901).
pub fn api_port(api: &str) -> u16 {
    api.trim_end_matches('/')
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8901)
}

/// The host an API base URL points at, lowercased and without port.
fn api_host(api: &str) -> String {
    let s = api.trim();
    let s = s.split("://").nth(1).unwrap_or(s);
    let s = s.split('/').next().unwrap_or(s);
    // `[::1]:8901` → `::1`.
    if let Some((h, _)) = s.rsplit_once("]:") {
        return h.strip_prefix('[').unwrap_or(h).to_lowercase();
    }
    match s.rsplit_once(':') {
        // `host:port` → `host`; a second colon means a bare IPv6 literal.
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !h.contains(':') => {
            h.to_lowercase()
        }
        _ => s.to_lowercase(),
    }
}

/// Whether the TUI's API URL is this machine: only then may a backend be
/// spawned here. Starting a "local" backend while watching a remote inductor
/// would orphan two processes nobody is looking at.
fn api_is_local(api: &str) -> bool {
    matches!(api_host(api).as_str(), "127.0.0.1" | "localhost" | "::1")
}

/// Single-quote a shell word. Paths in the wild contain spaces
/// (`Documents SSD`), and an unquoted redirect target splits in two.
fn shq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Whether a PID is alive right now. `kill -0` sends no signal; it only asks.
pub fn is_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn pid_file(root: &Path, name: &str) -> PathBuf {
    root.join(".bm").join(format!("{name}.pid"))
}

fn log_file(root: &Path, name: &str) -> PathBuf {
    root.join(".bm").join(format!("{name}.log"))
}

fn read_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// The binary to run: this executable itself for `bm-inductor`, its sibling
/// for `bm-agent`. Resolved from `current_exe` rather than `PATH` so a `/tmp`
/// copy starts `/tmp` binaries, not whatever happens to be installed.
fn sibling_bin(name: &str) -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let p = if exe.file_name().map(|n| n == name).unwrap_or(false) {
        exe.clone()
    } else {
        exe.parent().map(|d| d.join(name)).unwrap_or_else(|| PathBuf::from(name))
    };
    if p.is_file() {
        Ok(p)
    } else {
        anyhow::bail!("no {name} binary next to {}", exe.display())
    }
}

/// Run `bin args…` detached: immune to hangup, stdio to the log, PID back.
/// `nohup … & echo $!` prints the background PID and exits at once, so the
/// TUI never blocks on a server boot that takes seconds.
fn spawn_one(bin: &Path, args: &[String], log: &Path) -> anyhow::Result<u32> {
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut cmd = shq(&bin.to_string_lossy());
    for a in args {
        cmd.push(' ');
        cmd.push_str(&shq(a));
    }
    let script = format!("nohup {cmd} >> {} 2>&1 & echo $!", shq(&log.to_string_lossy()));
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(&script)
        .stdin(Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(Stdio::null())
        .output()?;
    if !out.status.success() {
        anyhow::bail!("spawning {} failed", bin.display());
    }
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("could not read the background PID for {}", bin.display()))
}

/// Args for the spawned inductor. The reconcile is deliberately EMPTY: booting
/// a backend must never invent work — a default range once auto-ran ch21
/// uninvited. Chapters arrive only through explicit enqueue (run screen,
/// `t`, API); a hand-run `serve --start/--count` keeps its own scope.
/// `bind` is LAN-wide when remote workers exist, loopback for solo runs.
fn serve_args(port: &str, bind: &str) -> Vec<String> {
    ["serve", "--port", port, "--bind", bind, "--start", "1", "--count", "0"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Where the inductor listens: remote workers dial the LAN address, so a
/// cluster backend must leave loopback. Pure so the choice is testable.
pub fn public_bind(has_remotes: bool) -> &'static str {
    if has_remotes {
        "0.0.0.0"
    } else {
        "127.0.0.1"
    }
}

/// This machine's LAN address for remote workers to dial. macOS first
/// (`ipconfig`), then the interface table itself (`ipconfig` stays silent on
/// statically-addressed interfaces — exactly how lab NICs are configured).
pub fn lan_ip() -> anyhow::Result<String> {
    for iface in ["en0", "en1"] {
        if let Some(ip) = iface_ip(iface) {
            return Ok(ip);
        }
    }
    Err(anyhow::anyhow!("no usable address on en0/en1"))
}

/// Parse the dial-back address for one remote: ask the routing table which
/// interface reaches it (`route get` on macOS, `ip route get` on Linux), then
/// read that interface's address. A multi-homed inductor (lab NIC + wifi)
/// must hand each box the address on *its* subnet — the first address found
/// is routinely the wrong one.
pub fn dial_back_ip(remote: &str) -> anyhow::Result<String> {
    let out = std::process::Command::new("route").arg("get").arg(remote).output();
    if let Ok(o) = out {
        let text = String::from_utf8_lossy(&o.stdout).to_string();
        if let Some(iface) = parse_route_iface(&text) {
            if let Some(ip) = iface_ip(&iface) {
                return Ok(ip);
            }
        }
    }
    let out = std::process::Command::new("ip").args(["route", "get", remote]).output();
    if let Ok(o) = out {
        let text = String::from_utf8_lossy(&o.stdout).to_string();
        if let Some(ip) = parse_route_src(&text) {
            return Ok(ip);
        }
    }
    lan_ip()
}

fn usable_ipv4(s: &str) -> bool {
    s != "127.0.0.1"
        && s.split('.').count() == 4
        && s.chars().all(|c| c.is_ascii_digit() || c == '.')
        && s.split('.').all(|o| o.parse::<u8>().is_ok())
}

/// Address of one interface: DHCP answer first, interface table second.
fn iface_ip(iface: &str) -> Option<String> {
    let out = std::process::Command::new("ipconfig").args(["getifaddr", iface]).output().ok()?;
    let ip = String::from_utf8_lossy(&out.stdout).split_whitespace().next().unwrap_or("").to_string();
    if usable_ipv4(&ip) {
        return Some(ip);
    }
    let out = std::process::Command::new("ifconfig").arg(iface).output().ok()?;
    parse_ifconfig_inet(&String::from_utf8_lossy(&out.stdout))
}

/// `route get 1.2.3.4` → `interface: en0` → `en0`. macOS.
fn parse_route_iface(out: &str) -> Option<String> {
    out.lines().find_map(|l| {
        let t = l.trim();
        t.strip_prefix("interface:").map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
    })
}

/// `1.2.3.4 via 9.9.9.9 dev eth0 src 10.0.0.5 ...` → `10.0.0.5`. Linux.
fn parse_route_src(out: &str) -> Option<String> {
    let words: Vec<&str> = out.split_whitespace().collect();
    words.iter().position(|w| *w == "src").and_then(|i| words.get(i + 1)).map(|s| s.to_string()).filter(|s| usable_ipv4(s))
}

/// First `inet A.B.C.D` (not `inet6`) in `ifconfig` output.
fn parse_ifconfig_inet(out: &str) -> Option<String> {
    out.lines().find_map(|l| {
        let t = l.trim();
        let rest = t.strip_prefix("inet ")?;
        let ip = rest.split_whitespace().next()?;
        usable_ipv4(ip).then(|| ip.to_string())
    })
}

/// Remote worker launch scripts. See `worker_kill_script` for why patterns
/// are bracketed: pkill/pgrep -f match full command lines, and a script's own
/// text must never match.
///
/// Two separate scripts, deliberately: the check script holds only bracketed
/// patterns (safe beside pgrep), while the launch script holds the plain
/// binary name but no pkill — so neither can match itself. Combining them
/// reintroduces the kill-own-shell bug through the launch line's literal.
fn remote_worker_check_script() -> String {
    "pgrep -f 'bm-agent worke[r]' | tr '\\n' ',' | sed 's/,$//;s/^/ALREADY:/'".into()
}

fn remote_worker_launch_script(inductor_url: &str, addr: &str) -> String {
    format!(
        "cd \"$HOME/{d}\" && nohup ./bm-agent worker --inductor {} --addr {} >> agent.log 2>&1 & echo STARTED:$!",
        shq(inductor_url),
        shq(addr),
        d = REMOTE_DIR,
    )
}
/// Start a worker on every remote machine (blocking): skip boxes that already
/// run one, launch the rest detached into `~/bm-worker/agent.log`. Each box
/// gets the inductor URL on its own subnet (see `dial_back_ip`).
/// Returns `(all_up, lines)` — a failed launch vetoes the start like a failed
/// provision does, so `B` never leaves a half-started cluster.
pub fn start_remote_workers(machines: &[Machine], api_port: u16) -> (bool, Vec<String>) {
    let mut lines = Vec::new();
    let mut ok = true;
    let mut remotes: Vec<&Machine> =
        machines.iter().filter(|m| !is_local_addr(&m.addr)).collect();
    remotes.sort_by(|a, b| a.addr.cmp(&b.addr));
    remotes.dedup_by(|a, b| a.addr == b.addr);
    for m in remotes {
        let inductor_url = match dial_back_ip(&m.addr) {
            Ok(ip) => format!("http://{ip}:{api_port}"),
            Err(e) => {
                lines.push(format!("[{}] no dial-back address ({e:#})", m.addr));
                ok = false;
                continue;
            }
        };
        // pgrep first: a second worker per box is harmless but noisy, and the
        // report should say what actually happened.
        let ssh = Ssh::for_machine(m);
        // Check first, launch second (two round trips — see the builders for
        // why one combined script is a self-match trap).
        let already: Option<String> = match ssh.run(&remote_worker_check_script(), 30) {
            Ok((_, stdout, _)) => stdout.trim().strip_prefix("ALREADY:").map(|s| s.to_string()),
            Err(e) => {
                lines.push(format!("[{}] worker check failed ({e:#})", m.addr));
                ok = false;
                continue;
            }
        };
        match already {
            Some(pids) if !pids.is_empty() => {
                lines.push(format!("[{}] worker already running (pid {pids})", m.addr));
                continue;
            }
            _ => {}
        }
        let script = remote_worker_launch_script(&inductor_url, &m.addr);
        match ssh.run(&script, 60) {
            Ok((0, stdout, _)) => {
                let out = stdout.trim().to_string();
                if let Some(pids) = out.strip_prefix("ALREADY:") {
                    lines.push(format!("[{}] worker already running (pid {pids})", m.addr));
                } else if let Some(pid) = out.strip_prefix("STARTED:") {
                    lines.push(format!("[{}] worker started (pid {pid})", m.addr));
                } else {
                    lines.push(format!("[{}] unexpected worker start output: {out}", m.addr));
                    ok = false;
                }
            }
            Ok((code, _, stderr)) => {
                lines.push(format!("[{}] worker start failed (exit {code}): {}", m.addr, stderr.trim()));
                ok = false;
            }
            Err(e) => {
                lines.push(format!("[{}] worker start failed ({e:#})", m.addr));
                ok = false;
            }
        }
    }
    (ok, lines)
}

/// Start whichever half of the local backend is missing. Never fails hard over
/// one half: a dead worker must not block the inductor, and vice versa. Lines
/// are facts for the event log; the TUI's refresh loop flips the status to
/// live on its own once the inductor answers. `bind` comes from
/// `public_bind`: LAN-wide when the cluster has remotes. `with_worker` false
/// spawns the inductor only — the degraded start launches each box's worker
/// after that box provisions, so an unready box never takes failing tasks.
pub fn start_backend(layout_root: &Path, api: &str, api_up: bool, bind: &str, with_worker: bool) -> anyhow::Result<Vec<String>> {
    if layout_root.as_os_str().is_empty() {
        anyhow::bail!("no repo root — restart the TUI from a checkout");
    }
    if !api_is_local(api) {
        anyhow::bail!("this starts a backend on THIS machine, but the TUI watches {api}");
    }
    let bin_i = sibling_bin("bm-inductor")?;
    let port = api_port(api).to_string();

    let mut lines = Vec::new();
    // Inductor half: skip when anything already answers — a second inductor on
    // the same port would just die on bind, noisily, in the log.
    if api_up {
        lines.push(format!("inductor already answering at {api} — not started again"));
    } else if let Some(pid) = read_pid(&pid_file(layout_root, "inductor")).filter(|p| is_alive(*p)) {
        lines.push(format!("inductor already running (pid {pid})"));
    } else {
        let log = log_file(layout_root, "inductor");
        let args = serve_args(&port, bind);
        match spawn_one(&bin_i, &args, &log) {
            Ok(pid) => {
                let _ = std::fs::write(pid_file(layout_root, "inductor"), pid.to_string());
                lines.push(format!("inductor starting in background (pid {pid}, {})", log.display()));
            }
            Err(e) => lines.push(format!("inductor failed to start: {e:#}")),
        }
    }
    // Worker half unless the caller stages it per-box (degraded start):
    // extra workers are harmless, so only our own PID suppresses.
    if with_worker {
        lines.extend(start_local_worker(layout_root, api));
    }
    Ok(lines)
}

/// Start the local worker unless this TUI already runs one. Split out so a
/// single-box `p` retry can launch exactly its box's worker without touching
/// the inductor half.
pub fn start_local_worker(layout_root: &Path, api: &str) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(pid) = read_pid(&pid_file(layout_root, "agent")).filter(|p| is_alive(*p)) {
        lines.push(format!("worker already running (pid {pid})"));
        return lines;
    }
    let bin_a = match sibling_bin("bm-agent") {
        Ok(b) => b,
        Err(e) => {
            lines.push(format!("worker failed to start: {e:#}"));
            return lines;
        }
    };
    let log = log_file(layout_root, "agent");
    let args = ["worker".to_string(), "--inductor".to_string(), api.to_string()];
    match spawn_one(&bin_a, &args, &log) {
        Ok(pid) => {
            let _ = std::fs::write(pid_file(layout_root, "agent"), pid.to_string());
            lines.push(format!("worker starting in background (pid {pid}, {})", log.display()));
        }
        Err(e) => lines.push(format!("worker failed to start: {e:#}")),
    }
    lines
}

fn signal(pid: u32, sig: &str) {
    let _ = std::process::Command::new("kill")
        .arg(format!("-{sig}"))
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Stop what this TUI started, by PID file. Anything without a PID file —
/// someone's tmux session, a hand-started backend — is reported, never killed.
/// (The cluster-wide sweep in `stop_everywhere` is the one that kills those.)
pub async fn stop_backend(layout_root: &Path) -> Vec<String> {
    let mut lines = Vec::new();
    for name in ["inductor", "agent"] {
        let pid_path = pid_file(layout_root, name);
        match read_pid(&pid_path) {
            None => lines.push(format!("{name}: not started by this TUI — nothing to stop")),
            Some(pid) if !is_alive(pid) => {
                let _ = std::fs::remove_file(&pid_path);
                lines.push(format!("{name}: already stopped (cleaned stale pid {pid})"));
            }
            Some(pid) => {
                signal(pid, "TERM");
                tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                if is_alive(pid) {
                    signal(pid, "KILL");
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                }
                if is_alive(pid) {
                    lines.push(format!("{name}: would not die (pid {pid}) — kill it by hand"));
                } else {
                    let _ = std::fs::remove_file(&pid_path);
                    lines.push(format!("{name}: stopped (was pid {pid})"));
                }
            }
        }
    }
    lines
}

/// Kill script for worker processes. The `[r]` is load-bearing: `pkill -f`
/// matches full command lines, so a plain `bm-agent worker` pattern would
/// match this very command and kill its own shell. The bracketed form matches
/// `bm-agent worker` without ever matching its own literal.
fn worker_kill_script() -> String {
    "pkill -f 'bm-agent worke[r]' 2>/dev/null; sleep 1; \
     pkill -9 -f 'bm-agent worke[r]' 2>/dev/null; sleep 1; \
     echo left=$(pgrep -f 'bm-agent worke[r]' 2>/dev/null | wc -l)".into()
}

/// Same for the per-render TTS sidecar: orphaned servers hold the port and
/// gigabytes of weights, and the next worker would just inherit the stale one.
fn sidecar_kill_script() -> String {
    "pkill -f 'tts_server\\.p[y]' 2>/dev/null; sleep 1; \
     pkill -9 -f 'tts_server\\.p[y]' 2>/dev/null; sleep 1; \
     echo left=$(pgrep -f 'tts_server\\.p[y]' 2>/dev/null | wc -l)".into()
}

pub(crate) fn is_local_addr(addr: &str) -> bool {
    matches!(addr, "127.0.0.1" | "localhost" | "::1")
}

/// Sweep one machine for leftover workers + sidecars. Returns the report
/// lines; the ssh transport failing is a report, never an error.
fn sweep_one(label: &str, ssh: &Ssh) -> Vec<String> {
    let mut out = Vec::new();
    for (what, script) in [("worker", worker_kill_script()), ("sidecar", sidecar_kill_script())] {
        match ssh.run(&script, 30) {
            Ok((_, stdout, _)) => {
                let left = stdout
                    .lines()
                    .rev()
                    .find_map(|l| {
                        l.trim()
                            .strip_prefix("left=")
                            .and_then(|n| n.trim().parse::<u32>().ok())
                    })
                    .unwrap_or(99);
                if left == 0 {
                    out.push(format!("{label}: {what}s stopped"));
                } else {
                    out.push(format!("{label}: {what}s stopped ({left} would not die — kill by hand)"));
                }
            }
            Err(e) => out.push(format!("{label}: sweep failed ({e:#}) — anything there keeps running")),
        }
    }
    out
}

/// Stop every worker in the cluster: the local backend by PID file, then a
/// local sweep for strays (hand-started shells, tmux), then every registered
/// remote machine over ssh. Tasks stranded on dead workers are requeued into
/// the ledger the moment no inductor answers — so stop-then-start loses
/// nothing to lease waits (a render lease is 90 minutes). Never fails hard —
/// each machine reports its own outcome, and one unreachable box never
/// silences the rest.
pub async fn stop_everywhere(layout_root: &Path, machines: &[Machine], api: &str) -> Vec<String> {
    let mut lines = stop_backend(layout_root).await;
    // Requeue stranded assignments, but only with no live scheduler: the file
    // is the inductor's to write while it answers.
    if inductor_up(api).await {
        lines.push("inductor still answering — ledger untouched (assigned tasks keep their leases)".into());
    } else {
        match requeue_assigned_in_ledger(layout_root) {
            Ok(back) if back.is_empty() => lines.push("no stranded tasks — nothing requeued".into()),
            Ok(back) => lines.push(format!(
                "requeued {} stranded task(s): {}",
                back.len(),
                back.iter().take(6).cloned().collect::<Vec<_>>().join(", ")
            )),
            Err(e) => lines.push(format!("ledger requeue failed ({e:#}) — stranded tasks keep leases")),
        }
    }
    // Sweeps block (ssh timeouts, kill grace periods) — off the runtime.
    let local_sweep = tokio::task::spawn_blocking(|| {
        let local = Ssh { target: "local".into(), port: 22, key: None, local: true };
        sweep_one("local", &local)
    })
    .await;
    match local_sweep {
        Ok(ls) => lines.extend(ls),
        Err(e) => lines.push(format!("local sweep task failed ({e})")),
    }
    let mut remotes: Vec<&Machine> =
        machines.iter().filter(|m| !is_local_addr(&m.addr)).collect();
    remotes.sort_by(|a, b| a.addr.cmp(&b.addr));
    remotes.dedup_by(|a, b| a.addr == b.addr);
    for m in remotes {
        let addr = m.addr.clone();
        let ssh = Ssh::for_machine(m);
        let swept = tokio::task::spawn_blocking(move || sweep_one(&addr, &ssh)).await;
        match swept {
            Ok(ls) => lines.extend(ls),
            Err(e) => lines.push(format!("[{}] sweep task failed ({e})", m.addr)),
        }
    }
    lines
}

/// Stop only the inductor half by PID file, leaving workers running: they
/// retry registration forever, and their in-flight reports still count after
/// the reboot (reconcile keeps assignments with fresh leases). Returns
/// `(gone, lines)` — `gone` means no inductor will be answering afterwards.
pub async fn stop_inductor(layout_root: &Path) -> (bool, Vec<String>) {
    let mut lines = Vec::new();
    let pid_path = pid_file(layout_root, "inductor");
    let gone = match read_pid(&pid_path) {
        None => {
            lines.push("inductor: not started by this TUI — nothing to stop".into());
            true
        }
        Some(pid) if !is_alive(pid) => {
            let _ = std::fs::remove_file(&pid_path);
            lines.push(format!("inductor: already stopped (cleaned stale pid {pid})"));
            true
        }
        Some(pid) => {
            signal(pid, "TERM");
            tokio::time::sleep(std::time::Duration::from_millis(800)).await;
            if is_alive(pid) {
                signal(pid, "KILL");
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            }
            if is_alive(pid) {
                lines.push(format!("inductor: would not die (pid {pid}) — kill it by hand"));
                false
            } else {
                let _ = std::fs::remove_file(&pid_path);
                lines.push(format!("inductor: stopped (was pid {pid})"));
                true
            }
        }
    };
    (gone, lines)
}

/// Remote addresses whose dial-back URL does not answer: a running inductor
/// bound to loopback (old `B`, hand start) is invisible to exactly these
/// boxes. Empty means every remote can see the inductor.
pub async fn lan_blackout(machines: &[Machine], api_port: u16) -> Vec<String> {
    let mut ips: Vec<(String, String)> = Vec::new();
    for m in machines {
        if is_local_addr(&m.addr) {
            continue;
        }
        match dial_back_ip(&m.addr) {
            Ok(ip) => ips.push((m.addr.clone(), ip)),
            Err(e) => ips.push((m.addr.clone(), format!("ERR {e:#}"))),
        }
    }
    ips.sort();
    ips.dedup();
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build();
    let http = match http {
        Ok(c) => c,
        Err(_) => return ips.into_iter().map(|(a, _)| a).collect(),
    };
    let mut dark = Vec::new();
    for (addr, ip) in ips {
        if ip.starts_with("ERR ") {
            dark.push(format!("{addr} ({ip})"));
            continue;
        }
        let ok = http
            .get(format!("http://{ip}:{api_port}/api/state"))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);
        if !ok {
            dark.push(addr);
        }
    }
    dark.sort();
    dark.dedup();
    dark
}

/// Any local worker process alive? The offline-swap guard: a mid-render
/// worker keeps rendering the old cast even with the inductor down.
pub fn local_workers_alive() -> bool {
    std::process::Command::new("pgrep")
        .arg("-f")
        .arg("bm-agent worke[r]")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Requeue every assigned/running task in the ledger file back to pending.
/// Only safe while no inductor answers: a live scheduler overwrites this file
/// on every mutation. Attempts are kept (a strike stays a strike); the
/// assignee, lease and detail are cleared. Returns the requeued task ids.
fn requeue_assigned_in_ledger(layout_root: &Path) -> anyhow::Result<Vec<String>> {
    let path = layout_root.join(".bm").join("ledger.json");
    let text = std::fs::read_to_string(&path)?;
    let mut doc: serde_json::Value = serde_json::from_str(&text)?;
    let mut back = Vec::new();
    if let Some(tasks) = doc.get_mut("tasks").and_then(|t| t.as_array_mut()) {
        for t in tasks.iter_mut() {
            let state = t.get("state").and_then(|s| s.as_str()).unwrap_or("");
            if state == "assigned" || state == "running" {
                let id = format!(
                    "{}:{}",
                    t.get("stage").and_then(|s| s.as_str()).unwrap_or("?"),
                    t.get("chapter").and_then(|c| c.as_u64()).unwrap_or(0)
                );
                if let Some(o) = t.as_object_mut() {
                    o.insert("state".into(), serde_json::Value::String("pending".into()));
                    o.insert("assigned_to".into(), serde_json::Value::Null);
                    o.insert("lease_until".into(), serde_json::Value::Null);
                    o.insert("detail".into(), serde_json::Value::String("requeued by X (worker gone)".into()));
                }
                back.push(id);
            }
        }
    }
    back.sort();
    if !back.is_empty() {
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(&doc)?)?;
        std::fs::rename(&tmp, &path)?;
    }
    Ok(back)
}

/// Is the inductor API answering? Decides whether the ledger file may be
/// touched (never while live).
pub(crate) async fn inductor_up(api: &str) -> bool {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build();
    match http {
        Ok(c) => c
            .get(format!("{}/api/state", api.trim_end_matches('/')))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false),
        Err(_) => false,
    }
}

// Per-machine catch-up outcomes are reported inline by the start sequence;
// no gate remains to test here.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_port_defaults_when_missing() {
        assert_eq!(api_port("http://127.0.0.1:8901"), 8901);
        assert_eq!(api_port("http://127.0.0.1:8901/"), 8901);
        assert_eq!(api_port("http://example:9999"), 9999);
        assert_eq!(api_port("http://example"), 8901);
    }

    #[test]
    fn only_loopback_counts_as_this_machine() {
        for a in ["http://127.0.0.1:8901", "http://localhost:8901", "http://[::1]:8901"] {
            assert!(api_is_local(a), "{a}");
        }
        for a in ["http://192.168.2.7:8901", "http://example:8901"] {
            assert!(!api_is_local(a), "{a}");
        }
    }

    #[test]
    fn shell_quoting_survives_spaces_and_quotes() {
        assert_eq!(shq("/tmp/Documents SSD/bm-agent"), "'/tmp/Documents SSD/bm-agent'");
        assert_eq!(shq("a'b"), "'a'\\''b'");
    }

    #[test]
    fn aliveness_is_true_for_self_and_false_for_nobody() {
        assert!(is_alive(std::process::id()), "this test process is alive");
        assert!(!is_alive(2_147_483_647), "no such PID");
    }

    #[test]
    fn spawned_backend_reconciles_nothing() {
        // The empty count is the whole point: boot invents no work, so no
        // default can ever auto-run chapters again.
        assert_eq!(
            serve_args("8901", "127.0.0.1"),
            vec!["serve", "--port", "8901", "--bind", "127.0.0.1", "--start", "1", "--count", "0"]
        );
    }

    #[test]
    fn bind_follows_the_cluster_shape() {
        assert_eq!(public_bind(false), "127.0.0.1", "solo stays loopback");
        assert_eq!(public_bind(true), "0.0.0.0", "remotes need the LAN");
    }

    #[test]
    fn dial_back_parsers_read_routing_tables() {
        let mac = "route to: 192.168.2.2\ndestination: 192.168.2.2\n  interface: en0\n      flags: <UP,HOST>\n";
        assert_eq!(parse_route_iface(mac), Some("en0".into()));
        assert_eq!(parse_route_iface("nothing here\n"), None);
        let linux = "192.168.2.2 via 192.168.1.1 dev eth0 src 192.168.1.50 uid 1000\n";
        assert_eq!(parse_route_src(linux), Some("192.168.1.50".into()));
        assert_eq!(parse_route_src("no src here\n"), None);
        let ifc = "en0: flags=8863<UP>\n\tinet 192.168.2.1 netmask 0xffffff00 broadcast 192.168.2.255\n\tinet6 fe80::1%en0\n\tstatus: active\n";
        assert_eq!(parse_ifconfig_inet(ifc), Some("192.168.2.1".into()));
        assert_eq!(parse_ifconfig_inet("\tinet 127.0.0.1 netmask 0xff000000\n"), None);
        assert_eq!(parse_ifconfig_inet("no addresses\n"), None);
    }

    #[test]
    fn kill_scripts_never_match_their_own_literal() {
        // pkill -f matches full command lines: if the worker pattern appeared
        // verbatim, the sweep would kill its own shell. The bracketed form is
        // the standard dodge — assert the dodge is present, not just intended.
        for script in [worker_kill_script(), sidecar_kill_script()] {
            assert!(script.contains("[r]") || script.contains("[y]"), "{script}");
        }
        assert!(
            !worker_kill_script().contains("bm-agent worker"),
            "verbatim pattern would self-match"
        );
        assert!(
            !sidecar_kill_script().contains("tts_server.py"),
            "verbatim pattern would self-match"
        );
        assert!(worker_kill_script().contains("left="), "sweep must report survivors");
    }

    #[test]
    fn remote_scripts_cannot_match_themselves() {
        // Live bugs, both seen: (1) an unbracketed name beside pkill kills the
        // script's own shell (empty output, no worker); (2) a bracketed name
        // as the executed command word (Usage error — the file is `bm-agent`,
        // the bracket only works as a *pattern*). So: check holds patterns
        // only, launch holds the plain name but no pkill.
        let check = remote_worker_check_script();
        assert!(!check.contains("bm-agent worker"), "self-match: {check}");
        assert!(check.contains("ALREADY:"), "{check}");
        let launch = remote_worker_launch_script("http://192.168.2.1:8901", "192.168.2.2");
        assert!(!launch.contains("pkill") && !launch.contains("pgrep"), "launch must not inspect: {launch}");
        assert!(launch.contains("./bm-agent worker --inductor"), "{launch}");
        assert!(launch.contains("http://192.168.2.1:8901") && launch.contains("192.168.2.2"), "{launch}");
    }

    #[test]
    fn stopping_inductor_by_pidfile_reports() {
        // No pid file: nothing to stop, and the caller may spawn fresh.
        let d = std::env::temp_dir().join("bm-backend-stop-inductor-empty");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join(".bm")).unwrap();
        let (gone, lines) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(stop_inductor(&d));
        assert!(gone, "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("not started by this TUI")), "{lines:?}");
    }

    #[test]
    fn stopping_with_no_pidfiles_reports_and_touches_nothing() {
        let d = std::env::temp_dir().join("bm-backend-stop-empty");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let lines = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(stop_backend(&d));
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|l| l.contains("not started by this TUI")), "{lines:?}");
    }

    #[test]
    fn stopping_kills_a_recorded_process() {
        // A real `sleep` stands in for a backend process — detached exactly
        // the way `spawn_one` detaches, so init (not this test) is its parent
        // and no zombie lingers to confuse the aliveness check.
        let d = std::env::temp_dir().join("bm-backend-stop-live");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join(".bm")).unwrap();
        let out = std::process::Command::new("sh")
            .arg("-c")
            // The redirect matters: without it the background job inherits the
            // pipe and `.output()` waits for its EOF (the full 60 seconds).
            .arg("sleep 60 >/dev/null 2>&1 & echo $!")
            .output()
            .expect("sh exists on test machines");
        let pid: u32 = String::from_utf8_lossy(&out.stdout).trim().parse().expect("a pid");
        assert!(is_alive(pid));
        std::fs::write(d.join(".bm/agent.pid"), pid.to_string()).unwrap();

        let lines = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(stop_backend(&d));
        assert!(lines.iter().any(|l| l.contains("stopped") && l.contains(&pid.to_string())), "{lines:?}");
        assert!(!is_alive(pid), "the process must be gone");
        assert!(!d.join(".bm/agent.pid").exists(), "the PID file goes with it");
    }

    #[test]
    fn starting_refuses_a_remote_api_and_a_missing_binary() {
        let d = std::env::temp_dir().join("bm-backend-start-guard");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        // Remote first: the guard fires before any binary is resolved.
        let err = start_backend(&d, "http://192.168.2.7:8901", false, "127.0.0.1", true).unwrap_err();
        assert!(err.to_string().contains("THIS machine"), "{err}");
        // Local but no binaries beside the test harness: names the problem.
        let err = start_backend(&d, "http://127.0.0.1:9", false, "127.0.0.1", true).unwrap_err();
        assert!(err.to_string().contains("no bm-"), "{err}");
    }
}
