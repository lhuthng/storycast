//! Voice assignment — ported from `synthesize.load_cast`.
//!
//! Reads the chapter script (roster, legacy characters, segments) plus the
//! bible, then fills in a voice for every speaker the cast file does not
//! already cover. Assignments are stable: an existing entry is never
//! overwritten, which is what makes re-renders cheap.

use crate::util::write_json;
use crate::voices::VoicePolicy;
use anyhow::Result;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

/// `character -> voice`. Ordered so the file on disk is diff-friendly.
pub type Cast = BTreeMap<String, String>;

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

/// The sample pool for this bible: `voice-pool.json` at the repo root in real
/// layouts, beside the bible itself in tests. Missing means "no pool".
fn pool_for_bible(bible_path: &Path) -> crate::pool::Pool {
    let dirs: Vec<&Path> = match bible_path.parent() {
        Some(d) => vec![d, d.parent().unwrap_or(d)],
        None => vec![],
    };
    for dir in dirs {
        let pool = crate::pool::load_pool(&dir.join("voice-pool.json"));
        if !pool.is_empty() {
            return pool;
        }
    }
    crate::pool::load_pool(Path::new("/nonexistent/voice-pool.json"))
}

/// The operator roster for these paths, real layout or test fixture: the first
/// `.bm/voices.json` found walking up from the bible, else a path that loads
/// as "no opinion". Missing means unrestricted, exactly like today.
fn roster_for_bible(bible_path: &Path) -> PathBuf {
    let mut dirs = Vec::new();
    if let Some(d) = bible_path.parent() {
        dirs.push(d.to_path_buf());
        if let Some(p) = d.parent() {
            dirs.push(p.to_path_buf());
        }
    }
    for dir in &dirs {
        let cand = dir.join(".bm/voices.json");
        if cand.is_file() {
            return cand;
        }
    }
    dirs.first()
        .cloned()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".bm/voices.json")
}

/// The assignable-voice policy for a render: the operator's own roster, never
/// the shipped catalogue alone. Assigning from the catalogue once voice-matched
/// a Northern preset the sidecar gate then refused — shelving the chapter for
/// a voice nobody was allowed to use.
pub fn policy_for_bible(engine: &str, bible_path: &Path) -> Result<VoicePolicy> {
    crate::voices::effective_policy(&roster_for_bible(bible_path), engine)
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// Resolve the full cast for a chapter, assigning any missing speaker.
///
/// A tagged character rolls from the compatible sample pool first (least-used
/// wins, so re-renders stay stable); anything untagged or unmatched falls back
/// to the preset pools exactly as before.
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
        // name persists like any clone's, so a later swap is just a swap.
        if let Some(tags) = char_tags.get(name).filter(|t| !t.is_empty()) {
            let options = crate::pool::candidates(&pool, tags);
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
        }
        let hint = hints.get(name).cloned().unwrap_or_default();
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
    Ok(cast)
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
    fn an_excluded_preset_is_never_assigned() {
        // The operator bans Northern: the assigner must not write Minh Đức
        // (or any Northern preset) into the cast for a gate to reject later —
        // that exact write shelved a real chapter three times.
        let d = tmpdir("policy-assign");
        std::fs::create_dir_all(d.join(".bm")).unwrap();
        std::fs::write(
            d.join(".bm/voices.json"),
            r#"{"version":1,"engines":{"vieneu":{"policy":{"excluded_accents":["Northern"]}}}}"#,
        )
        .unwrap();
        let script = d.join("script-01.json");
        std::fs::write(&script, r#"{"roster":["Ông Già"],"segments":[]}"#).unwrap();
        let bible = d.join("bible.json");
        std::fs::write(
            &bible,
            r#"{"characters":[{"name":"Ông Già","voice_hint":"elderly male, stern","tags":["old","male"],"proper_aliases":[]}]}"#,
        )
        .unwrap();

        let policy = policy_for_bible("vieneu", &bible).unwrap();
        assert!(!policy.allowed.is_empty(), "the exclusion must bite");
        let cast = load_cast(&script, &d.join("cast-vieneu.json"), &bible, &policy, false).unwrap();
        let got = cast.get("Ông Già").unwrap();
        assert!(
            policy.allowed.contains(got),
            "assigned {got:?} outside the policy"
        );
        assert_ne!(got, "Minh Đức");
    }

    #[test]
    fn a_malformed_roster_fails_the_render_policy_loudly() {
        let d = tmpdir("policy-broken");
        std::fs::create_dir_all(d.join(".bm")).unwrap();
        std::fs::write(d.join(".bm/voices.json"), "{ nope").unwrap();
        let err = policy_for_bible("vieneu", &d.join("bible.json")).unwrap_err();
        assert!(err.to_string().contains("parsing"), "{err}");
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
