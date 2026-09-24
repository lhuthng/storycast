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

/// The sample pool for this bible: the first non-empty `voice-pool.json`
/// walking up from the bible — beside it in tests, at the repo root in real
/// layouts (the bible lives two levels down, under `<workspace>/data/`).
/// Missing means "no pool".
fn pool_for_bible(bible_path: &Path) -> crate::pool::Pool {
    let mut dir = bible_path.parent();
    while let Some(d) = dir {
        let pool = crate::pool::load_pool(&d.join("voice-pool.json"));
        if !pool.is_empty() {
            return pool;
        }
        dir = d.parent();
    }
    crate::pool::load_pool(Path::new("/nonexistent/voice-pool.json"))
}

/// The assignable-voice policy for a render: the shipped catalogue. There is
/// no machine-local overlay.
pub fn policy_for_bible(engine: &str) -> VoicePolicy {
    crate::voices::effective_policy(engine)
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
pub fn load_cast(
    script_path: &Path,
    cast_path: &Path,
    bible_path: &Path,
    policy: &VoicePolicy,
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
        let preset = policy.pool_for_hint(&hint).to_vec();
        // The accent policy binds the assigner too, not just the gates: with
        // an exclusion in force, an excluded preset must never be written into
        // the cast for a gate to reject later. Empty `allowed` is "no
        // restriction", so the shipped catalogue behaves exactly as before.
        // A character with nothing admissible stays unassigned and fails
        // loudly at planning, naming them — instead of shelving three renders
        // against a voice nobody may use.
        let preset: Vec<String> = if policy.allowed.is_empty() {
            preset
        } else {
            preset
                .into_iter()
                .filter(|v| policy.allowed.iter().any(|a| a == v))
                .collect()
        };
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
mod tests {
    use super::*;
    use crate::voices::vieneu_policy;
    use serde_json::json;

    fn tmpdir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("bm-cast-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn assigns_a_voice_for_every_speaker() {
        let d = tmpdir("assign");
        let script = d.join("script-01.json");
        std::fs::write(
            &script,
            serde_json::to_string(&json!({
                "roster": ["Narrator", "Dịch Phong", "New Guy"],
                "segments": [{"speaker": "Narrator", "text": "x"}]
            }))
            .unwrap(),
        )
        .unwrap();
        let cast_path = d.join("cast-vieneu.json");
        let cast = load_cast(
            &script,
            &cast_path,
            &d.join("bible.json"),
            &vieneu_policy(),
            true,
        )
        .unwrap();
        assert!(cast.contains_key("Narrator"));
        assert!(cast.contains_key("New Guy"));
        assert!(cast_path.exists(), "save=true must persist");
        // The shipped policy restricts nothing, so the invariant is "no
        // violations" rather than "the name appears in an allow-list".
        let p = vieneu_policy();
        let pairs: Vec<(String, String)> =
            cast.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        assert!(p.violations(&pairs, &[]).is_empty());
    }

    #[test]
    fn anonymous_slots_receive_stable_voices_without_bible_entries() {
        let d = tmpdir("anonymous-slot");
        let bible = d.join("bible.json");
        std::fs::write(&bible, r#"{"characters":[]}"#).unwrap();
        let cast_path = d.join("cast-vieneu.json");
        let first = d.join("script-01.json");
        std::fs::write(
            &first,
            r#"{"roster":["Narrator","anonymous:anon-1"],
                "segments":[{"speaker":"anonymous:anon-1","text":"Mở cửa!"}]}"#,
        )
        .unwrap();

        let cast = load_cast(&first, &cast_path, &bible, &vieneu_policy(), true).unwrap();
        let voice = cast
            .get("anonymous:anon-1")
            .expect("anonymous slot assigned");
        let stored: Value = crate::read_json(&bible).unwrap();
        assert_eq!(stored, json!({"characters": []}), "not a Bible character");

        let second = d.join("script-02.json");
        std::fs::write(&second, r#"{"roster":["anonymous:anon-1"],"segments":[]}"#).unwrap();
        let again = load_cast(&second, &cast_path, &bible, &vieneu_policy(), true).unwrap();
        assert_eq!(again.get("anonymous:anon-1"), Some(voice));
    }

    #[test]
    fn never_overwrites_an_existing_assignment() {
        let d = tmpdir("stable");
        let script = d.join("script-01.json");
        std::fs::write(&script, r#"{"roster":["Narrator"],"segments":[]}"#).unwrap();
        let cast_path = d.join("cast-vieneu.json");
        std::fs::write(&cast_path, r#"{"Narrator":"Adam"}"#).unwrap();
        let cast = load_cast(
            &script,
            &cast_path,
            &d.join("bible.json"),
            &vieneu_policy(),
            true,
        )
        .unwrap();
        assert_eq!(cast.get("Narrator").unwrap(), "Adam");
    }

    #[test]
    fn save_false_leaves_the_file_untouched() {
        let d = tmpdir("readonly");
        let script = d.join("script-01.json");
        std::fs::write(&script, r#"{"roster":["Brand New"],"segments":[]}"#).unwrap();
        let cast_path = d.join("cast-vieneu.json");
        let cast = load_cast(
            &script,
            &cast_path,
            &d.join("bible.json"),
            &vieneu_policy(),
            false,
        )
        .unwrap();
        assert!(cast.contains_key("Brand New"));
        assert!(
            !cast_path.exists(),
            "read-only mode must not create the file"
        );
    }

    #[test]
    fn a_broad_character_tag_does_not_force_a_default_preset() {
        let d = tmpdir("clone-before-preset");
        std::fs::write(
            d.join("voice-pool.json"),
            r#"{"young-female-1":{"file":"refs/young-female-1.mp3","tags":["young","female"]},
                "young-male-1":{"file":"refs/young-male-1.mp3","tags":["young","male"]}}"#,
        )
        .unwrap();
        let script = d.join("script-01.json");
        std::fs::write(&script, r#"{"roster":["Hệ thống"],"segments":[]}"#).unwrap();
        let bible = d.join("bible.json");
        std::fs::write(
            &bible,
            r#"{"characters":[{"name":"Hệ thống","voice_hint":"adult male",
                "tags":["system"],"proper_aliases":[]}]}"#,
        )
        .unwrap();

        let cast = load_cast(
            &script,
            &d.join("cast-vieneu.json"),
            &bible,
            &vieneu_policy(),
            false,
        )
        .unwrap();
        assert_eq!(cast.get("Hệ thống").unwrap(), "young-male-1");
    }

    #[test]
    fn bible_hints_drive_gender_pool() {
        let d = tmpdir("bible");
        let script = d.join("script-01.json");
        std::fs::write(&script, r#"{"roster":["Cô Bé"],"segments":[]}"#).unwrap();
        let bible = d.join("bible.json");
        std::fs::write(
            &bible,
            r#"{"characters":[{"name":"Cô Bé","voice_hint":"girl","proper_aliases":[]}]}"#,
        )
        .unwrap();
        let cast = load_cast(
            &script,
            &d.join("cast-vieneu.json"),
            &bible,
            &vieneu_policy(),
            false,
        )
        .unwrap();
        let p = vieneu_policy();
        assert!(
            p.female.contains(cast.get("Cô Bé").unwrap()),
            "expected a female preset, got {:?}",
            cast.get("Cô Bé")
        );
    }

    // --- stage 2: the cast file stores keys, the reader accepts both --------

    fn on_disk(path: &Path) -> BTreeMap<String, String> {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn a_name_based_cast_reads_and_is_rewritten_as_keys() {
        // The un-migrated form. Reading must resolve names, and the next write
        // must key the file — otherwise the rename-fragility never goes away and
        // the migration is something an operator has to remember forever.
        let d = tmpdir("migrate-on-write");
        let script = d.join("script-01.json");
        std::fs::write(&script, r#"{"roster":["Narrator"],"segments":[]}"#).unwrap();
        let cast_path = d.join("cast-vieneu.json");
        std::fs::write(&cast_path, r#"{"Narrator":"Đức Trí"}"#).unwrap();

        let cast = load_cast(
            &script,
            &cast_path,
            &d.join("bible.json"),
            &vieneu_policy(),
            true,
        )
        .unwrap();
        assert_eq!(
            cast.get("Narrator").unwrap(),
            "Đức Trí",
            "in memory the pipeline still speaks names"
        );
        assert_eq!(
            on_disk(&cast_path).get("Narrator").unwrap(),
            "duc-tri",
            "on disk it is a key, so a rename cannot orphan the assignment"
        );
    }

    #[test]
    fn a_key_based_cast_reads_back_as_names_and_does_not_churn() {
        let d = tmpdir("from-keys");
        let script = d.join("script-01.json");
        std::fs::write(
            &script,
            r#"{"roster":["Narrator","Dịch Phong"],"segments":[]}"#,
        )
        .unwrap();
        let cast_path = d.join("cast-vieneu.json");
        std::fs::write(
            &cast_path,
            r#"{"Narrator":"duc-tri","Dịch Phong":"thai-son"}"#,
        )
        .unwrap();

        let cast = load_cast(
            &script,
            &cast_path,
            &d.join("bible.json"),
            &vieneu_policy(),
            true,
        )
        .unwrap();
        assert_eq!(cast.get("Narrator").unwrap(), "Đức Trí");
        assert_eq!(cast.get("Dịch Phong").unwrap(), "Thái Sơn");
        // Keys in, keys out: re-writing a migrated file is a no-op in shape.
        let disk = on_disk(&cast_path);
        assert_eq!(disk.get("Narrator").unwrap(), "duc-tri");
        assert_eq!(disk.get("Dịch Phong").unwrap(), "thai-son");
    }

    #[test]
    fn a_clone_without_a_key_keeps_its_name_on_disk() {
        // Clones have no catalogue key until stage 3, so the assignment is
        // written as a name — and must still resolve on the way back in.
        let d = tmpdir("clone-name");
        let script = d.join("script-01.json");
        std::fs::write(&script, r#"{"roster":["Suneo"],"segments":[]}"#).unwrap();
        let cast_path = d.join("cast-vieneu.json");
        std::fs::write(&cast_path, r#"{"Suneo":"Suneo"}"#).unwrap();

        let cast = load_cast(
            &script,
            &cast_path,
            &d.join("bible.json"),
            &vieneu_policy(),
            true,
        )
        .unwrap();
        assert_eq!(cast.get("Suneo").unwrap(), "Suneo");
        assert_eq!(on_disk(&cast_path).get("Suneo").unwrap(), "Suneo");
    }

    #[test]
    fn a_half_migrated_cast_works() {
        // The property the whole design rests on: a file with one keyed entry and
        // one named entry renders, so the migration can be interrupted.
        let d = tmpdir("half-migrated");
        let script = d.join("script-01.json");
        std::fs::write(
            &script,
            r#"{"roster":["Narrator","Dịch Phong"],"segments":[]}"#,
        )
        .unwrap();
        let cast_path = d.join("cast-vieneu.json");
        std::fs::write(
            &cast_path,
            r#"{"Narrator":"duc-tri","Dịch Phong":"Thái Sơn"}"#,
        )
        .unwrap();

        let cast = load_cast(
            &script,
            &cast_path,
            &d.join("bible.json"),
            &vieneu_policy(),
            false,
        )
        .unwrap();
        assert_eq!(cast.get("Narrator").unwrap(), "Đức Trí");
        assert_eq!(cast.get("Dịch Phong").unwrap(), "Thái Sơn");
    }

    #[test]
    fn without_an_overlay_there_is_nothing_to_exclude() {
        // No machine-local roster: the catalogue is unrestricted, so an old
        // man with no pool rolls the first male preset.
        let d = tmpdir("policy-assign");
        let script = d.join("script-01.json");
        std::fs::write(&script, r#"{"roster":["Ông Già"],"segments":[]}"#).unwrap();
        let bible = d.join("bible.json");
        std::fs::write(
            &bible,
            r#"{"characters":[{"name":"Ông Già","voice_hint":"elderly male, stern","tags":["old","male"],"proper_aliases":[]}]}"#,
        )
        .unwrap();

        let policy = policy_for_bible("vieneu");
        assert!(policy.allowed.is_empty(), "unrestricted");
        let cast = load_cast(&script, &d.join("cast-vieneu.json"), &bible, &policy, false).unwrap();
        let got = cast.get("Ông Già").unwrap();
        assert!(
            policy.male.contains(got),
            "an old man rolls a male preset: {got:?}"
        );
    }

    #[test]
    fn the_policy_is_the_catalogue_with_no_overlay() {
        // No machine-local roster exists any more: even a stray .bm/voices.json
        // is ignored, and the policy is the shipped catalogue (unrestricted).
        let d = tmpdir("policy-catalogue");
        std::fs::create_dir_all(d.join(".bm")).unwrap();
        std::fs::write(d.join(".bm/voices.json"), "{ nope").unwrap();
        let policy = policy_for_bible("vieneu");
        assert!(policy.allowed.is_empty(), "unrestricted");
    }

    #[test]
    fn an_unknown_voice_is_preserved_rather_than_silently_reassigned() {
        // The cast overview has to be able to flag this; substituting a valid
        // voice would hide a real problem behind a plausible render.
        let d = tmpdir("unknown");
        let cast_path = d.join("cast-vieneu.json");
        std::fs::write(&cast_path, r#"{"Narrator":"Đã Biến Mất"}"#).unwrap();
        let cast = read_cast("vieneu", &cast_path);
        assert_eq!(cast.get("Narrator").unwrap(), "Đã Biến Mất");
    }

    // --- the sample pool rolls first -----------------------------------------

    #[test]
    fn a_variant_speaker_name_resolves_to_the_assigned_voice() {
        // The map is keyed canonically — `load_cast` folds before assigning —
        // but the planner is handed the raw script string. A speaker written
        // as an alias, a case variant or a title-suffixed form must therefore
        // still find its voice, or the chapter is assigned a voice under one
        // name and looked up under another.
        let d = tmpdir("variant-lookup");
        let bible = d.join("bible.json");
        std::fs::write(
            &bible,
            r#"{"characters":[{"name":"Quản Vân Bằng","voice_hint":"old male",
                "proper_aliases":["Quản Vân Bằng","nam tử bị thương"]}]}"#,
        )
        .unwrap();
        let script = d.join("script-01.json");
        std::fs::write(
            &script,
            r#"{"roster":["Narrator","Nam tử bị thương"],
                "segments":[{"speaker":"Nam tử bị thương","text":"Cứu ta."}]}"#,
        )
        .unwrap();

        let cast = load_cast(
            &script,
            &d.join("cast-vieneu.json"),
            &bible,
            &vieneu_policy(),
            false,
        )
        .unwrap();
        let voice = cast.get("Quản Vân Bằng").expect("assigned under the name");
        assert_eq!(
            cast.get("Nam tử bị thương"),
            Some(voice),
            "the script's own spelling must resolve to the same voice"
        );
        // And the planner — which only ever sees that spelling — plans.
        let segs = vec![json!({"speaker": "Nam tử bị thương", "text": "Cứu ta."})];
        let units = crate::assemble::plan_render(
            &crate::assemble::Planned::plan(&segs),
            &cast,
            Path::new("segs"),
            true,
            None,
        )
        .expect("a variant speaker must plan");
        assert_eq!(units.len(), 1);
        assert_eq!(&units[0].voice, voice);
    }

    #[test]
    fn a_cast_read_off_disk_looks_up_exactly() {
        // The picker and the migration read the file directly and expect a
        // plain map: no bible, no folding, no surprise substitution.
        let d = tmpdir("plain-map");
        let path = d.join("cast-vieneu.json");
        std::fs::write(&path, r#"{"A":"Đức Trí"}"#).unwrap();
        let cast = read_cast("vieneu", &path);
        assert_eq!(cast.get("A").unwrap(), "Đức Trí");
        assert!(cast.get("a").is_none(), "no folding without a bible");
        assert_eq!(cast.into_map().len(), 1);
    }

    fn pool_fixture(d: &Path) {
        std::fs::write(
            d.join("voice-pool.json"),
            r#"{"young-female-1": {"file": "refs/young-female-1.mp3", "tags": ["young", "female"]},
                "old-male-1": {"file": "refs/old-male-1.mp3", "tags": ["old", "male"]}}"#,
        )
        .unwrap();
    }

    #[test]
    fn a_tagged_newcomer_rolls_from_the_pool() {
        let d = tmpdir("pool-roll");
        pool_fixture(&d);
        let script = d.join("script-01.json");
        std::fs::write(&script, r#"{"roster":["Cô Bé"],"segments":[]}"#).unwrap();
        let bible = d.join("bible.json");
        std::fs::write(
            &bible,
            r#"{"characters":[{"name":"Cô Bé","voice_hint":"girl, bright","tags":["young","female"],"proper_aliases":[]}]}"#,
        )
        .unwrap();
        let cast = load_cast(
            &script,
            &d.join("cast-vieneu.json"),
            &bible,
            &vieneu_policy(),
            false,
        )
        .unwrap();
        assert_eq!(cast.get("Cô Bé").unwrap(), "young-female-1");
    }

    #[test]
    fn a_clashing_character_falls_back_to_presets() {
        // young+male shares `young` with the pool's only sample, but `male`
        // clashes with its `female` — so no pool voice may speak him.
        let d = tmpdir("pool-clash");
        std::fs::write(
            d.join("voice-pool.json"),
            r#"{"young-female-1": {"file": "refs/young-female-1.mp3", "tags": ["young", "female"]}}"#,
        )
        .unwrap();
        let script = d.join("script-01.json");
        std::fs::write(&script, r#"{"roster":["Cậu Bé"],"segments":[]}"#).unwrap();
        let bible = d.join("bible.json");
        std::fs::write(
            &bible,
            r#"{"characters":[{"name":"Cậu Bé","voice_hint":"boy, polite","tags":["young","male"],"proper_aliases":[]}]}"#,
        )
        .unwrap();
        let cast = load_cast(
            &script,
            &d.join("cast-vieneu.json"),
            &bible,
            &vieneu_policy(),
            false,
        )
        .unwrap();
        let got = cast.get("Cậu Bé").unwrap();
        assert_ne!(
            got, "young-female-1",
            "a clashing sample must never voice him"
        );
        assert!(
            vieneu_policy().male.contains(got),
            "falls back to the male presets: {got:?}"
        );
    }

    #[test]
    fn the_pool_is_found_two_levels_up_like_the_real_layout() {
        // Real layout: the pool at the repo root, the bible two levels down
        // at `<workspace>/data/bible.json`. The old lookup only climbed one
        // level, loaded nothing, and every newcomer fell through to presets.
        let d = tmpdir("pool-walkup");
        pool_fixture(&d);
        let data = d.join("workspaces").join("book").join("data");
        std::fs::create_dir_all(&data).unwrap();
        let script = data.join("script-01.json");
        std::fs::write(&script, r#"{"roster":["Cô Bé"],"segments":[]}"#).unwrap();
        let bible = data.join("bible.json");
        std::fs::write(
            &bible,
            r#"{"characters":[{"name":"Cô Bé","voice_hint":"girl, bright","tags":["young","female"],"proper_aliases":[]}]}"#,
        )
        .unwrap();
        let cast = load_cast(
            &script,
            &data.join("cast-vieneu.json"),
            &bible,
            &vieneu_policy(),
            false,
        )
        .unwrap();
        assert_eq!(cast.get("Cô Bé").unwrap(), "young-female-1");
    }

    #[test]
    fn hint_tags_backfill_a_bible_that_predates_tags() {
        // No `tags` key at all: the voice_hint still routes to the pool.
        let d = tmpdir("pool-hint");
        pool_fixture(&d);
        let script = d.join("script-01.json");
        std::fs::write(&script, r#"{"roster":["Lão Ông"],"segments":[]}"#).unwrap();
        let bible = d.join("bible.json");
        std::fs::write(
            &bible,
            r#"{"characters":[{"name":"Lão Ông","voice_hint":"elderly male, stern","proper_aliases":[]}]}"#,
        )
        .unwrap();
        let cast = load_cast(
            &script,
            &d.join("cast-vieneu.json"),
            &bible,
            &vieneu_policy(),
            false,
        )
        .unwrap();
        assert_eq!(cast.get("Lão Ông").unwrap(), "old-male-1");
    }
}
