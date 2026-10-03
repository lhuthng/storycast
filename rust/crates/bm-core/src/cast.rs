//! Voice assignment — ported from `synthesize.load_cast`.
//!
//! Reads the chapter script (roster, legacy characters, segments) plus the
//! bible, then fills in a voice for every speaker the cast file does not
//! already cover. Assignments are stable: an existing entry is never
//! overwritten, which is what makes re-renders cheap.

use crate::util::write_json;
use crate::voices::VoicePolicy;
use anyhow::Result;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::ops::{Deref, DerefMut};
use std::path::Path;

/// `character -> voice`, plus the bible those character names were resolved
/// against. Ordered so the file on disk is diff-friendly.
///
/// The bible rides along because a script may name a speaker by *any* surface
/// form the bible knows — an alias, a case variant, a title-suffixed form —
/// while the map's keys are the canonical names `load_cast` assigned under.
/// [`Cast::get`] folds the form it is handed through the same resolver, so the
/// writer and every reader ask the same question.
///
/// Without it a script holding a variant form is assigned a voice under one
/// name and then looked up under another: `render ch180 failed: cast has no
/// voice for "Vân bá"`, on a chapter whose voice was assigned milliseconds
/// earlier — because the cast held the entry under `Lão giả`.
#[derive(Debug, Clone, Default)]
pub struct Cast {
    voices: BTreeMap<String, String>,
    /// `Null` for a cast read straight off disk: exact-key lookup only, which
    /// is what the picker and the migration want.
    bible: Value,
}

impl Cast {
    pub fn new() -> Self {
        Self::default()
    }

    /// The voice for `speaker`, whatever surface form it arrives in.
    ///
    /// Exact key first — the common case, and the only one a bible-less cast
    /// can answer — then the canonical name the bible folds it to. Falls
    /// through to the exact key again so a cast whose bible has no opinion
    /// behaves exactly as a plain map.
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
    /// picker's wire type) rather than a name lookup.
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
/// to stay a cast file, and the bible is the caller's to load.
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
///
/// The file may hold catalogue **keys** or display **names**: keys are what the
/// writer emits, and the form that survives a voice being renamed; names are
/// what an un-migrated file holds. Resolving both here is what lets the rest of
/// the pipeline keep speaking names while the persisted form stays stable — and
/// it makes the migration optional rather than a gate that must fire before
/// anything else may run.
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
/// name otherwise.
///
/// Writing keys is what makes the file survive a rename. A voice the catalogue
/// does not declare — an enrolled clone, until stage 3 gives it a key — is
/// written as its name, which still resolves on read, so an assignment is never
/// lost to the migration.
fn cast_for_disk(engine: &str, cast: &Cast) -> Cast {
    cast.iter()
        .map(|(character, voice)| {
            let value = crate::voices::key_for_name(engine, voice).unwrap_or_else(|| voice.clone());
            (character.clone(), value)
        })
        .collect()
}

/// Write a cast in its on-disk form: catalogue keys where a voice has one.
///
/// The only sanctioned way to persist a cast. A caller that serialises the map
/// itself writes display names, which silently reverts the file to the fragile
/// form — so writes go through here, and `cast_for_disk` stays private.
pub fn write_cast(engine: &str, path: &Path, cast: &Cast) -> Result<()> {
    write_json(path, &cast_for_disk(engine, cast))
}

/// The on-disk form of a cast without writing it.
///
/// Exposed so tooling can show what a write *would* produce — `roster
/// migrate-cast --dry-run` needs exactly this, and computing it a second way
/// would be a second chance to disagree with the writer.
pub fn cast_on_disk(engine: &str, cast: &Cast) -> Cast {
    cast_for_disk(engine, cast)
}

/// The sample pool for this bible: the **workspace's own** `voice-pool.json`.
///
/// Beside the bible in tests (both in one temp dir), or at the workspace root
/// in a real layout, where the bible lives under `<workspace>/data/`. **It does
/// not climb past the workspace**: the checkout's pool is beyond-myriads', and
/// a second book casting from it is exactly how `the-apothecary-diaries` got a
/// roster that was never its own. Missing means "no pool".
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
/// to the voices this engine's own store holds.
///
/// There is no machine-local overlay, but there **is** a per-engine store, and
/// it is the authority on what can speak: the catalogue lists every preset an
/// engine could voice, which for pocket is twenty-five names against a tree
/// that ships nine. A pool that crosses that gap produces a cast the sidecar
/// rejects by name. When the store cannot be read the catalogue stands, which
/// is the old behaviour rather than a cast with nothing in it.
pub fn policy_for_bible(engine: &str, layout: &crate::Layout) -> VoicePolicy {
    let policy = crate::voices::effective_policy(engine);
    match crate::pool::installed_voices(layout) {
        Some(installed) => policy.restricted_to(&installed),
        None => policy,
    }
}

/// Resolve the full cast for a chapter, assigning any missing speaker.
///
/// A tagged character rolls from the compatible sample pool first (least-used
/// wins, so re-renders stay stable). If the character's descriptive tags do not
/// match a sample, a voice-hint fallback (gender/age) is tried before presets;
/// only a genuinely missing or incompatible pool falls back to a catalogue
/// voice. This is the "discourage defaults" rule: ordinary new characters use
/// enrolled clones whenever the pool has a usable voice.
///
/// `save = false` is the read-only mode used by the merge stage and by the
/// completeness check: it must never mutate the cast just because it looked.
///
/// `installed` is what this engine's store holds (`pool::installed_voices`).
/// An assignment already on disk that the store cannot speak is **dropped and
/// re-rolled** rather than kept: it was either drawn from a catalogue the tree
/// does not satisfy, or written before the tree changed, and either way it is a
/// render that fails three times and shelves. `None` skips that check, which is
/// the behaviour for an engine with no per-engine store.
pub fn load_cast(
    script_path: &Path,
    cast_path: &Path,
    bible_path: &Path,
    policy: &VoicePolicy,
    installed: Option<&std::collections::BTreeSet<String>>,
    save: bool,
) -> Result<Cast> {
    // --- gather speakers and voice hints -------------------------------------
    // Voice hints live in the bible now; the script only names people. The
    // bible loads first because every gathered speaker is canonicalized
    // against it: a variant form ("Sở Cuồng sư") joins under its canonical
    // name ("Sở Cuồng Sư") instead of rolling a second voice.
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
    // (Speakers above were already canonicalized, so these keys line up.)
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
    // untouched file all arrive here as display names.
    for (character, voice) in read_cast(&policy.engine, cast_path) {
        // A stored voice the store cannot speak is not an assignment, it is a
        // promise the sidecar will refuse. Drop it here so the roll below gives
        // the character a voice that exists.
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
        // Narrator's voice below rather than rolling for one. Leaving it out of
        // the roll also keeps a near-duplicate clone from being spent — and
        // then held as `used` — on a one-off street greeting.
        if crate::digest::is_anonymous_speaker(name) {
            continue;
        }
        // The pool rolls first: compatible samples, least-used wins. A sample
        // name persists like any clone's, so a later swap is just a swap. If
        // descriptive tags do not match, try the character's voice hint before
        // considering presets; this is what prevents a new character from
        // silently becoming a catalogue voice just because its tags are broad
        // (`system`, `merchant`, `disciple`, ...).
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
            // the old neutral-to-male preset fallback is now the last resort,
            // not the first choice. A known but incompatible gender must not
            // borrow a clone from the wrong side of the pool.
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
        // planning, naming them — instead of shelving three renders against a
        // voice nobody may use.
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
    // after the roll, so a cast file that still holds a numbered slot's own
    // clone (or a hand-picked anonymous voice) is corrected, not honoured, the
    // next time the cast is written.
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
    // form, and the answer must be the key assigned above.
    Ok(cast.with_bible(bible))
}

#[cfg(test)]
mod tests;
