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

    /// Resolve a requested voice. Exact match first, then a
    /// case/diacritic/separator-insensitive one (so a key like `minh-duc`
    /// meets its display name `Minh Đức`).
    ///
    /// A named-but-unknown voice is an error, never a fallback: answering
    /// with the default voice bakes the wrong speaker into renders and
    /// previews that sound right-length and right-quality, so nobody notices
    /// until the merge is mixed. Only "no voice asked" (`None`/empty) takes
    /// the store default.
    pub fn resolve(&self, name: Option<&str>) -> Result<&Voice> {
        let want = name.map(str::trim).filter(|n| !n.is_empty());
        let Some(n) = want else {
            return self
                .default_voice
                .as_ref()
                .and_then(|d| self.voices.get(d))
                .or_else(|| self.voices.values().next())
                .context("the voice store is empty");
        };
        if let Some(v) = self.voices.get(n) {
            return Ok(v);
        }
        let folded = norm(n);
        if let Some(v) = self
            .voices
            .iter()
            .find(|(k, _)| norm(k) == folded)
            .map(|(_, v)| v)
        {
            return Ok(v);
        }
        bail!(
            "unknown voice {n:?} on this box — :prov to push it ({} known)",
            self.voices.len()
        )
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.voices.keys().map(|s| s.as_str())
    }
}

/// Folded voice id: `bm_core::util::fold` with separators dropped, so store
/// keys, display names and request values meet whatever form each side uses.
fn norm(s: &str) -> String {
    bm_core::util::fold(s)
        .chars()
        .filter(|c| !matches!(c, '-' | '_' | ' '))
        .collect()
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
    fn resolve_refuses_unknown_but_keeps_the_default_for_none() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_store(
            dir.path(),
            r#"{"default_voice":"B","presets":{"A":{"speaker_emb":[1.0],"codes":[]},
                                              "B":{"speaker_emb":[2.0],"codes":[]}}}"#,
        );
        let r = Roster::load(&p).unwrap();
        assert_eq!(r.resolve(Some("A")).unwrap().name, "A");
        // No silent fallback: a misnamed voice fails loudly instead of
        // rendering the wrong speaker into a merge.
        let err = r.resolve(Some("nope")).unwrap_err().to_string();
        assert!(err.contains("unknown voice"), "{err}");
        assert!(err.contains(":prov"), "{err}");
        // "No voice asked" still takes the store default.
        assert_eq!(r.resolve(None).unwrap().name, "B");
        assert_eq!(r.resolve(Some("  ")).unwrap().name, "B");

        // No default recorded: any voice beats silence.
        let p2 = write_store(
            dir.path(),
            r#"{"presets":{"A":{"speaker_emb":[1.0],"codes":[]}}}"#,
        );
        assert_eq!(Roster::load(&p2).unwrap().resolve(None).unwrap().name, "A");
    }

    #[test]
    fn resolve_matches_keys_against_display_names() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_store(
            dir.path(),
            r#"{"presets":{"Minh Đức":{"speaker_emb":[1.0],"codes":[]}}}"#,
        );
        let r = Roster::load(&p).unwrap();
        assert_eq!(r.resolve(Some("minh-duc")).unwrap().name, "Minh Đức");
        assert_eq!(r.resolve(Some("MINH DUC")).unwrap().name, "Minh Đức");
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
