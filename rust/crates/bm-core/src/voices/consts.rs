use serde::{Deserialize, Serialize};

/// What shape an engine's `models/voices.json` has — and therefore how a clone
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceStore {
    /// A **preset store**: `speaker_emb` + `codes` per voice, which the python
    Presets,
    /// A **clip store**: each entry is a reference `file` under the engine's
    Clips,
    /// No local store at all: nothing can be enrolled on this machine (a cloud
    None,
}

/// What one engine declares about itself.
pub struct EngineDecl {
    /// The `settings.engine` name, which is what every path keys on.
    pub name: &'static str,
    /// The sounds it voices as tags, as `(placeholder, gloss, tag)`.
    pub nonverbal: &'static [(&'static str, &'static str, &'static str)],
    /// The languages it can voice, as BCP-47 tags (`vi-VN`, `en-US`).
    /// the question a caller asks is "can this engine speak what this adapter
    /// writes", and an over-broad list answers it "yes" when nobody checked.
    pub languages: &'static [&'static str],
    /// Whether it can clone a voice from a reference clip.
    pub cloning: bool,
    /// The G2P dictionary's file name inside the engine's own `models/`.
    pub dict: Option<&'static str>,
    /// What its audio comes back as, before any resampling.
    pub sample_rate: u32,
    /// Channel count, same rule as `sample_rate`.
    pub channels: u16,
    /// The shape of its `models/voices.json`, and so how a clone is enrolled
    pub store: VoiceStore,
    /// The hottest sampling temperature this engine can be asked for.
    /// at 0.7 two of four sentences degraded — which is what "static, and
    /// sometimes inaudible" is. At 0 it is deterministic and clean. So the
    pub max_temperature: f64,
    /// The `bm-tts` cargo features this engine's sidecar must be built with.
    pub tts_features: &'static [&'static str],
}

/// `engine`'s sidecar cargo features, `None` when nothing declares that name.
pub fn tts_features(engine: &str) -> Option<&'static [&'static str]> {
    declaration(engine).map(|d| d.tts_features)
}

/// VieNeu-TTS v3 Turbo — the local engine, and the only one here that voices
pub const VIENEU: EngineDecl = EngineDecl {
    name: "vieneu",
    nonverbal: &[
        ("laugh", "laugh", "[cười]"),
        ("sigh", "sigh", "[thở dài]"),
        ("throat", "throat-clear", "[hắng giọng]"),
    ],
    languages: &["vi-VN"],
    cloning: true,
    dict: Some("sea_g2p.bin"),
    sample_rate: 48_000,
    channels: 1,
    store: VoiceStore::Presets,
    max_temperature: 1.0,
    tts_features: &[],
};

/// Gemini prebuilt TTS — cloud, and tagless. Every non-verbal sound stays as the
pub const GEMINI: EngineDecl = EngineDecl {
    name: "gemini",
    nonverbal: &[],
    languages: &["vi-VN", "en-US"],
    cloning: false,
    dict: None,
    sample_rate: 24_000,
    channels: 1,
    store: VoiceStore::None,
    max_temperature: 1.0,
    tts_features: &[],
};

/// Kyutai's Pocket TTS — the second local engine, and the one the English
pub const POCKET: EngineDecl = EngineDecl {
    name: "pocket",
    // **Zero, and measured.** See `EngineDecl::max_temperature`: at the mood
    nonverbal: &[],
    languages: &["en-US"],
    cloning: true,
    dict: None,
    sample_rate: 24_000,
    channels: 1,
    store: VoiceStore::Clips,
    max_temperature: 0.0,
    tts_features: &["pocket"],
};

/// Every engine this build declares, in declaration order.
pub const ENGINES: &[EngineDecl] = &[VIENEU, GEMINI, POCKET];

/// `engine`'s declaration, or `None` when nothing here declares that name.
pub fn declaration(engine: &str) -> Option<&'static EngineDecl> {
    ENGINES.iter().find(|d| d.name == engine)
}

/// The non-verbal vocabulary `engine` voices — empty when it voices none, **and
pub fn nonverbals(engine: &str) -> &'static [(&'static str, &'static str, &'static str)] {
    declaration(engine).map(|d| d.nonverbal).unwrap_or(&[])
}

/// Whether `engine` voices any non-verbal tags, for callers that only branch.
pub fn supports_nonverbal(engine: &str) -> bool {
    !nonverbals(engine).is_empty()
}

/// The languages `engine` can voice — empty when nothing declares it.
pub fn languages(engine: &str) -> &'static [&'static str] {
    declaration(engine).map(|d| d.languages).unwrap_or(&[])
}

/// Whether `engine` declares it can voice `language`.
pub fn voices_language(engine: &str, language: &str) -> bool {
    let want = language.trim();
    if want.is_empty() {
        return false;
    }
    let primary = |tag: &str| tag.split(['-', '_']).next().unwrap_or(tag).to_lowercase();
    let want_lower = want.to_lowercase();
    languages(engine).iter().any(|have| {
        have.eq_ignore_ascii_case(want)
            || (primary(have) == primary(&want_lower) && !primary(have).is_empty())
    })
}

/// Whether `engine` can clone a voice from a reference clip. An undeclared
pub fn clones(engine: &str) -> bool {
    declaration(engine).map(|d| d.cloning).unwrap_or(false)
}

/// The G2P dictionary file name inside `engine`'s `models/`, if it has one.
pub fn dictionary(engine: &str) -> Option<&'static str> {
    declaration(engine).and_then(|d| d.dict)
}

/// The shape of `engine`'s `models/voices.json` — see [`VoiceStore`].
pub fn store_kind(engine: &str) -> VoiceStore {
    declaration(engine)
        .map(|d| d.store)
        .unwrap_or(VoiceStore::None)
}

/// The hottest sampling temperature `engine` can be handed.
pub fn max_temperature(engine: &str) -> f64 {
    declaration(engine)
        .map(|d| d.max_temperature)
        .unwrap_or(1.0)
}

/// `wanted`, capped at what `engine` can actually speak.
pub fn clamp_temperature(engine: &str, wanted: f64) -> f64 {
    wanted.min(max_temperature(engine))
}

/// What `engine`'s audio comes back as: `(sample_rate, channels)`.
pub fn output_format(engine: &str) -> (u32, u16) {
    declaration(engine)
        .map(|d| (d.sample_rate, d.channels))
        .unwrap_or((GEMINI.sample_rate, GEMINI.channels))
}

/// The language an adapter id names, given the pack it is bound to.
pub fn adapter_language<'a>(pack: &str, adapter: &'a str) -> Option<&'a str> {
    let adapter = adapter.trim();
    if adapter.is_empty() || adapter == crate::paths::DEFAULT_ADAPTER {
        return None;
    }
    let pack = pack.trim();
    if !pack.is_empty() {
        if let Some(rest) = adapter.strip_prefix(pack).and_then(|r| r.strip_prefix('-')) {
            if !rest.is_empty() {
                return Some(rest);
            }
        }
    }
    Some(adapter)
}

/// Every male VieNeu preset, in the store's declaration order.
pub const VIENEU_MALE: [&str; 12] = [
    "Minh Đức",
    "Phạm Tuyên",
    "Thái Sơn",
    "Xuân Vĩnh",
    "Thanh Bình",
    "Minh Triết",
    "Quang Sơn",
    "Đức Trí",
    "Adam",
    "Mạnh Dũng",
    "Minh Quân",
    "Anh Khôi",
];

/// Every female VieNeu preset, in the store's declaration order.
pub const VIENEU_FEMALE: [&str; 11] = [
    "Trúc Ly",
    "Ngọc Linh",
    "Đoan Trang",
    "Mai Anh",
    "Thục Đoan",
    "Thùy Dung",
    "Ngọc Trân",
    "Mỹ Duyên",
    "Quỳnh Anh",
    "Kim Thanh",
    "Ngọc Huyền",
];

/// Gemini prebuilt voices, gendered by ear — Google's labels ("firm",
pub const GEMINI_MALE: [&str; 6] = ["Orus", "Charon", "Fenrir", "Algenib", "Gacrux", "Alnilam"];
pub const GEMINI_FEMALE: [&str; 7] = [
    "Vindemiatrix",
    "Leda",
    "Aoede",
    "Callirrhoe",
    "Despina",
    "Sulafat",
    "Achernar",
];
pub const GEMINI_NEUTRAL: [&str; 4] = ["Schedar", "Puck", "Erinome", "Rasalgethi"];

/// Pocket TTS's English voices, gendered by name and by sample — the catalog
pub const POCKET_MALE: [&str; 10] = [
    "bill-boerst",
    "charles",
    "george",
    "jean",
    "javert",
    "marius",
    "michael",
    "paul",
    "peter-yearsley",
    "stuart-bell",
];
pub const POCKET_FEMALE: [&str; 11] = [
    "alba", "anna", "azelma", "cosette", "eponine", "eve", "fantine", "jane", "lola", "mary",
    "vera",
];
pub const POCKET_NEUTRAL: [&str; 1] = ["caro-davy"];

/// Female markers are checked *first*: "female" contains "male".
const FEMALE_HINTS: [&str; 12] = [
    "female",
    "nữ",
    "cô",
    "chị",
    "tỷ",
    "muội",
    "gái",
    "girl",
    "woman",
    "lady",
    "bà",
    "muội tử",
];
const MALE_HINTS: [&str; 10] = [
    "male",
    "nam",
    "ông",
    "anh",
    "trai",
    "đàn ông",
    "boy",
    "man",
    "lão",
    "adult male",
];

/// Everything the cast assigner needs to pick a voice for a new character.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoicePolicy {
    pub engine: String,
    pub male: Vec<String>,
    pub female: Vec<String>,
    pub neutral: Vec<String>,
    /// Seed assignments, as `(character, voice)` pairs.
    pub default_cast: Vec<(String, String)>,
}

impl VoicePolicy {
    /// Infer gender from a bible `voice_hint`, female-first.
    pub fn pool_for_hint(&self, hint: &str) -> &[String] {
        let h = hint.to_lowercase();
        if FEMALE_HINTS.iter().any(|k| h.contains(k)) {
            &self.female
        } else if MALE_HINTS.iter().any(|k| h.contains(k)) {
            &self.male
        } else {
            &self.neutral
        }
    }

    /// This policy with every pool cut down to `installed`.
    pub fn restricted_to(&self, installed: &std::collections::BTreeSet<String>) -> VoicePolicy {
        let keep = |pool: &[String]| -> Vec<String> {
            pool.iter()
                .filter(|v| installed.contains(*v))
                .cloned()
                .collect()
        };
        VoicePolicy {
            engine: self.engine.clone(),
            male: keep(&self.male),
            female: keep(&self.female),
            neutral: keep(&self.neutral),
            default_cast: self.default_cast.clone(),
        }
    }
}

/// The shipped VieNeu policy: every declared preset, no preference.
pub fn vieneu_policy() -> VoicePolicy {
    let male: Vec<String> = VIENEU_MALE.iter().map(|s| s.to_string()).collect();
    let female: Vec<String> = VIENEU_FEMALE.iter().map(|s| s.to_string()).collect();
    VoicePolicy {
        engine: "vieneu".into(),
        neutral: male.clone(), // legacy behaviour: unknown gender -> male pool
        male,
        female,
        // The cast is the operator's, not the catalogue's: character names are
        default_cast: Vec::new(),
    }
}

/// The shipped Gemini policy: cloud prebuilt voices, no restriction, no cast.
pub fn gemini_policy() -> VoicePolicy {
    VoicePolicy {
        engine: "gemini".into(),
        male: GEMINI_MALE.iter().map(|s| s.to_string()).collect(),
        female: GEMINI_FEMALE.iter().map(|s| s.to_string()).collect(),
        neutral: GEMINI_NEUTRAL.iter().map(|s| s.to_string()).collect(),
        default_cast: Vec::new(),
    }
}

/// The shipped Pocket policy: the English catalog, no restriction, no cast.
pub fn pocket_policy() -> VoicePolicy {
    VoicePolicy {
        engine: "pocket".into(),
        male: POCKET_MALE.iter().map(|s| s.to_string()).collect(),
        female: POCKET_FEMALE.iter().map(|s| s.to_string()).collect(),
        neutral: POCKET_NEUTRAL.iter().map(|s| s.to_string()).collect(),
        default_cast: Vec::new(),
    }
}

pub fn policy_for(engine: &str) -> VoicePolicy {
    if engine == "vieneu" {
        vieneu_policy()
    } else if engine == "pocket" {
        pocket_policy()
    } else {
        gemini_policy()
    }
}

// --- voice metadata --------------------------------------------------------

/// Per-voice accent/style, transcribed from the preset store's own labels.
pub(crate) const PRESET_META: [(&str, &str, &str); 23] = [
    ("Minh Đức", "Northern", "tin tức"),
    ("Phạm Tuyên", "Northern", "tự nhiên"),
    ("Thái Sơn", "South", "kể chuyện"),
    ("Xuân Vĩnh", "Northern", "tự nhiên"),
    ("Thanh Bình", "Northern", "kể chuyện"),
    ("Trúc Ly", "Northern", "tự nhiên"),
    ("Ngọc Linh", "Northern", "kể chuyện"),
    ("Đoan Trang", "Northern", "tự nhiên"),
    ("Mai Anh", "Northern", "tin tức"),
    ("Thục Đoan", "South", "kể chuyện"),
    ("Minh Triết", "South", "tin tức"),
    ("Thùy Dung", "South", "tin tức"),
    ("Quang Sơn", "Central", "tự nhiên"),
    ("Ngọc Trân", "Central", "tự nhiên"),
    ("Mỹ Duyên", "South", "đọc truyện"),
    ("Quỳnh Anh", "Northern", "đọc truyện"),
    ("Đức Trí", "South", "đọc truyện"),
    ("Kim Thanh", "South", "đọc truyện"),
    ("Ngọc Huyền", "Northern", "tự nhiên"),
    ("Adam", "South", "tự nhiên"),
    ("Mạnh Dũng", "Northern", "tự nhiên"),
    ("Minh Quân", "Northern", "tự nhiên"),
    ("Anh Khôi", "Northern", "kể chuyện"),
];

/// The language the pipeline *speaks*. Every engine here is fed Vietnamese
pub(crate) const CONTENT_LANGUAGE: &str = "vi-VN";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn female_hints_win_over_the_male_substring() {
        let p = vieneu_policy();
        // "female" contains "male" — the female branch must be tested first.
        assert_eq!(p.pool_for_hint("adult female, cold"), &p.female);
        assert_eq!(p.pool_for_hint("adult male, stern"), &p.male);
        assert_eq!(p.pool_for_hint("giọng nữ trẻ"), &p.female);
        assert_eq!(p.pool_for_hint(""), &p.neutral);
    }
    /// The declarations answer for their engine, and nothing else does.
    #[test]
    fn only_pocket_needs_its_sidecar_built_with_a_feature() {
        // The regression this exists for: a cross-built `bm-tts` without
        assert_eq!(tts_features("pocket"), Some(&["pocket"][..]));
        assert_eq!(tts_features("vieneu"), Some(&[][..]));
        assert_eq!(tts_features("gemini"), Some(&[][..]));
        assert_eq!(tts_features("nothing-claims-this"), None);
        // Every declared engine answers, so no engine can be added without
        for d in ENGINES {
            assert_eq!(tts_features(d.name), Some(d.tts_features));
        }
    }

    /// The declarations answer for their engine, and nothing else does.
    #[test]
    fn an_engines_declaration_is_what_gates_its_tags() {
        assert!(supports_nonverbal("vieneu"));
        let tags = nonverbals("vieneu");
        assert_eq!(tags.len(), 3);
        assert!(
            tags.iter()
                .any(|(_key, _gloss, tag)| *tag == "[hắng giọng]"),
            "{tags:?}"
        );

        // Gemini declares none, so its prompt carries no rule at all: the
        assert!(!supports_nonverbal("gemini"));
        assert!(nonverbals("gemini").is_empty());

        // **And a name nobody declared answers the same way**, which is the
        assert!(!supports_nonverbal("not-an-engine"));
        assert!(nonverbals("not-an-engine").is_empty());
        assert!(declaration("not-an-engine").is_none());
        assert_eq!(declaration("vieneu").map(|d| d.name), Some("vieneu"));
        assert!(ENGINES.iter().any(|d| d.name == "gemini"));
    }

    /// The engine's other facts are declared here for the same reason the tags
    #[test]
    fn an_engine_declares_its_languages_and_whether_it_clones() {
        // VieNeu is the local, Vietnamese, cloning engine with a lexicon.
        assert_eq!(languages("vieneu"), &["vi-VN"]);
        assert!(voices_language("vieneu", "vi-VN"));
        assert!(!voices_language("vieneu", "en-US"));
        assert!(clones("vieneu"));
        assert_eq!(dictionary("vieneu"), Some("sea_g2p.bin"));

        // Gemini is cloud and multilingual, and clones nothing.
        assert!(voices_language("gemini", "en-US"));
        assert!(voices_language("gemini", "vi-VN"));
        assert!(!clones("gemini"));
        assert_eq!(dictionary("gemini"), None);

        // A name nobody declared makes no claim, so every predicate refuses.
        assert!(languages("not-an-engine").is_empty());
        assert!(!voices_language("not-an-engine", "vi-VN"));
        assert!(!clones("not-an-engine"));
        assert_eq!(dictionary("not-an-engine"), None);
        // ...and an empty language is never voiced, or `""` would be a match.
        assert!(!voices_language("gemini", ""));
    }

    /// The store declaration is what tells enrollment which shape `:A`/`:N`
    #[test]
    fn an_engine_declares_the_shape_of_its_voice_store() {
        assert_eq!(store_kind("vieneu"), VoiceStore::Presets);
        assert_eq!(store_kind("pocket"), VoiceStore::Clips);
        assert_eq!(store_kind("gemini"), VoiceStore::None);
        assert_eq!(store_kind("not-an-engine"), VoiceStore::None);
        // An engine that clones needs somewhere to put a clone: a store-less
        for d in ENGINES {
            if d.cloning {
                assert_ne!(
                    d.store,
                    VoiceStore::None,
                    "{} clones, so it needs a store",
                    d.name
                );
            }
        }
    }

    /// The primary subtag answers for its variants: a table of `vi-VN` has to
    #[test]
    fn a_declared_language_answers_for_its_region_variants() {
        assert!(voices_language("vieneu", "vi"));
        assert!(voices_language("vieneu", "VI-vn"), "case-insensitive");
        assert!(!voices_language("vieneu", "en"));
        assert!(voices_language("gemini", "en-GB"));
        // A subtag must not swallow its neighbours: `en` is not `eng`, and an
        assert!(!voices_language("vieneu", "eng"));
    }

    /// The two numbers the render path resamples against come from here now, so
    #[test]
    fn the_output_format_is_engine_specific_and_undeclared_is_not_vieneu() {
        assert_eq!(output_format("vieneu"), (48_000, 1));
        assert_eq!(output_format("gemini"), (24_000, 1));
        assert_eq!(output_format("not-an-engine"), (24_000, 1));
        assert_ne!(output_format("not-an-engine"), output_format("vieneu"));
    }

    /// The adapter's language is the id with the pack stripped, and an unnamed
    #[test]
    fn an_adapters_language_is_its_id_without_the_pack() {
        assert_eq!(adapter_language("xianxia", "xianxia-en-US"), Some("en-US"));
        assert_eq!(adapter_language("xianxia", "xianxia-vi-VN"), Some("vi-VN"));
        // No prefix: the id *is* the language (the older, unbounded spelling).
        assert_eq!(adapter_language("xianxia", "vi-VN"), Some("vi-VN"));
        // An unnamed adapter is the pre-split checkout, not a language.
        assert_eq!(adapter_language("xianxia", ""), None);
        assert_eq!(adapter_language("xianxia", "default"), None);
        // The pack was not a prefix after all, so nothing was stripped.
        assert_eq!(
            adapter_language("noir", "xianxia-en-US"),
            Some("xianxia-en-US")
        );
    }

    /// The two lists must not drift: a catalogue engine with no declaration is
    #[test]
    fn every_declared_engine_has_a_catalogue_row_and_the_reverse() {
        let declared: std::collections::BTreeSet<&str> = ENGINES.iter().map(|d| d.name).collect();
        let catalogued: std::collections::BTreeSet<&str> = crate::voices::RosterFile::catalogue()
            .engines
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(declared, catalogued);
    }

    #[test]
    fn the_shipped_policy_casts_nobody() {
        // The catalogue is public. A fresh clone must offer the whole roster,
        for p in [vieneu_policy(), gemini_policy()] {
            assert!(
                p.default_cast.is_empty(),
                "{}: the cast is the operator's, not the catalogue's",
                p.engine
            );
        }
    }
}
