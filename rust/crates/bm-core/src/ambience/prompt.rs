use super::*;
/// The palette's keys, sorted, what a script's `music` value is checked
pub fn palette_names(map: &SceneMap) -> Vec<String> {
    map.music_palette.keys().cloned().collect()
}

/// The palette rendered for the digest prompt: `name (tags; gloss), ...`.
pub fn palette_prompt(map: &SceneMap) -> String {
    map.music_palette
        .iter()
        .map(|(name, e)| {
            let mut parts = Vec::new();
            if !e.tags.is_empty() {
                parts.push(e.tags.join(", "));
            }
            let note = e.note.trim();
            if !note.is_empty() {
                parts.push(note.to_string());
            }
            if parts.is_empty() {
                name.clone()
            } else {
                format!("{name} ({})", parts.join("; "))
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// The scene map's rule vocabulary rendered for the digest prompt: every match
pub fn scene_prompt(map: &SceneMap) -> String {
    let mut words: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for rule in &map.rules {
        words.extend(
            rule.matches
                .iter()
                .filter(|w| !w.trim().is_empty())
                .cloned(),
        );
    }
    words.into_iter().collect::<Vec<String>>().join(", ")
}

/// Every tag any effect-pool sound answers to, sorted and deduped: the **bed**
pub fn effect_tags(pool: &ClipPool) -> Vec<String> {
    let mut out = std::collections::BTreeSet::new();
    for sound in pool.values() {
        for t in &sound.tags {
            out.insert(t.clone());
        }
    }
    out.into_iter().collect()
}

// ---------------------------------------------------------------------------
