use serde::{Deserialize, Serialize};

/// Every male VieNeu preset, in the store's declaration order.
///
/// Complete on purpose — see the module comment. Accents are per-voice and
/// exact (`Northern` / `Central` / `South`), taken from the SDK's own
/// `"<Giới tính> · <Vùng> · <Phong cách>"` labels rather than from a policy
/// guarantee, so an operator can exclude precisely one region.
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
    /// Hard allow-list. Empty means "no accent restriction" (Gemini).
    pub allowed: Vec<String>,
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

    /// Voices that a cast is permitted to use. `customs` are user-enrolled
    /// clones, which bypass the preset allow-list but never the accent policy
    /// (a clone is assumed vetted when it was enrolled).
    pub fn violations(
        &self,
        cast: &[(String, String)],
        customs: &[String],
    ) -> Vec<(String, String)> {
        if self.allowed.is_empty() {
            return Vec::new();
        }
        cast.iter()
            .filter(|(_, v)| {
                !self.allowed.iter().any(|a| a == v) && !customs.iter().any(|c| c == v)
            })
            .cloned()
            .collect()
    }
}

/// The shipped VieNeu policy: every declared preset, no preference.
///
/// This file is public, so it encodes nobody's regional taste. An empty
/// `allowed` means "no restriction" at every site that reads it.
pub fn vieneu_policy() -> VoicePolicy {
    let male: Vec<String> = VIENEU_MALE.iter().map(|s| s.to_string()).collect();
    let female: Vec<String> = VIENEU_FEMALE.iter().map(|s| s.to_string()).collect();
    VoicePolicy {
        engine: "vieneu".into(),
        neutral: male.clone(), // legacy behaviour: unknown gender -> male pool
        male,
        female,
        allowed: Vec::new(), // no restriction — see the doc comment
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
        allowed: Vec::new(),
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
/// store has it, and an operator who disagrees can say so in their own roster.
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

    #[test]
    fn the_shipped_policy_restricts_nothing_and_casts_nobody() {
        // The catalogue is public. It used to declare only the Central/South
        // presets and allow-list exactly those, which made one operator's
        // regional taste look like a fact about the engine. A fresh clone must
        // offer the whole roster, and must not arrive pre-cast with a stranger's
        // characters.
        for p in [vieneu_policy(), gemini_policy()] {
            assert!(
                p.allowed.is_empty(),
                "{}: empty means no restriction",
                p.engine
            );
            assert!(
                p.default_cast.is_empty(),
                "{}: the cast is the operator's, not the catalogue's",
                p.engine
            );
        }
    }

    #[test]
    fn violations_catch_a_non_policy_voice_but_allow_enrolled_clones() {
        // A restriction has to come from somewhere now, so state one.
        let mut p = vieneu_policy();
        p.allowed = vec!["Đức Trí".to_string()];
        let bad = vec![("A".to_string(), "Bắc Giang".to_string())];
        assert_eq!(p.violations(&bad, &[]).len(), 1);
        let custom = vec![("A".to_string(), "Suneo".to_string())];
        assert!(p.violations(&custom, &["Suneo".to_string()]).is_empty());
    }

    #[test]
    fn gemini_policy_has_no_accent_restriction() {
        let p = gemini_policy();
        assert!(p.allowed.is_empty());
        assert!(p
            .violations(&[("A".into(), "Anything".into())], &[])
            .is_empty());
    }
}
