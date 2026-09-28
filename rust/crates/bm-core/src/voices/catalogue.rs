use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use bm_proto::VoiceInfo;

use super::consts::{VoicePolicy, CONTENT_LANGUAGE};

// --- the committed catalogue ------------------------------------------------
//
// `voices.default.json` is the shipped roster: both engines, their metadata
// and their default cast, in one place.
//
// Stage 1 of `.docs/VOICE_CONFIG_PROPOSAL.md` adds it *alongside* the `const`
// tables above. Nothing in the render path reads it yet — the tests at the
// bottom of this file assert the two agree exactly. Stage 5 deletes the consts
// and makes this file load-bearing, at which point the duplication is gone;
// until then it is deliberate and checked rather than accidental and drifting.
// The catalogue itself is committed because the offline guarantee above is
// load-bearing: a fresh clone with no local config must still render.

/// The committed catalogue, embedded at compile time.
///
/// Embedded rather than read at runtime on purpose — the file cannot go
/// missing, move, or be half-edited at the moment a render needs it. Three hops
/// up from the manifest directory: `bm-core` -> `crates` -> `rust` -> root.
pub const CATALOGUE_JSON: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../voices.default.json"
));

/// `voices.default.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RosterFile {
    /// Schema version. Bumped when a field's *meaning* changes, not when a
    /// voice is added.
    pub version: u32,
    pub engines: BTreeMap<String, EngineRoster>,
}

/// One engine's slice of the catalogue.
///
/// `Default` is the "engine not declared" answer: no pools, no cast.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EngineRoster {
    /// Human-readable description, for the picker header.
    #[serde(default)]
    pub label: String,
    /// Declaration order is meaningful: the offline roster groups by gender in
    /// this order, so male voices come first, then female, then neutral.
    #[serde(default)]
    pub voices: Vec<RosterVoice>,
    /// character -> voice `key`. Keys, not display names, so renaming a voice
    /// is a presentation change that touches nothing else.
    #[serde(default)]
    pub default_cast: BTreeMap<String, String>,
}

/// One voice in the catalogue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RosterVoice {
    /// ASCII slug, `^[a-z0-9][a-z0-9-]*$`. This is the identity; `name` is
    /// display only. A slug also cannot contain `/` or `..`, which is what lets
    /// the audition cache be a path derived from it.
    pub key: String,
    pub name: String,
    /// `male` | `female` | `neutral`.
    #[serde(default)]
    pub gender: String,
    /// `Northern` | `Central` | `South` | `Central/South` | `unknown`.
    #[serde(default)]
    pub accent: String,
    /// Free text from the SDK label, e.g. `kể chuyện`.
    #[serde(default)]
    pub style: String,
}

impl RosterFile {
    /// Parse a catalogue from JSON. The primitive the embedded copy uses, and
    /// what tooling calls on the file at `Layout::roster_default()`.
    pub fn parse(json: &str) -> serde_json::Result<Self> {
        serde_json::from_str(json)
    }

    pub fn engine(&self, engine: &str) -> Option<&EngineRoster> {
        self.engines.get(engine)
    }

    /// The embedded catalogue.
    ///
    /// Panics only if the committed file is malformed, which the catalogue
    /// tests in this module make impossible to merge.
    pub fn catalogue() -> &'static RosterFile {
        static CATALOGUE: OnceLock<RosterFile> = OnceLock::new();
        CATALOGUE.get_or_init(|| {
            RosterFile::parse(CATALOGUE_JSON)
                .expect("voices.default.json is embedded and must parse")
        })
    }
}

impl EngineRoster {
    /// Voices of one gender, in catalogue order.
    fn pool(&self, gender: &str) -> Vec<String> {
        self.voices
            .iter()
            .filter(|v| v.gender == gender)
            .map(|v| v.name.clone())
            .collect()
    }

    /// The runtime policy, in the shape the rest of the crate already speaks.
    pub fn to_policy(&self, engine: &str) -> VoicePolicy {
        let male = self.pool("male");
        let female = self.pool("female");
        let mut neutral = self.pool("neutral");
        if neutral.is_empty() {
            // Kept deliberately: VieNeu declares no neutral preset, and an
            // unknown-gender character has always fallen back to the male pool.
            neutral = male.clone();
        }
        VoicePolicy {
            engine: engine.to_string(),
            male,
            female,
            neutral,
            default_cast: self.resolve_cast(),
        }
    }

    /// `character -> name`, resolving each cast key through the voice list.
    ///
    /// The catalogue stores keys; `VoicePolicy` still speaks display names until
    /// stage 2 lands. A key that resolves to nothing is dropped here, and the
    /// catalogue tests fail on it rather than letting a character reach a render
    /// with no voice.
    fn resolve_cast(&self) -> Vec<(String, String)> {
        self.default_cast
            .iter()
            .filter_map(|(character, key)| {
                self.voices
                    .iter()
                    .find(|v| &v.key == key)
                    .map(|v| (character.clone(), v.name.clone()))
            })
            .collect()
    }

    /// The offline roster, built from the catalogue instead of the consts.
    ///
    /// Same shape and same ordering as `offline_voices()`: male pool, female
    /// pool, then neutral, deduplicated — because the neutral pool aliases the
    /// male pool for VieNeu.
    pub fn to_offline_voices(&self, engine: &str) -> Vec<VoiceInfo> {
        let policy = self.to_policy(engine);
        let mut out: Vec<VoiceInfo> = Vec::new();
        for (pool, gender) in [
            (&policy.male, "male"),
            (&policy.female, "female"),
            (&policy.neutral, "neutral"),
        ] {
            for name in pool {
                if out.iter().any(|v| v.name == *name) {
                    continue; // the neutral pool aliases the male pool for VieNeu
                }
                let declared = self.voices.iter().find(|v| &v.name == name);
                let (accent, style) = match declared {
                    Some(v) => (v.accent.clone(), v.style.clone()),
                    // Undeclared: "unknown", not a policy guarantee — there is
                    // no longer a policy to guarantee anything.
                    None => ("unknown".to_string(), String::new()),
                };
                out.push(VoiceInfo {
                    key: declared.map(|v| v.key.clone()).unwrap_or_default(),
                    name: name.clone(),
                    gender: gender.to_string(),
                    accent,
                    language: CONTENT_LANGUAGE.to_string(),
                    style,
                    // A catalogue preset is never auto-assigned: the pool is.
                    pool_tags: Vec::new(),
                    enrolled: false,
                });
            }
        }
        out
    }
}

/// One engine of the catalogue: the single entry point for its declared
/// voices, with no machine-local overlay. The operator roster
/// (`.bm/voices.json`) is gone — nothing created it and every path using it is
/// removed — so the shipped catalogue is the whole roster.
pub fn effective_engine(engine: &str) -> EngineRoster {
    RosterFile::catalogue()
        .engine(engine)
        .cloned()
        .unwrap_or_default()
}

/// The effective `VoicePolicy` for `engine`.
pub fn effective_policy(engine: &str) -> VoicePolicy {
    effective_engine(engine).to_policy(engine)
}

/// The effective offline roster for `engine`.
pub fn effective_offline_voices(engine: &str) -> Vec<VoiceInfo> {
    effective_engine(engine).to_offline_voices(engine)
}

/// [`effective_engine`], kept for call-site compatibility: catalogue reads do
/// not fail, so the error half is always `None`.
pub fn effective_engine_lenient(engine: &str) -> (EngineRoster, Option<String>) {
    (effective_engine(engine), None)
}

// --- key <-> name resolution ------------------------------------------------
//
// `key` is identity and `name` is presentation, so anything *persisted* — the
// cast file above all — stores keys and resolves them back to names at the
// boundary. These three functions are that boundary.

/// The catalogue key for a voice's display name.
pub fn key_for_name(engine: &str, name: &str) -> Option<String> {
    RosterFile::catalogue()
        .engine(engine)?
        .voices
        .iter()
        .find(|v| v.name == name)
        .map(|v| v.key.clone())
}

/// The display name a catalogue key refers to.
pub fn name_for_key(engine: &str, key: &str) -> Option<String> {
    RosterFile::catalogue()
        .engine(engine)?
        .voices
        .iter()
        .find(|v| v.key == key)
        .map(|v| v.name.clone())
}

/// The display name for a cast value that may be a **key** or a **name**.
///
/// Keys are tried first, because that is what the cast file migrates to and the
/// form that survives a voice being renamed. A name still resolves, and that is
/// what makes the migration optional rather than a gate: a fully migrated cast
/// file, a half-migrated one and an untouched one all render.
///
/// A value matching neither comes back unchanged, deliberately. An unknown voice
/// has to stay visible so the cast overview can flag it (`unknown voice — stale
/// cast?`); quietly substituting a valid voice would hide a real problem behind
/// a plausible-sounding render.
pub fn resolve_voice_name(engine: &str, value: &str) -> String {
    match name_for_key(engine, value) {
        Some(name) => name,
        None => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voices::consts::{gemini_policy, vieneu_policy};
    use crate::voices::label::offline_voices;

    #[test]
    fn the_gemini_roster_claims_no_accent_of_its_own() {
        // Google's labels do not encode a region, so every Gemini voice reads
        // `unknown` — a fact about the engine, not a policy statement.
        let v = offline_voices("gemini");
        assert!(v.iter().all(|x| x.accent == "unknown"));
    }

    // --- stage 1: the catalogue must agree with the consts ------------------
    //
    // These are the whole point of stage 1. `voices.default.json` is a
    // transcription of the consts above, nothing reads it yet, and these tests
    // are what prove the transcription faithful *before* stage 5 deletes the
    // consts. A failure here means the file and the code have drifted, and
    // stage 5 would silently change which voice speaks.

    fn catalogue_engine(engine: &str) -> &'static EngineRoster {
        RosterFile::catalogue()
            .engine(engine)
            .unwrap_or_else(|| panic!("voices.default.json declares no `{engine}` engine"))
    }

    #[test]
    fn the_embedded_catalogue_parses_and_declares_both_engines() {
        assert_eq!(RosterFile::catalogue().version, 1);
        // 23 = every VieNeu preset the SDK store ships, Northern ones included.
        // The count is asserted rather than the names because the *completeness*
        // is the point: the catalogue used to declare only the 10 Central/South
        // presets, which excluded the other 13 by omission.
        assert_eq!(catalogue_engine("vieneu").voices.len(), 23);
        assert_eq!(catalogue_engine("gemini").voices.len(), 17);
        // Sanity: the embedded copy really is the file on disk.
        assert!(CATALOGUE_JSON.contains("\"vieneu\""));
        assert!(CATALOGUE_JSON.contains("\"gemini\""));
    }

    #[test]
    fn catalogue_keys_are_slugs_that_cannot_escape_a_path() {
        // The audition cache is `.bm/voices/samples/<key>.wav`, so a key
        // containing `/` or `..` would be a traversal.
        for (engine, roster) in &RosterFile::catalogue().engines {
            let mut seen = std::collections::HashSet::new();
            for v in &roster.voices {
                assert!(seen.insert(&v.key), "{engine}: duplicate key {}", v.key);
                let slug = !v.key.is_empty()
                    && v.key
                        .starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
                    && v.key
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
                assert!(slug, "{engine}: `{}` is not an ASCII slug", v.key);
            }
        }
    }

    #[test]
    fn catalogue_policy_matches_the_consts_field_for_field() {
        for (engine, expected) in [("vieneu", vieneu_policy()), ("gemini", gemini_policy())] {
            let got = catalogue_engine(engine).to_policy(engine);
            assert_eq!(got.engine, expected.engine, "{engine}: engine tag");
            assert_eq!(got.male, expected.male, "{engine}: male pool");
            assert_eq!(got.female, expected.female, "{engine}: female pool");
            assert_eq!(got.neutral, expected.neutral, "{engine}: neutral pool");
            // `default_cast` is order-insensitive at every call site: `cast.rs`
            // collects it into a map, `state.rs` into a BTreeSet. Compare the
            // assignments, not the iteration order.
            let as_map = |v: Vec<(String, String)>| v.into_iter().collect::<BTreeMap<_, _>>();
            assert_eq!(
                as_map(got.default_cast),
                as_map(expected.default_cast),
                "{engine}: default cast"
            );
        }
    }

    #[test]
    fn catalogue_offline_roster_matches_the_consts_voice_for_voice() {
        for engine in ["vieneu", "gemini"] {
            assert_eq!(
                catalogue_engine(engine).to_offline_voices(engine),
                offline_voices(engine),
                "{engine}: offline roster differs from the const tables"
            );
        }
    }

    #[test]
    fn every_default_cast_key_resolves_within_its_own_engine() {
        for (engine, roster) in &RosterFile::catalogue().engines {
            assert_eq!(
                roster.resolve_cast().len(),
                roster.default_cast.len(),
                "{engine}: a default_cast key resolves to no declared voice"
            );
        }
    }

    // --- stage 2: key <-> name resolution -----------------------------------

    #[test]
    fn keys_resolve_to_names_and_names_resolve_to_keys() {
        assert_eq!(
            key_for_name("vieneu", "Đức Trí").as_deref(),
            Some("duc-tri")
        );
        assert_eq!(
            name_for_key("vieneu", "duc-tri").as_deref(),
            Some("Đức Trí")
        );
        // A name the catalogue does not declare has no key — that is a clone.
        assert_eq!(key_for_name("vieneu", "Suneo"), None);
        assert_eq!(name_for_key("vieneu", "suneo"), None);
        // Both forms resolve to the same canonical display name, which is what
        // makes the cast migration optional rather than a gate.
        assert_eq!(resolve_voice_name("vieneu", "duc-tri"), "Đức Trí");
        assert_eq!(resolve_voice_name("vieneu", "Đức Trí"), "Đức Trí");
        // An unknown value is left exactly as it was, so the cast overview can
        // flag it instead of the pipeline quietly reassigning the speaker.
        assert_eq!(resolve_voice_name("vieneu", "Đã Biến Mất"), "Đã Biến Mất");
    }

    #[test]
    fn keys_are_scoped_to_their_own_engine() {
        // `charon` is a Gemini key; a VieNeu cast must not resolve it to a voice.
        assert_eq!(name_for_key("gemini", "charon").as_deref(), Some("Charon"));
        assert_eq!(name_for_key("vieneu", "charon"), None);
        assert_eq!(resolve_voice_name("vieneu", "charon"), "charon");
    }

    #[test]
    fn both_engines_key_every_voice_they_declare() {
        for engine in ["vieneu", "gemini"] {
            for v in catalogue_engine(engine).to_offline_voices(engine) {
                assert!(!v.key.is_empty(), "{}: {} has no key", engine, v.name);
                // The key must round-trip, or a cast lookup would miss.
                assert_eq!(
                    name_for_key(engine, &v.key).as_deref(),
                    Some(v.name.as_str()),
                    "{engine}: {} does not round-trip",
                    v.key
                );
            }
        }
    }

    // The catalogue is the whole policy: no machine-local overlay exists.
    #[test]
    fn the_catalogue_needs_no_overlay() {
        let p = effective_policy("vieneu");
        assert!(p.default_cast.is_empty(), "no seeded cast");
        let (engine, err) = effective_engine_lenient("vieneu");
        assert!(err.is_none());
        assert_eq!(engine.voices.len(), 23);
    }
}
