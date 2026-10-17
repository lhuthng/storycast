use super::parse::effective_kind;
use super::prompts::PreparedChapter;
use super::prompts::PreparedEvent;
use super::*;

/// The source event's own text with the answer's declared grammar fixes applied.
///
/// The gate rewrites the *source* and nothing else. Which words stand for a
/// non-verbal sound is a judgement about meaning, and the code does not get to
/// make one: a segment says the source's words, and the answer *declares* the
/// sound it wants spoken instead (see `validate_nonverbal`).
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
    text
}

/// Whether the segment's own words are the source event's words, in order. The
/// two spellings of a split — the halves touching, or one space between them —
/// are the same words; whitespace is not a change.
pub(crate) fn source_text_matches(expected: &str, actual: &[String]) -> bool {
    let expected = crate::util::squeeze_ws(expected);
    crate::util::squeeze_ws(&actual.concat()) == expected
        || crate::util::squeeze_ws(&actual.join(" ")) == expected
}

/// The non-verbal sound a segment declares, checked against the only things the
/// code can know: the engine's own keys, and the one stretch of the line the
/// sound stands for. Whether those words really *are* that sound is a judgement
/// about meaning — the answer's to make, not this function's.
fn validate_nonverbal(
    segment: &Value,
    i: usize,
    id: &str,
    kind: &str,
    text: &str,
    engine: &str,
) -> Result<()> {
    let Some(sound) = segment.get("nonverbal") else {
        return Ok(());
    };
    let decls = crate::voices::nonverbals(engine);
    let key = sound.get("key").and_then(Value::as_str).unwrap_or("");
    if !decls.iter().any(|n| n.key == key) {
        let allowed = if decls.is_empty() {
            "this engine voices no non-verbal sounds".to_string()
        } else {
            format!(
                "key must be one of {}",
                decls.iter().map(|n| n.key).collect::<Vec<_>>().join("/")
            )
        };
        anyhow::bail!("segment {i}: {key:?} is not a non-verbal key — {allowed}");
    }
    if kind == "narration" {
        anyhow::bail!(
            "source {id:?} is narration: the narrator reads the line as written, so it declares no \
             non-verbal sound"
        );
    }
    let replaces = sound.get("replaces").and_then(Value::as_str).unwrap_or("");
    if replaces.trim().is_empty() {
        anyhow::bail!(
            "segment {i}: `nonverbal.replaces` is empty — name the words the sound stands for"
        );
    }
    match text.matches(replaces).count() {
        1 => Ok(()),
        n => anyhow::bail!(
            "segment {i}: `nonverbal.replaces` {replaces:?} appears {n} times in the text — it must \
             be the one stretch the sound stands for"
        ),
    }
}

/// Check the source contract independently of the LLM's interpretation.
pub(crate) fn validate_source_alignment(
    data: &Value,
    prepared: &PreparedChapter,
    not_speech: &HashSet<String>,
    engine: &str,
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
        validate_nonverbal(segment, i, id, kind, text, engine)?;
    }

    for event in &prepared.events {
        if !seen.contains(event.id.as_str()) {
            anyhow::bail!("source event {:?} was dropped", event.id);
        }
        let actual = by_id.get(event.id.as_str()).cloned().unwrap_or_default();
        let expected = corrected_source(event, &fixes);
        if !source_text_matches(&expected, &actual) {
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
