use super::parse::effective_kind;
use super::parse::fixed_speakers;
use super::parse::is_anonymous_speaker;
use super::parse::is_voiced_kind;
use super::parse::not_speech_ids;
use super::prepare::first_person_singular;
use super::prompts::PreparedChapter;
use super::*;
/// The attribution contract is stricter than the legacy manual contract: every
pub(crate) fn validate_attributions(
    data: &Value,
    bible: &Value,
    prepared: &PreparedChapter,
) -> Result<BTreeMap<String, String>> {
    let mut speakers = fixed_speakers(data)?;
    let not_speech = not_speech_ids(data)?;
    // A first-person-singular passage is the thinker's own voice, so the
    for id in &not_speech {
        if let Some(event) = prepared.events.iter().find(|e| &e.id == id) {
            if event.kind == "thought" && first_person_singular(&event.text) {
                anyhow::bail!(
                    "source {id:?} is a first-person thought ({:?}) and cannot be retracted as \
                     narration — give it the character thinking it, never Narrator. A narrator aside \
                     to the reader is `we`/`you`-voiced, not `I`",
                    crate::util::head_chars(&event.text, 80)
                );
            }
        }
    }
    // Narration is mechanical, so it is written here rather than read from the
    for event in prepared.events.iter().filter(|e| !is_voiced_kind(&e.kind)) {
        speakers.insert(event.id.clone(), "Narrator".to_string());
    }
    let roster = data
        .get("roster")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("attribution answer has no roster"))?;
    let roster: Vec<&str> = roster.iter().filter_map(Value::as_str).collect();
    let expected: HashSet<&str> = prepared.events.iter().map(|e| e.id.as_str()).collect();
    for id in speakers.keys() {
        if !expected.contains(id.as_str()) {
            anyhow::bail!("attribution names unknown source id {id:?}");
        }
    }
    // `not_speech` wins over `speakers` for the ids it lists, rather than the
    for id in &not_speech {
        speakers.insert(id.clone(), "Narrator".to_string());
    }

    for event in &prepared.events {
        let speaker = speakers.get(&event.id).ok_or_else(|| {
            // The text goes in the complaint, not just the id. This string is
            anyhow::anyhow!(
                "attribution dropped source event {:?} — it is dialogue or a thought and reads {:?}; give it a speaker",
                event.id,
                crate::util::head_chars(&event.text, 80)
            )
        })?;
        // A dialogue event the model retracted is narration now, and is held
        let kind = effective_kind(event, &not_speech);
        match kind {
            "narration" if speaker != "Narrator" => anyhow::bail!(
                "source {:?} is narration but attribution assigns {speaker:?}; narration must be Narrator",
                event.id
            ),
            "dialogue" | "thought" if speaker == "Narrator" => anyhow::bail!(
                "source {:?} is {kind} but attribution assigns Narrator; use a canonical character or the reserved `Anonymous` — a hail nobody on cast is tagged saying belongs to the crowd, not to Narrator, and a thought belongs to its thinker, not to the narrator. Only a span that is not somebody talking or thinking (a quoted title, a term, a panel label, a third-person sentence about its own subject) may be Narrator, and it must also be listed in `not_speech`",
                event.id
            ),
            _ => {}
        }
        if !roster.contains(&speaker.as_str()) {
            if speaker == "Narrator" {
                anyhow::bail!(
                    "source {:?} is narration and this chapter has narration, but `roster` omits \"Narrator\" — add it to roster",
                    event.id
                );
            }
            // The same reasoning as the dropped-event complaint: a repair only
            anyhow::bail!(
                "source {:?} assigns {speaker:?}, but that speaker is not in the chapter roster — \
                 add {speaker:?} to `roster` or give the line another speaker. The line reads {:?}",
                event.id,
                crate::util::head_chars(&event.text, 80)
            );
        }
    }
    for (i, name) in roster.iter().enumerate() {
        if is_anonymous_speaker(name) && !speakers.values().any(|speaker| speaker == *name) {
            anyhow::bail!(
                "roster[{i}]: {name:?} is never used; drop it when nobody in the chapter is unnamed"
            );
        }
    }
    validate_digest_identity(data, bible)?;
    Ok(speakers)
}

fn word_set(text: &str) -> HashSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphabetic())
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect()
}

/// Rewrite free-form model voice descriptions into the one prefix the cast
pub(crate) fn canonical_voice_hint(hint: &str) -> String {
    let hint = hint.trim();
    let words = word_set(hint);
    let has = |word: &str| words.contains(word);
    let current = hint
        .split([',', ':', '-', '–'])
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase();
    if matches!(
        current.as_str(),
        "adult male" | "adult female" | "boy" | "girl" | "elderly male" | "elderly female"
    ) {
        return current;
    }

    let female = ["female", "woman", "lady", "nữ", "cô", "chị", "gái", "bà"]
        .iter()
        .any(|word| has(word));
    let male = ["male", "man", "nam", "ông", "anh", "trai", "lão"]
        .iter()
        .any(|word| has(word));
    let head = if has("elderly") || has("old") {
        if female {
            "elderly female"
        } else {
            "elderly male"
        }
    } else if has("girl") {
        "girl"
    } else if has("boy") {
        "boy"
    } else if female {
        "adult female"
    } else if male {
        "adult male"
    } else {
        // A system voice or an under-described newcomer still needs a speakable
        "adult male"
    };
    if hint.is_empty() {
        head.to_string()
    } else {
        format!("{head}: {hint}")
    }
}

/// Make the cast bookkeeping deterministic without touching the model's
pub(crate) fn validate_digest_identity(data: &Value, bible: &Value) -> Result<()> {
    let roster = data
        .get("roster")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("digest attribution has no roster"))?;
    let new_names: Vec<&str> = data
        .get("new_characters")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| c.get("name").and_then(Value::as_str))
        .collect();
    if let Some(name) = new_names.iter().find(|name| is_anonymous_speaker(name)) {
        anyhow::bail!(
            "new_character name {name:?} uses the reserved anonymous speaker namespace; unnamed dialogue is not a Bible character"
        );
    }
    let bible_names: Vec<&str> = bible
        .get("characters")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| c.get("name").and_then(Value::as_str))
        .collect();
    let characters = bible
        .get("characters")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for (i, name) in roster.iter().filter_map(Value::as_str).enumerate() {
        if name != "Narrator"
            && !is_anonymous_speaker(name)
            && !bible_names.contains(&name)
            && !new_names.contains(&name)
        {
            // `roster` is a join key, not a display list: `speaker` is matched
            let owners = alias_owners(&characters, name);
            if let Some(canonical) = owners.first().filter(|_| owners.len() == 1) {
                anyhow::bail!(
                    "roster[{i}]: {name:?} is an alias of {canonical:?} — put the canonical name in roster and keep {name:?} in mentions"
                );
            }
            anyhow::bail!("roster[{i}]: {name:?} is not a canonical known/new character name");
        }
    }
    if let Some(mentions) = data.get("mentions").and_then(Value::as_object) {
        for (form, owner) in mentions {
            let owner = owner.as_str().unwrap_or("");
            if is_anonymous_speaker(owner) {
                anyhow::bail!(
                    "mention {form:?} cannot be owned by the reserved anonymous speaker {owner:?}; mentions map named identities only"
                );
            }
            // Deliberately *not* checked against `roster`. `roster` is the
            let owners = alias_owners(&characters, form);
            if owners.len() > 1 {
                anyhow::bail!("mention {form:?} is ambiguous between {owners:?}");
            }
            if !owners.is_empty() && !owners.contains(owner) {
                anyhow::bail!(
                    "mention {form:?} is owned by {owners:?}, not {owner:?} — a mention's owner must be that character's canonical name"
                );
            }
        }
    }
    for (i, segment) in data
        .get("segments")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        if crate::util::is_sound_item(segment) {
            continue;
        }
        let speaker = segment.get("speaker").and_then(Value::as_str).unwrap_or("");
        if speaker != "Narrator"
            && !roster
                .iter()
                .filter_map(Value::as_str)
                .any(|name| name == speaker)
        {
            anyhow::bail!("segment {i}: speaker {speaker:?} is not a canonical roster name");
        }
    }
    Ok(())
}

/// Rewrite the names in an attribution answer that are an **unambiguous
pub(crate) fn canonicalize_aliases(data: &mut Value, bible: &Value) -> Vec<String> {
    let characters: Vec<Value> = bible
        .get("characters")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if characters.is_empty() {
        return Vec::new();
    }
    // The names that are already canonical — from the bible, and from the
    let canonical: HashSet<String> = characters
        .iter()
        .filter_map(|c| c.get("name").and_then(Value::as_str))
        .map(str::to_string)
        .chain(
            data.get("new_characters")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|c| c.get("name").and_then(Value::as_str))
                .map(str::to_string),
        )
        .collect();

    let resolve = |name: &str| -> Option<String> {
        if name == "Narrator" || is_anonymous_speaker(name) || canonical.contains(name) {
            return None;
        }
        let owners = alias_owners(&characters, name);
        if owners.len() != 1 {
            return None;
        }
        let one = owners.into_iter().next().expect("length just checked");
        (one != name).then_some(one)
    };

    let mut fixes = Vec::new();
    // `roster` is a join key, so it has to end up canonical *and* de-duplicated:
    if let Some(roster) = data.get_mut("roster").and_then(Value::as_array_mut) {
        for slot in roster.iter_mut() {
            let Some(name) = slot.as_str().map(str::to_string) else {
                continue;
            };
            if let Some(canon) = resolve(&name) {
                fixes.push(format!("roster {name:?} -> {canon:?}"));
                *slot = json!(canon);
            }
        }
        let mut seen = HashSet::new();
        roster.retain(|v| match v.as_str() {
            Some(name) => seen.insert(name.to_string()),
            None => true,
        });
    }
    if let Some(speakers) = data.get_mut("speakers").and_then(Value::as_object_mut) {
        for slot in speakers.values_mut() {
            let Some(name) = slot.as_str().map(str::to_string) else {
                continue;
            };
            if let Some(canon) = resolve(&name) {
                fixes.push(format!("speakers -> {canon:?} (was {name:?})"));
                *slot = json!(canon);
            }
        }
    }
    if let Some(mentions) = data.get_mut("mentions").and_then(Value::as_object_mut) {
        for slot in mentions.values_mut() {
            let Some(name) = slot.as_str().map(str::to_string) else {
                continue;
            };
            if let Some(canon) = resolve(&name) {
                fixes.push(format!("mentions -> {canon:?} (was {name:?})"));
                *slot = json!(canon);
            }
        }
    }
    // A staged segment carries the speaker too, and it is checked against the
    if let Some(segments) = data.get_mut("segments").and_then(Value::as_array_mut) {
        for segment in segments.iter_mut() {
            let Some(name) = segment
                .get("speaker")
                .and_then(Value::as_str)
                .map(str::to_string)
            else {
                continue;
            };
            if let Some(canon) = resolve(&name) {
                fixes.push(format!("segment speaker -> {canon:?} (was {name:?})"));
                segment["speaker"] = json!(canon);
            }
        }
    }
    fixes
}

/// Add the speakers an answer actually uses to the `roster` it returned.
pub(crate) fn complete_roster(
    data: &mut Value,
    bible: &Value,
    prepared: &PreparedChapter,
) -> Vec<String> {
    let legit: HashSet<String> = bible
        .get("characters")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| c.get("name").and_then(Value::as_str))
        .map(str::to_string)
        .chain(
            data.get("new_characters")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|c| c.get("name").and_then(Value::as_str))
                .map(str::to_string),
        )
        .collect();
    let speaks =
        |name: &str| name == "Narrator" || is_anonymous_speaker(name) || legit.contains(name);

    // Every name the answer puts on a line.
    let mut used: Vec<String> = data
        .get("speakers")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|m| m.values())
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    // Narration is spoken by the preparer, not the answer, so its speaker is
    if prepared.events.iter().any(|e| !is_voiced_kind(&e.kind)) {
        used.push("Narrator".to_string());
    }
    // A retracted span is read as narration too, which is the same `Narrator`.
    if !not_speech_ids(data).unwrap_or_default().is_empty() {
        used.push("Narrator".to_string());
    }

    let Some(roster) = data.get_mut("roster").and_then(Value::as_array_mut) else {
        return Vec::new();
    };
    let mut have: HashSet<String> = roster
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    let mut fixes = Vec::new();
    for name in used {
        if !speaks(&name) || !have.insert(name.clone()) {
            continue;
        }
        fixes.push(format!("roster += {name:?}"));
        roster.push(json!(name));
    }
    fixes
}

/// Bible characters that claim `form` through `proper_aliases`, compared under
fn alias_owners(characters: &[Value], form: &str) -> std::collections::BTreeSet<String> {
    let key = canon_key(form);
    characters
        .iter()
        .filter(|c| {
            c.get("proper_aliases")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .any(|alias| canon_key(alias) == key)
        })
        .filter_map(|c| c.get("name").and_then(Value::as_str).map(str::to_string))
        .collect()
}
