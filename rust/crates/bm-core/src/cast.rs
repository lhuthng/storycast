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
use std::path::Path;

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

/// Read a cast file, or an empty map when it does not exist yet.
pub fn read_cast(path: &Path) -> Cast {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Cast>(&t).ok())
        .unwrap_or_default()
}

/// Resolve the full cast for a chapter, assigning any missing speaker.
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
    let mut hints: BTreeMap<String, String> = BTreeMap::new();
    let mut speakers: Vec<String> = vec!["Narrator".to_string()];
    let mut seen: HashSet<String> = speakers.iter().cloned().collect();
    let add_speaker = |speakers: &mut Vec<String>, seen: &mut HashSet<String>, name: &str| {
        if !name.is_empty() && seen.insert(name.to_string()) {
            speakers.push(name.to_string());
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
                    name.to_string(),
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
    let bible = crate::digest::load_bible(bible_path);
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
            }
        }
    }

    // --- merge defaults, then the on-disk cast -------------------------------
    let mut cast: Cast = policy.default_cast.iter().cloned().collect();
    if let Ok(text) = std::fs::read_to_string(cast_path) {
        if let Ok(on_disk) = serde_json::from_str::<Cast>(&text) {
            for (k, v) in on_disk {
                cast.insert(k, v);
            }
        }
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
        let hint = hints.get(name).cloned().unwrap_or_default();
        let pool = policy.pool_for_hint(&hint).to_vec();
        let pick = pool
            .iter()
            .min_by_key(|v| {
                (
                    // prefer a voice not already speaking in this chapter
                    chapter_voices.contains(*v) as u8,
                    // then the globally least-used one
                    used.iter().filter(|u| u == v).count(),
                    // then stable pool order
                    pool.iter().position(|p| p == *v).unwrap_or(usize::MAX),
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
        write_json(cast_path, &cast)?;
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
        let p = vieneu_policy();
        for v in cast.values() {
            assert!(p.allowed.contains(v), "voice {v} violates the accent policy");
        }
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
        assert!(!cast_path.exists(), "read-only mode must not create the file");
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
}
