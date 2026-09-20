use std::path::{Path, PathBuf};
use std::process::Stdio;

/// Single-quote a shell word. Paths in the wild contain spaces
/// (`Documents SSD`), and an unquoted redirect target splits in two.
pub(crate) fn shq(s: &str) -> String {
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

pub(crate) fn pid_file(root: &Path, name: &str) -> PathBuf {
    root.join(".bm").join(format!("{name}.pid"))
}

pub(crate) fn log_file(root: &Path, name: &str) -> PathBuf {
    root.join(".bm").join(format!("{name}.log"))
}

pub(crate) fn read_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// The binary to run: this executable itself for `bm-inductor`, its sibling
/// for `bm-agent`. Resolved from `current_exe` rather than `PATH` so a `/tmp`
/// copy starts `/tmp` binaries, not whatever happens to be installed.
pub(crate) fn sibling_bin(name: &str) -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let p = if exe.file_name().map(|n| n == name).unwrap_or(false) {
        exe.clone()
    } else {
        exe.parent()
            .map(|d| d.join(name))
            .unwrap_or_else(|| PathBuf::from(name))
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
pub(crate) fn spawn_one(bin: &Path, args: &[String], log: &Path) -> anyhow::Result<u32> {
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut cmd = shq(&bin.to_string_lossy());
    for a in args {
        cmd.push(' ');
        cmd.push_str(&shq(a));
    }
    let script = format!(
        "nohup {cmd} >> {} 2>&1 & echo $!",
        shq(&log.to_string_lossy())
    );
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
///
/// `root` is passed rather than left to discovery: the child inherits this
/// process's cwd, which is not necessarily the root the operator named with
/// `--root`, and a backend that resolved a *different* root would reconcile a
/// different workspace's ledger while this dashboard watched it.
pub(crate) fn serve_args(root: &Path, port: &str, bind: &str) -> Vec<String> {
    [
        "serve".to_string(),
        "--root".to_string(),
        root.to_string_lossy().into_owned(),
        "--port".to_string(),
        port.to_string(),
        "--bind".to_string(),
        bind.to_string(),
        "--start".to_string(),
        "1".to_string(),
        "--count".to_string(),
        "0".to_string(),
    ]
    .to_vec()
}

pub(crate) fn signal(pid: u32, sig: &str) {
    let _ = std::process::Command::new("kill")
        .arg(format!("-{sig}"))
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quoting_survives_spaces_and_quotes() {
        assert_eq!(
            shq("/tmp/Documents SSD/bm-agent"),
            "'/tmp/Documents SSD/bm-agent'"
        );
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
        // default can ever auto-run chapters again. The root travels with it
        // for the same reason the count does — the child must not guess.
        assert_eq!(
            serve_args(Path::new("/repo"), "8901", "127.0.0.1"),
            vec![
                "serve",
                "--root",
                "/repo",
                "--port",
                "8901",
                "--bind",
                "127.0.0.1",
                "--start",
                "1",
                "--count",
                "0"
            ]
        );
    }
}
