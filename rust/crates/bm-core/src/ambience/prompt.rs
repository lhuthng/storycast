use super::*;
/// The palette's keys, sorted, what a script's `music` value is checked
/// against, and what a rejection message lists back to the analyzer.
pub fn palette_names(map: &SceneMap) -> Vec<String> {
    map.music_palette.keys().cloned().collect()
}

/// The palette rendered for the digest prompt: `name (tags; gloss), ...`.
///
/// Built from the map rather than written into the prompt text, so adding a
/// value (and the clip that answers it) is one edit to one file. A prompt that
/// listed its own vocabulary would drift the moment the pool changed. The tags
/// ride along so the analyzer sees what each mood *means* in pool terms, the
/// merge scores those same tags, so a mood picked for its tags resolves to the
/// track the analyzer had in mind.
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
/// word the resolved rules can match, sorted and deduped.
///
/// **The place vocabulary, which is not the bed vocabulary.** A `scene` label
/// is matched against [`Rule::matches`], so those words are what the analyzer
/// has to write for a rule to fire at all; [`effect_tags`] is the pool's answer
/// to the tag sets the rules hand it. The two were being conflated: the prompt
/// injected the effect tags, called them "the vocabulary it answers to", and
/// warned that a label built from anything else "gets silence" — so a rule
/// whose match word is not an effect tag (`palace`, `hall`, `garden`, `gate`,
/// `morning`, `dusk`) matched a word the model had been told not to use. On the
/// shipped map 15 of 61 match words are effect tags, so the rules were running
/// on the intersection.
///
/// This is the same arrangement as [`palette_prompt`]: the vocabulary is
/// rendered from the map rather than written into a prompt, so a pack that adds
/// a rule reaches the analyzer without a pack being able to edit a prompt —
/// which is the whole reason the music palette lives pack-side. Without it a
/// rule is decoration the operator cannot see.
///
/// Every rule contributes, `default` does not: it has no match set, it is what
/// a label matches when nothing else did.
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
/// vocabulary the digest prompt offers the analyzer. A scene built from these
/// words resolves to a pooled sound by tag overlap instead of by keyword luck.
///
/// The *place* vocabulary is [`scene_prompt`], and the two are not the same
/// list. A place word that is no bed word is still the right thing to write:
/// the rules route it, and the bed is chosen by tag overlap, not by the label.
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
// what a pool is still being used for
// ---------------------------------------------------------------------------
