use serde::{Deserialize, Serialize};

/// What one engine declares about itself.
///
/// **This table is the API, and nothing outside it branches on an engine's
/// name.** A stage that needs to know whether the bound engine voices
/// non-verbal tags asks [`nonverbals`]; a build that grows a third engine adds
/// a row here and is handled everywhere the declaration is read. An `if engine
/// == "…"` in a consumer is the thing this exists to avoid — the engine axis is
/// meant to be extended by declaration, not by editing every reader.
///
/// The engine's *facts*, not the pipeline's preferences: its front end is the
/// authority on what a tag means (`bm-tts/src/text.rs` maps each to an
/// `<|emotion_N|>` token), and this is the pipeline's copy of what it can say.
/// Voices and sample rate stay in the catalogue; this is what has no voice-list
/// home.
pub struct EngineDecl {
    /// The `settings.engine` name, which is what every path keys on.
    pub name: &'static str,
    /// The sounds it voices as tags, as `(placeholder, gloss, tag)`.
    ///
    /// The first field is the half a prompt refers to (`{tag_laugh}`); the gloss
    /// is only what the rendered vocabulary calls the sound. Empty means the
    /// engine voices none, which is a declaration and not a gap.
    pub nonverbal: &'static [(&'static str, &'static str, &'static str)],
}

/// VieNeu-TTS v3 Turbo — the local engine, and the only one here that voices
/// tags: its front end turns each into an `<|emotion_N|>` token.
pub const VIENEU: EngineDecl = EngineDecl {
    name: "vieneu",
    nonverbal: &[
        ("laugh", "laugh", "[cười]"),
        ("sigh", "sigh", "[thở dài]"),
        ("throat", "throat-clear", "[hắng giọng]"),
    ],
};

/// Gemini prebuilt TTS — cloud, and tagless. Every non-verbal sound stays as the
/// words the chapter wrote, because a bracket this engine does not implement is
/// read aloud.
pub const GEMINI: EngineDecl = EngineDecl {
    name: "gemini",
    nonverbal: &[],
};

/// Every engine this build declares, in declaration order.
pub const ENGINES: &[EngineDecl] = &[VIENEU, GEMINI];

/// `engine`'s declaration, or `None` when nothing here declares that name.
pub fn declaration(engine: &str) -> Option<&'static EngineDecl> {
    ENGINES.iter().find(|d| d.name == engine)
}

/// The non-verbal vocabulary `engine` voices — empty when it voices none, **and
/// empty when nothing declares it at all**.
///
/// A slice rather than an `Option` on purpose: both answers mean the same thing
/// to a caller (there is no tag to write), so no reader has to tell them apart
/// to be correct, and a typo in `settings.engine` cannot produce a prompt that
/// asks for tokens.
pub fn nonverbals(engine: &str) -> &'static [(&'static str, &'static str, &'static str)] {
    declaration(engine).map(|d| d.nonverbal).unwrap_or(&[])
}

/// Whether `engine` voices any non-verbal tags, for callers that only branch.
pub fn supports_nonverbal(engine: &str) -> bool {
    !nonverbals(engine).is_empty()
}

/// Every male VieNeu preset, in the store's declaration order.
///
/// Complete on purpose — see the module comment. Accents are per-voice and
/// exact (`Northern` / `Central` / `South`), taken from the SDK's own
/// `"<Giới tính> · <Vùng> · <Phong cách>"` labels rather than from a policy
/// guarantee.
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
/// "breezy") do not encode gender.
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
}

/// The shipped VieNeu policy: every declared preset, no preference.
///
/// This file is public, so it encodes nobody's regional taste.
pub fn vieneu_policy() -> VoicePolicy {
    let male: Vec<String> = VIENEU_MALE.iter().map(|s| s.to_string()).collect();
    let female: Vec<String> = VIENEU_FEMALE.iter().map(|s| s.to_string()).collect();
    VoicePolicy {
        engine: "vieneu".into(),
        neutral: male.clone(), // legacy behaviour: unknown gender -> male pool
        male,
        female,
        // The cast is the operator's, not the catalogue's: character names are
        // specific to one novel, and shipping them would put someone's book in
        // everyone's clone.
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

pub fn policy_for(engine: &str) -> VoicePolicy {
    if engine == "vieneu" {
        vieneu_policy()
    } else {
        gemini_policy()
    }
}

// --- voice metadata --------------------------------------------------------
//
// The operator picks voices by ear and by description, so the picker needs
// more than a bare name. The SDK already labels every preset
// `"<Name> — <Giới tính> · <Vùng> · <Phong cách>"`; the sidecar's `/roster`
// parses that. This module carries the same shape for when no sidecar answers,
// because a scheduled render must not depend on the sidecar being up.

/// Per-voice accent/style, transcribed from the preset store's own labels.
///
/// `vieneu/assets/voices_v3_turbo.json` carries
/// `"<Giới tính> · <Vùng> · <Phong cách>"` for every preset, and this table is
/// that text, not a guess. It used to default the accent column to the *policy
/// guarantee* (`Central/South`) — which was true only because every preset in
/// the list was Central/South, and therefore said nothing about any one voice.
/// With the full roster declared, each entry states its real region.
///
/// `Xuân Vĩnh` is the one entry worth a caveat: the store labels it `Bắc` while
/// the model card and several forks call it Southern. It is recorded as the
/// store has it.
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
/// novel text, so this is the content language, not the voice's full
/// multilingual range.
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
        // bracket would be read aloud rather than voiced.
        assert!(!supports_nonverbal("gemini"));
        assert!(nonverbals("gemini").is_empty());

        // **And a name nobody declared answers the same way**, which is the
        // point of a slice over an `Option`: adding an engine is adding a row
        // here, and a typo in `settings.engine` cannot produce a prompt that
        // asks for a token.
        assert!(!supports_nonverbal("not-an-engine"));
        assert!(nonverbals("not-an-engine").is_empty());
        assert!(declaration("not-an-engine").is_none());
        assert_eq!(declaration("vieneu").map(|d| d.name), Some("vieneu"));
        assert!(ENGINES.iter().any(|d| d.name == "gemini"));
    }

    /// The two lists must not drift: a catalogue engine with no declaration is
    /// an engine no stage can ask about, and a declaration with no catalogue row
    /// is an engine with no voices. Adding one without the other is a build
    /// failure rather than a silent half-addition.
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
        // and must not arrive pre-cast with a stranger's characters.
        for p in [vieneu_policy(), gemini_policy()] {
            assert!(
                p.default_cast.is_empty(),
                "{}: the cast is the operator's, not the catalogue's",
                p.engine
            );
        }
    }
}
