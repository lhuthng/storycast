use super::*;

pub fn load_map(path: &Path) -> Result<SceneMap> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading scene map {}", path.display()))?;
    let map: SceneMap = serde_json::from_str(&text)
        .with_context(|| format!("parsing scene map {}", path.display()))?;
    Ok(map)
}

/// First matching rule wins (ordered specific -> general).
///
/// Substring matching, not token matching, and on purpose: `market-stall-morning`
/// has to reach the `market` rule. The cost is that a keyword can fire on a word
/// that is only part of a compound label, which is why the rules are ordered
/// most-specific-first and why the generic place nouns that used to sit in a
/// catch-all (`shop`, `room`) are no longer keywords here.
pub fn match_scene(scene: &str, cfg: &SceneMap) -> SceneRule {
    let s = scene.to_lowercase();
    for rule in &cfg.rules {
        if rule.matches.iter().any(|k| s.contains(&k.to_lowercase())) {
            return SceneRule {
                effect: rule.effect.clone(),
                level: rule.level,
                reverb: rule.reverb.clone(),
                pause_before_s: rule.pause_before_s,
            };
        }
    }
    cfg.default.clone()
}

/// The majority value of a field across a run, ties to the last seen.
///
/// Shared by the scene and the music summary so a run cannot end up summarised
/// two different ways, and so "which line wins a tie" is decided once.
fn majority(vals: impl Iterator<Item = String>) -> String {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for v in vals {
        if v.is_empty() {
            continue;
        }
        match counts.iter_mut().find(|(t, _)| *t == v) {
            Some((_, n)) => *n += 1,
            None => counts.push((v, 1)),
        }
    }
    counts
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .map(|(t, _)| t)
        .unwrap_or_default()
}

fn seg_field(segments: &[Value], i: usize, key: &str) -> String {
    segments
        .get(i)
        .and_then(|s| s.get(key))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Majority scene tag per run (runs are consecutive same-speaker lines).
pub fn run_scenes(segments: &[Value], runs: &[Run]) -> Vec<String> {
    runs.iter()
        .map(|run| majority(run.idx.iter().map(|i| seg_field(segments, *i, "scene"))))
        .collect()
}

/// Majority `music` value per run, falling back to the legacy keyword lookup.
///
/// A run that declares no value anywhere is a run from a script written before
/// the field existed, so it is scored by `legacy_scene_music` off its scene
/// label, the shim that keeps chapters already on disk mergeable. Mixed runs
/// resolve the same way per run, which is the graceful reading: a hand-edited
/// old script still merges rather than losing its music entirely.
pub fn run_music(segments: &[Value], runs: &[Run], cfg: &SceneMap) -> Vec<String> {
    let mut musics = run_scenes(segments, runs);
    for (run, scene) in runs.iter().zip(musics.iter_mut()) {
        let declared = majority(run.idx.iter().map(|i| seg_field(segments, *i, "music")));
        *scene = if declared.is_empty() {
            legacy_music(scene, cfg)
        } else {
            declared
        };
    }
    musics
}

/// One segment's mood: its declared `music` value, else the legacy lookup over
/// its scene label. The single-segment form of [`run_music`], for the cloud
/// engine path where one turn *is* one segment.
pub fn resolve_music(scene: &str, declared: Option<&str>, cfg: &SceneMap) -> String {
    match declared.map(str::trim).filter(|m| !m.is_empty()) {
        Some(m) => m.to_string(),
        None => legacy_music(scene, cfg),
    }
}

/// The migration shim: keyword rules over a scene label, to a palette value.
pub fn legacy_music(scene: &str, cfg: &SceneMap) -> String {
    let s = scene.to_lowercase();
    for rule in &cfg.legacy_scene_music.rules {
        if rule.matches.iter().any(|k| s.contains(&k.to_lowercase())) {
            return rule.music.clone();
        }
    }
    cfg.legacy_scene_music.default.music.clone()
}

// ---------------------------------------------------------------------------
// the timeline
// ---------------------------------------------------------------------------
