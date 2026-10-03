use super::*;

/// The gender/age prefixes a `voice_hint` is allowed to start with.
const VOICE_HEADS: [&str; 6] = [
    "adult male",
    "adult female",
    "boy",
    "girl",
    "elderly male",
    "elderly female",
];

/// Inline non-verbal cues the VieNeu v3 Turbo emotion checkpoint renders as
const ALLOWED_INLINE_TAGS: [&str; 9] = [
    "cười",
    "chuckle",
    "cuoi",
    "thở dài",
    "sigh",
    "tho dai",
    "hắng giọng",
    "clear throat",
    "hang giong",
];

/// Bracketed spans in segment text, without the brackets.
pub(crate) fn inline_tags(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        let after = &rest[open + 1..];
        let Some(close) = after.find(']') else { break };
        out.push(after[..close].trim().to_string());
        rest = &after[close + 1..];
    }
    out
}

fn split_voice_head(hint: &str) -> String {
    hint.split([',', ':', '-', '–'])
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase()
}

/// Tags for one bible character entry: its `tags` field, or the voice_hint for
pub fn tags_of(entry: &Value) -> Vec<String> {
    let tags = normalise_tags(entry.get("tags"));
    if tags.is_empty() {
        let hint = entry
            .get("voice_hint")
            .and_then(|h| h.as_str())
            .unwrap_or("");
        crate::pool::tags_from_hint(hint)
    } else {
        tags
    }
}

/// Lowercase, deduped tags for a bible entry. Anything goes — the pool matches
pub(crate) fn normalise_tags(v: Option<&Value>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in v.and_then(|x| x.as_array()).cloned().unwrap_or_default() {
        let t = t.as_str().unwrap_or("").trim().to_lowercase();
        if !t.is_empty() && !t.contains(char::is_whitespace) && !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

/// Names a script may attribute to: the context pass's roster, `Narrator`, and
fn known_names(bible: &Value, context: &Value) -> Vec<String> {
    let mut known: Vec<String> = context
        .get("roster")
        .and_then(|r| r.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    known.push("Narrator".to_string());
    if let Some(chars) = bible.get("characters").and_then(|c| c.as_array()) {
        for c in chars {
            if let Some(n) = c.get("name").and_then(|n| n.as_str()) {
                known.push(n.to_string());
            }
        }
    }
    known
}

/// The context pass's answer: who is in this chapter, and what it is about.
pub fn validate_context(data: &Value, bible: &Value) -> Result<()> {
    if !data.is_object() {
        anyhow::bail!("top-level must be a JSON object");
    }
    let known = known_names(bible, data);

    if let Some(mentions) = data.get("mentions").and_then(|m| m.as_object()) {
        for (form, owner) in mentions {
            let owner = owner.as_str().unwrap_or("");
            if !known.iter().any(|k| k == owner)
                && !known.iter().any(|k| k == &resolve_speaker(bible, owner))
            {
                anyhow::bail!("mention {form:?} -> unknown {owner:?}");
            }
        }
    }

    if let Some(ncs) = data.get("new_characters").and_then(|c| c.as_array()) {
        for nc in ncs {
            if nc
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .is_empty()
            {
                anyhow::bail!("new_character without name");
            }
            let hint = nc.get("voice_hint").and_then(|h| h.as_str()).unwrap_or("");
            let head = split_voice_head(hint);
            if !VOICE_HEADS.contains(&head.as_str()) {
                anyhow::bail!(
                    "new_character {}: voice_hint must start with gender/age",
                    nc.get("name").and_then(|n| n.as_str()).unwrap_or("?")
                );
            }
            // Tags are what the sample pool rolls on; without them a character
            let Some(tags) = nc.get("tags").and_then(|t| t.as_array()) else {
                anyhow::bail!(
                    "new_character {}: missing tags array",
                    nc.get("name").and_then(|n| n.as_str()).unwrap_or("?")
                );
            };
            for t in tags {
                let s = t.as_str().unwrap_or("");
                if s.trim().is_empty() || s.contains(char::is_whitespace) {
                    anyhow::bail!(
                        "new_character {}: tags must be single tokens, got {t:?}",
                        nc.get("name").and_then(|n| n.as_str()).unwrap_or("?")
                    );
                }
            }
        }
    }
    Ok(())
}

/// The script pass's answer: the segments, and the three layers riding on them.
pub fn validate_script(
    data: &Value,
    bible: &Value,
    context: &Value,
    palette: &[String],
) -> Result<()> {
    if !data.is_object() {
        anyhow::bail!("top-level must be a JSON object");
    }
    let segments = data
        .get("segments")
        .and_then(|s| s.as_array())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("no segments"))?;
    let known = known_names(bible, context);

    let mut has_speakable_segment = false;
    for (i, s) in segments.iter().enumerate() {
        // A sound item is not a line and has nothing to validate here — but it
        if crate::util::is_sound_item(s) {
            continue;
        }
        let speaker = s.get("speaker").and_then(|v| v.as_str()).unwrap_or("");
        // A variant spelling that resolves to a known character is fine — the
        if !known.iter().any(|k| k == speaker)
            && !known.iter().any(|k| k == &resolve_speaker(bible, speaker))
        {
            anyhow::bail!("segment {i}: unknown speaker {speaker:?}");
        }
        let text = s.get("text").and_then(|t| t.as_str()).unwrap_or("");
        if text.is_empty() {
            anyhow::bail!("segment {i}: empty text");
        }
        // `kind` is code-attached, and `thought` is the only value code ever
        match s.get("kind") {
            None => {}
            Some(Value::String(kind)) if kind == "thought" => {}
            Some(other) => anyhow::bail!(
                "segment {i}: `kind` may only be \"thought\" (the marker the digest attaches \
                 to a thought event), got {other}"
            ),
        }
        has_speakable_segment |= crate::util::has_speakable_content(text);
        // Only the engine's three emotion cues may stand in brackets —
        for tag in inline_tags(text) {
            if !ALLOWED_INLINE_TAGS.contains(&tag.to_lowercase().as_str()) {
                anyhow::bail!(
                    "segment {i}: [{tag}] is not a voice tag ([cười]/[thở dài]/[hắng giọng] only)"
                );
            }
        }
    }
    if !has_speakable_segment {
        anyhow::bail!("script has no speakable content to render");
    }

    // The music field is the *only* thing that decides a track, so it is a
    let music: Vec<(usize, &str)> = segments
        .iter()
        .enumerate()
        .filter(|(_, s)| !crate::util::is_sound_item(s))
        .map(|(i, s)| {
            (
                i,
                s.get("music").and_then(|m| m.as_str()).unwrap_or("").trim(),
            )
        })
        .collect();
    if music.iter().any(|(_, m)| !m.is_empty()) && !palette.is_empty() {
        for (i, m) in music.iter() {
            if m.is_empty() {
                anyhow::bail!(
                    "segment {i}: missing `music` — when any segment declares one, every \
                     segment must (use \"none\" where silence is right)"
                );
            }
            if !palette.iter().any(|p| p == m) {
                anyhow::bail!(
                    "segment {i}: music {m:?} is not in the palette ({})",
                    palette.join(", ")
                );
            }
        }
    }

    // Every word of the chapter is spoken exactly once. Two adjacent segments
    let mut prev: Option<(usize, String, Option<String>, String)> = None;
    for (i, s) in segments.iter().enumerate() {
        if crate::util::is_sound_item(s) {
            continue;
        }
        let text = s
            .get("text")
            .and_then(|t| t.as_str())
            .map(crate::util::squeeze_ws)
            .unwrap_or_default();
        if text.is_empty() {
            continue;
        }
        let source = s
            .get("source_id")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let speaker = s
            .get("speaker")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if let Some((j, ref last, ref last_source, ref last_speaker)) = prev {
            if *last == text {
                match (last_source, source.as_deref()) {
                    (Some(a), Some(b)) if a == b => anyhow::bail!(
                        "segments {j} and {i} repeat source {a:?} word for word — a split partitions the line into two different halves, it never repeats the whole text"
                    ),
                    (Some(_), Some(_)) => {}
                    _ => {
                        // ponytail: chorus exception — two different voices saying the
                        if !(last_speaker != &speaker
                            && last_speaker != "Narrator"
                            && speaker != "Narrator")
                        {
                            anyhow::bail!(
                                "segments {j} and {i} speak the same line twice — a quoted line belongs ONLY to its speaker's segment, never also to a Narrator one"
                            )
                        }
                    }
                }
            }
        }
        prev = Some((i, text, source, speaker));
    }

    Ok(())
}

pub(crate) fn has_diacritic(word: &str) -> bool {
    word.chars().any(|c| VI_DIACRITICS.contains(c))
}

/// Effect tags ride on segments for a later merge pass to score; this one
pub fn validate_effect_tags(data: &Value, effect_tags: &[String]) -> Result<()> {
    let Some(segments) = data.get("segments").and_then(|s| s.as_array()) else {
        return Ok(());
    };
    for (i, s) in segments.iter().enumerate() {
        if crate::util::is_sound_item(s) {
            continue; // a sound item carries no effect tags
        }
        let Some(fx) = s.get("effect") else {
            continue;
        };
        let Some(arr) = fx.as_array() else {
            anyhow::bail!("segment {i}: `effect` must be an array of pool tags");
        };
        for t in arr {
            let t = t.as_str().unwrap_or("");
            if !effect_tags.iter().any(|e| e == t) {
                anyhow::bail!(
                    "segment {i}: effect tag {t:?} is not a pool tag ({})",
                    effect_tags.join(", ")
                );
            }
        }
    }
    Ok(())
}

/// A `hit` longer than this is refused: the silence it holds is a failed
const HIT_MAX_S: f64 = 8.0;

/// The `segments` array holds two kinds of item, and this is what keeps them
pub fn validate_injects(data: &Value, pool: &crate::audio_pool::ClipPool) -> Result<()> {
    let names = || pool.keys().cloned().collect::<Vec<_>>().join(", ");
    let Some(segments) = data.get("segments").and_then(|s| s.as_array()) else {
        return Ok(());
    };
    if data.get("injects").is_some() {
        anyhow::bail!(
            "`injects` is not a top-level array — a sound is its own item inside `segments`, \
             written between the two halves of the line it belongs to"
        );
    }
    for (i, s) in segments.iter().enumerate() {
        let names_a_sound = s.get("sound").is_some() || s.get("stop").is_some();
        if s.get("text").is_some() {
            if names_a_sound {
                anyhow::bail!(
                    "segment {i}: a line carries `sound` — a sound is not a field on a line. \
                     Split the line in two and put {{\"sound\": ...}} between the halves, so the \
                     renderer is never handed it"
                );
            }
            // `sound_after` / `stop_after` are how the *prompt* asks for a sound;
            for key in ["sound_after", "stop_after"] {
                if s.get(key).is_some() {
                    anyhow::bail!(
                        "segment {i}: `{key}` is a prompt-side field — it is lifted into its own \
                         sound item before the script is written, and a script that still carries \
                         one would merge with the sound missing"
                    );
                }
            }
            continue;
        }
        if !names_a_sound {
            anyhow::bail!(
                "segment {i}: not a line (needs `speaker` + `text`) and not a sound \
                 (needs `sound` or `stop`)"
            );
        }
        // A sound fires at the seam it sits in, so there has to be a seam: the
        if i == 0 || !segments[..i].iter().any(|p| p.get("text").is_some()) {
            anyhow::bail!(
                "segment {i}: a sound with no line before it has no seam to fire at — \
                 write the first half of the sentence first"
            );
        }
        if let Some(stop) = s.get("stop") {
            let stop = stop.as_str().unwrap_or("");
            if !pool.contains_key(stop) {
                anyhow::bail!(
                    "segment {i}: stop {stop:?} names no pooled sound ({})",
                    names()
                );
            }
            // A stop for a sound nothing started is silence with extra steps:
            let started = segments[..i]
                .iter()
                .any(|p| p.get("sound").and_then(|v| v.as_str()) == Some(stop));
            if !started {
                anyhow::bail!(
                    "segment {i}: stop {stop:?} has no earlier {{\"sound\": {stop:?}}} — nothing \
                     is running to fade. A bed needs both: the start where the scene opens and \
                     this stop where it moves on"
                );
            }
            continue;
        }
        let sound = s.get("sound").and_then(|v| v.as_str()).unwrap_or("");
        let Some(entry) = pool.get(sound) else {
            anyhow::bail!(
                "segment {i}: sound {sound:?} is not in the vocabulary ({})",
                names()
            );
        };
        // A sound item names a sound. `mode`/`hold`/`level` belong to the clip
        for key in ["mode", "hold", "level"] {
            if s.get(key).is_some() {
                anyhow::bail!(
                    "segment {i}: `{key}` is not the script's to set — a sound's behaviour lives \
                     with the sound in assets/inject-pool.json. Write {{\"sound\": {sound:?}}} and \
                     nothing else"
                );
            }
        }
        let mode = entry.mode.as_deref().unwrap_or("hit");
        if mode == "hit" {
            if let Some(d) = entry.dur_s {
                if d > HIT_MAX_S {
                    anyhow::bail!(
                        "segment {i}: {sound:?} is a {d:.0}s `hit` in the pool — that is {d:.0}s \
                         of dead air. Give it `\"mode\": \"overlap\"` in assets/inject-pool.json"
                    );
                }
            }
        }
    }
    Ok(())
}

/// The chapter's name, rewritten out of the machine-translated headline.
pub fn validate_title(data: &Value) -> Result<()> {
    let Some(raw) = data.get("title").and_then(|t| t.as_str()) else {
        anyhow::bail!("no `title` — every chapter needs a name (rule 13)");
    };
    let title = raw.trim();
    if title.is_empty() {
        anyhow::bail!("`title` is empty (rule 13)");
    }
    // The headline's own punctuation is the tell that it was copied through
    if let Some(c) = title.chars().find(|c| matches!(c, '?' | '!')) {
        anyhow::bail!(
            "title {title:?} carries a {c:?} — that is the machine-translated headline, \
             not a name (rule 13)"
        );
    }
    if title.starts_with("Chương") {
        anyhow::bail!("title {title:?} repeats the chapter number — the filename adds it");
    }
    let words = title.split_whitespace().count();
    if words < 2 {
        anyhow::bail!("title {title:?} is one word — name what the chapter is about");
    }
    if words > 10 {
        anyhow::bail!("title {title:?} is {words} words — a name, not a sentence (max 10)");
    }
    // The verbatim-copy check: the headline minus "Chương N:" is what the site
    if let Some(segs) = data.get("segments").and_then(|s| s.as_array()) {
        let head = segs
            .first()
            .and_then(|s| s.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("");
        if let Some((_, rest)) = head.split_once(':') {
            if rest.trim().trim_end_matches([' ', '.', '…']) == title {
                anyhow::bail!(
                    "title {title:?} is the crawled headline copied through — rewrite it (rule 13)"
                );
            }
        }
    }
    Ok(())
}
