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
///
/// Ownership is by key, not by "whoever ran last": the cast pass owns identity
/// (`title`, `atmosphere`, `excerpt`, `roster`, `mentions`, `new_characters`,
/// `new_aliases`) and the script pass owns the speech (`segments`, `fixes`).
/// Neither can overwrite the other's keys, so a round that helpfully invents a
/// `title` of its own is ignored rather than silently believed.
pub(crate) fn merge_rounds(context: &Value, script: &Value) -> Value {
    let mut out = serde_json::Map::new();
    // `excerpt` belongs to the cast pass like `title` and `atmosphere` do: it
    // is the cast answer's statement of the state the chapter ends in. It was
    // missing here, and because the miss is silent — the writer at the script
    // build reads `data.get("excerpt")` and falls back to `""` — every chapter
    // was digested with a working excerpt that never reached its script, and
    // the next chapter's `---PREVIOUSLY---` block was always empty.
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
///
/// A crowd is a chorus, not a cast: every unnamed speaker in a chapter shares
/// this name and speaks in the Narrator's voice, which is what the script, the
/// cast file and the mix all show. Numbered slots (`anonymous:anon-1`) are gone
/// from new digests, near-identical one-off clones for a street greeting were
/// the loudest thing in a scene, but they still resolve here, and are still
/// voiced by the Narrator, so chapters already on disk keep rendering.
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
///
/// One definition for both gates. A retracted id reads as narration everywhere,
/// so the speaker rule, the delimiter rule and the prompt's own wording cannot
/// disagree about whether a span is still dialogue.
/// The kinds the attribution pass has to give a voice to: a spoken line and a
/// thought. Narration is mechanical, so it is written by code and never asked
/// about — anything this returns false for gets `Narrator` without the model's
/// answer having a say.
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
///
/// Optional: an answer without the field is an answer that agrees with the
/// preparer, which is what every stored script before this field existed means.
/// A non-string entry is refused rather than skipped, so a model that meant to
/// retract a line cannot have it silently ignored.
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
/// mechanical narration/dialogue classification.
///
/// **The one thing the model may overrule is "this quote is speech",** and only
/// downwards. `prepare_chapter` calls a quoted span dialogue because a
/// delimiter opened it — except a span too short to be speech embedded in
/// running prose, which stays narration outright — so a longer quoted title
/// or term still has to be spoken by somebody. The attribution pass already holds the span with the narration
/// on both sides, which is the evidence a title needs and a keyword list
/// cannot supply: a title is bracketed by prose that continues the sentence,
/// a speech is followed by a tag. So `not_speech` lets the model say so.
///
/// **The direction is what makes this safe.** Narration is still written here
/// in code, never read from the answer, and the reverse — prose promoted to
/// dialogue — is not expressible. The model can retract the preparer's guess;
/// it cannot invent a speaker for prose, nor talk a real character out of a
/// line it is given.
fn normalize_attribution_metadata(data: &mut Value, bible: &Value, prepared: &PreparedChapter) {
    let mut roster: Vec<String> = data
        .get("roster")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
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
        if !new_names.insert(name.clone()) {
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
    // omitted one while still assigning that character correctly; synthesize a
    // minimal adult-male entry rather than rejecting every worker for the same
    // metadata omission.
    let known_bible_name = |name: &str| {
        bible
            .get("characters")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|c| c.get("name").and_then(Value::as_str) == Some(name))
            || resolve_speaker(bible, name) != name
    };
    for name in roster.clone() {
        if name == "Narrator"
            || is_anonymous_speaker(&name)
            || known_bible_name(&name)
            || new_names.contains(&name)
        {
            continue;
        }
        new_names.insert(name.clone());
        new_characters.push(json!({
            "name": name,
            "personality": "",
            "voice_hint": "adult male",
            "tags": ["male"],
            "proper_aliases": [name]
        }));
    }

    // A mention map is evidence for later books, not a join table needed by this
    // chapter's already-fixed speakers. Keep only literal, stable name-bearing
    // forms with a known named owner; discard rows that can neither resolve nor
    // be audited, and forms whose referent depends on the local scene.
    let source = prepared
        .events
        .iter()
        .map(|event| event.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let known_owner = |owner: &str| {
        owner == "Narrator"
            || roster.iter().any(|name| name == owner)
            || known_bible_name(owner)
            || new_names.contains(owner)
    };
    let mentions = data
        .get("mentions")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|map| map.iter())
        .filter_map(|(form, owner)| {
            let form = form.trim();
            let owner = owner.as_str()?.trim();
            // A mentions object has chapter scope but no scenario key, so it
            // cannot faithfully represent a form whose referent changes within
            // the chapter (`Đồ nhi`, `sư tôn`, pronouns). Keep only stable,
            // name-bearing forms; the source-aware prompt uses the actual
            // neighboring narration to resolve those ambiguous references.
            (!form.is_empty()
                && !canon::is_scenario_dependent(form)
                && source.contains(form)
                && known_owner(owner))
            .then(|| (form.to_string(), json!(owner)))
        })
        .collect::<serde_json::Map<_, _>>();

    data["roster"] = json!(roster);
    data["mentions"] = Value::Object(mentions);
    data["new_characters"] = Value::Array(new_characters);
}

/// `split` says the answer is for one part of a chapter staged in windows,
/// which is the only thing it changes: a part's answer must carry a `summary`,
/// because that summary is what the next part is handed. A one-window chapter
/// is validated exactly as before — the field is neither asked for nor
/// required, which is what keeps a short chapter's answer identical to the
/// pre-window digest's.
/// The excerpt's cap, in characters. Two to four sentences of state is the
/// contract; the cap is what a runaway answer costs when the model ignores
/// it — a paragraph, not a second chapter riding into every future prompt.
pub(crate) const EXCERPT_CHARS: usize = 600;

pub(crate) fn parse_attribution(
    raw: &str,
    bible: &Value,
    prepared: &PreparedChapter,
    split: bool,
) -> Result<Value, Complaint> {
    // Everything here is the attribution answer's own doing, so the blame is
    // uniform: no amount of staging can fix a roster that names a stranger.
    let mine = |e: anyhow::Error| Complaint::new(Round::Attribution, e);
    let cleaned = strip_fences(raw);
    let mut data = parse_json_repaired(cleaned)
        .with_context(|| "attribution is not valid JSON".to_string())
        .map_err(mine)?;
    normalize_attribution_metadata(&mut data, bible, prepared);
    // Unambiguous aliases are corrected here, before a single check runs, and
    // never excused afterwards: `validate_digest_identity` stays exactly as
    // strict as it was, it is just no longer handed a name that has one
    // legitimate canonical spelling. Announced, because a silent rewrite is a
    // bug of its own.
    for fix in canonicalize_aliases(&mut data, bible) {
        eprintln!("attribution alias corrected: {fix}");
    }
    for fix in complete_roster(&mut data, bible, prepared) {
        eprintln!("attribution roster completed: {fix}");
    }
    // The excerpt is a **soft** field: absent, blank, or over-long is
    // squeezed and capped, never a refusal. `speakers` is the product and is
    // hard-validated; a missing excerpt only means the next chapter runs
    // with one less memory, which is the ordinary degradation and the same
    // "if any" the prompt side already accepts.
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
    // owns, because staging reads `speakers` from this same object.
    let speakers = validate_attributions(&data, bible, prepared).map_err(mine)?;
    data["speakers"] = json!(speakers);
    Ok(data)
}

/// The `summary` a part's attribution answer must carry: what the parts after it
/// need to know and cannot look up.
///
/// Refused rather than defaulted when blank, the same way every other required
/// field here is, and for a stronger reason than most: the summary is the whole
/// of what a later part knows about this one, so a part that omits it leaves
/// the rest of the chapter staged against nothing — which is the failure the
/// field exists to remove, and it would be silent. The complaint reaches the
/// model as the ordinary one repair, so a model that skipped the field gets a
/// second chance in the same round.
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
/// staging output. A speaker emitted or changed by the staging model is
/// ignored, and so is a `kind` it invented; identity belongs to pass one.
///
/// `thoughts` is the id set the preparer carved as thoughts minus the ones the
/// attribution pass retracted (`not_speech`), so the marker the mixer keys the
/// thought sound on always agrees with the kind the source gate checks.
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
/// seam.
///
/// A thought is a line like any other to the renderer, so the marker plus this
/// lift is what gives it a sound without teaching any layer downstream about
/// thoughts: the item is the same sibling `{"sound": …}` the staging pass
/// writes (`expand_sound_fields`), placed after the event's **last** segment —
/// a thought split for a TTS run fires one stinger, at its end — and skipped
/// when a `sound` the answer wrote already sits there, so a sting the analyzer
/// chose is never doubled. A `stop` is not a replacement: it closes a running
/// bed, and the declared thought sound still fires beside it.
///
/// `sound` comes from `scene-map.json`'s `thought.sound` and is checked against
/// the inject pool when the vocabulary is loaded, so a name that reaches here
/// is one `validate_injects` will accept.
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
        let start_already = next
            .filter(|n| crate::util::is_sound_item(n))
            .map(|n| n.get("sound").is_some())
            .unwrap_or(false);
        lifted.push(segment.clone());
        if thought && !continues && !start_already {
            lifted.push(json!({ "sound": sound }));
        }
    }
    *segments = lifted;
}

/// Parse the staging answer after attaching immutable speakers, then run the
/// ordinary script checks and the source-integrity gate.
pub(crate) fn parse_staged_script(
    raw: &str,
    bible: &Value,
    context: &Value,
    prepared: &PreparedChapter,
    vocab: &Vocabulary,
) -> Result<Value, Complaint> {
    // Two blames, and the split is the point. Most of this is the staging
    // answer's own doing, so a second ask is the remedy. But two checks read
    // the **cast** rather than the answer: if the attribution pass never gave
    // an event a speaker, the staging model was handed an event it had no
    // business staging, and no staging retry will invent the missing row.
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
    collapse_redundant_sounds(&mut data);
    lift_thought_stingers(&mut data, vocab.thought_stinger.as_deref());
    validate_script(&data, bible, context, &vocab.palette).map_err(mine)?;
    validate_effect_tags(&data, &vocab.effects).map_err(mine)?;
    validate_injects(&data, &vocab.injects).map_err(mine)?;
    validate_source_alignment(&data, prepared, &not_speech).map_err(mine)?;
    Ok(data)
}

/// Collapse a written non-verbal sound the answer left beside its tag.
///
/// The source gate builds its `expected` with `retag_text`, so the answer has
/// to come through the same door: `"[hắng giọng] Khụ khụ khụ, ban đầu…"` is
/// stored as `"[hắng giọng] ban đầu…"`. Leaving it is wrong twice over, the
/// tag *and* the words get spoken, and refusing it stalls a chapter whose
/// model is otherwise right, which is exactly what ch22 did across every racer.
/// `retag_text` is idempotent and word-boundary disciplined, and text with no
/// written sound is untouched.
pub(crate) fn collapse_redundant_sounds(data: &mut Value) {
    let Some(segments) = data.get_mut("segments").and_then(Value::as_array_mut) else {
        return;
    };
    for segment in segments.iter_mut() {
        if crate::util::is_sound_item(segment) {
            continue;
        }
        let Some(text) = segment.get("text").and_then(Value::as_str) else {
            continue;
        };
        if let Some(collapsed) = retag_text(text) {
            segment["text"] = json!(collapsed);
        }
    }
}
