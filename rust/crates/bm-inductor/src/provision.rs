use super::stage::agent_binary_for;
use super::stage::tts_binary_for;
use super::stage::tts_runtime_dir;
use super::*;

/// What one provision run concluded.
///
/// `ready` and `reachable` are separate on purpose, and the difference is the
/// one that matters to an operator staring at a box that will not come up.
/// `!reachable` means ssh never answered, so *nothing* was learned: no
/// platform, no binary pushed, no stamp read. The failure is a network fact,
/// not a provisioning one, and a caller that already knows the box was
/// launched seconds ago can keep saying "still booting" instead of "broken".
pub struct ProvisionOutcome {
    /// The post-provision probe says the box runs this exact agent build with
    /// TTS, the gate a start hides behind.
    pub ready: bool,
    /// ssh answered at all.
    pub reachable: bool,
    /// The log, one line per step, prefixed with the address.
    pub lines: Vec<String>,
    /// Why the run stopped, when it stopped before a step could log it
    /// a missing local build, an unreadable profile pointer. Carried rather
    /// than recovered by grepping `lines`: the dashboard used to scan the log
    /// for the words "missing"/"not found"/"failed", and every pre-flight
    /// message that did not use one of them ("no TTS sidecar binary for …")
    /// left the machine pane reporting a bare `provision INCOMPLETE` and
    /// telling the operator to retry the same click forever.
    pub stop: Option<String>,
}

/// A run that stopped before any step: the message is logged *and* carried,
/// so the log and the machine pane cannot disagree about why.
pub(crate) fn stopped(
    log: &mut bm_core::provision::LiveLog,
    addr: &str,
    why: impl std::fmt::Display,
    reachable: bool,
) -> ProvisionOutcome {
    let why = why.to_string();
    log.push(format!("[{addr}] {why}"));
    ProvisionOutcome {
        ready: false,
        reachable,
        stop: Some(why),
        lines: std::mem::take(&mut log.lines),
    }
}

/// Blocking provision run shared by the CLI and the TUI background task.
///
/// `live` streams each log line to the TUI event pane as it happens (slow
/// steps read as progress, not a stall); `None` keeps collect-only for the
/// CLI, which prints everything at the end.
// Each parameter is a distinct fact about one box, and bundling them into an
// options struct would be a redesign of a funnel three front ends call.
#[allow(clippy::too_many_arguments)]
pub fn provision_machine(
    layout: &Layout,
    addr: &str,
    user: &str,
    port: u16,
    key: Option<String>,
    force: bool,
    live: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    // `owner/name` of the releases hosting the model artifact, for a one-shot
    // that overrides the workspace setting. `None` means "read the settings",
    // which is what both the TUI and a plain `provision` want.
    release_repo: Option<String>,
) -> ProvisionOutcome {
    if bm_core::is_local_node(addr) {
        // No mirror to fill: the local worker runs in place from this repo
        // prompts, assets, models and binaries are read where they stand, so
        // syncing a `~/bm-worker` copy would only spend disk and let a stale
        // copy fail the readiness gate below. Launching the worker stays the
        // caller's job (`:B` catch-up, `make agent`).
        return ProvisionOutcome {
            ready: true,
            reachable: true,
            stop: None,
            lines: vec![format!(
                "[{addr}] local machine — runs from the repo, nothing to provision"
            )],
        };
    }
    use bm_core::provision::{provision, LiveLog, Ssh};
    let mut log = LiveLog::new(live.clone());
    let probe_ssh = Ssh {
        target: format!("{user}@{addr}"),
        port,
        key: key.clone(),
        local: bm_core::is_local_node(addr),
    };
    let pre = probe_ssh.probe(&layout.engine);
    log.push(format!("[{addr}] {}", pre.summary()));
    // Unreachable means nothing downstream can run: no platform was learned
    // (os/arch stay empty, the old flow continued and failed confusingly on
    // "no agent binary for /"), no binary can be pushed, no worker launched.
    // Name the network cause and stop.
    if !pre.reachable {
        return stopped(
            &mut log,
            addr,
            format!(
                "cannot provision: the box never answered ssh ({}) — check it is up, and that its address is reachable from here (an EC2 private IP like 172.31.x.x is only routable from inside the VPC; the pool prefers public IPs)",
                pre.note
            ),
            false,
        );
    }
    // The binding **in force**, not the checkout pointer: a workspace's own
    // pack travels in the bundle, and this is the line that has to agree with
    // the pointer `provision` writes to the worker. An unset binding is the
    // no-profile refusal, exactly as a missing pointer was.
    let binding = match bm_core::profile::in_force(layout)
        .ok()
        .filter(|b| !b.is_unset())
    {
        Some(b) => b,
        None => {
            return stopped(
                &mut log,
                addr,
                "no local profile loaded — load one first (`:profile` in the dashboard; `tools/profile.sh fetch/unpack <name>`) — workers verify it at startup",
                true,
            );
        }
    };
    log.push(format!(
        "[{addr}] profile: {} ({})",
        bm_core::profile::label(&binding),
        &binding.pack.hash[..12.min(binding.pack.hash.len())]
    ));
    let binary = match agent_binary_for(pre.os.as_str(), pre.arch.as_str(), layout) {
        Ok(b) => b,
        Err(e) => return stopped(&mut log, addr, e, true),
    };
    log.push(format!("[{addr}] agent binary: {}", binary.display()));
    let tts = match tts_binary_for(pre.os.as_str(), pre.arch.as_str(), layout, &mut log, addr) {
        Ok(b) => b,
        Err(e) => return stopped(&mut log, addr, e, true),
    };
    log.push(format!("[{addr}] tts sidecar: {}", tts.display()));
    let mut m = Machine::new(addr, user, port, key, "worker");
    m.tts_url = Some("http://127.0.0.1:8818".into());
    // The policy decides *what this box is handed* (`provision` plans the
    // sources from it), so it has to be this box's and not the four-stage
    // default a bare `Machine::new` carries. Without this every push sent the
    // full set and the narrowing was inert — the box got prompts it has no
    // digest to read and clips it never plays. `machines.json` is where the
    // policy panel persists one (the API writes it on every save), and it is
    // the only source that outlives this process: the live copy belongs to the
    // scheduler, which may not be running. A box with no stored policy keeps
    // the default, which is the safe direction.
    carry_task_policy(&mut m, layout);
    let (after, mut flow) = provision(
        &m,
        layout,
        &binary,
        &tts,
        tts_runtime_dir(pre.os.as_str(), pre.arch.as_str(), layout).as_deref(),
        env!("CARGO_PKG_VERSION"),
        force,
        Some(pre),
        live,
        release_repo.as_deref(),
    );
    // Already streamed live inside `provision`, collect silently here.
    log.lines.append(&mut flow);
    ProvisionOutcome {
        ready: after.configured(env!("CARGO_PKG_VERSION")),
        reachable: true,
        stop: None,
        lines: log.lines,
    }
}

/// A re-provision must not reset the operator's work policy: the machine
/// `cmd_provision` registers is fresh (`task_policy: None`), and both
/// registration paths persist it, the live POST and the ledger fallback.
/// Carry the stored policy forward so re-provisioning keeps the order and
/// toggles from the policy panel.
pub(crate) fn carry_task_policy(m: &mut Machine, layout: &Layout) {
    if m.task_policy.is_none() {
        m.task_policy = bm_core::provision::load_boxes(&layout.machines())
            .iter()
            .find(|b| b.addr == m.addr)
            .and_then(|b| b.task_policy.clone());
    }
}

pub(crate) fn check_bins() -> anyhow::Result<()> {
    for bin in ["ssh", "rsync", "ffmpeg"] {
        let found = std::env::var_os("PATH")
            .map(|paths| std::env::split_paths(&paths).any(|d| d.join(bin).is_file()))
            .unwrap_or(false);
        if !found {
            anyhow::bail!(
                "{bin} not found on PATH: provisioning needs ssh/rsync, merging needs ffmpeg"
            );
        }
    }
    Ok(())
}
