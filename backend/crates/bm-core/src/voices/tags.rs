//! Tags a voice preset and a character share.

/// Tags that cannot describe the same voice. Gender is not one of them: a voice
/// may be offered for both male and female characters.
pub const CONFLICTS: [(&str, &str); 4] = [
    ("child", "adult"),
    ("child", "old"),
    ("young", "old"),
    ("strong", "weak"),
];

/// One tag in its canonical spelling: trimmed, lowercase, spaces folded to `-`.
pub fn normalize(raw: &str) -> Option<String> {
    let tag = raw
        .trim()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("-");
    if tag.is_empty() {
        return None;
    }
    Some(match tag.as_str() {
        "elderly" => "old".into(),
        "powerful" => "strong".into(),
        _ => tag,
    })
}

/// Normalized, deduped tags in first-seen order.
pub fn normalized<I, S>(tags: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out = Vec::new();
    for tag in tags {
        let Some(tag) = normalize(tag.as_ref()) else {
            continue;
        };
        if !out.contains(&tag) {
            out.push(tag);
        }
    }
    out
}

/// The conflicting pair in `tags`, if there is one.
pub fn conflict(tags: &[String]) -> Option<(&'static str, &'static str)> {
    CONFLICTS
        .iter()
        .copied()
        .find(|(a, b)| tags.iter().any(|t| t == a) && tags.iter().any(|t| t == b))
}

/// At least one shared tag, and no conflicting pair.
pub fn compatible(voice: &[String], character: &[String]) -> bool {
    if !voice.iter().any(|t| character.contains(t)) {
        return false;
    }
    !CONFLICTS.iter().any(|(a, b)| {
        (voice.iter().any(|t| t == a) && character.iter().any(|t| t == b))
            || (voice.iter().any(|t| t == b) && character.iter().any(|t| t == a))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_normalize_and_dedupe() {
        assert_eq!(
            normalized([" Young ", "young", "Adult Male"]),
            ["young", "adult-male"]
        );
        assert_eq!(normalized(["elderly", "powerful"]), ["old", "strong"]);
        assert!(normalized(["", "   "]).is_empty());
    }

    #[test]
    fn only_real_opposites_conflict() {
        let tags = normalized(["male", "young", "strong"]);
        assert_eq!(conflict(&tags), None);
        // A voice may serve both genders.
        assert_eq!(conflict(&normalized(["male", "female"])), None);
        assert_eq!(
            conflict(&normalized(["young", "old"])),
            Some(("young", "old"))
        );
        // Young adult is a person, not a contradiction.
        assert_eq!(conflict(&normalized(["young", "adult"])), None);
    }

    #[test]
    fn compatibility_needs_a_shared_tag_and_no_conflict() {
        let voice = normalized(["male", "adult", "strong"]);
        assert!(compatible(&voice, &normalized(["male", "adult"])));
        // `adult` is shared, and a male voice may answer a female character.
        assert!(compatible(&voice, &normalized(["female", "adult"])));
        assert!(!compatible(&voice, &normalized(["male", "weak"])));
        assert!(!compatible(&voice, &normalized(["female"])));
        assert!(!compatible(&voice, &normalized(["calm"])));
    }
}
