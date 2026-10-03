use bm_proto::VoiceInfo;

use super::catalogue::key_for_name;
use super::consts::{policy_for, CONTENT_LANGUAGE, PRESET_META};

fn preset_meta(name: &str) -> Option<&'static (&'static str, &'static str, &'static str)> {
    PRESET_META.iter().find(|(n, _, _)| *n == name)
}

/// Gender from an SDK label field. Female is tested first: `"female"`
fn gender_of(field: &str) -> &'static str {
    let f = field.to_lowercase();
    if f.contains("female") || f.contains("nữ") {
        "female"
    } else if f.contains("male") || f.contains("nam") {
        "male"
    } else if f.contains("neutral") || f.contains("trung tính") {
        "neutral"
    } else {
        "unknown"
    }
}

/// Accent from an SDK label field.
fn accent_of(field: &str) -> &'static str {
    let f = field.to_lowercase();
    if f.contains("bắc") || f.contains("bac") {
        "Northern"
    } else if f.contains("trung") {
        "Central"
    } else if f.contains("nam") {
        "South"
    } else {
        "unknown"
    }
}

/// Split `"Thái Sơn — Nam · Trung · Kể chuyện"` into its name and fields.
fn split_label(label: &str) -> (String, Vec<String>) {
    for sep in ['—', '–'] {
        if let Some((name, rest)) = label.split_once(sep) {
            let fields = rest
                .split('·')
                .map(|f| f.trim().to_string())
                .filter(|f| !f.is_empty())
                .collect();
            return (name.trim().to_string(), fields);
        }
    }
    // ASCII fallback for labels that use a plain hyphen surrounded by spaces.
    if let Some((name, rest)) = label.split_once(" - ") {
        let fields = rest
            .split('·')
            .map(|f| f.trim().to_string())
            .filter(|f| !f.is_empty())
            .collect();
        return (name.trim().to_string(), fields);
    }
    (label.trim().to_string(), Vec::new())
}

/// The voice's own name, out of a roster label.
pub fn voice_name(label: &str) -> String {
    split_label(label).0
}

/// One voice from an SDK `(label, id)` pair.
fn voice_from_label(label: &str, id: &str, engine: &str) -> VoiceInfo {
    let (name, fields) = split_label(label);
    let name = if name.is_empty() {
        id.to_string()
    } else {
        name
    };
    let enrolled = label == id;
    let gender = fields.first().map(|f| gender_of(f)).unwrap_or("unknown");
    let accent = fields.get(1).map(|f| accent_of(f)).unwrap_or("unknown");
    let style = if fields.len() > 2 {
        fields[2..].join(" · ")
    } else {
        String::new()
    };
    // A preset's key comes from the catalogue. An enrolled clone has none until
    let key = key_for_name(engine, &name).unwrap_or_default();
    VoiceInfo {
        key,
        enrolled,
        name,
        gender: gender.to_string(),
        accent: accent.to_string(),
        language: CONTENT_LANGUAGE.to_string(),
        style,
        // Labels come from the SDK, which knows nothing about the sample
        pool_tags: Vec::new(),
    }
}

/// Turn the sidecar's `(label, id)` roster into voice infos.
pub fn voices_from_labels(engine: &str, labels: &[(String, String)]) -> Vec<VoiceInfo> {
    labels
        .iter()
        .map(|(label, id)| voice_from_label(label, id, engine))
        .collect()
}

/// The bundled roster: policy pools plus the metadata table above.
pub fn offline_voices(engine: &str) -> Vec<VoiceInfo> {
    let policy = policy_for(engine);
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
            let (accent, style) = match preset_meta(name) {
                Some((_, accent, style)) => (*accent, *style),
                // Undeclared: say so rather than appealing to a policy
                None => ("unknown", ""),
            };
            out.push(VoiceInfo {
                key: key_for_name(engine, name).unwrap_or_default(),
                name: name.clone(),
                gender: gender.to_string(),
                accent: accent.to_string(),
                language: CONTENT_LANGUAGE.to_string(),
                style: style.to_string(),
                pool_tags: Vec::new(),
                enrolled: false,
            });
        }
    }
    out
}

/// Operator-enrolled clones, read from `voices.json` (`name -> refs/clip.wav`).
pub fn enrolled_voices(path: &std::path::Path) -> Vec<VoiceInfo> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    let Some(obj) = doc.as_object() else {
        return Vec::new();
    };
    obj.keys()
        .filter(|k| !k.starts_with('_'))
        .map(|name| VoiceInfo {
            // No key: `voices.json` names clones but does not key them. Stage 3
            key: String::new(),
            name: name.clone(),
            gender: "unknown".into(),
            accent: "unknown".into(),
            language: CONTENT_LANGUAGE.into(),
            style: "enrolled clone".into(),
            // Enrolled, but not necessarily pooled: `voices.json` does not say
            pool_tags: Vec::new(),
            enrolled: true,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_fields_are_positional_so_nam_is_male_then_south() {
        // "Nam" appears twice with two different meanings; only position
        let v = voice_from_label("Thái Sơn — Nam · Nam · Kể chuyện", "thai_son", "vieneu");
        assert_eq!(v.name, "Thái Sơn");
        assert_eq!(v.gender, "male");
        assert_eq!(v.accent, "South");
        assert_eq!(v.style, "Kể chuyện");
        assert!(!v.enrolled);
        assert_eq!(v.language, "vi-VN");
    }

    #[test]
    fn female_beats_the_male_substring_in_a_label_field() {
        let v = voice_from_label("Thục Đoan — Nữ · Trung · kể chuyện", "thuc_doan", "vieneu");
        assert_eq!(v.gender, "female");
        assert_eq!(v.accent, "Central");
    }

    #[test]
    fn a_bare_label_is_an_enrolled_clone() {
        let v = voice_from_label("Suneo", "Suneo", "vieneu");
        assert_eq!(v.name, "Suneo");
        assert!(v.enrolled);
        assert_eq!(v.gender, "unknown");
    }

    #[test]
    fn offline_roster_lists_every_declared_preset_exactly_once() {
        let v = offline_voices("vieneu");
        // The neutral pool aliases the male pool for VieNeu: no duplicates.
        assert_eq!(
            v.len(),
            23,
            "{:?}",
            v.iter().map(|x| &x.name).collect::<Vec<_>>()
        );
        assert_eq!(v.iter().filter(|x| x.gender == "male").count(), 12);
        assert_eq!(v.iter().filter(|x| x.gender == "female").count(), 11);
        // Accents are per-voice and real, not a policy guarantee.
        assert_eq!(
            v.iter().find(|x| x.name == "Quang Sơn").unwrap().accent,
            "Central"
        );
        assert_eq!(
            v.iter().find(|x| x.name == "Minh Đức").unwrap().accent,
            "Northern"
        );
        assert_eq!(v.iter().find(|x| x.name == "Adam").unwrap().accent, "South");
        assert_eq!(
            v.iter().find(|x| x.name == "Adam").unwrap().style,
            "tự nhiên"
        );
    }

    #[test]
    fn neutral_is_not_misread_as_female() {
        // "neutral" contains "nu"; only the diacritic form may mean female.
        let v = voice_from_label("Puck — Neutral · unknown · breezy", "puck", "gemini");
        assert_eq!(v.gender, "neutral");
    }

    #[test]
    fn catalogue_voices_carry_their_key_and_undeclared_ones_carry_none() {
        let v = offline_voices("vieneu");
        assert!(
            v.iter().all(|x| !x.key.is_empty()),
            "every preset is catalogued"
        );
        assert_eq!(
            v.iter().find(|x| x.name == "Đức Trí").unwrap().key,
            "duc-tri"
        );
        // A label the catalogue does not declare (an enrolled clone) gets no key
        let clone = voice_from_label("Suneo", "Suneo", "vieneu");
        assert_eq!(clone.key, "");
        assert!(clone.enrolled);
    }
}
