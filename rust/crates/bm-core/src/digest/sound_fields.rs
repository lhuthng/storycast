use super::prompts::PreparedChapter;
use super::*;
/// Write a chapter's script where every consumer reads it.
///
/// One write site, so the worker's path and the operator's cannot land the same
/// artifact differently.
pub(crate) fn expand_sound_fields(segments: &[Value]) -> Result<Vec<Value>> {
    let mut out = Vec::with_capacity(segments.len());
    for (i, s) in segments.iter().enumerate() {
        let mut line = s.clone();
        let take = |key: &str, line: &mut Value| -> Option<String> {
            line.as_object_mut()
                .and_then(|o| o.remove(key))
                .map(|v| v.as_str().map(str::trim).unwrap_or("").to_string())
        };
        let fields = [
            ("sound_after", take("sound_after", &mut line)),
            ("stop_after", take("stop_after", &mut line)),
        ];
        for (key, v) in &fields {
            // An empty string is what a line looks like when nobody decided.
            // `none` is the token the prompt asks for; a blank is refused, so
            // the choice is made per line instead of defaulted away.
            if v.as_deref() == Some("") {
                anyhow::bail!(
                    "segment {i}: `{key}` is empty — write \"none\" when nothing fires at this \
                     seam. A blank value is a line nobody decided about"
                );
            }
        }
        out.push(line);
        // A sound and then its stop, in that order: the pair brackets the line
        // the model marked, and a stop can never precede its own start.
        for (key, v) in &fields {
            let Some(v) = v.as_deref().filter(|v| !v.eq_ignore_ascii_case("none")) else {
                continue;
            };
            out.push(if *key == "sound_after" {
                json!({ "sound": v })
            } else {
                json!({ "stop": v })
            });
        }
    }
    Ok(out)
}

/// Fill fields the staging answer carried forward by omission.
///
/// The prompt asks for `mood`/`scene`/`music` only where they change and for
/// `text` only where it changes, so a segment that omits one inherits the
/// previous segment's value. `text` falls back to the prepared event's own text
/// by `source_id` — the split/fix cases must still emit it, which the contract
/// says outright. This runs before the validators, so they see the same fully
/// populated segments an older, verboser answer would have produced and nothing
/// downstream has to know the model-facing shape got smaller.
pub(crate) fn carry_forward_fields(data: &mut Value, prepared: &PreparedChapter) {
    let source_text: std::collections::HashMap<&str, &str> = prepared
        .events
        .iter()
        .map(|e| (e.id.as_str(), e.text.as_str()))
        .collect();
    let Some(segments) = data.get_mut("segments").and_then(Value::as_array_mut) else {
        return;
    };
    let mut last: std::collections::HashMap<&'static str, Value> = std::collections::HashMap::new();
    for segment in segments.iter_mut() {
        if crate::util::is_sound_item(segment) {
            continue;
        }
        let Some(obj) = segment.as_object_mut() else {
            continue;
        };
        for key in ["mood", "scene", "music"] {
            if field_is_blank(obj, key) {
                if let Some(carried) = last.get(key) {
                    obj.insert(key.to_string(), carried.clone());
                }
            } else {
                last.insert(key, obj.get(key).cloned().unwrap_or(Value::Null));
            }
        }
        if field_is_blank(obj, "text") {
            if let Some(text) = obj
                .get("source_id")
                .and_then(Value::as_str)
                .and_then(|id| source_text.get(id))
            {
                obj.insert("text".into(), json!(*text));
            }
        }
    }

    // The *head* of the chapter, which has nothing to inherit. The prompt asks
    // for `music` where it changes, so a chapter whose first bed arrives at line
    // 12 legitimately omits the field at lines 1-11 — and the validator refuses
    // any blank once a single segment declares one, which is what `segment 0:
    // missing music` was: 2 of ch386's 15 attempts. An empty value already means
    // "no bed" to the mixer (`resolve_music`), so filling the blanks before the
    // first declaration only says out loud what the mix does anyway.
    //
    // Only when something *is* declared: a script with no `music` at all
    // predates the field and has to keep taking the legacy merge path rather
    // than become a chapter of explicit silence.
    let first_declared = segments.iter().position(|s| {
        !crate::util::is_sound_item(s)
            && !field_is_blank(s.as_object().unwrap_or(&serde_json::Map::new()), "music")
    });
    if let Some(first) = first_declared {
        for segment in segments.iter_mut().take(first) {
            if crate::util::is_sound_item(segment) {
                continue;
            }
            if let Some(obj) = segment.as_object_mut() {
                if field_is_blank(obj, "music") {
                    obj.insert("music".into(), json!("none"));
                }
            }
        }
    }
}

/// Whether a carried field is absent or empty, the two ways a model declines to
/// state it. An empty array counts, so a deliberate `["rain"]` is a value and a
/// bare `[]` is not mistaken for one.
pub(crate) fn field_is_blank(obj: &serde_json::Map<String, Value>, key: &str) -> bool {
    match obj.get(key) {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.trim().is_empty(),
        Some(Value::Array(a)) => a.is_empty(),
        _ => false,
    }
}

/// Phrases from rule 10's own sweep that are literal on the page in this genre.
///
/// Narrow on purpose: a hit here can fail a chapter, so a word that is usually a
/// metaphor does not belong on the list. `dao` alone is out for that reason
/// `dao phay` is in. Bare `chém` is out for the same reason: ch262's only hit
/// was the idiom "muốn chém muốn giết" (kill me if you want), no slash staged,
/// and the gate refused every correct answer. The compounds (`rút kiếm`,
/// `vung kiếm`) stay; they name an action, not a figure of speech.
const SOUND_CUES: [&str; 20] = [
    "phun ra",
    "máu tươi",
    "máu văng",
    "thổ huyết",
    "thái thịt",
    "phòng bếp",
    "vào bếp",
    "nấu đồ ăn",
    "nấu ăn",
    "rửa sạch",
    "vòi nước",
    "lật xem",
    "lật sách",
    "giở sách",
    "lật đến",
    "niệm chú",
    "bắn tên",
    "rút kiếm",
    "vung kiếm",
    "đâm",
];

/// Block probability after `failures` consecutive same-gap failures: 90%,
/// then -25% each time (0.90, 0.68, 0.51, 0.38...). Pure so the curve is
/// pinned without spending LLM calls; the caller accepts below a coin flip.
pub(crate) fn gap_block_p(failures: u32) -> f64 {
    0.9 * 0.75f64.powi(failures as i32)
}

/// Two sound-design answers that cannot be right, checked in that order.
///
/// Both are things the prompt says in as many words and the model does anyway,
/// and both are silent failures: the chapter merges, sounds fine at a glance,
/// and has no sound design where the prose staged one. Neither is a judgment
/// call, which is why they can be gated at all, a chapter that places three
/// sounds and misses a fourth is the model's business, and no word list can
/// second-guess it.
///
/// 1. A `loop`ed bed started and never stopped. The prompt calls this "the one
///    way to get a bed wrong": the clip plays once and stops dead. Measured on
///    ch9, a 25 s bed opened into a 130 s kitchen, then digital silence.
/// 2. A chapter that stages a sound and places none at all, the cue list from
///    rule 10's own last check, matched against the chapter text.
pub(crate) fn sound_design_gap(
    script: &Value,
    chapter_text: &str,
    pool: &crate::audio_pool::ClipPool,
) -> Option<String> {
    unclosed_beds(script, pool).or_else(|| silent_design(script, chapter_text, "chapter"))
}

/// Rule 1 alone: looping beds a script opened and never stopped, **in the order
/// they opened**.
///
/// Split out of [`sound_design_gap`] for the windowed digest, and the split is
/// load-bearing rather than tidiness. This rule is a fact about the *whole
/// chapter*: a bed started at the end of one part and stopped at the start of
/// the next is closed, and a check that ran per part would refuse a chapter
/// whose long scene is simply wider than one prompt. The repair it asks for goes
/// to the part that placed the surviving `sound` — the only repair that does not
/// re-stage prose nobody complained about.
pub(crate) fn unclosed_beds(script: &Value, pool: &crate::audio_pool::ClipPool) -> Option<String> {
    let open = open_beds(script, pool);
    if open.is_empty() {
        return None;
    }
    Some(format!(
        "{} is a looping bed started with no `stop_after` — it plays once and stops dead. \
         Close it on the line where the scene moves on",
        open.join(", ")
    ))
}

/// Rule 1's finding on its own: the looping beds still open when the script
/// ends, in the order they opened.
///
/// Separated from the sentence it becomes because a windowed digest needs the
/// **name**, not the complaint: the part to re-ask is the part that placed the
/// surviving `sound`, and only the name finds it.
pub(crate) fn open_beds(script: &Value, pool: &crate::audio_pool::ClipPool) -> Vec<String> {
    let Some(segments) = script.get("segments").and_then(|s| s.as_array()) else {
        return Vec::new();
    };
    let mut open: Vec<&str> = Vec::new();
    for item in segments {
        if let Some(sound) = item.get("sound").and_then(|v| v.as_str()) {
            if pool.get(sound).map(|e| e.looped).unwrap_or(false) {
                open.push(sound);
            }
        }
        if let Some(stop) = item.get("stop").and_then(|v| v.as_str()) {
            if let Some(i) = open.iter().position(|s| *s == stop) {
                open.remove(i);
            }
        }
    }
    open.into_iter().map(str::to_string).collect()
}

/// Rule 2 alone: the text stages sounds and this script places none.
///
/// A fact about one **part** of a chapter, which is why it is the half a
/// windowed digest checks window by window: `chapter_text` here is the text that
/// window's staging answer was actually answering, so the complaint it produces
/// is about prose the model saw rather than prose it was never shown. `what`
/// names the subject in the message — `chapter`, or `part` for a window — the
/// same way `classify_fetch` is told the noun it is describing.
pub(crate) fn silent_design(script: &Value, chapter_text: &str, what: &str) -> Option<String> {
    let segments = script.get("segments").and_then(|s| s.as_array())?;
    let placed = segments
        .iter()
        .filter(|i| crate::util::is_sound_item(i))
        .count();
    if placed > 0 {
        return None;
    }
    let hit: Vec<&str> = SOUND_CUES
        .iter()
        .copied()
        .filter(|c| chapter_text.contains(c))
        .collect();
    if hit.is_empty() {
        return None;
    }
    Some(format!(
        "this {what} stages sounds ({}) and this {what}'s script places none — the last check in \
         rule 10 was skipped. Place a sound for each moment the text stages",
        hit.join(", ")
    ))
}
