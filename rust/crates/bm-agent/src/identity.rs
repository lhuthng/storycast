use super::*;

/// Who this worker is, as every report identifies it.
///
/// A struct rather than four arguments because the identity is passed to two
/// callers now, the heartbeat loop that pushes beats and the status endpoint
/// that answers for one, and four strings in the same order is exactly the
/// shape that gets transposed silently.
#[derive(Debug, Clone)]
pub(crate) struct WorkerIdentity {
    pub(crate) worker_id: String,
    pub(crate) addr: String,
    pub(crate) hostname: String,
    pub(crate) alias: String,
    /// This box's worker root. Carried here because a beat has to say which
    /// stages the sources bundle on disk actually covers, and that bundle is
    /// read from `sources-manifest.json` under this root.
    pub(crate) root: PathBuf,
}

/// The `(stage, adapter)` slots this box's sources bundle covers, from the
/// manifest the last provision left at the worker root.
///
/// Read **per beat** rather than cached at startup: a push happens under a
/// running agent, so a list read once at boot would keep withholding the work
/// the box has just been handed until somebody restarted the worker. A missing
/// or unreadable manifest is an empty list, which the inductor reads as "no
/// opinion" rather than "covers nothing".
///
/// The manifest is the box's own statement of what it holds; the inductor turns
/// it into a gate against the adapter the offer is *for*, which is why the list
/// is two-dimensional — one bundle carries every language, and a stage name
/// alone cannot say which of them this box can run.
pub(crate) fn bundle_slots(root: &Path) -> Vec<String> {
    std::fs::read_to_string(root.join(bm_core::provision::sources::MANIFEST_NAME))
        .ok()
        .and_then(|t| serde_json::from_str::<bm_core::provision::sources::SourcesManifest>(&t).ok())
        .map(|m| m.slots)
        .unwrap_or_default()
}

/// The heartbeat for right now. **One builder for both directions.**
///
/// The pull protocol posts this on a timer and the inverted protocol answers
/// `GET /status` with it. Two constructions would be two chances for the
/// inductor's liveness bookkeeping and its panes to disagree about what a
/// worker is doing, depending on which way the report travelled.
///
/// `sidecar_keep` is the worker's own belief about its sidecar: pull mode has
/// no instruction channel, so it is always `true` here, serve mode passes
/// what the inductor last told it, and the dispatcher's convergence reads
/// that back to detect a box that rebooted into its default.
pub(crate) fn heartbeat_now(
    p: &Progress,
    who: &WorkerIdentity,
    probe: &mut LoadProbe,
    sidecar_keep: bool,
    tts_threads: Option<u32>,
) -> Heartbeat {
    let (cpu_pct, mem_pct, mem_gb, sidecars, sidecar_gb) = probe.sample();
    // A completed stage must not survive the task boundary as a live-looking
    // heartbeat. `clear_task` normally removes the whole block together, but
    // the completion report and the next status poll are separate concurrent
    // operations. If a poll observes the terminal activity after the task id
    // has already been cleared, report the authoritative state instead of
    // resurrecting `digest chN done` as though the worker were still working.
    let stale_completion = p.task_id.is_none() && p.activity.contains(" done");
    let (task_id, stage, chapter, progress, activity) = if stale_completion {
        (None, None, None, 0.0, "idle".to_string())
    } else {
        (
            p.task_id.clone(),
            p.stage.as_deref().and_then(bm_proto::Stage::parse),
            p.chapter,
            p.frac,
            p.activity.clone(),
        )
    };
    Heartbeat {
        worker_id: who.worker_id.clone(),
        addr: who.addr.clone(),
        task_id,
        stage,
        chapter,
        progress,
        activity,
        eta_secs: None,
        ts: bm_proto::now_secs(),
        hostname: who.hostname.clone(),
        alias: who.alias.clone(),
        cpu_pct,
        mem_pct,
        mem_gb,
        sidecars,
        sidecar_gb,
        capabilities: capabilities(),
        sources_stages: bundle_slots(&who.root),
        sidecar_keep: Some(sidecar_keep),
        tts_threads,
        cores: std::thread::available_parallelism()
            .ok()
            .map(|n| n.get() as u32),
    }
}

/// What this worker can run, in one place.
///
/// Both `Register` and the `/status` answer carry it, and the inductor's
/// render gate reads whichever arrived. Two copies would let a worker claim
/// one thing on registration and another on its status poll, and the gate
/// would believe whichever it saw last.
///
/// `render-segments` is the migration gate: the inductor only offers render
/// tasks to a box that can produce units.
///
/// `merge` is advertised **only when both ffmpeg and sox are on PATH**. The
/// merge stage shells out to ffmpeg for the beds and to sox for every voice
/// treatment's room, so a box missing either would take merges it cannot
/// finish, three strikes and the chapter shelves. Reporting the capability
/// truthfully lets the scheduler skip merge here and give the box its other
/// stages, instead of poisoning the ledger with failures it cannot help.
pub(crate) fn capabilities() -> Vec<String> {
    let mut caps = vec![
        "crawl".into(),
        "digest".into(),
        "render".into(),
        "render-segments".into(),
    ];
    if bm_core::assemble::ffmpeg_available() && bm_core::assemble::sox_available() {
        caps.push("merge".into());
    }
    caps
}

pub(crate) fn set_task(shared: &Shared, offer: &TaskOffer) {
    if let Ok(mut p) = shared.lock() {
        p.task_id = Some(offer.task_id.clone());
        p.stage = Some(offer.stage.as_str().to_string());
        p.chapter = Some(offer.chapter);
        p.frac = 0.0;
        p.activity = format!("{} ch{}", offer.stage, offer.chapter);
        // A new offer is the inductor, talking. Whatever the stash held is
        // now stale by definition, the task it described has been re-queued
        // and re-decided, so its completion must not land late and surprise
        // the ledger. (The hook had its whole silent window to deliver it.)
        p.pending = None;
    }
}

/// Say so when the box's own profile is not the one the task came from.
///
/// The **pack** is the leg of the binding a box can disagree about silently.
/// The adapter and the engine are in every cache path it writes, so a
/// mismatch shows up in the filenames; the pack is a property of the `assets/`
/// and `prompts/` a provision left here, and a box carrying another pack's
/// registries is being handed prompts that read files this pack never wrote.
///
/// Warned, not refused — "refuse where bytes are made, warn on load" — and
/// refused on the inductor, where the workspace's binding, the adapter it
/// names and the engine those bytes would be spoken with are all in one hand.
/// Silent when either side is silent, so an older inductor and a pre-split
/// pointer are both just quiet.
pub(crate) fn warn_on_pack_mismatch(layout: &Layout, offer: &TaskOffer) {
    if offer.pack.is_empty() {
        return;
    }
    let Ok(binding) = bm_core::profile::read_binding(&layout.root) else {
        return;
    };
    if !binding.pack.name.is_empty() && binding.pack.name != offer.pack {
        println!(
            "warning: this box holds profile '{}' but the task came from '{}' — its assets are \
             another profile's; re-provision it",
            binding.pack.name, offer.pack
        );
    }
}

pub(crate) fn clear_task(shared: &Shared) {
    if let Ok(mut p) = shared.lock() {
        p.task_id = None;
        p.stage = None;
        p.chapter = None;
        p.frac = 0.0;
        p.activity = "idle".to_string();
    }
}

/// Install the offer's credentials into this process and return the variable
/// names that were set, never the values, which do not belong in a log.
///
/// Two consumers read them straight out of the environment: the generation
/// backends in `bm-core::digest::llm` (which is why a provisioned box used to
/// die on `GEMINI_API_KEY missing` — provisioning never copies `.bm/`, so a
/// remote box holds no key file of its own), and the TTS sidecar, a
/// child process the worker spawns for a render and which inherits this
/// environment at `spawn()`.
///
/// **The inductor wins.** It holds the only copy the operator maintains (the
/// `L` screen, `.bm/llm.json`), so a value it sends replaces whatever this
/// box had — a stale key on one machine is precisely the failure this
/// replaces. An *empty* value is skipped rather than blanked, so an offer
/// from an inductor that has nothing configured changes nothing at all.
///
/// `set_var` is process-global; it runs here, before the stage is dispatched
/// and before any child is spawned, which is the only point at which no other
/// thread is reading the environment.
pub(crate) fn install_credentials(creds: &bm_proto::Credentials) -> Vec<&'static str> {
    for (name, value) in creds.pairs() {
        std::env::set_var(name, value);
    }
    creds.names()
}

pub(crate) fn hostname_simple() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "localhost".to_string())
}
