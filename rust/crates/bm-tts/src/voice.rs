//! The preset voice roster.
//!
//! `vieneu/assets/voices_v3_turbo.json` ships 58 preset voices, and each carries
//! its **speaker embedding and reference codes precomputed** — so a preset needs
//! no enrollment at all. No fbank, no speaker encoder, no codec encode: read two
//! arrays and generate.
//!
//! Only *cloned* voices, built from a reference wav at provisioning time, need
//! the encoder path. That is a much smaller problem than it looks from the
//! reference's API, where both arrive through the same `_resolve_ref`.
//!
//! The store is the single source of truth for the roster. `list_preset_voices`
//! in the reference derives its labels from this file too, so the names here are
//! the same strings `/voices` and the inductor's cast editor already use.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct Voice {
    pub name: String,
    pub gender: String,
    pub style: String,
    pub description: String,
    pub speaker_emb: Vec<f32>,
    pub codes: Vec<Vec<i64>>,
}

#[derive(Debug, Deserialize)]
struct RawVoice {
    #[serde(default)]
    description: String,
    #[serde(default)]
    gender: String,
    #[serde(default)]
    style: String,
    speaker_emb: Vec<f32>,
    codes: Vec<Vec<i64>>,
}

#[derive(Debug, Deserialize)]
struct RawStore {
    #[serde(default)]
    default_voice: Option<String>,
    presets: BTreeMap<String, RawVoice>,
}

#[derive(Debug, Clone)]
pub struct Roster {
    pub voices: BTreeMap<String, Voice>,
    pub default_voice: Option<String>,
}

impl Roster {
    pub fn load(path: &Path) -> Result<Roster> {
        let raw: RawStore = serde_json::from_str(
            &std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?,
        )
        .with_context(|| format!("parsing {}", path.display()))?;
        if raw.presets.is_empty() {
            bail!("{} has no presets", path.display());
        }
        let mut voices = BTreeMap::new();
        for (name, v) in raw.presets {
            if v.speaker_emb.is_empty() {
                bail!("preset {name} has an empty speaker_emb");
            }
            voices.insert(
                name.clone(),
                Voice {
                    name,
                    gender: v.gender,
                    style: v.style,
                    description: v.description,
                    speaker_emb: v.speaker_emb,
                    codes: v.codes,
                },
            );
        }
        Ok(Roster {
            voices,
            default_voice: raw.default_voice,
        })
    }

    pub fn get(&self, name: &str) -> Option<&Voice> {
        self.voices.get(name)
    }

    /// Resolve a requested voice, falling back to the store's default and then
    /// to any voice at all.
    ///
    /// The fallback is deliberate and matches the reference's behaviour of
    /// answering *something*: a render that names a voice this box does not have
    /// should be audible and obviously wrong rather than silent.
    pub fn resolve(&self, name: Option<&str>) -> Result<&Voice> {
        if let Some(n) = name {
            if let Some(v) = self.voices.get(n) {
                return Ok(v);
            }
        }
        if let Some(d) = self.default_voice.as_ref().and_then(|d| self.voices.get(d)) {
            return Ok(d);
        }
        self.voices
            .values()
            .next()
            .context("the voice store is empty")
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.voices.keys().map(|s| s.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_store(dir: &Path, json: &str) -> std::path::PathBuf {
        let p = dir.join("voices_v3_turbo.json");
        std::fs::write(&p, json).unwrap();
        p
    }

    #[test]
    fn reads_a_preset_and_its_arrays() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_store(
            dir.path(),
            r#"{"default_voice":"B",
                "presets":{
                  "A":{"description":"d","gender":"Nam","style":"Kể chuyện",
                       "speaker_emb":[0.1,0.2],"codes":[[1,2],[3,4]]},
                  "B":{"speaker_emb":[0.3],"codes":[]}}}"#,
        );
        let r = Roster::load(&p).unwrap();
        assert_eq!(r.names().collect::<Vec<_>>(), vec!["A", "B"]);
        let a = r.get("A").unwrap();
        assert_eq!(a.speaker_emb, vec![0.1, 0.2]);
        assert_eq!(a.codes, vec![vec![1, 2], vec![3, 4]]);
        assert_eq!(a.gender, "Nam");
        // Optional fields default rather than failing the whole store.
        assert_eq!(r.get("B").unwrap().description, "");
    }

    #[test]
    fn resolve_falls_back_to_the_default_then_to_anything() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_store(
            dir.path(),
            r#"{"default_voice":"B","presets":{"A":{"speaker_emb":[1.0],"codes":[]},
                                              "B":{"speaker_emb":[2.0],"codes":[]}}}"#,
        );
        let r = Roster::load(&p).unwrap();
        assert_eq!(r.resolve(Some("A")).unwrap().name, "A");
        assert_eq!(r.resolve(Some("nope")).unwrap().name, "B");
        assert_eq!(r.resolve(None).unwrap().name, "B");

        // No default recorded: any voice beats silence.
        let p2 = write_store(
            dir.path(),
            r#"{"presets":{"A":{"speaker_emb":[1.0],"codes":[]}}}"#,
        );
        assert_eq!(Roster::load(&p2).unwrap().resolve(None).unwrap().name, "A");
    }

    #[test]
    fn an_empty_store_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_store(dir.path(), r#"{"presets":{}}"#);
        let err = Roster::load(&p).unwrap_err().to_string();
        assert!(err.contains("no presets"), "{err}");
    }

    /// A preset with no speaker embedding could not be conditioned on, so it is
    /// a broken store rather than a voice to skip silently.
    #[test]
    fn a_preset_with_no_anchor_is_refused_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_store(
            dir.path(),
            r#"{"presets":{"A":{"speaker_emb":[],"codes":[]}}}"#,
        );
        let err = Roster::load(&p).unwrap_err().to_string();
        assert!(err.contains("A"), "{err}");
        assert!(err.contains("speaker_emb"), "{err}");
    }
}
