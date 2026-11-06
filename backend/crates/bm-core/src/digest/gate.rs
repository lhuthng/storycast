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

/// The words a span says, in order. Whitespace and punctuation are not words,
/// so a re-punctuated line is still the source's line, and a split that lands
/// mid-word still spells it.
fn words(text: &str) -> Vec<&str> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect()
}

/// How a segment's words stand against the source event's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Align {
    /// The source's words, in the source's spelling.
    Same,
    /// The same words, with a tone mark or a capital different: (the source's
    /// spelling, the answer's) for each word that slipped.
    Slip(Vec<(String, String)>),
    /// Different words, or the same words out of order: a change of content.
    Changed(String),
}

/// The first place two word lists part, with a few words either side so the
/// log line names the difference before it is cut.
fn drift(expected: &[&str], got: &[&str]) -> String {
    const SIDE: usize = 7;
    let at = expected
        .iter()
        .zip(got)
        .position(|(a, b)| a != b)
        .unwrap_or_else(|| expected.len().min(got.len()));
    let window = |ws: &[&str]| {
        let from = at.saturating_sub(SIDE);
        let to = (at + SIDE).min(ws.len());
        let mut text = ws[from..to].join(" ");
        if from > 0 {
            text.insert(0, '…');
        }
        if to < ws.len() {
            text.push('…');
        }
        text
    };
    format!(
        "the source reads {:?} where the answer reads {:?}",
        window(expected),
        window(got)
    )
}

fn alignment(expected: &[&str], got: &[&str]) -> Align {
    if got == expected {
        return Align::Same;
    }
    if got.len() == expected.len() {
        let mut slips: Vec<(String, String)> = Vec::new();
        for (want, have) in expected.iter().zip(got) {
            if want == have {
                continue;
            }
            if crate::util::fold(want) != crate::util::fold(have) {
                return Align::Changed(drift(expected, got));
            }
            slips.push((want.to_string(), have.to_string()));
        }
        if !slips.is_empty() {
            return Align::Slip(slips);
        }
    }
    Align::Changed(drift(expected, got))
}

/// The source gate's text rule: the same words, in order. A tone mark that
/// slipped is reported rather than refused — it moves one vowel and says
/// nothing different — while every other difference is the content the gate
/// exists to hold.
pub(crate) fn source_alignment(expected: &str, actual: &[String]) -> Align {
    let expected = words(expected);
    // The two spellings of a split: halves that touch, and halves with a space
    // between them. The gate judges whichever answers better.
    let touching_text = actual.concat();
    let spaced_text = actual.join(" ");
    let touching = words(&touching_text);
    let spaced = words(&spaced_text);
    [
        alignment(&expected, &touching),
        alignment(&expected, &spaced),
    ]
    .into_iter()
    .min_by_key(|seen| match seen {
        Align::Same => 0,
        Align::Slip(_) => 1,
        Align::Changed(_) => 2,
    })
    .expect("two spellings of the same split")
}

/// Whether a segment says the source's words in the source's spelling: a
/// sound split's halves must reproduce a staged line exactly.
pub(crate) fn source_text_matches(expected: &str, actual: &[String]) -> bool {
    source_alignment(expected, actual) == Align::Same
}

/// A word the answer spelt with another tone mark, and the source's spelling.
#[derive(Debug)]
pub(crate) struct Spelling {
    pub id: String,
    pub source: String,
    pub answer: String,
}

/// The whole-word spans of `word` in `text`.
fn word_spans(text: &str, word: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    if word.is_empty() {
        return spans;
    }
    let mut at = 0usize;
    while let Some(found) = text[at..].find(word) {
        let start = at + found;
        let end = start + word.len();
        let left = text[..start].chars().next_back();
        let right = text[end..].chars().next();
        if left.is_none_or(|c| !c.is_alphanumeric()) && right.is_none_or(|c| !c.is_alphanumeric()) {
            spans.push((start, end));
        }
        at = end;
    }
    spans
}

/// `text` with its one whole-word `from` said as `to`.
fn spelled_as_source(text: &str, from: &str, to: &str) -> Option<String> {
    let mut spans = word_spans(text, from);
    if spans.len() != 1 {
        return None;
    }
    let (start, end) = spans.pop()?;
    Some(format!("{}{to}{}", &text[..start], &text[end..]))
}

/// Put the chapter's own spelling back where the answer slipped a tone mark.
/// The prompt already asks for character-for-character copy, and the text the
/// operator reviewed is the one the script speaks; a word the event says more
/// than once is left as written, because which of them slipped is unknowable.
pub(crate) fn apply_source_spelling(data: &mut Value, spellings: &[Spelling]) {
    if spellings.is_empty() {
        return;
    }
    let Some(segments) = data.get_mut("segments").and_then(Value::as_array_mut) else {
        return;
    };
    for slip in spellings {
        let carried: Vec<&mut Value> = segments
            .iter_mut()
            .filter(|s| s.get("source_id").and_then(Value::as_str) == Some(slip.id.as_str()))
            .collect();
        let seen: usize = carried
            .iter()
            .filter_map(|s| s.get("text").and_then(Value::as_str))
            .map(|text| word_spans(text, &slip.answer).len())
            .sum();
        if seen != 1 {
            eprintln!(
                "staging: source event {:?} writes {:?} where the chapter says {:?}, in {seen} \
                 place(s); left as written",
                slip.id, slip.answer, slip.source
            );
            continue;
        }
        for segment in carried {
            let Some(text) = segment.get("text").and_then(Value::as_str) else {
                continue;
            };
            if let Some(fixed) = spelled_as_source(text, &slip.answer, &slip.source) {
                eprintln!(
                    "staging: source event {:?} wrote {:?} for the chapter's {:?}; the chapter's \
                     spelling stands",
                    slip.id, slip.answer, slip.source
                );
                segment["text"] = json!(fixed);
            }
        }
    }
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
) -> Result<Vec<Spelling>> {
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

    let mut spellings = Vec::new();
    for event in &prepared.events {
        if !seen.contains(event.id.as_str()) {
            anyhow::bail!("source event {:?} was dropped", event.id);
        }
        let actual = by_id.get(event.id.as_str()).cloned().unwrap_or_default();
        let expected = corrected_source(event, &fixes);
        match source_alignment(&expected, &actual) {
            Align::Same => {}
            Align::Slip(slips) => {
                spellings.extend(slips.into_iter().map(|(source, answer)| Spelling {
                    id: event.id.clone(),
                    source,
                    answer,
                }))
            }
            Align::Changed(why) => anyhow::bail!(
                "source event {:?} was changed, duplicated, or split out of order: {why}",
                event.id
            ),
        }
    }
    Ok(spellings)
}
