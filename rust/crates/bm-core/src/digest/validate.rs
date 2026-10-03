use super::parse::effective_kind;
use super::parse::fixed_speakers;
use super::parse::is_anonymous_speaker;
use super::parse::is_voiced_kind;
use super::parse::not_speech_ids;
use super::prepare::first_person_singular;
use super::prompts::PreparedChapter;
use super::*;
/// The attribution contract is stricter than the legacy manual contract: every
/// named label is canonical, Narrator is reserved, and anonymous dialogue uses
/// the validated reusable `anonymous:anon-N` namespace.
pub(crate) fn validate_attributions(
    data: &Value,
    bible: &Value,
    prepared: &PreparedChapter,
) -> Result<BTreeMap<String, String>> {
    let mut speakers = fixed_speakers(data)?;
    let not_speech = not_speech_ids(data)?;
    // A first-person-singular passage is the thinker's own voice, so the
    // retraction does not apply to it. A live ch1 run is why: the answer listed
    // `I need to just get this job done.` and `Hope my old man's eating
    // properly.` in `not_speech`, and the Narrator read Maomao's thoughts
    // aloud. The escape hatch is for quoted non-speech and for narrator asides
    // to the reader — `we`/`you`-voiced, never the `I` a thought is made of.
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
    // answer: the prompt never asks about these ids, and a model that answers
    // anyway cannot change who speaks prose.
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
    // two having to agree.
    //
    // A live run is why. The model listed four ids, agreed with itself on
    // three, and gave the fourth a character. Requiring agreement refused the
    // whole chapter, and the one repair pass made it worse — it dropped an
    // unrelated event — so the chapter failed outright over one ambiguous id.
    // A gate that can deadlock is worse than the mistake it prevents, which is
    // the same lesson `sound_design_gap` encodes.
    //
    // So the listing is taken as the decision and the speaker is written here,
    // exactly as narration's is. Which half the model actually meant is unknow-
    // able, and this way the cost of guessing wrong is one narrated span
    // rather than a refused chapter.
    for id in &not_speech {
        speakers.insert(id.clone(), "Narrator".to_string());
    }

    for event in &prepared.events {
        let speaker = speakers.get(&event.id).ok_or_else(|| {
            // The text goes in the complaint, not just the id. This string is
            // the whole of what the one repair pass is told, and a bare
            // `e0076` asks the model to act on a label it can no longer look
            // up — the prepared view is thousands of characters back in the
            // prompt by now. ch347 failed on exactly this: the model dropped
            // `e0076` ("A!"), was told only the id, dropped it again, and the
            // chapter burned every racer. Naming the words lets one repair
            // actually repair.
            anyhow::anyhow!(
                "attribution dropped source event {:?} — it is dialogue or a thought and reads {:?}; give it a speaker",
                event.id,
                crate::util::head_chars(&event.text, 80)
            )
        })?;
        // A dialogue event the model retracted is narration now, and is held
        // to the narration rule instead. The check below cannot read as
        // "dialogue assigned Narrator" for an id that was retracted, or every
        // legitimate retraction would be refused.
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
            // works if it can see the line it is being asked to fix. ch347 hit
            // this too, on a different id, in the same failing chapter.
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
/// validator understands, while preserving the description after the colon.
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
        // identity. The default is explicit and stable; later metadata may
        // change it without blocking the chapter.
        "adult male"
    };
    if hint.is_empty() {
        head.to_string()
    } else {
        format!("{head}: {hint}")
    }
}

/// Make the cast bookkeeping deterministic without touching the model's
/// semantic decisions.
///
/// A free model will occasionally omit a character from `new_characters`, use
/// `aliases` for `proper_aliases`, write `young female` instead of the six
/// accepted prefixes, or leave an empty object behind. None of those changes
/// who spoke the prepared events. The old behavior shelved every racer for them;
/// this pass repairs the metadata and leaves speaker/source validation strict.
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
            // against it, and the cast is keyed by the canonical name, so an
            // alias here resolves to no voice. Naming the canonical spelling
            // is what lets the repair converge, without it the model retries
            // the same alias until the racers give up on the chapter.
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
            // speaker list, while a `mentions` entry may name a character who is
            // only mentioned in this chapter and never speaks, which is
            // ordinary: ch22 names Vũ Kiệt in narration and nowhere else, so
            // there was no legal owner for the form until this check went.
            // `validate_context` has already refused an owner that no bible
            // character or declared new character claims, and it resolves an
            // alias through the bible on purpose.
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
/// alias** of a bible character into that character's canonical name.
///
/// The gate refused these, and refusing was expensive in a way that did not
/// match the size of the mistake: ch51 came back with `Ninh Huyền Vũ` where the
/// bible's canonical name is `Huyền Vũ lão tổ`, the person was right, the
/// spelling was the chapter's own, and the whole round was thrown away and
/// re-asked for it. A name that has exactly one canonical spelling is not a
/// fact the model has to get right — it is one the bible already knows.
///
/// **This corrects the answer, it does not relax the check.** Nothing here
/// excuses an error: `validate_digest_identity` still refuses an unknown name,
/// still refuses an ambiguous one, and still refuses everything else it refused
/// before. The pass only rewrites what the bible can resolve on its own, which
/// is why an ambiguous form — two characters claiming the same alias — is left
/// exactly as it was, to fail with the message that names both owners.
///
/// `Narrator` and the reserved anonymous speakers are never touched: neither is
/// a bible character, and both are legitimate names in their own right.
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
    // characters this very answer declares as new. A canonical name is never
    // rewritten, even when some other character lists it as an alias.
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
    // an answer that listed both spellings would otherwise carry one character
    // twice and be refused for a duplicate the rewrite itself created.
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
    // same roster. Missing this is how the first rewrite pass turned a refusal
    // into a different refusal: `segment 0: speaker "Ninh Huyền Vũ" is not a
    // canonical roster name`, with the roster already corrected above it.
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
///
/// The other half of the same class of mistake as [`canonicalize_aliases`].
/// `roster` is the join key the speaker map is resolved against, and the gate
/// refuses a line whose speaker is missing from it — but the roster is also
/// *derivable* from the answer: every name the answer assigns, plus `Narrator`
/// for the narration the preparer owns, is a name that speaks. ch51's `Được!`
/// line cost a whole round to a roster that simply forgot to list the
/// `Anonymous` it had just used.
///
/// **Only names the chapter can legitimately speak are added.** A name the
/// bible does not carry and the answer did not declare as new is left out, so it
/// still fails the canonical-name check with the message that says so: this
/// completes bookkeeping, it never admits a stranger, and it never rewrites
/// `speakers` itself.
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
    // never in `speakers` — but it is still in the chapter, and the gate checks
    // the roster for every event.
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
/// `canon_key`. Exactly one owner means the alias has a single legitimate
/// canonical spelling, which is what a repair message needs to be able to name.
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
