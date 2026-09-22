//! The one channel that runs backwards: a reverse tunnel per remote worker.
//!
//! ## Why this exists at all
//!
//! The transport is inverted on purpose (§7): the inductor dials every worker,
//! so nothing needs a route *to* the inductor — which a box on the public
//! internet cannot have to a laptop behind NAT. The cost was one asymmetry:
//! the inductor learns a task's outcome only through the connection it opened
//! (`POST /task`'s response *is* the stage), so a mid-task **uplink blip**
//! kills the request the answer was travelling on. The task still completes
//! on the box, the lease expires, the chapter re-renders — wasted GPU-hours
//! with nothing to show.
//!
//! The fix keeps the inversion: the inductor uses the route it already has
//! (ssh, the same one provisioning uses) to hold open a **reverse** forward
//! per box — `ssh -N -R {hook}:127.0.0.1:{api} box`. The worker then gains a
//! loopback address that *is* the inductor's control API, and may answer
//! through it (`POST /api/complete`) when the primary channel died. This is a
//! hook, not a protocol change: the pushed task stays primary, the hook fires
//! only on its failure, and the inductor's own completion gate (`stale`
//! report checks, render's file-on-disk proof, per-task strikes) judges
//! whatever arrives — the tunnel grants reachability, never authority.
//!
//! ## The supervisor
//!
//! One child `ssh` per remote box, restarted whenever it exits — a tunnel is
//! infrastructure, and infrastructure that only works while nothing goes
//! wrong is decoration. Boxes join and leave the registry while this runs, so
//! the set is re-derived every pass; a box that vanished has its child
//! killed, a box that joined gets one. Local boxes and boxes with `task_port:
//! null` are skipped (no `Ssh` route, no drive — the same rule `dispatch`
//! reads its targets from). Failure is loud in the log, quiet in the ledger:
//! the hook is an optimization, and an absent one must not read as an error
//! state.

use crate::api::Shared;
use bm_core::provision::{resolve_key, Ssh};
use bm_proto::DEFAULT_HOOK_PORT;
use std::collections::HashMap;
use std::time::Duration;

/// The inductor's own control-API port, as the tunnels should forward to.
/// `serve` is the only caller that knows the real one, so it is handed in.
pub async fn supervise(state: Shared, api_port: u16) {
    // Live children by address. The pass below keeps this in step with the
    // registry: spawn for new boxes, kill for departed ones, respawn for dead
    // clients.
    let mut children: HashMap<String, tokio::process::Child> = HashMap::new();
    // Consecutive failed spawns per box. Not state, a *diagnostic* — but it has
    // to outlive a single pass, and one pass is one log line per box per five
    // seconds.
    let mut failing: HashMap<String, u32> = HashMap::new();
    loop {
        match pass(&state, api_port, &mut children, &mut failing).await {
            Ok(()) => {}
            // A poisoned lock or a dead registry read is not fatal — the next
            // pass retries. Bail only on something truly unrecoverable.
            Err(e) => println!("tunnel: pass failed ({e:#})"),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// One reconciliation: the wanted set (every remote, driven box) vs the live
/// children. Spawn what is missing, kill what should not exist, and reap
/// children that exited on their own (dead NAT, refused key, port already
/// bound remotely) — the next pass respawns them.
async fn pass(
    state: &Shared,
    api_port: u16,
    children: &mut HashMap<String, tokio::process::Child>,
    failing: &mut HashMap<String, u32>,
) -> anyhow::Result<()> {
    // Kill first: a box that left the registry must not keep a channel open.
    let targets = tunnel_targets(state).await;
    let wanted: std::collections::HashSet<String> =
        targets.iter().map(|t| t.addr.clone()).collect();
    let stale: Vec<String> = children
        .keys()
        .filter(|addr| !wanted.contains(*addr))
        .cloned()
        .collect();
    for addr in stale {
        if let Some(mut child) = children.remove(&addr) {
            let _ = child.kill().await;
            println!("tunnel: {addr} left the registry — tunnel closed");
        }
    }

    for t in targets {
        // Alive? `try_wait` answers without waiting; a reaped-or-errored probe
        // counts as gone so the slot is rebuilt below.
        let exited = match children.get_mut(&t.addr) {
            Some(c) => c.try_wait().ok().flatten(),
            None => None,
        };
        match (children.contains_key(&t.addr), exited) {
            // Running fine. If it had been failing, the *recovery* is the edge
            // worth a line — an operator wants to know the box came back, not
            // that it stayed away.
            (true, None) => {
                if let Some(n) = failing.remove(&t.addr) {
                    println!("tunnel: {} is up again after {n} failed attempt(s)", t.addr);
                }
                continue;
            }
            (true, Some(status)) => {
                children.remove(&t.addr);
                let n = {
                    let e = failing.entry(t.addr.clone()).or_insert(0);
                    *e += 1;
                    *e
                };
                // **Edge-triggered, and it has to be.** A box that is unreachable
                // for an hour is *one* fact; printed every five seconds it is 720
                // lines. That is what happened on 2026-09-22: 1339 identical
                // lines, 16% of the whole inductor log and 198 of its last 200,
                // burying every real event under a message that never changed.
                // Same rule the duplicate-sidecar alarm already uses. The first
                // failure and every sixtieth after it (≈5 min) are said out
                // loud; the rest are quiet.
                if n == 1 {
                    println!(
                        "tunnel: {} client exited ({}) — respawning, and will retry quietly \
                         every 5s until it holds",
                        t.addr,
                        status.code().unwrap_or(-1)
                    );
                } else if n % 60 == 0 {
                    println!(
                        "tunnel: {} still down after {n} attempts (~{} min), same exit each \
                         time — check the box is reachable and the key still works",
                        t.addr,
                        n * 5 / 60
                    );
                }
            }
            (false, _) => {}
        }
        match tokio::process::Command::new("ssh")
            .args(t.ssh.reverse_hook_args(DEFAULT_HOOK_PORT, api_port))
            // The tunnel is a pure pipe: no stdin to inherit, nothing for a
            // signal storm to reach. Inherited stdio would also tie the
            // child's lifetime to the terminal.
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(child) => {
                children.insert(t.addr.clone(), child);
            }
            Err(e) => {
                // Usually no `ssh` on PATH — say so once per pass, not per
                // box, because the remedy is machine-wide.
                println!("tunnel: {} spawn failed: {e}", t.addr);
            }
        }
    }
    Ok(())
}

/// A box worth a tunnel: how to reach it over ssh plus its address.
struct TunnelTarget {
    addr: String,
    ssh: Ssh,
}

/// Every remote, driven box — the same population `dispatch` drives, reached
/// through the same ssh chain (`Ssh::for_machine`), so a tunnel always rides
/// the identity the box already trusts. Local boxes need no tunnel (their
/// workers share this process's loopback) and undriven boxes (`task_port:
/// null`) get nothing, exactly as they are offered nothing.
async fn tunnel_targets(state: &Shared) -> Vec<TunnelTarget> {
    let inner = state.lock().await;
    let settings_key = inner.settings.ssh.key.clone();
    let mut out: Vec<TunnelTarget> = inner
        .machines
        .values()
        .filter(|m| m.task_port.is_some())
        .filter(|m| !bm_core::is_local_node(&m.addr))
        .map(|m| {
            // Same chain as `segments::targets`: the box's key, else the app
            // default, else ssh decides. Resolved here (not left to ssh) so
            // the tunnel honours exactly the key the provisioner pushed with.
            let (key, _) = resolve_key(m.ssh_key.as_deref(), settings_key.as_deref());
            TunnelTarget {
                addr: m.addr.clone(),
                ssh: Ssh {
                    target: m.ssh_target(),
                    port: m.ssh_port,
                    key: key.map(|p| p.to_string_lossy().to_string()),
                    local: false,
                },
            }
        })
        .collect();
    out.sort_by(|a, b| a.addr.cmp(&b.addr));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> (tempfile::TempDir, crate::api::Shared) {
        let d = tempfile::tempdir().unwrap();
        let layout = bm_core::Layout::new(d.path());
        let inner = crate::state::Inner::new(layout, bm_core::config::Settings::default());
        (d, std::sync::Arc::new(tokio::sync::Mutex::new(inner)))
    }

    #[tokio::test]
    async fn targets_are_remote_driven_boxes_only() {
        let (_d, st) = scratch();
        {
            let mut inner = st.lock().await;
            for addr in ["192.168.2.2", "10.0.0.5"] {
                let mut m = bm_proto::Machine::new(addr, "thang", 22, None, "worker");
                m.task_port = Some(bm_proto::DEFAULT_TASK_PORT);
                inner.machines.insert(addr.into(), m);
            }
            let mut local = bm_proto::Machine::new("127.0.0.1", "me", 22, None, "worker");
            local.task_port = Some(bm_proto::DEFAULT_TASK_PORT);
            inner.machines.insert("127.0.0.1".into(), local);
            // Driven off: no drive, no tunnel.
            let mut undriven = bm_proto::Machine::new("10.0.0.9", "thang", 22, None, "worker");
            undriven.task_port = None;
            inner.machines.insert("10.0.0.9".into(), undriven);
        }
        let got = tunnel_targets(&st).await;
        let addrs: Vec<&str> = got.iter().map(|t| t.addr.as_str()).collect();
        assert_eq!(addrs, vec!["10.0.0.5", "192.168.2.2"], "sorted remotes only");
        assert!(!got.iter().any(|t| t.ssh.local), "local never tunnels to itself");
    }

    #[tokio::test]
    async fn the_tunnel_forwards_the_hook_port_at_the_api() {
        // The whole contract in one argv: worker loopback {hook} -> inductor
        // {api}. The two ends must spell the ports the same way, so the
        // constant is asserted here against the same values the builder got.
        let (_d, st) = scratch();
        let mut m = bm_proto::Machine::new("192.168.2.2", "thang", 22, None, "worker");
        m.task_port = Some(bm_proto::DEFAULT_TASK_PORT);
        st.lock().await.machines.insert(m.addr.clone(), m);

        let t = &tunnel_targets(&st).await[0];
        let args = t.ssh.reverse_hook_args(DEFAULT_HOOK_PORT, 8901);
        // Spawned without a shell, so `-R` and its forward are separate argv
        // entries — assert the pair, not a fused `-R:` spelling.
        let fwd = args
            .iter()
            .position(|a| a == "-R")
            .map(|i| args[i + 1].clone())
            .unwrap_or_default();
        assert_eq!(
            fwd,
            format!("{DEFAULT_HOOK_PORT}:127.0.0.1:8901"),
            "argv must forward {DEFAULT_HOOK_PORT} -> api 8901: {args:?}"
        );
        assert!(args.contains(&"-N".to_string()), "no remote command: {args:?}");
        assert!(
            args.contains(&"ExitOnForwardFailure=yes".to_string()),
            "a failed bind must kill the client: {args:?}"
        );
    }

    #[tokio::test]
    async fn a_departed_box_has_its_tunnel_killed() {
        let (_d, st) = scratch();
        // A child that would outlive the registry entry: a sleep, spawned the
        // same way the supervisor spawns ssh.
        let child = tokio::process::Command::new("sh")
            .args(["-c", "exec sleep 60"])
            .spawn()
            .unwrap();
        let mut children: HashMap<String, tokio::process::Child> =
            HashMap::from([("192.168.2.2".into(), child)]);
        pass(&st, 8901, &mut children, &mut HashMap::new())
            .await
            .unwrap();
        assert!(
            !children.contains_key("192.168.2.2"),
            "a box that left must not keep a channel"
        );
    }

    /// A child that has **already exited**, the shape a refused ssh leaves
    /// behind. Reaped before the pass, so `try_wait` answers `Some(status)`
    /// deterministically rather than on a timer.
    async fn dead_child() -> tokio::process::Child {
        let mut c = tokio::process::Command::new("sh")
            .args(["-c", "exit 255"])
            .spawn()
            .unwrap();
        c.wait().await.unwrap();
        c
    }

    #[tokio::test]
    async fn a_box_that_stays_down_is_counted_across_passes_not_printed_each_time() {
        // 1339 identical lines were 16% of the inductor log on 2026-09-22, and
        // 198 of its last 200 — every real event buried under a message that
        // never changed, which is a large part of why "there was no error"
        // anywhere. The fix is that the failure *streak* survives a pass and
        // only the first failure (and every sixtieth) is printed, so the thing
        // worth asserting is the streak.
        let (_d, st) = scratch();
        let mut m = bm_proto::Machine::new("192.168.2.2", "thang", 22, None, "worker");
        m.task_port = Some(bm_proto::DEFAULT_TASK_PORT);
        st.lock().await.machines.insert(m.addr.clone(), m);

        let mut failing: HashMap<String, u32> = HashMap::new();
        let mut children: HashMap<String, tokio::process::Child> = HashMap::new();
        // Starting a tunnel is not failing: the first pass spawns one and the
        // streak is untouched.
        pass(&st, 8901, &mut children, &mut failing).await.unwrap();
        assert!(children.contains_key("192.168.2.2"), "a tunnel was started");
        assert_eq!(failing.get("192.168.2.2"), None, "starting is not failing");

        // Each pass finds the client dead and respawns it, as a refused ssh
        // does. The streak has to climb across passes — that is the whole
        // mechanism that keeps the log quiet.
        for expected in 1..=3 {
            if let Some(mut old) = children.remove("192.168.2.2") {
                let _ = old.kill().await;
            }
            children.insert("192.168.2.2".into(), dead_child().await);
            pass(&st, 8901, &mut children, &mut failing).await.unwrap();
            assert_eq!(
                failing.get("192.168.2.2").copied(),
                Some(expected),
                "the streak survives the pass"
            );
        }

        // And a tunnel that *holds* clears it, so the next failure is a fresh
        // edge rather than a continuation — which is what makes the recovery
        // line printable exactly once.
        if let Some(mut old) = children.remove("192.168.2.2") {
            let _ = old.kill().await;
        }
        let alive = tokio::process::Command::new("sh")
            .args(["-c", "exec sleep 30"])
            .spawn()
            .unwrap();
        children.insert("192.168.2.2".into(), alive);
        pass(&st, 8901, &mut children, &mut failing).await.unwrap();
        assert_eq!(
            failing.get("192.168.2.2"),
            None,
            "a live tunnel clears the streak"
        );
        if let Some(mut c) = children.remove("192.168.2.2") {
            let _ = c.kill().await;
        }
    }
}
