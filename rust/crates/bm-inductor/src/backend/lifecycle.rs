use super::net::{api_is_local, api_port, is_local_addr, worker_inductor_url};
use super::process::{
    is_alive, log_file, pid_file, read_pid, serve_args, shq, sibling_bin, signal, spawn_one,
};
use bm_core::provision::{Ssh, REMOTE_DIR};
use bm_core::Layout;
use bm_proto::Machine;
use std::path::Path;

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

/// The remote half of the launch. **No inductor URL is passed**, and that is
/// the point of the whole inversion: a worker that is never told where the
/// inductor is cannot dial it, so a box behind NAT on either end is a
/// non-issue rather than a thing to route around.
fn remote_worker_launch_script(addr: &str) -> String {
    format!(
        "cd \"$HOME/{d}\" && nohup ./bm-agent --root \"$HOME/{d}\" worker --serve-tasks {p} --addr {} >> agent.log 2>&1 & echo STARTED:$!",
        shq(addr),
        d = REMOTE_DIR,
        p = bm_proto::DEFAULT_TASK_PORT,
    )
}

/// Start a worker on each machine. **The one place a worker is started.**
///
/// The mechanism differs — a box on this machine is a child process, any other
/// is started over ssh — but that is a difference in *transport*, not in
/// worker: both get the same command line, both serve-only, both driven the
/// same way afterwards. Keeping the choice here is what stops it becoming two
/// divergent copies, which is what it was.
///
/// Returns `(all_up, lines)` like [`start_remote_workers`], so callers keep
/// one error path whether the box was local or not.
pub fn start_workers(machines: &[Machine], layout_root: &Path) -> (bool, Vec<String>) {
    let mut ok = true;
    let mut lines = Vec::new();
    for m in machines.iter().filter(|m| is_local_addr(&m.addr)) {
        for l in start_local_worker(layout_root) {
            lines.push(format!("[{}] {l}", m.addr));
        }
    }
    let remotes: Vec<Machine> = machines
        .iter()
        .filter(|m| !is_local_addr(&m.addr))
        .cloned()
        .collect();
    if !remotes.is_empty() {
        let (o, l) = start_remote_workers(&remotes);
        ok &= o;
        lines.extend(l);
    }
    (ok, lines)
}

/// Start a worker on every remote machine (blocking): skip boxes that already
/// run one, launch the rest detached into `~/bm-worker/agent.log`.
///
/// Returns `(all_up, lines)` — a failed launch vetoes the start like a failed
/// provision does, so `B` never leaves a half-started cluster.
pub fn start_remote_workers(machines: &[Machine]) -> (bool, Vec<String>) {
    let mut lines = Vec::new();
    let mut ok = true;
    let mut remotes: Vec<&Machine> = machines
        .iter()
        .filter(|m| !is_local_addr(&m.addr))
        .collect();
    remotes.sort_by(|a, b| a.addr.cmp(&b.addr));
    remotes.dedup_by(|a, b| a.addr == b.addr);
    for m in remotes {
        // pgrep first: a second worker per box is harmless but noisy, and the
        // report should say what actually happened.
        let ssh = Ssh::for_machine(m);
        // Check first, launch second (two round trips — see the builders for
        // why one combined script is a self-match trap).
        let already: Option<String> = match ssh.run(&remote_worker_check_script(), 30) {
            Ok((_, stdout, _)) => stdout
                .trim()
                .strip_prefix("ALREADY:")
                .map(|s| s.to_string()),
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
        let script = remote_worker_launch_script(&m.addr);
        match ssh.run(&script, 60) {
            Ok((0, stdout, _)) => {
                let out = stdout.trim().to_string();
                if let Some(pids) = out.strip_prefix("ALREADY:") {
                    lines.push(format!("[{}] worker already running (pid {pids})", m.addr));
                } else if let Some(pid) = out.strip_prefix("STARTED:") {
                    lines.push(format!("[{}] worker started (pid {pid})", m.addr));
                } else {
                    lines.push(format!(
                        "[{}] unexpected worker start output: {out}",
                        m.addr
                    ));
                    ok = false;
                }
            }
            Ok((code, _, stderr)) => {
                lines.push(format!(
                    "[{}] worker start failed (exit {code}): {}",
                    m.addr,
                    stderr.trim()
                ));
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
pub fn start_backend(
    layout_root: &Path,
    api: &str,
    api_up: bool,
    bind: &str,
    with_worker: bool,
) -> anyhow::Result<Vec<String>> {
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
        lines.push(format!(
            "inductor already answering at {api} — not started again"
        ));
    } else if let Some(pid) = read_pid(&pid_file(layout_root, "inductor")).filter(|p| is_alive(*p))
    {
        lines.push(format!("inductor already running (pid {pid})"));
    } else {
        let log = log_file(layout_root, "inductor");
        let args = serve_args(layout_root, &port, bind);
        match spawn_one(&bin_i, &args, &log) {
            Ok(pid) => {
                let _ = std::fs::write(pid_file(layout_root, "inductor"), pid.to_string());
                lines.push(format!(
                    "inductor starting in background (pid {pid}, {})",
                    log.display()
                ));
            }
            Err(e) => lines.push(format!("inductor failed to start: {e:#}")),
        }
    }
    // Worker half unless the caller stages it per-box (degraded start):
    // extra workers are harmless, so only our own PID suppresses.
    if with_worker {
        lines.extend(start_local_worker(layout_root));
    }
    Ok(lines)
}

/// Start the local worker unless this TUI already runs one. Split out so a
/// single-box `p` retry can launch exactly its box's worker without touching
/// the inductor half.
pub fn start_local_worker(layout_root: &Path) -> Vec<String> {
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
    // `--root` for the same reason as the inductor's: the child inherits this
    // process's cwd, which need not be the root the operator named.
    //
    // No `--inductor`: a serve-only worker holds no address for one, so it
    // cannot call home. The inductor drives it over `--serve-tasks` instead.
    let args = [
        "--root".to_string(),
        layout_root.to_string_lossy().into_owned(),
        "worker".to_string(),
        "--serve-tasks".to_string(),
        bm_proto::DEFAULT_TASK_PORT.to_string(),
    ];
    match spawn_one(&bin_a, &args, &log) {
        Ok(pid) => {
            let _ = std::fs::write(pid_file(layout_root, "agent"), pid.to_string());
            lines.push(format!(
                "worker starting in background (pid {pid}, {})",
                log.display()
            ));
        }
        Err(e) => lines.push(format!("worker failed to start: {e:#}")),
    }
    lines
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
                    lines.push(format!(
                        "{name}: would not die (pid {pid}) — kill it by hand"
                    ));
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
     echo left=$(pgrep -f 'bm-agent worke[r]' 2>/dev/null | wc -l)"
        .into()
}

/// Same for the per-render TTS sidecar: orphaned servers hold the port and
/// gigabytes of weights, and the next worker would just inherit the stale one.
/// Both patterns: the Python server is retired but a stray may outlive the
/// migration, and `bm-tts` is what everything spawns now. (`-x` matches the
/// process name only, so unlike the worker's `-f` pattern it cannot match the
/// shell running this script — no bracket dodge needed.)
fn sidecar_kill_script() -> String {
    "pkill -f 'tts_server\\.p[y]' 2>/dev/null; pkill -x bm-tts 2>/dev/null; sleep 1; \
     pkill -9 -f 'tts_server\\.p[y]' 2>/dev/null; pkill -9 -x bm-tts 2>/dev/null; sleep 1; \
     echo left=$(($(pgrep -f 'tts_server\\.p[y]' 2>/dev/null | wc -l) + $(pgrep -x bm-tts 2>/dev/null | wc -l)))"
        .into()
}

/// Sweep one machine for leftover workers + sidecars. Returns the report
/// lines; the ssh transport failing is a report, never an error.
fn sweep_one(label: &str, ssh: &Ssh) -> Vec<String> {
    let mut out = Vec::new();
    for (what, script) in [
        ("worker", worker_kill_script()),
        ("sidecar", sidecar_kill_script()),
    ] {
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
                    out.push(format!(
                        "{label}: {what}s stopped ({left} would not die — kill by hand)"
                    ));
                }
            }
            Err(e) => out.push(format!(
                "{label}: sweep failed ({e:#}) — anything there keeps running"
            )),
        }
    }
    out
}

/// Remote task endpoints for an HTTP shutdown: `(addr, port)`, one per box.
/// A `None` port is the operator saying "do not drive this one" — ssh sweep
/// only, never a shutdown knock.
fn shutdown_targets(machines: &[Machine]) -> Vec<(String, u16)> {
    let mut out: Vec<(String, u16)> = machines
        .iter()
        .filter(|m| !is_local_addr(&m.addr))
        .filter_map(|m| m.task_port.map(|p| (m.addr.clone(), p)))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Ask every remote worker to stop over its own task port: one 5 s HTTP call
/// each, all at once. The ssh pkill below stays as the backstop — but on a
/// slow link the ssh handshake alone can outlast the sweep while the worker
/// would have answered HTTP in a second, and that gap is how pre-X workers
/// survived X and squatted on their tasks afterwards. Never fails hard.
async fn shutdown_remotes_http(layout_root: &Path, machines: &[Machine]) -> Vec<String> {
    let mut lines = Vec::new();
    let Some(token) = bm_core::token::read(layout_root) else {
        lines.push("no cluster token — remote shutdown skipped, ssh sweep only".into());
        return lines;
    };
    let http = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            lines.push(format!("http client failed ({e:#}) — ssh sweep only"));
            return lines;
        }
    };
    let mut jobs = Vec::new();
    for (addr, port) in shutdown_targets(machines) {
        let (http, token) = (http.clone(), token.clone());
        jobs.push(tokio::spawn(async move {
            let url = format!("http://{addr}:{port}/shutdown");
            match http.post(&url).bearer_auth(&token).send().await {
                Ok(r) if r.status().is_success() => {
                    format!("[{addr}] worker stopping (shutdown acknowledged)")
                }
                Ok(r) => format!("[{addr}] shutdown refused ({}) — ssh sweep follows", r.status()),
                Err(e) => format!("[{addr}] no shutdown answer ({e:#}) — ssh sweep follows"),
            }
        }));
    }
    for j in jobs {
        lines.push(
            j.await
                .unwrap_or_else(|e| format!("shutdown task failed ({e}) — ssh sweep follows")),
        );
    }
    lines
}

/// Stop every worker in the cluster: remote workers over HTTP first, then the
/// local backend by PID file, then a local sweep for strays (hand-started
/// shells, tmux), then every registered remote machine over ssh. Tasks
/// stranded on dead workers are requeued into the ledger the moment no
/// inductor answers — so stop-then-start loses nothing to lease waits (a
/// render lease is 90 minutes). Never fails hard — each machine reports its
/// own outcome, and one unreachable box never silences the rest.
pub async fn stop_everywhere(layout: &Layout, machines: &[Machine], api: &str) -> Vec<String> {
    let mut lines = shutdown_remotes_http(&layout.root, machines).await;
    lines.extend(stop_backend(&layout.root).await);
    // Requeue stranded assignments, but only with no live scheduler: the file
    // is the inductor's to write while it answers.
    if inductor_up(api).await {
        lines.push(
            "inductor still answering — ledger untouched (assigned tasks keep their leases)".into(),
        );
    } else {
        match requeue_assigned_in_ledger(layout) {
            Ok(back) if back.is_empty() => {
                lines.push("no stranded tasks — nothing requeued".into())
            }
            Ok(back) => lines.push(format!(
                "requeued {} stranded task(s): {}",
                back.len(),
                back.iter().take(6).cloned().collect::<Vec<_>>().join(", ")
            )),
            Err(e) => lines.push(format!(
                "ledger requeue failed ({e:#}) — stranded tasks keep leases"
            )),
        }
    }
    // Sweeps block (ssh timeouts, kill grace periods) — off the runtime.
    let local_sweep = tokio::task::spawn_blocking(|| {
        let local = Ssh {
            target: "local".into(),
            port: 22,
            key: None,
            local: true,
        };
        sweep_one("local", &local)
    })
    .await;
    match local_sweep {
        Ok(ls) => lines.extend(ls),
        Err(e) => lines.push(format!("local sweep task failed ({e})")),
    }
    let mut remotes: Vec<&Machine> = machines
        .iter()
        .filter(|m| !is_local_addr(&m.addr))
        .collect();
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
            lines.push(format!(
                "inductor: already stopped (cleaned stale pid {pid})"
            ));
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
                lines.push(format!(
                    "inductor: would not die (pid {pid}) — kill it by hand"
                ));
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
///
/// Probes the same URL [`start_remote_workers`] hands out, advertised address
/// included. Checking a different address than the one the worker was given
/// would report a blackout the worker does not have — or miss one it does.
pub async fn lan_blackout(
    machines: &[Machine],
    api_port: u16,
    advertise: Option<&str>,
) -> Vec<String> {
    let mut ips: Vec<(String, String)> = Vec::new();
    for m in machines {
        if is_local_addr(&m.addr) {
            continue;
        }
        match worker_inductor_url(advertise, &m.addr, api_port) {
            Ok(url) => ips.push((m.addr.clone(), url)),
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
    for (addr, url) in ips {
        if url.starts_with("ERR ") {
            dark.push(format!("{addr} ({url})"));
            continue;
        }
        let ok = http
            .get(format!("{url}/api/state"))
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

/// Requeue every assigned/running task in the ledger file back to pending.
/// Only safe while no inductor answers: a live scheduler overwrites this file
/// on every mutation. Attempts are kept (a strike stays a strike); the
/// assignee, lease and detail are cleared. Returns the requeued task ids.
fn requeue_assigned_in_ledger(layout: &Layout) -> anyhow::Result<Vec<String>> {
    let path = layout.ledger();
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
                    o.insert(
                        "detail".into(),
                        serde_json::Value::String("requeued by X (worker gone)".into()),
                    );
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
    fn shutdown_targets_skips_local_and_undriven_boxes() {
        // One entry per remote box on its task port: HTTP shutdown knocks
        // exactly where the inductor drives. Loopback never dials out, and
        // a `None` port is the operator saying "do not drive this one" —
        // ssh sweep only, never a shutdown knock.
        let lan = Machine::new("192.168.2.2", "thang", 22, None, "worker");
        assert_eq!(
            shutdown_targets(&[lan]),
            vec![("192.168.2.2".into(), bm_proto::DEFAULT_TASK_PORT)]
        );
        let mut quiet = Machine::new("192.168.2.2", "thang", 22, None, "worker");
        quiet.task_port = None;
        let local = Machine::new("127.0.0.1", "me", 22, None, "worker");
        assert!(shutdown_targets(&[quiet, local]).is_empty());
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
        assert!(
            worker_kill_script().contains("left="),
            "sweep must report survivors"
        );
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
        let launch = remote_worker_launch_script("192.168.2.2");
        assert!(
            !launch.contains("pkill") && !launch.contains("pgrep"),
            "launch must not inspect: {launch}"
        );
        assert!(launch.contains("./bm-agent"), "{launch}");
        // **This assertion is the constraint.** A launched worker is never
        // handed an inductor address, so it has nothing to dial — the
        // guarantee is the absence of the argument, not a flag saying "do not
        // call home". Reintroducing `--inductor` here would quietly restore
        // the requirement that the inductor be reachable *from* a cloud
        // worker, which is the one thing the inversion removed.
        assert!(
            !launch.contains("--inductor"),
            "a launched worker must not be given an inductor address: {launch}"
        );
        assert!(launch.contains("worker --serve-tasks"), "{launch}");
        // The root travels with the launch: the worker mirror has no rust/
        // tree, so a child left to discover its own root found nothing.
        assert!(launch.contains("--root \"$HOME/bm-worker\""), "{launch}");
        assert!(launch.contains("192.168.2.2"), "{launch}");
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
        assert!(
            lines.iter().any(|l| l.contains("not started by this TUI")),
            "{lines:?}"
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
        assert!(
            lines.iter().all(|l| l.contains("not started by this TUI")),
            "{lines:?}"
        );
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
        let pid: u32 = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .expect("a pid");
        assert!(is_alive(pid));
        std::fs::write(d.join(".bm/agent.pid"), pid.to_string()).unwrap();

        let lines = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(stop_backend(&d));
        assert!(
            lines
                .iter()
                .any(|l| l.contains("stopped") && l.contains(&pid.to_string())),
            "{lines:?}"
        );
        assert!(!is_alive(pid), "the process must be gone");
        assert!(
            !d.join(".bm/agent.pid").exists(),
            "the PID file goes with it"
        );
    }

    #[test]
    fn starting_refuses_a_remote_api_and_a_missing_binary() {
        let d = std::env::temp_dir().join("bm-backend-start-guard");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        // Remote first: the guard fires before any binary is resolved.
        let err =
            start_backend(&d, "http://192.168.2.7:8901", false, "127.0.0.1", true).unwrap_err();
        assert!(err.to_string().contains("THIS machine"), "{err}");
        // Local but no binaries beside the test harness: names the problem.
        let err = start_backend(&d, "http://127.0.0.1:9", false, "127.0.0.1", true).unwrap_err();
        assert!(err.to_string().contains("no bm-"), "{err}");
    }
}
