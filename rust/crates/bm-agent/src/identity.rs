use super::*;

/// Who this worker is, as every report identifies it.
#[derive(Debug, Clone)]
pub(crate) struct WorkerIdentity {
    pub(crate) worker_id: String,
    pub(crate) addr: String,
    pub(crate) hostname: String,
    pub(crate) alias: String,
    /// This box's worker root. Carried here because a beat has to say which
    pub(crate) root: PathBuf,
}

/// The `(stage, adapter)` slots this box's sources bundle covers, from the
/// or unreadable manifest is an empty list, which the inductor reads as "no
/// opinion" rather than "covers nothing".
pub(crate) fn bundle_slots(root: &Path) -> Vec<String> {
    std::fs::read_to_string(root.join(bm_core::provision::sources::MANIFEST_NAME))
        .ok()
        .and_then(|t| serde_json::from_str::<bm_core::provision::sources::SourcesManifest>(&t).ok())
        .map(|m| m.slots)
        .unwrap_or_default()
}

/// The heartbeat for right now. **One builder for both directions.**
pub(crate) fn heartbeat_now(
    p: &Progress,
    who: &WorkerIdentity,
    probe: &mut LoadProbe,
    sidecar_keep: bool,
    tts_threads: Option<u32>,
) -> Heartbeat {
    let (cpu_pct, mem_pct, mem_gb, sidecars, sidecar_gb) = probe.sample();
    // A completed stage must not survive the task boundary as a live-looking
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
        p.pending = None;
    }
}

/// Say so when the box's own profile is not the one the task came from.
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
