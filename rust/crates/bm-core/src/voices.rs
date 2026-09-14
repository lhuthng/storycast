//! Voice rosters and the accent policy.
//!
//! These mirror `python/tts_vieneu.py` (VieNeu presets + the Central/South
//! policy) and the Gemini pools from the legacy `synthesize.py`. The TTS
//! sidecar is the authority at runtime — the inductor asks it for `/policy` —
//! but the workers need a usable fallback so a scheduled render never depends
//! on the sidecar being up just to decide who speaks.

use serde::{Deserialize, Serialize};

/// Central/South male presets. Northern voices are deliberately absent.
pub const VIENEU_MALE: [&str; 5] = ["Thái Sơn", "Đức Trí", "Adam", "Minh Triết", "Quang Sơn"];

/// Central/South female presets.
pub const VIENEU_FEMALE: [&str; 5] = [
    "Thục Đoan",
    "Mỹ Duyên",
    "Thùy Dung",
    "Kim Thanh",
    "Ngọc Trân",
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
    "female", "nữ", "cô", "chị", "tỷ", "muội", "gái", "girl", "woman", "lady", "bà", "muội tử",
];
const MALE_HINTS: [&str; 10] = [
    "male", "nam", "ông", "anh", "trai", "đàn ông", "boy", "man", "lão", "adult male",
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
    pub fn violations(&self, cast: &[(String, String)], customs: &[String]) -> Vec<(String, String)> {
        if self.allowed.is_empty() {
            return Vec::new();
        }
        cast.iter()
            .filter(|(_, v)| !self.allowed.iter().any(|a| a == v) && !customs.iter().any(|c| c == v))
            .cloned()
            .collect()
    }
}

/// The VieNeu policy: Central/South presets only.
pub fn vieneu_policy() -> VoicePolicy {
    let male: Vec<String> = VIENEU_MALE.iter().map(|s| s.to_string()).collect();
    let female: Vec<String> = VIENEU_FEMALE.iter().map(|s| s.to_string()).collect();
    let mut allowed = male.clone();
    allowed.extend(female.clone());
    VoicePolicy {
        engine: "vieneu".into(),
        neutral: male.clone(), // legacy behaviour: unknown gender -> male pool
        male,
        female,
        allowed,
        default_cast: vec![
            ("Narrator".into(), "Đức Trí".into()),
            ("Dịch Phong".into(), "Thái Sơn".into()),
            ("Lạc Lan Tuyết".into(), "Thục Đoan".into()),
            ("Doãn Lạc Ly".into(), "Mỹ Duyên".into()),
            ("Chủ hàng sát vách".into(), "Quang Sơn".into()),
        ],
    }
}

/// The Gemini policy: cloud prebuilt voices, no accent restriction.
pub fn gemini_policy() -> VoicePolicy {
    VoicePolicy {
        engine: "gemini".into(),
        male: GEMINI_MALE.iter().map(|s| s.to_string()).collect(),
        female: GEMINI_FEMALE.iter().map(|s| s.to_string()).collect(),
        neutral: GEMINI_NEUTRAL.iter().map(|s| s.to_string()).collect(),
        allowed: Vec::new(),
        default_cast: vec![
            ("Narrator".into(), "Charon".into()),
            ("Dịch Phong".into(), "Orus".into()),
            ("Lạc Lan Tuyết".into(), "Vindemiatrix".into()),
            ("Doãn Lạc Ly".into(), "Leda".into()),
        ],
    }
}

pub fn policy_for(engine: &str) -> VoicePolicy {
    if engine == "vieneu" {
        vieneu_policy()
    } else {
        gemini_policy()
    }
}

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
    fn vieneu_policy_excludes_northern_presets() {
        let p = vieneu_policy();
        assert!(p.allowed.contains(&"Đức Trí".to_string()));
        assert!(!p.allowed.contains(&"Xuân Vĩnh".to_string()));
        assert_eq!(p.allowed.len(), 10);
    }

    #[test]
    fn violations_catch_a_non_policy_voice_but_allow_enrolled_clones() {
        let p = vieneu_policy();
        let bad = vec![("A".to_string(), "Bắc Giang".to_string())];
        assert_eq!(p.violations(&bad, &[]).len(), 1);
        let custom = vec![("A".to_string(), "Suneo".to_string())];
        assert!(p.violations(&custom, &["Suneo".to_string()]).is_empty());
    }

    #[test]
    fn gemini_policy_has_no_accent_restriction() {
        let p = gemini_policy();
        assert!(p.allowed.is_empty());
        assert!(p.violations(&[("A".into(), "Anything".into())], &[]).is_empty());
    }
}
