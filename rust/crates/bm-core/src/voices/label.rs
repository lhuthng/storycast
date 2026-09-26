use bm_proto::VoiceInfo;

use super::catalogue::{key_for_name, EngineRoster};
use super::consts::{policy_for, CONTENT_LANGUAGE, PRESET_META};

fn preset_meta(name: &str) -> Option<&'static (&'static str, &'static str, &'static str)> {
    PRESET_META.iter().find(|(n, _, _)| *n == name)
}

/// Gender from an SDK label field. Female is tested first: `"female"`
/// contains `"male"`.
///
/// The Vietnamese check is the diacritic form `nữ` only — matching bare `nu`
/// would make `"neutral"` report as female.
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
///
/// Positional parsing is what makes this work at all: `"Nam"` means *male* in
/// the gender slot and *South* in the accent slot. This function only ever
/// sees the accent slot.
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
///
/// Enrolled clones carry a bare label with no separator, which is exactly how
/// they are distinguished from presets.
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
///
/// The sidecar composes `/voices` labels as `"<name> — <description>"` for any
/// voice that has a description, and as the bare name for one that does not —
/// so a preset arrives as `"Adam — Nam · Nam · Giọng đọc tự nhiên"` and a
/// hand-enrolled clone as `"Suneo"`. Anything comparing a store entry against a
/// *name* has to split first, and has to split it the same way [`voice_from_label`]
/// does, or a voice the picker shows as a preset is declared in one place and
/// undeclared in another.
///
/// Public because there is now a second caller: the provisioning check that asks
/// whether every voice in a box's store is declared somewhere.
pub fn voice_name(label: &str) -> String {
    split_label(label).0
}

/// One voice from an SDK `(label, id)` pair.
///
/// A bare label (`label == id`) is an enrolled clone: no gender or accent is
/// claimed, and it is flagged so the picker can distinguish cast members the
/// operator added by hand from the shipped presets.
fn voice_from_label(label: &str, id: &str, allowed: &[String], engine: &str) -> VoiceInfo {
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
    // `roster add` gives it one (stage 3), so the field stays empty rather than
    // inventing a slug that the next rename would silently invalidate.
    let key = key_for_name(engine, &name).unwrap_or_default();
    VoiceInfo {
        key,
        // `allowed.is_empty()` is "no restriction", the same reading
        // `VoicePolicy::violations` and `offline_voices` use. Omitting it here
        // would make the sidecar-backed roster reject every preset the moment
        // the policy became permissive, while the offline roster accepted them.
        allowed: enrolled || allowed.is_empty() || allowed.contains(&name),
        enrolled,
        name,
        gender: gender.to_string(),
        accent: accent.to_string(),
        language: CONTENT_LANGUAGE.to_string(),
        style,
    }
}

/// Turn the sidecar's `(label, id)` roster into voice infos.
///
/// `allowed` is passed in rather than resolved here, because the effective
/// policy depends on the operator's roster and this function has no path to it.
pub fn voices_from_labels(
    engine: &str,
    labels: &[(String, String)],
    allowed: &[String],
) -> Vec<VoiceInfo> {
    labels
        .iter()
        .map(|(label, id)| voice_from_label(label, id, allowed, engine))
        .collect()
}

/// The bundled roster: policy pools plus the metadata table above.
///
/// Used when the TTS sidecar is unreachable, so the picker still shows the
/// whole cast with whatever is known about each voice.
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
                // guarantee that no longer exists.
                None => ("unknown", ""),
            };
            out.push(VoiceInfo {
                key: key_for_name(engine, name).unwrap_or_default(),
                name: name.clone(),
                gender: gender.to_string(),
                accent: accent.to_string(),
                language: CONTENT_LANGUAGE.to_string(),
                style: style.to_string(),
                enrolled: false,
                allowed: policy.allowed.is_empty() || policy.allowed.iter().any(|a| a == name),
            });
        }
    }
    out
}

/// One line describing the active accent policy, for the picker header.
///
/// Derived from the roster rather than restated. This used to hardcode
/// "Central/South presets only (Northern excluded)" — the same regional
/// preference in a fourth place, phrased as though it were a property of the
/// engine rather than somebody's choice.
pub fn policy_note(roster: &EngineRoster) -> String {
    let pol = &roster.policy;
    if pol.allowed_accents.is_empty() && pol.excluded_accents.is_empty() {
        return format!(
            "accent policy: none — all {} declared presets are assignable",
            roster.voices.len()
        );
    }
    let allowed = roster
        .voices
        .iter()
        .filter(|v| roster.accent_allowed(&v.accent))
        .count();
    let mut parts: Vec<String> = Vec::new();
    if !pol.allowed_accents.is_empty() {
        parts.push(format!("only {}", pol.allowed_accents.join("/")));
    }
    if !pol.excluded_accents.is_empty() {
        parts.push(format!("excluding {}", pol.excluded_accents.join("/")));
    }
    format!(
        "accent policy: {} — {allowed} of {} declared presets assignable; enrolled clones always pass",
        parts.join(", "),
        roster.voices.len()
    )
}

/// Operator-enrolled clones, read from `voices.json` (`name -> refs/clip.wav`).
///
/// These are part of the cast whether or not the sidecar lists them, so the
/// picker shows them even when the sidecar is down. The `_note` key is
/// documentation, not a voice.
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
            // moves this file to `.bm/voices.json` with a declared key per clone.
            key: String::new(),
            name: name.clone(),
            gender: "unknown".into(),
            accent: "unknown".into(),
            language: CONTENT_LANGUAGE.into(),
            style: "enrolled clone".into(),
            enrolled: true,
            allowed: true, // vetted when it was enrolled
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voices::catalogue::RosterFile;
    use crate::voices::consts::{gemini_policy, vieneu_policy};

    #[test]
    fn label_fields_are_positional_so_nam_is_male_then_south() {
        // "Nam" appears twice with two different meanings; only position
        // disambiguates them.
        let v = voice_from_label(
            "Thái Sơn — Nam · Nam · Kể chuyện",
            "thai_son",
            &vieneu_policy().allowed,
            "vieneu",
        );
        assert_eq!(v.name, "Thái Sơn");
        assert_eq!(v.gender, "male");
        assert_eq!(v.accent, "South");
        assert_eq!(v.style, "Kể chuyện");
        assert!(!v.enrolled);
        assert!(v.allowed, "Thái Sơn is a Central/South preset");
        assert_eq!(v.language, "vi-VN");
    }

    #[test]
    fn female_beats_the_male_substring_in_a_label_field() {
        let v = voice_from_label(
            "Thục Đoan — Nữ · Trung · kể chuyện",
            "thuc_doan",
            &vieneu_policy().allowed,
            "vieneu",
        );
        assert_eq!(v.gender, "female");
        assert_eq!(v.accent, "Central");
    }

    #[test]
    fn a_bare_label_is_an_enrolled_clone_that_passes_the_policy() {
        let v = voice_from_label("Suneo", "Suneo", &vieneu_policy().allowed, "vieneu");
        assert_eq!(v.name, "Suneo");
        assert!(v.enrolled);
        assert!(v.allowed, "clones are vetted at enrolment");
        assert_eq!(v.gender, "unknown");
    }

    #[test]
    fn excluding_an_accent_is_what_makes_a_northern_preset_unassignable() {
        // The shipped policy admits every preset; the exclusion is the
        // operator's. This is the path that turns
        // `"excluded_accents": ["Northern"]` into an unassignable voice.
        let mut roster = RosterFile::catalogue()
            .engine("vieneu")
            .expect("vieneu is declared")
            .clone();
        roster.policy.excluded_accents = vec!["Northern".to_string()];

        let p = roster.to_policy("vieneu");
        assert!(
            !p.allowed.contains(&"Minh Đức".to_string()),
            "Northern, excluded"
        );
        assert!(p.allowed.contains(&"Đức Trí".to_string()), "South, kept");
        assert!(
            p.allowed.contains(&"Quang Sơn".to_string()),
            "Central, kept"
        );
        assert_eq!(p.allowed.len(), 10, "23 declared minus the 13 Northern");

        // The same rule, applied to a live sidecar label.
        let v = voice_from_label(
            "Xuân Vĩnh — Nam · Bắc · đọc truyện",
            "xuan_vinh",
            &p.allowed,
            "vieneu",
        );
        assert_eq!(v.accent, "Northern");
        assert!(!v.allowed, "the operator's roster excludes Northern");
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
        assert!(
            v.iter().all(|x| x.allowed),
            "the shipped policy admits everything"
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
        let v = voice_from_label(
            "Puck — Neutral · unknown · breezy",
            "puck",
            &gemini_policy().allowed,
            "gemini",
        );
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
        // rather than a derived slug that the next rename would invalidate.
        let clone = voice_from_label("Suneo", "Suneo", &vieneu_policy().allowed, "vieneu");
        assert_eq!(clone.key, "");
        assert!(clone.enrolled);
    }
}
