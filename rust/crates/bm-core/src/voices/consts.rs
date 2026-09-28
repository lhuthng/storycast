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
    /// The languages it can voice, as BCP-47 tags (`vi-VN`, `en-US`).
    ///
    /// This is a claim about the *engine*, not about the pipeline: an adapter
    /// whose language is not in this list is a mismatch rather than a setting.
    /// Deliberately narrow — listing the languages this project actually ships
    /// an adapter for is more useful than transcribing a model card, because
    /// the question a caller asks is "can this engine speak what this adapter
    /// writes", and an over-broad list answers it "yes" when nobody checked.
    pub languages: &'static [&'static str],
    /// Whether it can clone a voice from a reference clip.
    ///
    /// `:A` / `:N` (add a sample, name a voice) assume this, and a cloud engine
    /// has no reference clip to enroll — so the capability belongs here rather
    /// than being assumed by every caller.
    pub cloning: bool,
    /// The G2P dictionary's file name inside the engine's own `models/`.
    ///
    /// `None` for an engine whose text front end needs no pronunciation
    /// lexicon. Named per engine on purpose: the path used to hardcode
    /// VieNeu's `sea_g2p.bin`, so a second engine read the wrong dictionary and
    /// *mispronounced* instead of failing on a missing file.
    pub dict: Option<&'static str>,
    /// What its audio comes back as, before any resampling.
    pub sample_rate: u32,
    /// Channel count, same rule as `sample_rate`.
    pub channels: u16,
}

/// VieNeu-TTS v3 Turbo — the local engine, and the only one here that voices
/// tags: its front end turns each into an `<|emotion_N|>` token.
///
/// Vietnamese, and only Vietnamese: the model is a Vietnamese TTS, so an
/// English adapter behind it is the mismatch this list exists to name. It is
/// the engine that clones (`refs/` and `:A` are its machinery) and the one with
/// a lexicon, which is why `sea_g2p.bin` is named here and not in a path
/// function.
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
};

/// Gemini prebuilt TTS — cloud, and tagless. Every non-verbal sound stays as the
/// words the chapter wrote, because a bracket this engine does not implement is
/// read aloud.
///
/// Multilingual by nature; the two languages listed are the ones with an
/// adapter in this project, not the model's whole range — an under-declaration
/// refuses a language we could voice, which is the safe direction. No local
/// lexicon (the service does its own front end) and no cloning (a reference
/// clip is not something a cloud prebuilt voice accepts).
pub const GEMINI: EngineDecl = EngineDecl {
    name: "gemini",
    nonverbal: &[],
    languages: &["vi-VN", "en-US"],
    cloning: false,
    dict: None,
    sample_rate: 24_000,
    channels: 1,
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

/// The languages `engine` can voice — empty when nothing declares it.
pub fn languages(engine: &str) -> &'static [&'static str] {
    declaration(engine).map(|d| d.languages).unwrap_or(&[])
}

/// Whether `engine` declares it can voice `language`.
///
/// Matched on the exact BCP-47 tag first, then on the primary subtag, so a
/// declared `vi-VN` answers for `vi` and a declared `en` answers for `en-GB` —
/// the split nobody wants to maintain a second table for. **Undeclared answers
/// false**, like every other declaration lookup here: an engine nobody declared
/// makes no claim, and a claim is what a caller acts on.
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
/// engine cannot, because nothing said it could.
pub fn clones(engine: &str) -> bool {
    declaration(engine).map(|d| d.cloning).unwrap_or(false)
}

/// The G2P dictionary file name inside `engine`'s `models/`, if it has one.
pub fn dictionary(engine: &str) -> Option<&'static str> {
    declaration(engine).and_then(|d| d.dict)
}

/// What `engine`'s audio comes back as: `(sample_rate, channels)`.
///
/// Falls back to Gemini's rate for an undeclared name, which is the historical
/// `if engine == "vieneu" … else` behaviour — the one answer that has to be
/// preserved is that a name nobody declared is *not* VieNeu's 48 kHz.
pub fn output_format(engine: &str) -> (u32, u16) {
    declaration(engine)
        .map(|d| (d.sample_rate, d.channels))
        .unwrap_or((GEMINI.sample_rate, GEMINI.channels))
}

/// The language an adapter id names, given the pack it is bound to.
///
/// An adapter is `<pack>-<language>` (ids are pack-bound: `xianxia-en-US`, not
/// `en-US`), so the language is the id with the pack's prefix removed. An id
/// that does not carry the prefix is its own language.
///
/// `None` for an unnamed adapter — `default` is the pre-split checkout, which
/// makes no claim about a language and so cannot be mismatched.
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

    /// The engine's other facts are declared here for the same reason the tags
    /// are: a caller asks the table, and an undeclared name gets the answer that
    /// refuses rather than the answer that guesses.
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

    /// The primary subtag answers for its variants: a table of `vi-VN` has to
    /// answer a crawl tagged `vi`, and `en-US` has to answer `en-GB`, or the
    /// list would have to be re-typed per region.
    #[test]
    fn a_declared_language_answers_for_its_region_variants() {
        assert!(voices_language("vieneu", "vi"));
        assert!(voices_language("vieneu", "VI-vn"), "case-insensitive");
        assert!(!voices_language("vieneu", "en"));
        assert!(voices_language("gemini", "en-GB"));
        // A subtag must not swallow its neighbours: `en` is not `eng`, and an
        // unrelated region of a declared language is still that language.
        assert!(!voices_language("vieneu", "eng"));
    }

    /// The two numbers the render path resamples against come from here now, so
    /// the one behaviour that must not change is that a name nobody declared is
    /// **not** VieNeu's 48 kHz.
    #[test]
    fn the_output_format_is_engine_specific_and_undeclared_is_not_vieneu() {
        assert_eq!(output_format("vieneu"), (48_000, 1));
        assert_eq!(output_format("gemini"), (24_000, 1));
        assert_eq!(output_format("not-an-engine"), (24_000, 1));
        assert_ne!(output_format("not-an-engine"), output_format("vieneu"));
    }

    /// The adapter's language is the id with the pack stripped, and an unnamed
    /// adapter claims nothing — which is what keeps a pre-split checkout from
    /// tripping the mismatch the (pack-bound) ids make meaningful.
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
