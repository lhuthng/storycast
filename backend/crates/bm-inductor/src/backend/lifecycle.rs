use super::net::{api_is_local, api_port, is_local_addr, worker_inductor_url};
use super::process::{
    is_alive, log_file, pid_file, read_pid, serve_args, shq, sibling_bin, signal, spawn_one,
};
use bm_core::provision::{Ssh, REMOTE_DIR};
use bm_core::Layout;
use bm_proto::Machine;
use std::path::Path;

/// Remote worker launch scripts. See `worker_kill_script` for why patterns
fn remote_worker_check_script() -> String {
    "pgrep -f 'bm-agent.*worke[r]' | tr '\\n' ',' | sed 's/,$//;s/^/ALREADY:/'".into()
}

/// The remote half of the launch. **No inductor URL is passed**, and that is
///
/// With a deadline, a watchdog also stops the worker and shuts the box down at
/// that second: on EC2 `--instance-initiated-shutdown-behavior terminate` turns
/// the shutdown into a termination, so the box cannot outlive its work even if
/// this inductor is gone.
fn remote_worker_launch_script(addr: &str, expires_at: Option<u64>) -> String {
    let start = format!(
        "cd \"$HOME/{d}\" && nohup ./bm-agent --root \"$HOME/{d}\" worker --serve-tasks {p} --addr {} >> agent.log 2>&1 & echo STARTED:$!",
        shq(addr),
        d = REMOTE_DIR,
        p = bm_proto::DEFAULT_TASK_PORT,
    );
    let Some(at) = expires_at else {
        return start;
    };
    let secs = at.saturating_sub(bm_proto::now_secs());
    format!(
        "{start}; ( sleep {secs}; pkill -f 'bm-agent.*worke[r]' >/dev/null 2>&1 || true; \
         sync; sudo -n shutdown -h now >/dev/null 2>&1 || true ) >/dev/null 2>&1 &"
    )
}

/// Start a worker on each machine. **The one place a worker is started.**
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
        let (o, l) = start_remote_workers(&remotes, layout_root);
        ok &= o;
        lines.extend(l);
    }
    (ok, lines)
}

/// Start a worker on every remote machine (blocking): skip boxes that already
pub fn start_remote_workers(machines: &[Machine], layout_root: &Path) -> (bool, Vec<String>) {
    let mut lines = Vec::new();
    let mut ok = true;
    let layout = Layout::new(layout_root);
    let mut remotes: Vec<&Machine> = machines
        .iter()
        .filter(|m| !is_local_addr(&m.addr))
        .collect();
    remotes.sort_by(|a, b| a.addr.cmp(&b.addr));
    remotes.dedup_by(|a, b| a.addr == b.addr);
    for m in remotes {
        let at = m.handle();
        // The provider picks the transport. An InstaBox box has no ssh at all:
        // sshing to its published URL is a DNS error, not a worker start.
        let ssh = match crate::provision::transport_for(&layout, m) {
            Ok(ssh) => ssh,
            Err(e) => {
                lines.push(format!("[{at}] no transport: {e}"));
                ok = false;
                continue;
            }
        };
        // Check first, launch second (two round trips — see the builders for
        let already: Option<String> = match ssh.run(&remote_worker_check_script(), 30) {
            Ok((_, stdout, _)) => stdout
                .trim()
                .strip_prefix("ALREADY:")
                .map(|s| s.to_string()),
            Err(e) => {
                lines.push(format!("[{at}] worker check failed ({e:#})"));
                ok = false;
                continue;
            }
        };
        match already {
            Some(pids) if !pids.is_empty() => {
                lines.push(format!("[{at}] worker already running (pid {pids})"));
                continue;
            }
            _ => {}
        }
        // Past its deadline the box is going away: starting a worker on it is
        // work that would be thrown away, and the watchdog would kill it anyway.
        if m.expires_at.is_some_and(|until| until <= bm_proto::now_secs()) {
            lines.push(format!("[{at}] past its deadline — not starting a worker"));
            continue;
        }
        let script = remote_worker_launch_script(&m.addr, m.expires_at);
        match ssh.run(&script, 60) {
            Ok((0, stdout, _)) => {
                let out = stdout.trim().to_string();
                if let Some(pids) = out.strip_prefix("ALREADY:") {
                    lines.push(format!("[{at}] worker already running (pid {pids})"));
                } else if let Some(pid) = out.strip_prefix("STARTED:") {
                    lines.push(format!("[{at}] worker started (pid {pid})"));
                } else {
                    lines.push(format!("[{at}] unexpected worker start output: {out}"));
                    ok = false;
                }
            }
            Ok((code, _, stderr)) => {
                lines.push(format!(
                    "[{at}] worker start failed (exit {code}): {}",
                    stderr.trim()
                ));
                ok = false;
            }
            Err(e) => {
                lines.push(format!("[{at}] worker start failed ({e:#})"));
                ok = false;
            }
        }
    }
    (ok, lines)
}

/// Start whichever half of the local backend is missing. Never fails hard over
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
    if with_worker {
        lines.extend(start_local_worker(layout_root));
    }
    Ok(lines)
}

/// Start the local worker unless this TUI already runs one. Split out so a
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
fn worker_kill_script() -> String {
    "pkill -f 'bm-agent.*worke[r]' 2>/dev/null; sleep 1; \
     pkill -9 -f 'bm-agent.*worke[r]' 2>/dev/null; sleep 1; \
     echo left=$(pgrep -f 'bm-agent.*worke[r]' 2>/dev/null | wc -l)"
        .into()
}

/// Same for the per-render TTS sidecar: orphaned servers hold the port and
fn sidecar_kill_script() -> String {
    "pkill -f 'tts_server\\.p[y]' 2>/dev/null; pkill -x bm-tts 2>/dev/null; sleep 1; \
     pkill -9 -f 'tts_server\\.p[y]' 2>/dev/null; pkill -9 -x bm-tts 2>/dev/null; sleep 1; \
     echo left=$(($(pgrep -f 'tts_server\\.p[y]' 2>/dev/null | wc -l) + $(pgrep -x bm-tts 2>/dev/null | wc -l)))"
        .into()
}

/// Sweep one machine for leftover workers + sidecars. Returns whether the box
/// answered at all, and the report.
fn sweep_one(label: &str, ssh: &Ssh) -> (bool, Vec<String>) {
    let mut out = Vec::new();
    let mut reached = false;
    for (what, script) in [
        ("worker", worker_kill_script()),
        ("sidecar", sidecar_kill_script()),
    ] {
        match ssh.run(&script, 30) {
            Ok((_, stdout, _)) => {
                reached = true;
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
            Err(e) => out.push(format!("{label}: sweep failed ({e:#}) — nothing answered")),
        }
    }
    (reached, out)
}

/// Whether there is anywhere to dial: an InstaBox box with no published port
/// answers to nothing, and a sweep would only spend its timeout proving that.
fn dialable(m: &Machine) -> bool {
    let via_api = m.provider.as_deref() == Some(bm_core::cloud::Provider::InstaBox.as_str());
    !(via_api && m.base_url.as_deref().map(str::is_empty).unwrap_or(true))
}

/// File boxes that answered nothing as offline, so the scheduler stops counting
/// them as live and their rows go back to the pool instead of waiting on a box
/// nobody can reach.
async fn mark_unreachable(api: &str, machines: &[Machine]) {
    let Ok(http) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    else {
        return;
    };
    let url = format!("{}/api/machines/state", api.trim_end_matches('/'));
    for m in machines {
        let _ = http
            .post(&url)
            .json(&serde_json::json!({
                "addr": m.addr,
                "state": bm_proto::MachineState::Offline.as_str(),
                "note": "unreachable at stop — nothing there answered",
            }))
            .send()
            .await;
    }
}

/// Remote task endpoints for an HTTP shutdown: `(addr, base URL)`, one per box.
fn shutdown_targets(machines: &[Machine]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = machines
        .iter()
        .filter(|m| !is_local_addr(&m.addr))
        .filter_map(|m| {
            let port = m.task_port?;
            let base = m
                .base_url
                .as_deref()
                .map(str::trim)
                .filter(|u| !u.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("http://{}:{port}", m.addr));
            Some((m.addr.clone(), base))
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Ask every remote worker to stop over its own task port: one 5 s HTTP call
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
    for (addr, base) in shutdown_targets(machines) {
        let (http, token) = (http.clone(), token.clone());
        jobs.push(tokio::spawn(async move {
            let url = format!("{base}/shutdown");
            match http.post(&url).bearer_auth(&token).send().await {
                Ok(r) if r.status().is_success() => {
                    format!("[{addr}] worker stopping (shutdown acknowledged)")
                }
                Ok(r) => format!(
                    "[{addr}] shutdown refused ({}) — ssh sweep follows",
                    r.status()
                ),
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
pub async fn stop_everywhere(layout: &Layout, machines: &[Machine], api: &str) -> Vec<String> {
    let mut lines = shutdown_remotes_http(&layout.root, machines).await;
    lines.extend(stop_backend(&layout.root).await);
    // Requeue stranded assignments, but only with no live scheduler: the file
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
            exec: None,
        };
        sweep_one("local", &local)
    })
    .await;
    match local_sweep {
        Ok((_, ls)) => lines.extend(ls),
        Err(e) => lines.push(format!("local sweep task failed ({e})")),
    }
    let mut remotes: Vec<Machine> = machines
        .iter()
        .filter(|m| !is_local_addr(&m.addr))
        .cloned()
        .collect();
    remotes.sort_by(|a, b| a.addr.cmp(&b.addr));
    remotes.dedup_by(|a, b| a.addr == b.addr);
    let (dial, mut unreachable): (Vec<Machine>, Vec<Machine>) =
        remotes.into_iter().partition(dialable);
    // One task per box: a dead one costs its own timeout, not every other box's
    // as well.
    let mut jobs = Vec::new();
    for m in dial {
        let layout = layout.clone();
        jobs.push(tokio::spawn(async move {
            let addr = m.addr.clone();
            let at = m.handle();
            let swept = match crate::provision::transport_for(&layout, &m) {
                Ok(ssh) => tokio::task::spawn_blocking(move || sweep_one(&addr, &ssh)).await,
                Err(e) => Ok((false, vec![format!("{at}: no transport ({e})")])),
            };
            (m, swept)
        }));
    }
    for j in jobs {
        match j.await {
            Ok((m, Ok((reached, ls)))) => {
                lines.extend(ls);
                if !reached {
                    unreachable.push(m);
                }
            }
            Ok((m, Err(e))) => {
                lines.push(format!("[{}] sweep task failed ({e})", m.addr));
                unreachable.push(m);
            }
            Err(e) => lines.push(format!("sweep task failed ({e})")),
        }
    }
    if !unreachable.is_empty() {
        mark_unreachable(api, &unreachable).await;
        for m in &unreachable {
            lines.push(format!(
                "{}: unreachable — flagged offline, its rows return to the pool",
                m.handle()
            ));
        }
    }
    lines
}

/// Stop only the inductor half by PID file, leaving workers running: they
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
                    o.insert("racers".into(), serde_json::Value::Array(vec![]));
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
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_targets_skips_local_and_undriven_boxes() {
        // One entry per remote box on its task port: HTTP shutdown knocks
        let lan = Machine::new("192.168.2.2", "thang", 22, None, "worker");
        assert_eq!(
            shutdown_targets(&[lan]),
            vec![(
                "192.168.2.2".into(),
                format!("http://192.168.2.2:{}", bm_proto::DEFAULT_TASK_PORT)
            )]
        );
        let mut quiet = Machine::new("192.168.2.2", "thang", 22, None, "worker");
        quiet.task_port = None;
        let local = Machine::new("127.0.0.1", "me", 22, None, "worker");
        assert!(shutdown_targets(&[quiet, local]).is_empty());
    }

    #[test]
    fn a_worker_is_never_started_over_ssh_on_an_instabox_box() {
        // The url is not a hostname: ssh reads it as a bad CNAME and exits 255.
        let layout = Layout::new("");
        let mut m = Machine::new(
            "https://8917-x.instabox.tech",
            "instabox",
            22,
            None,
            "worker",
        );
        m.provider = Some(bm_core::cloud::Provider::InstaBox.as_str().to_string());
        m.provider_id = Some("m_agq".into());
        match crate::provision::transport_for(&layout, &m) {
            Ok(ssh) => {
                assert!(ssh.exec.is_some(), "an InstaBox box has no ssh to dial");
                assert!(!ssh.target.contains("://"), "target is a label: {}", ssh.target);
            }
            // No key on this checkout: refusing is the other correct answer.
            Err(e) => assert!(!e.is_empty()),
        }
        assert!(!dialable(&m), "a box with no published port is not swept");
    }

    #[test]
    fn a_box_with_no_address_is_not_dialable() {
        // An InstaBox box with no published port answers to nothing: sweeping it
        // would spend two 30 s timeouts to learn that, and `:X` would look hung.
        let mut boxed =
            Machine::new("m_agqzns2gx7lfvbhf7ddsayzq2m", "instabox", 22, None, "worker");
        boxed.provider = Some(bm_core::cloud::Provider::InstaBox.as_str().to_string());
        assert!(!dialable(&boxed));
        boxed.base_url = Some("https://8917-x.instabox.tech".into());
        assert!(dialable(&boxed));
        // Any other provider is dialled by its address alone.
        assert!(dialable(&Machine::new("18.1.2.3", "ubuntu", 22, None, "worker")));
    }

    #[test]
    fn kill_scripts_never_match_their_own_literal() {
        // pkill -f matches full command lines: if the worker pattern appeared
        for script in [worker_kill_script(), sidecar_kill_script()] {
            assert!(script.contains("[r]") || script.contains("[y]"), "{script}");
        }
        assert!(
            !worker_kill_script().contains("bm-agent worker"),
            "contiguous pattern matches nothing: the launch puts --root between them"
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
        let check = remote_worker_check_script();
        assert!(!check.contains("bm-agent worker"), "self-match: {check}");
        assert!(check.contains("ALREADY:"), "{check}");
        let launch = remote_worker_launch_script("192.168.2.2", None);
        assert!(
            !launch.contains("pkill") && !launch.contains("pgrep"),
            "launch must not inspect: {launch}"
        );
        assert!(launch.contains("./bm-agent"), "{launch}");
        // **This assertion is the constraint.** A launched worker is never
        // guarantee is the absence of the argument, not a flag saying "do not
        // call home". Reintroducing `--inductor` here would quietly restore
        assert!(
            !launch.contains("--inductor"),
            "a launched worker must not be given an inductor address: {launch}"
        );
        assert!(launch.contains("worker --serve-tasks"), "{launch}");
        // The sweep patterns must match what this launch produces. The worker
        for script in [worker_kill_script(), remote_worker_check_script()] {
            assert!(script.contains("bm-agent.*worke[r]"), "{script}");
        }
        let argv: &str = &launch;
        let (a, w) = (
            argv.find("bm-agent").expect("binary in launch"),
            argv.find("worker --serve-tasks")
                .expect("subcommand in launch"),
        );
        assert!(a < w, "binary before subcommand: {argv}");
        assert!(
            !argv.contains("bm-agent worker"),
            "never adjacent — the contiguous pattern is what broke: {argv}"
        );
        // The root travels with the launch: the worker mirror has no backend/
        assert!(launch.contains("--root \"$HOME/bm-worker\""), "{launch}");
        assert!(launch.contains("192.168.2.2"), "{launch}");
    }

    #[test]
    fn a_deadline_arms_a_watchdog_and_a_box_without_one_gets_none() {
        let at = bm_proto::now_secs() + 3600;
        let launch = remote_worker_launch_script("192.0.2.9", Some(at));
        let secs = at.saturating_sub(bm_proto::now_secs());
        assert!(
            launch.contains(&format!("sleep {secs}"))
                || launch.contains(&format!("sleep {}", secs.saturating_sub(1))),
            "{launch}"
        );
        assert!(launch.contains("worke[r]"), "stops the worker: {launch}");
        assert!(
            launch.contains("shutdown -h now"),
            "and the box, the only part that stops billing: {launch}"
        );
        assert!(
            launch.starts_with("cd \"$HOME/bm-worker\""),
            "the worker launch itself is unchanged: {launch}"
        );
        assert!(launch.contains("192.0.2.9"), "{launch}");
        assert!(
            !remote_worker_launch_script("192.0.2.9", None).contains("shutdown"),
            "no deadline, no shutdown"
        );
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
        let d = std::env::temp_dir().join("bm-backend-stop-live");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join(".bm")).unwrap();
        let out = std::process::Command::new("sh")
            .arg("-c")
            // The redirect matters: without it the background job inherits the
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
