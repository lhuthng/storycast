//! Voice assignment — ported from `synthesize.load_cast`.

use crate::util::write_json;
use crate::voices::VoicePolicy;
use anyhow::Result;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::ops::{Deref, DerefMut};
use std::path::Path;

/// `character -> voice`, plus the bible those character names were resolved
#[derive(Debug, Clone, Default)]
pub struct Cast {
    voices: BTreeMap<String, String>,
    /// `Null` for a cast read straight off disk: exact-key lookup only, which
    bible: Value,
}

impl Cast {
    pub fn new() -> Self {
        Self::default()
    }

    /// The voice for `speaker`, whatever surface form it arrives in.
    pub fn get(&self, speaker: &str) -> Option<&String> {
        if let Some(v) = self.voices.get(speaker) {
            return Some(v);
        }
        if self.bible.is_null() {
            return None;
        }
        let canonical = crate::digest::resolve_speaker(&self.bible, speaker);
        self.voices.get(&canonical)
    }

    /// The bare map, for callers whose contract is the file's shape (the
    pub fn into_map(self) -> BTreeMap<String, String> {
        self.voices
    }

    /// Attach the bible the names in this cast were resolved against.
    pub fn with_bible(mut self, bible: Value) -> Self {
        self.bible = bible;
        self
    }
}

impl Deref for Cast {
    type Target = BTreeMap<String, String>;
    fn deref(&self) -> &Self::Target {
        &self.voices
    }
}

impl DerefMut for Cast {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.voices
    }
}

impl FromIterator<(String, String)> for Cast {
    fn from_iter<I: IntoIterator<Item = (String, String)>>(iter: I) -> Self {
        Cast {
            voices: iter.into_iter().collect(),
            bible: Value::Null,
        }
    }
}

impl IntoIterator for Cast {
    type Item = (String, String);
    type IntoIter = std::collections::btree_map::IntoIter<String, String>;
    fn into_iter(self) -> Self::IntoIter {
        self.voices.into_iter()
    }
}

impl<'a> IntoIterator for &'a Cast {
    type Item = (&'a String, &'a String);
    type IntoIter = std::collections::btree_map::Iter<'a, String, String>;
    fn into_iter(self) -> Self::IntoIter {
        self.voices.iter()
    }
}

/// The file and the wire format are the map, never the bible: a cast file has
impl Serialize for Cast {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.voices.serialize(s)
    }
}

impl<'de> Deserialize<'de> for Cast {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Cast {
            voices: BTreeMap::deserialize(d)?,
            bible: Value::Null,
        })
    }
}

fn strings(v: Option<&Value>) -> Vec<String> {
    v.and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// Read a cast file, resolving every value to a display name.
pub fn read_cast(engine: &str, path: &Path) -> Cast {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Cast>(&t).ok())
        .map(|cast| {
            cast.into_iter()
                .map(|(character, voice)| {
                    (character, crate::voices::resolve_voice_name(engine, &voice))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The on-disk form of a cast: catalogue keys where a voice has one, the display
fn cast_for_disk(engine: &str, cast: &Cast) -> Cast {
    cast.iter()
        .map(|(character, voice)| {
            let value = crate::voices::key_for_name(engine, voice).unwrap_or_else(|| voice.clone());
            (character.clone(), value)
        })
        .collect()
}

/// Write a cast in its on-disk form: catalogue keys where a voice has one.
pub fn write_cast(engine: &str, path: &Path, cast: &Cast) -> Result<()> {
    write_json(path, &cast_for_disk(engine, cast))
}

/// The on-disk form of a cast without writing it.
pub fn cast_on_disk(engine: &str, cast: &Cast) -> Cast {
    cast_for_disk(engine, cast)
}

/// The sample pool for this bible: the **workspace's own** `voice-pool.json`.
fn pool_for_bible(bible_path: &Path) -> crate::pool::Pool {
    let beside = bible_path.parent();
    let workspace = beside.and_then(|d| d.parent());
    for dir in [beside, workspace].into_iter().flatten() {
        let pool = crate::pool::load_pool(&dir.join("voice-pool.json"));
        if !pool.is_empty() {
            return pool;
        }
    }
    crate::pool::load_pool(Path::new("/nonexistent/voice-pool.json"))
}

/// The assignable-voice policy for a render: the shipped catalogue, narrowed
pub fn policy_for_bible(engine: &str, layout: &crate::Layout) -> VoicePolicy {
    let policy = crate::voices::effective_policy(engine);
    match crate::pool::installed_voices(layout) {
        Some(installed) => policy.restricted_to(&installed),
        None => policy,
    }
}

/// Resolve the full cast for a chapter, assigning any missing speaker.
pub fn load_cast(
    script_path: &Path,
    cast_path: &Path,
    bible_path: &Path,
    policy: &VoicePolicy,
    installed: Option<&std::collections::BTreeSet<String>>,
    save: bool,
) -> Result<Cast> {
    // --- gather speakers and voice hints -------------------------------------
    let bible = crate::digest::load_bible(bible_path);
    let mut hints: BTreeMap<String, String> = BTreeMap::new();
    let mut speakers: Vec<String> = vec!["Narrator".to_string()];
    let mut seen: HashSet<String> = speakers.iter().cloned().collect();
    let add_speaker = |speakers: &mut Vec<String>, seen: &mut HashSet<String>, name: &str| {
        let name = crate::digest::resolve_speaker(&bible, name);
        if !name.is_empty() && seen.insert(name.clone()) {
            speakers.push(name);
        }
    };

    let script = std::fs::read_to_string(script_path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok());

    if let Some(data) = &script {
        // Legacy shape (pre-bible scripts) carried characters inline.
        if let Some(chars) = data.get("characters").and_then(|c| c.as_array()) {
            for c in chars {
                let name = c.get("name").and_then(|n| n.as_str()).unwrap_or("");
                if name.is_empty() {
                    continue;
                }
                hints.insert(
                    crate::digest::resolve_speaker(&bible, name),
                    c.get("voice_hint")
                        .and_then(|h| h.as_str())
                        .unwrap_or("")
                        .to_lowercase(),
                );
                add_speaker(&mut speakers, &mut seen, name);
            }
        }
        for name in strings(data.get("roster")) {
            add_speaker(&mut speakers, &mut seen, &name);
        }
        if let Some(segments) = data.get("segments").and_then(|s| s.as_array()) {
            for s in segments {
                if let Some(sp) = s.get("speaker").and_then(|v| v.as_str()) {
                    add_speaker(&mut speakers, &mut seen, sp);
                }
            }
        }
    }

    // Voice hints live in the bible now; the script only names people.
    let mut char_tags: BTreeMap<String, Vec<String>> = BTreeMap::new();
    if let Some(chars) = bible.get("characters").and_then(|c| c.as_array()) {
        for c in chars {
            let name = c.get("name").and_then(|n| n.as_str()).unwrap_or("");
            if !name.is_empty() {
                hints.entry(name.to_string()).or_insert_with(|| {
                    c.get("voice_hint")
                        .and_then(|h| h.as_str())
                        .unwrap_or("")
                        .to_lowercase()
                });
                char_tags.insert(name.to_string(), crate::digest::tags_of(c));
            }
        }
    }
    let pool = pool_for_bible(bible_path);

    // --- merge defaults, then the on-disk cast -------------------------------
    let mut cast: Cast = policy.default_cast.iter().cloned().collect();
    // `read_cast` resolves keys *and* names, so a migrated, half-migrated or
    for (character, voice) in read_cast(&policy.engine, cast_path) {
        // A stored voice the store cannot speak is not an assignment, it is a
        if let Some(have) = installed {
            if !have.contains(&voice) {
                continue;
            }
        }
        cast.insert(character, voice);
    }

    // --- assign the gaps -----------------------------------------------------
    let mut used: Vec<String> = cast.values().cloned().collect();
    let mut chapter_voices: HashSet<String> = speakers
        .iter()
        .filter_map(|n| cast.get(n).cloned())
        .collect();

    for name in &speakers {
        if cast.contains_key(name) {
            continue;
        }
        // The crowd is a chorus, not a cast: an unnamed speaker borrows the
        if crate::digest::is_anonymous_speaker(name) {
            continue;
        }
        // The pool rolls first: compatible samples, least-used wins. A sample
        let hint = hints.get(name).cloned().unwrap_or_default();
        let mut options = char_tags
            .get(name)
            .filter(|tags| !tags.is_empty())
            .map(|tags| crate::pool::candidates(&pool, tags))
            .unwrap_or_default();
        let hint_tags = crate::pool::tags_from_hint(&hint);
        if options.is_empty() {
            options = crate::pool::candidates(&pool, &hint_tags);
        }
        if options.is_empty() && hint_tags.is_empty() {
            // Unknown gender is still allowed to use a clone when one exists;
            options = pool
                .keys()
                .filter(|name| name.as_str() != "Narrator")
                .cloned()
                .collect();
        }
        let pick = options
            .iter()
            .min_by_key(|v| {
                (
                    chapter_voices.contains(*v) as u8,
                    used.iter().filter(|u| u == v).count(),
                    options.iter().position(|p| p == *v).unwrap_or(usize::MAX),
                )
            })
            .cloned();
        if let Some(voice) = pick {
            used.push(voice.clone());
            chapter_voices.insert(voice.clone());
            cast.insert(name.clone(), voice);
            continue;
        }
        // A character with no pool left stays unassigned and fails loudly at
        let preset: Vec<String> = policy.pool_for_hint(&hint).to_vec();
        let pick = preset
            .iter()
            .min_by_key(|v| {
                (
                    // prefer a voice not already speaking in this chapter
                    chapter_voices.contains(*v) as u8,
                    // then the globally least-used one
                    used.iter().filter(|u| u == v).count(),
                    // then stable pool order
                    preset.iter().position(|p| p == *v).unwrap_or(usize::MAX),
                )
            })
            .cloned();
        if let Some(voice) = pick {
            used.push(voice.clone());
            chapter_voices.insert(voice.clone());
            cast.insert(name.clone(), voice);
        }
    }

    // Every unnamed speaker speaks in the Narrator's voice — and this runs
    if let Some(narrator) = cast.get("Narrator").cloned() {
        let anonymous: Vec<String> = speakers
            .iter()
            .filter(|name| crate::digest::is_anonymous_speaker(name))
            .cloned()
            .collect();
        for name in anonymous {
            cast.insert(name, narrator.clone());
        }
    }

    if save {
        // Persist keys, not names — a renamed voice must not orphan the cast.
        write_cast(&policy.engine, cast_path, &cast)?;
    }
    // The bible rides out with the cast: every caller that later asks "who
    // speaks this line?" is holding a script whose speaker may be a surface
    Ok(cast.with_bible(bible))
}

#[cfg(test)]
mod tests;
