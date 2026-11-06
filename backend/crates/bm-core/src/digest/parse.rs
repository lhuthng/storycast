use super::gate::apply_source_spelling;
use super::gate::validate_source_alignment;
use super::json::parse_json_repaired;
use super::json::strip_fences;
use super::manual::Vocabulary;
use super::prompts::PreparedChapter;
use super::prompts::PreparedEvent;
use super::run::Complaint;
use super::run::Round;
use super::sound_fields::carry_forward_fields;
use super::sound_fields::expand_sound_fields;
use super::validate::canonical_voice_hint;
use super::validate::canonicalize_aliases;
use super::validate::complete_roster;
use super::validate::validate_attributions;
use super::*;
/// Put the two rounds back into the one object everything downstream reads.
pub(crate) fn merge_rounds(context: &Value, script: &Value) -> Value {
    let mut out = serde_json::Map::new();
    // `excerpt` belongs to the cast pass like `title` and `atmosphere` do: it
    for key in [
        "title",
        "atmosphere",
        "excerpt",
        "roster",
        "mentions",
        "new_characters",
        "new_aliases",
        "speakers",
    ] {
        if let Some(v) = context.get(key) {
            out.insert(key.to_string(), v.clone());
        }
    }
    for key in ["segments", "fixes"] {
        if let Some(v) = script.get(key) {
            out.insert(key.to_string(), v.clone());
        }
    }
    Value::Object(out)
}

/// The one reserved speaker name for a person the source never names.
pub(crate) const ANONYMOUS_SPEAKER: &str = "Anonymous";

pub(crate) fn is_anonymous_speaker(speaker: &str) -> bool {
    if speaker == ANONYMOUS_SPEAKER {
        return true;
    }
    let Some(number) = speaker.strip_prefix("anonymous:anon-") else {
        return false;
    };
    number
        .parse::<u32>()
        .is_ok_and(|n| n > 0 && number == n.to_string())
}

pub(crate) fn fixed_speakers(data: &Value) -> Result<BTreeMap<String, String>> {
    let speakers = data
        .get("speakers")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("attribution answer has no speakers object"))?;
    speakers
        .iter()
        .map(|(id, speaker)| {
            let speaker = speaker
                .as_str()
                .filter(|name| !name.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("source {id:?} has an empty speaker"))?;
            Ok((id.clone(), speaker.to_string()))
        })
        .collect()
}

/// What an event is, once the attribution pass's retraction is applied.
pub(crate) fn is_voiced_kind(kind: &str) -> bool {
    matches!(kind, "dialogue" | "thought")
}

pub(crate) fn effective_kind<'a>(
    event: &'a PreparedEvent,
    not_speech: &HashSet<String>,
) -> &'a str {
    if not_speech.contains(&event.id) {
        "narration"
    } else {
        &event.kind
    }
}

/// The ids the attribution answer claims are quoted non-speech.
pub(crate) fn not_speech_ids(data: &Value) -> Result<HashSet<String>> {
    let Some(list) = data.get("not_speech") else {
        return Ok(HashSet::new());
    };
    if list.is_null() {
        return Ok(HashSet::new());
    }
    let list = list.as_array().ok_or_else(|| {
        anyhow::anyhow!("attribution `not_speech` must be an array of source ids")
    })?;
    list.iter()
        .map(|id| {
            id.as_str().map(str::to_string).ok_or_else(|| {
                anyhow::anyhow!("attribution `not_speech` holds a non-string: {id:?}")
            })
        })
        .collect()
}

/// Validate the complete source-id to speaker map against the preparer's
fn normalize_attribution_metadata(
    data: &mut Value,
    bible: &Value,
    prepared: &PreparedChapter,
) -> Vec<String> {
    // A mention map is evidence for later books, not a join table needed by this
    let source = prepared
        .events
        .iter()
        .map(|event| event.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let raw_mentions = data
        .get("mentions")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    // A description is scene-local. If the chapter only calls a figure "người
    // áo đen" and never names them, a bible alias must not name them either.
    let mut described_only: HashSet<String> = HashSet::new();
    for (form, owner) in &raw_mentions {
        let Some(owner) = owner
            .as_str()
            .map(str::trim)
            .filter(|owner| !owner.is_empty())
        else {
            continue;
        };
        let resolved = resolve_speaker(bible, owner);
        if !is_anonymous_speaker(owner)
            && !is_anonymous_speaker(&resolved)
            && resolved != "Narrator"
            && canon::is_descriptive_epithet(form)
            && !canon::named_in_source(&source, &resolved, bible, &raw_mentions)
        {
            described_only.insert(owner.to_string());
            described_only.insert(resolved);
        }
    }
    // Rule 1 tells the model to leave descriptions out of `mentions`, so check
    // the cast too: a roster name the chapter never names, whose only match is
    // a descriptive bible alias, is the same unnamed figure.
    let mut candidates: HashSet<&str> = HashSet::new();
    if let Some(roster) = data.get("roster").and_then(Value::as_array) {
        candidates.extend(roster.iter().filter_map(Value::as_str).map(str::trim));
    }
    if let Some(speakers) = data.get("speakers").and_then(Value::as_object) {
        candidates.extend(speakers.values().filter_map(Value::as_str).map(str::trim));
    }
    for name in candidates {
        if name.is_empty() || name == "Narrator" || is_anonymous_speaker(name) {
            continue;
        }
        let resolved = resolve_speaker(bible, name);
        if resolved == "Narrator"
            || is_anonymous_speaker(&resolved)
            || canon::named_in_source(&source, &resolved, bible, &raw_mentions)
        {
            continue;
        }
        if canon::describes_in_source(&source, &resolved, bible) {
            described_only.insert(name.to_string());
            described_only.insert(resolved);
        }
    }

    let mut roster: Vec<String> = data
        .get("roster")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty() && !described_only.contains(name))
        .collect();
    roster.dedup();
    if let Some(speakers) = data.get("speakers").and_then(Value::as_object) {
        roster.retain(|name| {
            !is_anonymous_speaker(name)
                || speakers
                    .values()
                    .any(|speaker| speaker.as_str() == Some(name.as_str()))
        });
    }

    let mut new_characters: Vec<Value> = Vec::new();
    let mut new_names: HashSet<String> = HashSet::new();
    for mut character in data
        .get("new_characters")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        let Some(name) = character
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty() && !is_anonymous_speaker(name))
            .map(str::to_string)
        else {
            continue;
        };
        if described_only.contains(&name) || !new_names.insert(name.clone()) {
            continue;
        }

        let original_hint = character
            .get("voice_hint")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let voice_hint = canonical_voice_hint(original_hint);
        let mut tags = tags::normalise_tags(character.get("tags"));
        if tags.is_empty() {
            tags = crate::pool::tags_from_hint(original_hint);
        }
        if tags.is_empty() {
            tags = crate::pool::tags_from_hint(&voice_hint);
        }

        let aliases = character
            .get("proper_aliases")
            .or_else(|| character.get("aliases"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|alias| !alias.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        let mut aliases = aliases;
        if !aliases.iter().any(|alias| alias == &name) {
            aliases.insert(0, name.clone());
        }
        aliases.dedup();

        character["name"] = json!(name);
        character["personality"] = character
            .get("personality")
            .cloned()
            .filter(Value::is_string)
            .unwrap_or(json!(""));
        character["voice_hint"] = json!(voice_hint);
        character["tags"] = json!(tags);
        character["proper_aliases"] = json!(aliases);
        character.as_object_mut().map(|o| o.remove("aliases"));
        new_characters.push(character);
    }

    // Every named roster entry needs a persisted identity. The model may have
    let known_bible_name = |name: &str| {
        bible
            .get("characters")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|c| c.get("name").and_then(Value::as_str) == Some(name))
            || resolve_speaker(bible, name) != name
    };
    let mut undescribed: Vec<String> = Vec::new();
    for name in roster.clone() {
        if name == "Narrator"
            || is_anonymous_speaker(&name)
            || known_bible_name(&name)
            || new_names.contains(&name)
        {
            continue;
        }
        undescribed.push(name.clone());
        new_names.insert(name.clone());
        new_characters.push(json!({
            "name": name,
            "personality": "",
            // A roster speaker the model did not describe: say "unknown", not
            // "male". The gender is resolved from the text later; a wrong male
            // voice baked here outlived every correction.
            "voice_hint": "unknown",
            "tags": [],
            "proper_aliases": [name]
        }));
    }

    let known_owner = |owner: &str| {
        owner == "Narrator"
            || roster.iter().any(|name| name == owner)
            || known_bible_name(owner)
            || new_names.contains(owner)
    };
    let mentions = raw_mentions
        .iter()
        .filter_map(|(form, owner)| {
            let form = form.trim();
            let owner = owner.as_str()?.trim();
            // A mentions object has chapter scope but no scenario key, so it
            (!form.is_empty()
                && !described_only.contains(owner)
                && !canon::is_scenario_dependent(form)
                && !canon::is_descriptive_epithet(form)
                && source.contains(form)
                && known_owner(owner))
            .then(|| (form.to_string(), json!(owner)))
        })
        .collect::<serde_json::Map<_, _>>();

    // A figure the chapter only describes is the reserved anonymous speaker,
    // even when a bible alias names them. Rewrite before validation, so the
    // answer is accepted instead of re-asked with the same names.
    if !described_only.is_empty() {
        if let Some(speakers) = data.get_mut("speakers").and_then(Value::as_object_mut) {
            for speaker in speakers.values_mut() {
                let Some(name) = speaker.as_str().map(str::trim) else {
                    continue;
                };
                if described_only.contains(name)
                    || described_only.contains(&resolve_speaker(bible, name))
                {
                    *speaker = json!(ANONYMOUS_SPEAKER);
                }
            }
        }
        let used_anonymous: HashSet<String> = data
            .get("speakers")
            .and_then(Value::as_object)
            .map(|speakers| {
                speakers
                    .values()
                    .filter_map(|speaker| speaker.as_str())
                    .filter(|name| is_anonymous_speaker(name))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        roster.retain(|name| !is_anonymous_speaker(name) || used_anonymous.contains(name));
        for name in used_anonymous {
            if !roster.iter().any(|entry| entry == &name) {
                roster.push(name);
            }
        }
    }

    data["roster"] = json!(roster);
    data["mentions"] = Value::Object(mentions);
    data["new_characters"] = Value::Array(new_characters);
    undescribed
}

/// `split` says the answer is for one part of a chapter staged in windows,
pub(crate) const EXCERPT_CHARS: usize = 600;

pub(crate) fn parse_attribution(
    raw: &str,
    bible: &Value,
    prepared: &PreparedChapter,
    split: bool,
) -> Result<Value, Complaint> {
    parse_attribution_gated(raw, bible, prepared, split, true)
}

/// The lenient entry point's gated twin: `tolerate` false refuses an answer
/// that leaves a roster speaker undescribed, so the run loop can spend its one
/// repair asking for the description instead of synthesizing a silent one.
pub(crate) fn parse_attribution_gated(
    raw: &str,
    bible: &Value,
    prepared: &PreparedChapter,
    split: bool,
    tolerate: bool,
) -> Result<Value, Complaint> {
    // Everything here is the attribution answer's own doing, so the blame is
    let mine = |e: anyhow::Error| Complaint::new(Round::Attribution, e);
    let cleaned = strip_fences(raw);
    let mut data = parse_json_repaired(cleaned)
        .with_context(|| "attribution is not valid JSON".to_string())
        .map_err(mine)?;
    let undescribed = normalize_attribution_metadata(&mut data, bible, prepared);
    if !tolerate && !undescribed.is_empty() {
        return Err(mine(anyhow::anyhow!(
            "roster names {undescribed:?} have no new_characters entry — describe each (name, \
             personality, and a voice_hint head) or drop it from roster"
        )));
    }
    // Unambiguous aliases are corrected here, before a single check runs, and
    for fix in canonicalize_aliases(&mut data, bible) {
        eprintln!("attribution alias corrected: {fix}");
    }
    for fix in complete_roster(&mut data, bible, prepared) {
        eprintln!("attribution roster completed: {fix}");
    }
    // The excerpt is a **soft** field: absent, blank, or over-long is
    let excerpt = data
        .get("excerpt")
        .and_then(Value::as_str)
        .map(|s| head_chars(&squeeze_ws(s), EXCERPT_CHARS))
        .unwrap_or_default();
    data["excerpt"] = json!(excerpt);
    validate_context(&data, bible).map_err(mine)?;
    validate_title(&data).map_err(mine)?;
    if split {
        take_summary(&data).map_err(mine)?;
    }
    // The validated map is written back with the narration rows the preparer
    let speakers = validate_attributions(&data, bible, prepared).map_err(mine)?;
    data["speakers"] = json!(speakers);
    Ok(data)
}

/// The `summary` a part's attribution answer must carry: what the parts after it
fn take_summary(data: &Value) -> Result<String> {
    data.get("summary")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "attribution has no `summary` — this chapter is longer than one pass, so each \
                 part's answer must say what happened in it; the parts after this one are handed \
                 that summary and never see these events"
            )
        })
}

/// Attach the already validated speaker map — and the thought marker — to
pub(crate) fn attach_fixed_speakers(
    data: &mut Value,
    speakers: &BTreeMap<String, String>,
    thoughts: &HashSet<String>,
) -> Result<()> {
    let segments = data
        .get_mut("segments")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| anyhow::anyhow!("staging answer has no segments"))?;
    for (i, segment) in segments.iter_mut().enumerate() {
        if crate::util::is_sound_item(segment) {
            continue;
        }
        let id = segment
            .get("source_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("segment {i}: missing source_id"))?;
        let speaker = speakers
            .get(&id)
            .ok_or_else(|| anyhow::anyhow!("segment {i}: source id {id:?} has no attribution"))?;
        segment["speaker"] = Value::String(speaker.clone());
        if thoughts.contains(&id) {
            segment["kind"] = Value::String("thought".into());
        } else if let Some(object) = segment.as_object_mut() {
            object.remove("kind");
        }
    }
    Ok(())
}

/// Lift the pack's declared thought sound into a sound item at each thought's
pub(crate) fn lift_thought_stingers(data: &mut Value, sound: Option<&str>) {
    let Some(sound) = sound.map(str::trim).filter(|s| !s.is_empty()) else {
        return;
    };
    let Some(segments) = data.get_mut("segments").and_then(Value::as_array_mut) else {
        return;
    };
    let mut lifted: Vec<Value> = Vec::with_capacity(segments.len());
    for (i, segment) in segments.iter().enumerate() {
        let thought = !crate::util::is_sound_item(segment)
            && segment.get("kind").and_then(Value::as_str) == Some("thought");
        let source = segment.get("source_id").and_then(Value::as_str);
        let next = segments.get(i + 1);
        let continues = next.and_then(|n| n.get("source_id").and_then(Value::as_str)) == source;
        // A query starts a sound at the seam just as a name does, so neither
        // needs the pack's thought stinger under it.
        let start_already = next
            .filter(|n| crate::util::is_sound_item(n))
            .map(|n| n.get("sound").is_some() || n.get("sound_tags").is_some())
            .unwrap_or(false);
        lifted.push(segment.clone());
        if thought && !continues && !start_already {
            lifted.push(json!({ "sound": sound }));
        }
    }
    *segments = lifted;
}

/// Parse the staging answer after attaching immutable speakers, then run the
pub(crate) fn parse_staged_script(
    raw: &str,
    bible: &Value,
    context: &Value,
    prepared: &PreparedChapter,
    vocab: &Vocabulary,
) -> Result<Value, Complaint> {
    // Two blames, and the split is the point. Most of this is the staging
    let mine = |e: anyhow::Error| Complaint::new(Round::Staging, e);
    let theirs = |e: anyhow::Error| Complaint::new(Round::Attribution, e);
    let cleaned = strip_fences(raw);
    let mut data = parse_json_repaired(cleaned)
        .with_context(|| "staging is not valid JSON".to_string())
        .map_err(mine)?;
    carry_forward_fields(&mut data, prepared);
    if let Some(segs) = data.get("segments").and_then(|s| s.as_array()).cloned() {
        data["segments"] = json!(expand_sound_fields(&segs).map_err(mine)?);
    }
    apply_tag_aliases(&mut data, &vocab.aliases);
    discard_unknown_effect_tags(&mut data, &vocab.effects);
    let not_speech = not_speech_ids(context).map_err(theirs)?;
    let thoughts: HashSet<String> = prepared
        .events
        .iter()
        .filter(|e| e.kind == "thought" && !not_speech.contains(&e.id))
        .map(|e| e.id.clone())
        .collect();
    attach_fixed_speakers(
        &mut data,
        &fixed_speakers(context).map_err(theirs)?,
        &thoughts,
    )
    .map_err(theirs)?;
    resolve_nonverbals(&mut data, &vocab.engine);
    lift_thought_stingers(&mut data, vocab.thought_stinger.as_deref());
    validate_script(&data, bible, context, &vocab.palette).map_err(mine)?;
    validate_inline_tags(&data, &vocab.engine).map_err(mine)?;
    validate_effect_tags(&data, &vocab.effects).map_err(mine)?;
    validate_injects(&data, &vocab.injects).map_err(mine)?;
    let spellings =
        validate_source_alignment(&data, prepared, &not_speech, &vocab.engine).map_err(mine)?;
    apply_source_spelling(&mut data, &spellings);
    Ok(data)
}

/// Fill each declared sound in with the engine's own tag. The code knows the
/// engine's keys and their spellings and copies the one the answer chose; it
/// never chooses one itself, and the tag lands in the plan (see
/// `assemble::plan::Planned`), not in the script's text.
pub(crate) fn resolve_nonverbals(data: &mut Value, engine: &str) {
    let decls = crate::voices::nonverbals(engine);
    let Some(segments) = data.get_mut("segments").and_then(Value::as_array_mut) else {
        return;
    };
    for segment in segments.iter_mut() {
        let Some(key) = segment
            .get("nonverbal")
            .and_then(|n| n.get("key"))
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            continue;
        };
        let decl = decls.iter().find(|n| n.key == key.as_str());
        if let (Some(decl), Some(sound)) = (
            decl,
            segment.get_mut("nonverbal").and_then(Value::as_object_mut),
        ) {
            sound.insert("tag".to_string(), json!(decl.tag));
        }
    }
}
