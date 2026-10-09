use super::prompts::PreparedChapter;
use super::*;
/// Write a chapter's script where every consumer reads it.
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
            if v.as_deref() == Some("") {
                anyhow::bail!(
                    "segment {i}: `{key}` is empty — write \"none\" when nothing fires at this \
                     seam. A blank value is a line nobody decided about"
                );
            }
        }
        out.push(line);
        // A sound and then its stop, in that order: the pair brackets the line
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
pub(crate) fn field_is_blank(obj: &serde_json::Map<String, Value>, key: &str) -> bool {
    match obj.get(key) {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.trim().is_empty(),
        Some(Value::Array(a)) => a.is_empty(),
        _ => false,
    }
}
/// Two sound-design answers that cannot be right, checked in that order: a
/// `loop`ed bed started and never stopped, then a chapter that stages a sound
/// and places none.
pub(crate) fn sound_design_gap(
    script: &Value,
    chapter_text: &str,
    pool: &crate::audio_pool::ClipPool,
    cues: &[String],
) -> Option<String> {
    unclosed_beds(script, pool).or_else(|| silent_design(script, chapter_text, "chapter", cues))
}

/// Rule 1 alone: looping beds a script opened and never stopped, **in the order
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

/// Rule 2 alone: the text stages sounds and this script places none. `cues` is
/// the language's own sweep of rule 10 (the adapter's `sound-cues.json`), so an
/// English chapter is checked against English prose and a missing list simply
/// turns the gate off rather than firing on the wrong words.
pub(crate) fn silent_design(
    script: &Value,
    chapter_text: &str,
    what: &str,
    cues: &[String],
) -> Option<String> {
    let segments = script.get("segments").and_then(|s| s.as_array())?;
    let placed = segments
        .iter()
        .filter(|i| crate::util::is_sound_item(i))
        .count();
    if placed > 0 {
        return None;
    }
    let hit: Vec<&str> = cues
        .iter()
        .map(String::as_str)
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
