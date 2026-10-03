use super::sidecar::sidecar_client;
use super::*;

/// Everything about voices that disk alone knows: shipped catalogue, enrolled
/// clones, pool samples. No sidecar, no inductor, milliseconds, never hangs.
fn disk_voices(
    layout: &bm_core::Layout,
    engine: &str,
    effective: &bm_core::voices::EngineRoster,
) -> Vec<VoiceInfo> {
    let mut voices = effective.to_offline_voices(engine);
    for clone in bm_core::voices::enrolled_voices(&layout.voices_manifest()) {
        if !voices.iter().any(|v| v.name == clone.name) {
            voices.push(clone);
        }
    }
    // The sample pool rides the same list: a pooled sample shows its tags where
    // the style was, so the picker filter (`young`) finds it, and a sample the
    // registry names but nothing enrolled yet still shows, as vetted-at-adding
    // like any clone (the render fails loudly if it never gets enrolled). The
    // tags also ride along as `pool_tags`, which is what makes the picker able
    // to tell an auto-assignable voice from a unique one.
    for (name, entry) in bm_core::pool::load_pool(&layout.voice_pool()) {
        let style = if entry.tags.is_empty() {
            "named voice".to_string()
        } else {
            format!("pool: {}", entry.tags.join(", "))
        };
        match voices.iter_mut().find(|v| v.name == name) {
            Some(v) => {
                v.style = style;
                v.pool_tags = entry.tags.clone();
            }
            None => voices.push(VoiceInfo {
                key: String::new(),
                name,
                gender: "unknown".into(),
                accent: "unknown".into(),
                language: "vi-VN".into(),
                style,
                pool_tags: entry.tags.clone(),
                enrolled: true,
            }),
        }
    }
    // Assignable voices first, then by gender then name: a stable order means
    // the picker's cursor does not jump between refreshes.
    voices.sort_by(|a, b| (&a.gender, &a.name).cmp(&(&b.gender, &b.name)));
    voices
}

/// Roster with no scheduler and no sidecar: what the picker shows instantly.
/// A live upgrade may follow, but picking never waits for it.
pub(crate) fn local_roster(layout: &bm_core::Layout) -> Roster {
    let settings = bm_core::config::Settings::load(&layout.settings());
    let engine = settings.engine.clone();
    let mut inner = Inner::new(layout.clone(), settings);
    inner.load_ledger();
    let characters = inner.known_characters();
    let cast = inner.cast_snapshot();
    let (effective, _) = bm_core::voices::effective_engine_lenient(&engine);
    Roster {
        engine: engine.clone(),
        source: "offline".into(),
        voices: disk_voices(layout, &engine, &effective),
        cast,
        characters,
    }
}

/// Assemble the roster the picker renders: the sidecar's structured roster when
/// it answers, then its label form, then the bundled table.
///
/// Enrolled clones from `voices.json` are merged in regardless, so a clone the
/// operator added by hand never disappears just because the sidecar is down.
pub(crate) async fn build_roster(
    layout: &bm_core::Layout,
    engine: &str,
    characters: Vec<String>,
    cast: BTreeMap<String, String>,
) -> Roster {
    // Loopback: a serving sidecar answers in ms, a loading one 503s, a dead
    // one refuses, none of which is worth more than 2s of picker. (Was 10s
    // × 2: every :s press stared at "loading roster" for 20s+ while booting.)
    let http = sidecar_client(Duration::from_secs(2));
    let mut source = "offline".to_string();
    let mut voices: Vec<VoiceInfo> = Vec::new();

    // The effective roster is the shipped catalogue.
    let (effective, _) = bm_core::voices::effective_engine_lenient(engine);

    if let Ok(r) = http.get(format!("{SIDECAR}/roster")).send().await {
        if let Ok(v) = r.json::<Vec<VoiceInfo>>().await {
            if !v.is_empty() {
                voices = v;
                source = "live".into();
            }
        }
    }
    // A sidecar older than this build still answers /voices with SDK labels.
    if voices.is_empty() {
        if let Ok(r) = http.get(format!("{SIDECAR}/voices")).send().await {
            if let Ok(pairs) = r.json::<Vec<Vec<String>>>().await {
                let labels: Vec<(String, String)> = pairs
                    .into_iter()
                    .filter_map(|p| match p.as_slice() {
                        [label, id] => Some((label.clone(), id.clone())),
                        _ => None,
                    })
                    .collect();
                if !labels.is_empty() {
                    voices = bm_core::voices::voices_from_labels(engine, &labels);
                    source = "live (labels)".into();
                }
            }
        }
    }
    if voices.is_empty() {
        voices = disk_voices(layout, engine, &effective);
    }
    // The merges below are no-ops on the disk path (same names, same styles)
    // and complete a live answer with the local truth.
    for clone in bm_core::voices::enrolled_voices(&layout.voices_manifest()) {
        if !voices.iter().any(|v| v.name == clone.name) {
            voices.push(clone);
        }
    }
    // The sample pool rides the same list: a pooled sample shows its tags where
    // the style was, so the picker filter (`young`) finds it, and a sample the
    // registry names but nothing enrolled yet still shows, as vetted-at-adding
    // like any clone (the render fails loudly if it never gets enrolled). The
    // tags also ride along as `pool_tags`, which is what makes the picker able
    // to tell an auto-assignable voice from a unique one.
    for (name, entry) in bm_core::pool::load_pool(&layout.voice_pool()) {
        let style = if entry.tags.is_empty() {
            "named voice".to_string()
        } else {
            format!("pool: {}", entry.tags.join(", "))
        };
        match voices.iter_mut().find(|v| v.name == name) {
            Some(v) => {
                v.style = style;
                v.pool_tags = entry.tags.clone();
            }
            None => voices.push(VoiceInfo {
                key: String::new(),
                name,
                gender: "unknown".into(),
                accent: "unknown".into(),
                language: "vi-VN".into(),
                style,
                pool_tags: entry.tags.clone(),
                enrolled: true,
            }),
        }
    }
    // Assignable voices first, then by gender then name: a stable order means
    // the picker's cursor does not jump between refreshes.
    voices.sort_by(|a, b| (&a.gender, &a.name).cmp(&(&b.gender, &b.name)));
    Roster {
        engine: engine.to_string(),
        source,
        voices,
        cast,
        characters,
    }
}
