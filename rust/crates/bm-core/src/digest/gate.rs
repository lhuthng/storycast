use super::parse::effective_kind;
use super::prompts::PreparedChapter;
use super::prompts::PreparedEvent;
use super::*;
/// Apply the answer's explicitly declared grammar fixes to one source event.
pub(crate) fn corrected_source(event: &PreparedEvent, fixes: &[Value]) -> String {
    let mut text = event.text.clone();
    for fix in fixes {
        let (Some(before), Some(after)) = (
            fix.get("before").and_then(Value::as_str),
            fix.get("after").and_then(Value::as_str),
        ) else {
            continue;
        };
        if !before.is_empty() {
            text = text.replace(before, after);
        }
    }
    // The prompt permits the three engine voice tags to replace a written
    retag_text(&text).unwrap_or(text)
}
fn normalized_source(text: &str) -> String {
    let mut out = text.to_string();
    for tag in ["[cười]", "[thở dài]", "[hắng giọng]"] {
        // The tag, *and the punctuation it swallowed when it took the sound's
        for punct in [",", ";", ":", ".", "…", "!", "?"] {
            out = out.replace(&format!("{tag}{punct}"), " ");
            out = out.replace(&format!("{punct}{tag}"), " ");
        }
        out = out.replace(tag, " ");
    }
    crate::util::squeeze_ws(&out)
}

/// Text with every *written* non-verbal sound, and every tag standing in for
pub(crate) fn source_without_written_sound(text: &str) -> String {
    let mut out = text.to_lowercase();
    for sound in [
        "[cười]",
        "[thở dài]",
        "[hắng giọng]",
        "thở dài một hơi rồi",
        "thở dài một tiếng",
        "thở dài một hơi",
        "thở dài",
        "ha ha ha",
        "ha ha",
        "haha",
        "hắc hắc",
        "hô hô",
        "haizz",
        "haiz",
        "khụ khụ khụ",
        "khụ khụ",
        "khụ",
    ] {
        for punct in [",", ";", ":", ".", "…", "!", "?"] {
            out = out.replace(&format!("{sound}{punct}"), " ");
        }
        out = out.replace(sound, " ");
    }
    crate::util::squeeze_ws(&out)
}

pub(crate) fn source_text_matches(expected: &str, actual: &[String]) -> bool {
    let expected = normalized_source(expected);
    let joined = actual
        .iter()
        .map(|s| normalized_source(s))
        .collect::<Vec<_>>();
    normalized_source(&joined.concat()) == expected
        || normalized_source(&joined.join(" ")) == expected
        || source_without_written_sound(&expected)
            == normalized_source(&joined.concat()).to_lowercase()
        || source_without_written_sound(&expected)
            == normalized_source(&joined.join(" ")).to_lowercase()
}

/// The tag whose written sound is still sitting in the text, as the corpus
fn leftover_written_sound(text: &str) -> Option<(&'static str, &'static str)> {
    let lower = text.to_lowercase();
    for (tag, spellings) in [
        ("[cười]", &["haha", "ha ha", "hắc hắc", "hô hô"][..]),
        // Longest spelling first, for the same reason the matcher does it: the
        (
            "[thở dài]",
            &[
                "thở dài một hơi rồi",
                "thở dài một tiếng",
                "thở dài một hơi",
                "thở dài",
                "haizz",
                "haiz",
            ][..],
        ),
        ("[hắng giọng]", &["khụ khụ", "khụ"][..]),
    ] {
        if !lower.contains(tag) {
            continue;
        }
        if let Some(spelling) = spellings.iter().find(|spelling| lower.contains(**spelling)) {
            return Some((tag, spelling));
        }
    }
    None
}

/// Explain a mismatch that is *only* about written non-verbal sound.
fn written_sound_hint(expected: &str, actual: &str) -> Option<String> {
    let (tag, literal) = leftover_written_sound(actual)?;
    if source_without_written_sound(expected) != source_without_written_sound(actual) {
        return None;
    }
    Some(format!(
        "text still contains the written sound {literal:?} that {tag} stands for; the tag goes \
         exactly where the sound was and its words are deleted — never both, and never moved to \
         the start of the line"
    ))
}

/// Check the source contract independently of the LLM's interpretation.
pub(crate) fn validate_source_alignment(
    data: &Value,
    prepared: &PreparedChapter,
    not_speech: &HashSet<String>,
) -> Result<()> {
    let segments = data
        .get("segments")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("digest script has no segments"))?;
    let fixes = data
        .get("fixes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let events: std::collections::HashMap<&str, &PreparedEvent> = prepared
        .events
        .iter()
        .map(|event| (event.id.as_str(), event))
        .collect();

    let mut by_id: std::collections::BTreeMap<&str, Vec<String>> =
        std::collections::BTreeMap::new();
    let mut speakers: std::collections::HashMap<&str, String> = std::collections::HashMap::new();
    let mut last_index = 0usize;
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (i, segment) in segments.iter().enumerate() {
        if crate::util::is_sound_item(segment) {
            continue;
        }
        let id = segment
            .get("source_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("segment {i}: missing source_id"))?;
        let event = events.get(id).ok_or_else(|| {
            anyhow::anyhow!(
                "segment {i}: unknown source_id {id:?} — use an id from the prepared chapter"
            )
        })?;
        let index = prepared
            .events
            .iter()
            .position(|candidate| candidate.id == event.id)
            .expect("event came from the prepared chapter");
        if index < last_index {
            anyhow::bail!("segment {i}: source order regressed at {id:?}");
        }
        last_index = index;
        seen.insert(id);
        let text = segment
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("segment {i}: missing text"))?;
        by_id.entry(id).or_default().push(text.to_string());

        let speaker = segment.get("speaker").and_then(Value::as_str).unwrap_or("");
        if let Some(previous) = speakers.insert(id, speaker.to_string()) {
            if previous != speaker {
                anyhow::bail!(
                    "source {id:?} is split across speakers {previous:?} and {speaker:?}; a sound or TTS split cannot change the speaker"
                );
            }
        }
        let kind = effective_kind(event, not_speech);
        // The thought marker is code-attached (`attach_fixed_speakers`), so a
        match (kind, segment.get("kind").and_then(Value::as_str)) {
            ("thought", Some("thought")) => {}
            ("thought", other) => anyhow::bail!(
                "source {id:?} is a thought but segment {i} carries kind {other:?} — a thought's \
                 segment must carry `\"kind\": \"thought\"`"
            ),
            (_, Some(marker)) => anyhow::bail!(
                "source {id:?} is {kind} but segment {i} carries kind {marker:?} — only a thought \
                 event's segment may carry `\"kind\": \"thought\"`"
            ),
            (_, None) => {}
        }
        match kind {
            "narration" if speaker != "Narrator" => anyhow::bail!(
                "source {id:?} is narration but segment {i} is assigned to {speaker:?}; narration must be Narrator"
            ),
            "dialogue" | "thought" if speaker == "Narrator" => anyhow::bail!(
                "source {id:?} is {kind} but segment {i} is assigned to Narrator"
            ),
            _ => {}
        }
        // A retracted span is narration now, so it is a narration segment and
        if matches!(kind, "dialogue" | "thought")
            && (text.contains('"') || text.contains('“') || text.contains('”'))
        {
            anyhow::bail!("source {id:?}: quote delimiters must not be merged into segment {i}");
        }
    }

    for event in &prepared.events {
        if !seen.contains(event.id.as_str()) {
            anyhow::bail!("source event {:?} was dropped", event.id);
        }
        let actual = by_id.get(event.id.as_str()).cloned().unwrap_or_default();
        let expected = corrected_source(event, &fixes);
        if !source_text_matches(&expected, &actual) {
            if let Some(hint) = written_sound_hint(&expected, &actual.concat()) {
                anyhow::bail!("source event {:?}: {hint}", event.id);
            }
            anyhow::bail!(
                "source event {:?} was changed, duplicated, or split out of order (expected {:?}, got {:?})",
                event.id,
                expected,
                actual
            );
        }
    }
    Ok(())
}
