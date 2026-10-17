//! The chapter-level sound pass: where the chapter is set, what plays under it,
//! and which spot sounds fire — decided once with the whole chapter in view
//! rather than window by window. Text, speakers and mood are staging's.

use super::attribution::replace_or_miss;
use super::attribution::warn_missing_sections;
use super::gate::source_text_matches;
use super::json::parse_json_repaired;
use super::json::strip_fences;
use super::manual::Vocabulary;
use super::prompts::load_map;
use super::prompts::PreparedChapter;
use super::sound_fields::carry_forward_fields;
use super::sound_fields::expand_sound_fields;
use super::sound_fields::seam_value;
use super::sound_fields::silent_design;
use super::sound_fields::unclosed_beds;
use super::*;

/// The staged lines as the sound answer's world: one entry per speakable
/// segment, the 1-based `id` the answer keys on.
fn sound_view(segments: &[Value]) -> Vec<Value> {
    let mut lines = Vec::new();
    let mut id = 0usize;
    for segment in segments {
        if crate::util::is_sound_item(segment) {
            continue;
        }
        id += 1;
        lines.push(json!({
            "id": id,
            "speaker": segment.get("speaker").cloned().unwrap_or(json!("")),
            "text": segment.get("text").cloned().unwrap_or(json!("")),
        }));
    }
    lines
}

/// The pack's four sound vocabularies, as `sound.txt` renders them.
struct SoundVocab {
    palette: String,
    scene_words: String,
    effects: String,
    injects: String,
}

fn sound_vocab(layout: &Layout) -> Result<SoundVocab> {
    let map = load_map(layout)?;
    let pool = crate::audio_pool::load_pool(&layout.assets().join("effect-pool.json"));
    Ok(SoundVocab {
        palette: crate::ambience::palette_prompt(&map),
        scene_words: crate::ambience::scene_prompt(&map),
        effects: crate::ambience::effect_tags(&pool).join(", "),
        injects: crate::ambience::inject_prompt(&crate::audio_pool::load_pool(
            &layout.assets().join("inject-pool.json"),
        )),
    })
}

/// Substitute the vocabularies a `sound.txt` may render, naming any it lost.
fn render_vocab(template: &str, vocab: &SoundVocab) -> (String, Vec<String>) {
    let mut body = template.to_string();
    let mut missed: Vec<String> = Vec::new();
    for (needle, replacement) in [
        ("{music_palette}", vocab.palette.as_str()),
        ("{scene_words}", vocab.scene_words.as_str()),
        ("{effect_tags}", vocab.effects.as_str()),
        ("{inject_sounds}", vocab.injects.as_str()),
    ] {
        replace_or_miss(&mut body, needle, replacement, &mut missed);
    }
    (body, missed)
}

/// Build the sound pass's prompt from the adapter's `sound.txt`: the pack's
/// vocabularies, the chapter's staged lines, and the contract the answer must
/// satisfy. The rules live in the template, so a language words its own sound
/// design without a Rust change.
pub(crate) fn build_sound_prompt(
    layout: &Layout,
    context: &Value,
    segments: &[Value],
) -> Result<String> {
    let path = layout.sound_prompt();
    let template = std::fs::read_to_string(&path)
        .with_context(|| format!("reading prompt template {}", path.display()))?;
    let (mut body, mut missed) = render_vocab(&template, &sound_vocab(layout)?);
    let atmosphere = context
        .get("atmosphere")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let atmosphere = if atmosphere.is_empty() {
        "(not stated)"
    } else {
        atmosphere
    };
    let lines = serde_json::to_string_pretty(&sound_view(segments))?;
    for (needle, replacement) in [
        ("{atmosphere}", atmosphere),
        ("{staged_lines}", lines.as_str()),
    ] {
        replace_or_miss(&mut body, needle, replacement, &mut missed);
    }
    warn_missing_sections("sound prompt", &missed);
    Ok(body)
}

/// The sound vocabulary and rules (8-10) from the adapter's `sound.txt`, as the
/// **by-hand** staging pass needs them: it sounds a chapter inline, so it renders
/// the same rules the sound pass would. One source for both paths.
pub(crate) fn sound_rules(layout: &Layout) -> Result<String> {
    let path = layout.sound_prompt();
    let template = std::fs::read_to_string(&path)
        .with_context(|| format!("reading prompt template {}", path.display()))?;
    let (body, _) = render_vocab(&template, &sound_vocab(layout)?);
    let start = body.find(SOUND_RULES_HEAD).ok_or_else(|| {
        anyhow::anyhow!(
            "{} has no `{SOUND_RULES_HEAD}` block, so the by-hand staging pass \
             would sound a chapter with no sound rules",
            path.display()
        )
    })?;
    let end = body.find(SOUND_RULES_TAIL).unwrap_or(body.len());
    Ok(format!("\n{}\n", body[start..end].trim_end()))
}

/// The bounds of the by-hand block inside `sound.txt`.
const SOUND_RULES_HEAD: &str = "---MUSIC PALETTE---";
const SOUND_RULES_TAIL: &str = "---SOUND OUTPUT CONTRACT---";

/// Apply one answer entry's sound fields to a cloned line. Absent fields stay
/// absent (carried later) and so does a `null` — a model that says "nothing"
/// that way is saying what an omission says; an empty string is refused.
fn apply_sound_fields(line: &mut Value, entry: &Value, id: usize) -> Result<(), String> {
    for key in ["scene", "music"] {
        match entry.get(key).and_then(Value::as_str) {
            Some(v) if !v.trim().is_empty() => line[key] = json!(v.trim()),
            _ => {}
        }
    }
    for key in ["sound_after", "stop_after"] {
        let Some(v) = entry.get(key).filter(|v| !v.is_null()) else {
            continue;
        };
        // A string travels as written and keeps its delimiters; an array is
        // normalized to its names, since only a hand-written script sends one.
        let value = match v {
            Value::String(s) => json!(s.trim()),
            other => other.clone(),
        };
        if let Err(e) = seam_value(&value, key) {
            return Err(format!("sound line {id}: {e:#}"));
        }
        line[key] = value;
    }
    Ok(())
}

/// Read the sound answer against the staged lines, producing the chapter's
/// final segment list (sounds lifted out as their own items).
pub(crate) fn parse_sound_script(
    raw: &str,
    staged: &[Value],
    chapter_text: &str,
    vocab: &Vocabulary,
) -> Result<Vec<Value>, String> {
    let data = parse_json_repaired(strip_fences(raw))
        .map_err(|e| format!("sound answer is not valid JSON: {e}"))?;
    let entries = data
        .get("segments")
        .and_then(Value::as_array)
        .ok_or_else(|| "sound answer has no `segments` array".to_string())?;

    let lines = staged
        .iter()
        .filter(|s| !crate::util::is_sound_item(s))
        .cloned()
        .collect::<Vec<_>>();
    let total = lines.len();

    let mut by_id: BTreeMap<usize, Vec<&Value>> = BTreeMap::new();
    for (i, entry) in entries.iter().enumerate() {
        let id = entry
            .get("id")
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("sound segment {i}: missing integer `id`"))?
            as usize;
        if id == 0 || id > total {
            return Err(format!(
                "sound segment {i}: id {id} is not a line — this chapter has {total} line(s), \
                 numbered 1..{total}"
            ));
        }
        by_id.entry(id).or_default().push(entry);
    }

    let mut out: Vec<Value> = Vec::with_capacity(total);
    for (index, line) in lines.iter().enumerate() {
        let id = index + 1;
        let Some(group) = by_id.get(&id) else {
            return Err(format!(
                "sound line {id} is missing — answer for every line, in order"
            ));
        };
        let text = line.get("text").and_then(Value::as_str).unwrap_or("");
        if group.len() == 1 {
            let mut item = line.clone();
            apply_sound_fields(&mut item, group[0], id)?;
            out.push(item);
            continue;
        }
        // A split: every part must carry an exact half that, together with its
        // siblings in order, spells the staged line.
        let mut halves: Vec<String> = Vec::new();
        for entry in group.iter() {
            let half = entry.get("text").and_then(Value::as_str).ok_or_else(|| {
                format!(
                    "sound line {id}: a split needs `text` on every part — to place a sound inside \
                     the line, give each half its exact text"
                )
            })?;
            halves.push(half.to_string());
        }
        if !source_text_matches(text, &halves) {
            return Err(format!(
                "sound line {id}: the parts do not spell the line exactly — the halves, in order, \
                 must reproduce {:?}",
                crate::util::head_chars(text, 60)
            ));
        }
        for (entry, half) in group.iter().zip(halves) {
            let mut item = line.clone();
            item["text"] = json!(half);
            apply_sound_fields(&mut item, entry, id)?;
            out.push(item);
        }
    }

    let mut data = json!({ "segments": out });
    // No prepared events ride here: text arrives already staged, so this only
    // carries `scene` and `music` forward (and the silent head) across the
    // whole chapter.
    let none = PreparedChapter {
        events: Vec::new(),
        prompt_json: String::new(),
        unbalanced_at: None,
    };
    carry_forward_fields(&mut data, &none);
    let segs = data["segments"].as_array().cloned().unwrap_or_default();
    let segs = expand_sound_fields(&segs).map_err(|e| format!("{e:#}"))?;
    let mut data = json!({ "segments": segs, "fixes": [] });
    apply_tag_aliases(&mut data, &vocab.aliases);
    discard_unknown_effect_tags(&mut data, &vocab.effects);
    validate_music(&data, &vocab.palette).map_err(|e| format!("{e:#}"))?;
    validate_effect_tags(&data, &vocab.effects).map_err(|e| format!("{e:#}"))?;
    validate_injects(&data, &vocab.injects).map_err(|e| format!("{e:#}"))?;
    if let Some(gap) = unclosed_beds(&data, &vocab.injects) {
        return Err(gap);
    }
    if let Some(gap) = silent_design(&data, chapter_text, "chapter", &vocab.sound_cues) {
        return Err(gap);
    }
    Ok(data["segments"].as_array().cloned().unwrap_or_default())
}

#[cfg(test)]
mod tests;
