//! Local backend lifecycle: start/stop the inductor + worker from the TUI.
//!
//! The TUI is only a client of the inductor API — with nothing behind it, it
//! is a nicely rendered empty dashboard. These helpers let it bootstrap a
//! solo backend itself: an inductor and a worker as detached background
//! processes with logs under `.bm/`, so the whole pipeline runs from the one
//! terminal the TUI owns.
//!
//! PID files (`.bm/inductor.pid`, `.bm/agent.pid`) record what *this TUI*
//! started, and only those processes may be stopped here. A backend started
//! elsewhere — tmux, another shell — has no PID files, so it is reported and
//! never touched.

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
fn serve_args(port: &str) -> Vec<String> {
    ["serve", "--port", port, "--bind", "127.0.0.1", "--start", "1", "--count", "0"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Start whichever half of the local backend is missing. Never fails hard over
/// one half: a dead worker must not block the inductor, and vice versa. Lines
/// are facts for the event log; the TUI's refresh loop flips the status to
/// live on its own once the inductor answers.
pub fn start_backend(layout_root: &Path, api: &str, api_up: bool) -> anyhow::Result<Vec<String>> {
    if layout_root.as_os_str().is_empty() {
        anyhow::bail!("no repo root — restart the TUI from a checkout");
    }
    if !api_is_local(api) {
        anyhow::bail!("this starts a backend on THIS machine, but the TUI watches {api}");
    }
    let bin_i = sibling_bin("bm-inductor")?;
    let bin_a = sibling_bin("bm-agent")?;
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
        let args = serve_args(&port);
        match spawn_one(&bin_i, &args, &log) {
            Ok(pid) => {
                let _ = std::fs::write(pid_file(layout_root, "inductor"), pid.to_string());
                lines.push(format!("inductor starting in background (pid {pid}, {})", log.display()));
            }
            Err(e) => lines.push(format!("inductor failed to start: {e:#}")),
        }
    }
    // Worker half: extra workers are harmless, so only our own PID suppresses.
    if let Some(pid) = read_pid(&pid_file(layout_root, "agent")).filter(|p| is_alive(*p)) {
        lines.push(format!("worker already running (pid {pid})"));
    } else {
        let log = log_file(layout_root, "agent");
        let args = ["worker".to_string(), "--inductor".to_string(), api.to_string()];
        match spawn_one(&bin_a, &args, &log) {
            Ok(pid) => {
                let _ = std::fs::write(pid_file(layout_root, "agent"), pid.to_string());
                lines.push(format!("worker starting in background (pid {pid}, {})", log.display()));
            }
            Err(e) => lines.push(format!("worker failed to start: {e:#}")),
        }
    }
    Ok(lines)
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
            serve_args("8901"),
            vec!["serve", "--port", "8901", "--bind", "127.0.0.1", "--start", "1", "--count", "0"]
        );
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
        let err = start_backend(&d, "http://192.168.2.7:8901", false).unwrap_err();
        assert!(err.to_string().contains("THIS machine"), "{err}");
        // Local but no binaries beside the test harness: names the problem.
        let err = start_backend(&d, "http://127.0.0.1:9", false).unwrap_err();
        assert!(err.to_string().contains("no bm-"), "{err}");
    }
}
