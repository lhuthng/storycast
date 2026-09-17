use super::canon::{resolve_speaker, VI_DIACRITICS};
use anyhow::{anyhow, Result};
use serde_json::Value;

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
/// sound instead of speech — researched from the installed engine
/// (`vieneu_utils/phonemize_text.py`, `_EMOTION_TAG_TO_K`): exactly these
/// three, in English, Vietnamese and unaccented forms. Any other bracketed
/// span is phonemized as ORDINARY TEXT (read aloud!), so the digest may only
/// emit these, and validation below rejects the rest.
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
fn inline_tags(text: &str) -> Vec<String> {
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
/// entries written before tags existed — so the pool works without re-digesting
/// the whole book.
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
/// by equality — but each tag must be a non-empty token, not a sentence.
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
/// every character the bible already knows.
///
/// The bible stays in the list on purpose. The roster is what the script pass is
/// *told* to use, but a name the bible knows is a real character either way, and
/// the inductor canonicalizes the script on completion — failing a chapter over
/// a spelling the cast assigner can already resolve would trade a working
/// chapter for a tidier one.
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
///
/// There are no segments to check here — the script pass owns those — so this
/// covers only what the context pass is asked for. It runs before the script
/// pass is prompted at all, and that is the point: a cast list with a dangling
/// owner in it would otherwise be handed to the next pass as fact, and the next
/// pass has no way to tell.
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
            // can only ever draw preset voices. `[]` is valid (the ageless),
            // a missing key or a sentence is not.
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
///
/// `context` is the context pass's output for the same chapter — the roster it
/// resolved is the cast the script pass was told to attribute against, and it is
/// read here rather than from the script so a speaker the context pass never
/// listed is judged against the same list the prompt showed.
///
/// `palette` is the closed music vocabulary from the scene map
/// (`ambience::palette_names`). Pass it empty to skip the music check — a map
/// that declares no palette cannot be used to judge a value.
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

    for (i, s) in segments.iter().enumerate() {
        // A sound item is not a line and has nothing to validate here — but it
        // is also the one item that must never be spoken, so it is skipped
        // rather than defaulted into an empty speaker and an empty text.
        if crate::util::is_sound_item(s) {
            continue;
        }
        let speaker = s.get("speaker").and_then(|v| v.as_str()).unwrap_or("");
        // A variant spelling that resolves to a known character is fine — the
        // inductor canonicalizes the script on completion.
        if !known.iter().any(|k| k == speaker)
            && !known.iter().any(|k| k == &resolve_speaker(bible, speaker))
        {
            anyhow::bail!("segment {i}: unknown speaker {speaker:?}");
        }
        let text = s.get("text").and_then(|t| t.as_str()).unwrap_or("");
        if text.is_empty() {
            anyhow::bail!("segment {i}: empty text");
        }
        // Only the engine's three emotion cues may stand in brackets —
        // anything else is spoken aloud literally downstream.
        for tag in inline_tags(text) {
            if !ALLOWED_INLINE_TAGS.contains(&tag.to_lowercase().as_str()) {
                anyhow::bail!(
                    "segment {i}: [{tag}] is not a voice tag ([cười]/[thở dài]/[hắng giọng] only)"
                );
            }
        }
    }

    // The music field is the *only* thing that decides a track, so it is a
    // closed vocabulary rather than a hint: a value outside the palette is
    // rejected here, where the digest can still ask for a repair, instead of
    // being silently mixed down to nothing. A script where no segment declares
    // one at all predates the field — those keep merging through the legacy
    // shim, so the ~200 chapters already on disk are not stranded.
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

    Ok(())
}

fn has_diacritic(word: &str) -> bool {
    word.chars().any(|c| VI_DIACRITICS.contains(c))
}

/// Effect tags ride on segments for a later merge pass to score; this one
/// only checks they name real pool tags. Absent everywhere is an old digest
/// and still validates — the merge keeps scoring `scene` keywords until the
/// pool-tag path lands, so yesterday's scripts are not stranded.
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
/// chapter, not a bold choice. The number lives here rather than in the pool
/// so the rule reads the same for every sound — the pool's `dur_s` is the
/// measurement, this is the judgment.
const HIT_MAX_S: f64 = 8.0;

/// The `segments` array holds two kinds of item, and this is what keeps them
/// apart: a line (`speaker` + `text`) and a **sound** (`sound`, or `stop`).
///
/// The injection is "half a sentence, the sound, the other half" — so the sound
/// is written as its own item between the two halves, and it carries no `text`
/// at all. That is the whole reason for the shape: a renderer is handed the
/// lines, and there is no syntax in them to read, because the syntax was never
/// inside one. A `sound` key on a line is therefore not a near-miss to be
/// tolerated, it is the bug this replaced — and it would be read by nobody, so
/// the chapter would merge with the effect missing and never say so.
///
/// The merge is lenient (it skips what it cannot play), but the digest is
/// strict: a bad name here fails the chapter while the analyzer can still
/// repair it.
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
            // the pipeline lifts them into items before a script is written. One
            // surviving to here means something bypassed that, and a field read
            // by nobody is a chapter that merges without the sound and says
            // nothing — so it is refused rather than tolerated.
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
        // first half of the sentence it was written for.
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
            // `stop_actives` skips a sound that is not running, so the chapter
            // would merge without it and never say so. This is not hypothetical
            // — the analyzer's first two-pass run emitted exactly this, a lone
            // `{"stop": "cooking"}` and no start, and it validated.
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
        // and live in `assets/inject-pool.json` — the mixer reads them from
        // there and ignores them here, so one written into the script would be
        // a chapter that merges with behaviour nobody chose and never says so.
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
///
/// The crawled headline is a word-for-word Chinese→Vietnamese translation
/// (`Chương 9: Tê! Thật là khủng khiếp dao phay`, `Chương 10: Tiền bối đối với
/// dao phay yêu cầu đều cao như vậy?`) and it is what the mp3 is named after.
/// The digest has read the chapter, so it is the one place that can fix it —
/// which makes this the gate that keeps the fix from being a different flavour
/// of the same problem. Strict, like the rest of the digest: a bad title fails
/// the chapter while the analyzer can still repair it.
pub fn validate_title(data: &Value) -> Result<()> {
    let Some(raw) = data.get("title").and_then(|t| t.as_str()) else {
        anyhow::bail!("no `title` — every chapter needs a name (rule 13)");
    };
    let title = raw.trim();
    if title.is_empty() {
        anyhow::bail!("`title` is empty (rule 13)");
    }
    // The headline's own punctuation is the tell that it was copied through
    // rather than rewritten: a name is not a question or an exclamation.
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
    // gave us, and re-emitting it unchanged is the failure this rule exists for.
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

/// EN policy is trust-based; flag obvious violations for the review gate.
pub fn warn_vietnamese(data: &Value, bible: &Value) -> Vec<String> {
    let mut skip: Vec<String> = [
        "dich", "lac", "doan", "thanh", "nguyen", "tran", "ngo", "phong", "tuyet", "ly",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if let Some(chars) = bible.get("characters").and_then(|c| c.as_array()) {
        for c in chars {
            if let Some(name) = c.get("name").and_then(|n| n.as_str()) {
                skip.extend(name.to_lowercase().split_whitespace().map(String::from));
            }
            if let Some(aliases) = c.get("proper_aliases").and_then(|a| a.as_array()) {
                skip.extend(
                    aliases
                        .iter()
                        .filter_map(|a| a.as_str())
                        .map(|a| a.to_lowercase()),
                );
            }
        }
    }

    let looks_vi = |s: &str| -> bool {
        s.split(|c: char| !c.is_alphabetic())
            .filter(|w| !w.is_empty())
            .any(|w| has_diacritic(w) && !skip.iter().any(|s| s == &w.to_lowercase()))
    };

    let mut warns = Vec::new();
    if let Some(ncs) = data.get("new_characters").and_then(|c| c.as_array()) {
        for nc in ncs {
            let name = nc.get("name").and_then(|n| n.as_str()).unwrap_or("?");
            for key in ["personality", "voice_hint"] {
                let v = nc.get(key).and_then(|x| x.as_str()).unwrap_or("");
                if looks_vi(v) {
                    warns.push(format!(
                        "   WARN: {name}.{key} looks Vietnamese, expected English"
                    ));
                }
            }
        }
    }
    let atmosphere = data
        .get("atmosphere")
        .and_then(|a| a.as_str())
        .unwrap_or("");
    if looks_vi(atmosphere) {
        warns.push("   WARN: atmosphere looks Vietnamese, expected English".to_string());
    }
    warns
}

/// A written-out laugh word (lowercased): ha, haha, hắc, hô, khà.
fn is_laugh_word(w: &str) -> bool {
    matches!(w, "ha" | "haha" | "hắc" | "hô" | "khà")
}

/// Words that are only laughter in repetition: a lone `hô` is the verb "to
/// shout" (`hô to`, `xưng hô`), and lone `hắc`/`khà` are unobserved. `ha`
/// alone is always the scoff — it is not a Vietnamese word otherwise.
fn needs_company(w: &str) -> bool {
    matches!(w, "hô" | "hắc" | "khà")
}

/// Rewrite written-out non-verbal sounds into the engine's three tags.
/// `[cười]` for laughter, `[thở dài]` for Haizz, `[hắng giọng]` for coughs.
/// A tag replaces the literal, never accompanies it; at most one tag is
/// introduced per text (a second literal run is left for a human — deleting
/// spoken content silently is worse than a missed tag). Returns `None` when
/// nothing changes.
///
/// Deliberately untouched: Hừ (contempt — no tag fits), Ừm (a spoken
/// acknowledgment), exclamations (Ồ, Hả, Trời ơi — spoken words), tongue
/// clicks, and narration verbs. Only what the prompt's rule 9 names.
pub fn retag_text(text: &str) -> Option<String> {
    // A tag already present: only trim a matching literal run immediately
    // after it ("[cười] Ha ha ha..." → "[cười]"). Never add a second tag, and
    // never trim a different kind (`[hắng giọng] Hừ!` keeps its scoff).
    for (tag, kind) in [("[cười]", 0u8), ("[thở dài]", 1u8), ("[hắng giọng]", 2u8)] {
        if let Some(pos) = text.find(tag) {
            let after = pos + tag.len();
            let rest = &text[after..];
            let mut k = 0usize;
            while rest[k..].starts_with(' ') {
                k += 1;
            }
            if rest[k..].starts_with('"') {
                k += 1;
                while rest[k..].starts_with(' ') {
                    k += 1;
                }
            }
            let run = match kind {
                0 => match_sound_run(&rest[k..], Sound::Laugh),
                1 => match_sound_run(&rest[k..], Sound::Sigh),
                _ => match_sound_run(&rest[k..], Sound::Cough),
            };
            if let Some(len) = run {
                let rs = k;
                let mut te = k + len;
                while rest[te..].starts_with([' ', '\t', '.', ',', '…', '!', ';', ':']) {
                    te += rest[te..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
                }
                let mut out = text.to_string();
                if rest[te..]
                    .trim_matches([' ', '\t', '.', ',', '…', '!', ';', ':', '"', '”'])
                    .is_empty()
                {
                    // The whole remainder was the laugh (maybe quoted): drop
                    // it all rather than stranding a dangling quote.
                    out.truncate(after);
                } else {
                    out.replace_range(after + rs..after + te, "");
                }
                return Some(out);
            }
            return None;
        }
    }
    // No tag: convert the first literal run found, if any.
    let mut best: Option<(usize, usize, &str)> = None;
    for (tag, matcher) in [
        ("[cười]", Sound::Laugh),
        ("[thở dài]", Sound::Sigh),
        ("[hắng giọng]", Sound::Cough),
    ] {
        if let Some((start, len)) = find_sound_run(text, matcher) {
            if best.map(|(s, _, _)| start < s).unwrap_or(true) {
                best = Some((start, len, tag));
            }
        }
    }
    let (start, len, tag) = best?;
    // Trim spaces before the run (one separates the tag from prose, unless
    // the run opens the text or follows an opening quote), and punctuation
    // plus spaces after it (the tag carries the tone now).
    let mut from = start;
    while from > 0 && text[..from].ends_with(' ') {
        from -= 1;
    }
    let sep = if from == 0 || text[..from].ends_with(['"', '“', '(', '[']) {
        ""
    } else {
        " "
    };
    let mut end = start + len;
    while text[end..].starts_with([' ', '\t', '.', ',', '…', '!', ';', ':']) {
        end += text[end..]
            .chars()
            .next()
            .map(|c| c.len_utf8())
            .unwrap_or(1);
    }
    // The consumed trailing space is gone: re-separate when the remainder
    // starts with a word character (but never before a closing quote).
    let rest = &text[end..];
    let gap = if rest.is_empty() || rest.starts_with(['"', '”', ')', ']', '?']) {
        ""
    } else {
        " "
    };
    let mut out = String::with_capacity(text.len() + 8);
    out.push_str(&text[..from]);
    out.push_str(sep);
    out.push_str(tag);
    out.push_str(gap);
    out.push_str(rest);
    Some(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sound {
    Laugh,
    Sigh,
    Cough,
}

/// Byte length of the sound run starting at `s` (words only, no trailing
/// punctuation), or `None`. Word boundaries on both sides: `ha` inside
/// `hai` (number two) or `aha` (eureka) never matches.
fn match_sound_run(s: &str, kind: Sound) -> Option<usize> {
    let chars: Vec<(usize, char)> = s.char_indices().collect();
    let n = chars.len();
    let mut i = 0usize;
    // An optional standalone `a` before a ha-run ("A ha ha, ...").
    let word_at = |i: usize| -> Option<(String, usize, usize)> {
        if i >= n || !chars[i].1.is_alphabetic() {
            return None;
        }
        let mut j = i;
        let mut w = String::new();
        while j < n && chars[j].1.is_alphabetic() {
            for c in chars[j].1.to_lowercase() {
                w.push(c);
            }
            j += 1;
        }
        Some((w, chars[i].0, if j < n { chars[j].0 } else { s.len() }))
    };
    // Leading `a` only counts when a laugh word follows it.
    if kind == Sound::Laugh {
        if let Some((w, _, wend)) = word_at(0) {
            if w == "a" {
                let mut k = wend;
                while k < s.len() && s[k..].starts_with(' ') {
                    k += 1;
                }
                if let Some((w2, _, _)) = word_at(byte_idx(&chars, k, s.len())) {
                    if is_laugh_word(&w2) {
                        i = byte_idx(&chars, k, s.len());
                    } else {
                        return None;
                    }
                } else {
                    return None;
                }
            }
        }
    }
    let mut consumed = 0usize;
    let mut words = 0u32;
    let mut first = String::new();
    loop {
        // Skip single spaces between words.
        let mut k = i;
        if words > 0 {
            if k < s.len() && s[k..].starts_with(' ') {
                k += 1;
            } else {
                break;
            }
        }
        let ci = byte_idx(&chars, k, s.len());
        let Some((w, _, wend)) = word_at(ci) else {
            break;
        };
        let ok = match kind {
            Sound::Laugh => is_laugh_word(&w),
            Sound::Sigh => {
                let mut c = w.chars();
                matches!((c.next(), c.next()), (Some('h'), Some('a')))
                    && w[2..].chars().all(|c| c == 'i' || c == 'z')
                    && w[2..].contains('z')
            }
            Sound::Cough => w == "khụ",
        };
        if !ok {
            break;
        }
        if words == 0 {
            first = w;
        }
        words += 1;
        consumed = wend;
        i = wend;
    }
    if words == 0 {
        return None;
    }
    // A lone hô/hắc/khà is prose, not laughter.
    if words == 1 && kind == Sound::Laugh && needs_company(&first) {
        return None;
    }
    Some(consumed)
}

/// Char-index of byte offset `b` (clamped to the end).
fn byte_idx(chars: &[(usize, char)], b: usize, len: usize) -> usize {
    if b >= len {
        return chars.len();
    }
    chars
        .iter()
        .position(|(off, _)| *off >= b)
        .unwrap_or(chars.len())
}

/// Earliest `(byte start, byte len)` of a sound run anywhere in `text`,
/// with a non-alphabetic boundary (or string edge) on both sides.
fn find_sound_run(text: &str, kind: Sound) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < text.len() {
        // Candidate starts: string start or right after a non-alphabetic.
        let boundary = if i == 0 {
            true
        } else {
            text[..i]
                .chars()
                .next_back()
                .map(|c| !c.is_alphabetic())
                .unwrap_or(true)
        };
        if boundary {
            if let Some(len) = match_sound_run(&text[i..], kind) {
                // Trailing boundary: end of string or non-alphabetic next.
                let end_ok = text[i + len..]
                    .chars()
                    .next()
                    .map(|c| !c.is_alphabetic())
                    .unwrap_or(true);
                if end_ok {
                    return Some((i, len));
                }
            }
        }
        // Advance one char (ASCII fast path keeps the common case cheap).
        i += if bytes[i] < 0x80 {
            1
        } else {
            text[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1)
        };
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A stand-in palette: the shipped map's values, which is what the digest
    /// passes in. Tests that care about the music check pass their own.
    fn pal() -> Vec<String> {
        ["quiet", "warm", "busy", "battle", "grand", "none"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    fn bible_with(name: &str, aliases: &[&str]) -> Value {
        json!({"characters": [{
            "name": name,
            "personality": "x",
            "voice_hint": "adult male",
            "proper_aliases": aliases,
            "first_seen": "01",
            "chapters_seen": []
        }]})
    }

    /// The cast pass's output for a script under test: the roster it declares.
    /// `validate_script` attributes against this, not against the script itself.
    fn ctx_of(script: &Value) -> Value {
        json!({"roster": script.get("roster").cloned().unwrap_or(json!([]))})
    }

    #[test]
    fn inline_tags_accept_the_engine_three_and_nothing_else() {
        assert_eq!(inline_tags("Hắn [cười] lớn."), vec!["cười"]);
        assert_eq!(inline_tags("[thở dài] Rồi đi."), vec!["thở dài"]);
        assert!(inline_tags("Không có gì.").is_empty());
        assert_eq!(inline_tags("a [b] c [d]"), vec!["b", "d"]);

        let tagged = |text: &str| {
            json!({
                "segments": [{"speaker": "Narrator", "text": text, "direction": "Say calm in Vietnamese: x"}],
                "roster": ["Narrator"]
            })
        };
        let ok = |t: &str| {
            let d = tagged(t);
            validate_script(&d, &json!({"characters": []}), &ctx_of(&d), &pal())
        };
        ok("Hắn [cười].").unwrap();
        ok("Nàng [CƯỜI].").unwrap();
        ok("Hắn [sigh].").unwrap();
        let err = ok("Dừng [pause] lại.").unwrap_err();
        assert!(err.to_string().contains("[pause]"), "{err}");
        // Invented tags are read aloud downstream — that is why they fail here.
        let err = ok("Hắn [khóc].").unwrap_err();
        assert!(err.to_string().contains("voice tag"), "{err}");
    }

    #[test]
    fn retag_text_converts_laughs_sighs_and_coughs() {
        // Real shapes from the corpus.
        assert_eq!(
            retag_text("Ha ha ha!"),
            Some("[cười]".into()),
            "a bare laugh becomes a bare tag"
        );
        assert_eq!(
            retag_text("\"Ha ha, khách sáo quá.\""),
            Some("\"[cười] khách sáo quá.\"".into())
        );
        assert_eq!(
            retag_text("Thật à, ta đúng là kỳ tài ngút trời ha ha..."),
            Some("Thật à, ta đúng là kỳ tài ngút trời [cười]".into())
        );
        assert_eq!(
            retag_text("\"A ha ha, đã lâu không gặp.\""),
            Some("\"[cười] đã lâu không gặp.\"".into())
        );
        assert_eq!(
            retag_text("\"Hắc hắc, tới đây!\""),
            Some("\"[cười] tới đây!\"".into())
        );
        assert_eq!(retag_text("Hô hô hô hô."), Some("[cười]".into()));
        assert_eq!(
            retag_text("Haizz, đứa trẻ này..."),
            Some("[thở dài] đứa trẻ này...".into())
        );
        assert_eq!(
            retag_text("\"Khụ khụ, thôi không được đâu.\""),
            Some("\"[hắng giọng] thôi không được đâu.\"".into())
        );
        // Tag plus literal collapses to the tag.
        assert_eq!(
            retag_text("[cười] Ha ha ha. Cứ kêu đi!"),
            Some("[cười] Cứ kêu đi!".into())
        );
        assert_eq!(
            retag_text("[cười] \"Ha ha, khách sáo quá.\""),
            Some("[cười] \"khách sáo quá.\"".into())
        );
        // A tag already present blocks a second one: no change at all.
        assert_eq!(retag_text("[cười] Haizz..."), None);
    }

    #[test]
    fn retag_text_leaves_everything_else_alone() {
        // Contempt, acknowledgments, exclamations, clicks, verbs: no tag fits.
        for t in [
            "Hừ!",
            "Thanh Sơn lão tổ hừ lạnh một tiếng.",
            "Ừm!",
            "\"Ừm...\" Mậu Mậu gãi đầu.",
            "Ồ, đến rồi!",
            "Trời ơi!",
            "\"Hả?\"",
            "Dịch Phong khẽ tặc lưỡi.",
            "Lạc Lan Tuyết hít sâu một hơi, nghiêm túc nói:",
            "như trút được gánh nặng, thở phào nhẹ nhõm",
            "Không có gì đặc biệt.",
        ] {
            assert_eq!(retag_text(t), None, "{t:?} must not change");
        }
        // Word-boundary discipline: `hai` (two) and `aha` (eureka) are words.
        assert_eq!(retag_text("mang thêm hai cái ghế ra đây."), None);
        assert_eq!(retag_text("Aha, ra vậy!"), None);
        // A lone `hô` is the verb "to shout", not laughter — only repetition counts.
        assert_eq!(retag_text("Mọi người hô to."), None);
        assert_eq!(retag_text("cách xưng hô của ngươi."), None);
        assert_eq!(retag_text("Hô hô hô hô."), Some("[cười]".into()));
    }

    #[test]
    fn validate_rejects_a_speaker_outside_the_roster() {
        let data = json!({
            "segments": [{"speaker": "Ghost", "text": "hi", "direction": "Say calm in Vietnamese: hi"}],
            "roster": ["Narrator"]
        });
        let err = validate_script(&data, &json!({"characters": []}), &ctx_of(&data), &pal())
            .unwrap_err();
        assert!(err.to_string().contains("unknown speaker"), "{err}");
        // ...and the same script passes when the cast pass listed the speaker.
        let listed = json!({"roster": ["Narrator", "Ghost"]});
        validate_script(&data, &json!({"characters": []}), &listed, &pal()).unwrap();
    }

    #[test]
    fn validate_ignores_direction_and_rejects_a_bad_voice_hint() {
        // `direction` used to be required ("Say ..."); nothing consumes it, so
        // it is neither required nor checked now — old scripts keep passing.
        let no_dir = json!({
            "segments": [{"speaker": "Narrator", "text": "hi"}],
            "roster": ["Narrator"]
        });
        validate_script(&no_dir, &json!({"characters": []}), &ctx_of(&no_dir), &pal()).unwrap();

        let bad_hint = json!({
            "segments": [{"speaker": "Narrator", "text": "hi"}],
            "roster": ["Narrator"],
            "new_characters": [{"name": "X", "voice_hint": "mysterious"}]
        });
        let err = validate_context(&bad_hint, &json!({"characters": []})).unwrap_err();
        assert!(err.to_string().contains("gender/age"), "{err}");
    }

    #[test]
    fn validate_accepts_a_well_formed_digest() {
        let data = json!({
            "atmosphere": "A market at dawn.",
            "roster": ["Narrator", "Dịch Phong"],
            "mentions": {"hắn": "Dịch Phong"},
            "new_characters": [{"name": "Lão Trần", "voice_hint": "elderly male, gruff", "tags": ["old", "male"]}],
            "segments": [{"speaker": "Narrator", "text": "Trời sáng.", "direction": "Say calm in Vietnamese: Trời sáng."}]
        });
        // The cast pass owns identity and the script pass owns the speech, so a
        // well-formed digest is two answers and each half is checked by its own.
        validate_context(&data, &json!({"characters": []})).unwrap();
        validate_script(&data, &json!({"characters": []}), &ctx_of(&data), &pal()).unwrap();
    }

    #[test]
    fn validate_rejects_a_missing_or_sloppy_tags_array() {
        let base = || {
            json!({
                "segments": [{"speaker": "Narrator", "text": "hi", "direction": "Say calm in Vietnamese: hi"}],
                "roster": ["Narrator"],
            })
        };
        // Missing key entirely.
        let mut no_tags = base();
        no_tags["new_characters"] = json!([{"name": "X", "voice_hint": "adult male, gruff"}]);
        assert!(validate_context(&no_tags, &json!({"characters": []})).is_err());

        // A sentence is not a tag.
        let mut sloppy = base();
        sloppy["new_characters"] =
            json!([{"name": "X", "voice_hint": "adult male, gruff", "tags": ["old man"]}]);
        let err = validate_context(&sloppy, &json!({"characters": []})).unwrap_err();
        assert!(err.to_string().contains("single tokens"), "{err}");

        // `[]` is the honest answer for the ageless — and it validates.
        let mut ageless = base();
        ageless["new_characters"] =
            json!([{"name": "X", "voice_hint": "elderly male, flat", "tags": []}]);
        validate_context(&ageless, &json!({"characters": []})).unwrap();
    }

    #[test]
    fn validate_closes_the_music_vocabulary_and_keeps_old_scripts_mergeable() {
        let one = |music: &str| {
            json!({
                "segments": [{"speaker": "Narrator", "text": "x", "music": music}],
                "roster": ["Narrator"]
            })
        };
        let bible = json!({"characters": []});
        let check = |music: &str| {
            let d = one(music);
            validate_script(&d, &bible, &ctx_of(&d), &pal())
        };

        check("quiet").unwrap();
        // `none` is a value, not an absence.
        check("none").unwrap();

        // Out of the palette: rejected, and the message names it so the repair
        // round has something to repair *to*.
        let err = check("melancholy").unwrap_err();
        assert!(err.to_string().contains("palette"), "{err}");
        assert!(err.to_string().contains("quiet"), "{err}");

        // Half-declared is rejected: the field is a statement about every
        // segment, or about none of them.
        let mixed = json!({
            "segments": [
                {"speaker": "Narrator", "text": "x", "music": "quiet"},
                {"speaker": "Narrator", "text": "y"}
            ],
            "roster": ["Narrator"]
        });
        let err = validate_script(&mixed, &bible, &ctx_of(&mixed), &pal()).unwrap_err();
        assert!(err.to_string().contains("missing `music`"), "{err}");

        // No value anywhere: a script from before the field existed. It still
        // validates, because the scene map's legacy shim gives it a mood.
        let legacy = json!({
            "segments": [{"speaker": "Narrator", "text": "x", "scene": "street-day"}],
            "roster": ["Narrator"]
        });
        validate_script(&legacy, &bible, &ctx_of(&legacy), &pal()).unwrap();

        // A map with no palette cannot judge a value, so it does not try.
        let d = one("melancholy");
        validate_script(&d, &bible, &ctx_of(&d), &[]).unwrap();
    }

    #[test]
    fn validate_effect_tags_accepts_pool_tags_and_old_digests() {
        let fx = ["rain".to_string(), "night".to_string()];
        let seg = |effect: Value| {
            json!({
                "segments": [{"speaker": "Narrator", "text": "x", "effect": effect}],
                "roster": ["Narrator"]
            })
        };
        validate_effect_tags(&seg(json!(["rain", "night"])), &fx).unwrap();
        validate_effect_tags(&seg(json!([])), &fx).unwrap();
        // No `effect` anywhere: a digest from before the field existed.
        validate_effect_tags(
            &json!({
                "segments": [{"speaker": "Narrator", "text": "x"}],
                "roster": ["Narrator"]
            }),
            &fx,
        )
        .unwrap();
        let err = validate_effect_tags(&seg(json!(["rain", "thunderstorm"])), &fx).unwrap_err();
        assert!(err.to_string().contains("thunderstorm"), "{err}");
        let err = validate_effect_tags(&seg(json!("rain")), &fx).unwrap_err();
        assert!(err.to_string().contains("must be an array"), "{err}");
    }

    #[test]
    fn validate_injects_accepts_sound_items_and_refuses_bad_names_and_long_hits() {
        use crate::audio_pool::{ClipPool, Sound};
        let mk = |dur: f64, mode: &str| Sound {
            tags: vec![],
            files: vec!["injects/x.mp3".into()],
            looped: false,
            dur_s: Some(dur),
            mode: Some(mode.into()),
            hold: None,
            level: None,
        };
        let pool: ClipPool = [
            ("blood", mk(1.1, "hit")),
            ("boil", mk(51.0, "overlap")),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        // A line, then the sound items that follow it. Every sound below sits
        // where the script would have written one: between two halves.
        let doc = |sounds: Value| {
            let mut items = vec![json!({"speaker": "Narrator", "text": "Hắn vung kiếm."})];
            items.extend(sounds.as_array().cloned().unwrap_or_default());
            items.push(json!({"speaker": "Narrator", "text": "Rồi hắn gục xuống."}));
            json!({"segments": items, "roster": ["Narrator"]})
        };
        // A sound is a name and nothing else; a stop is a name and nothing else.
        // The stop has to have something running to fade, so its start comes
        // first in the array — see the orphan-stop test below.
        validate_injects(
            &doc(json!([{"sound": "boil"}, {"sound": "blood"}, {"stop": "boil"}])),
            &pool,
        )
        .unwrap();
        validate_injects(&doc(json!([])), &pool).unwrap();
        validate_injects(
            &json!({"segments": [{"speaker": "Narrator", "text": "x"}]}),
            &pool,
        )
        .unwrap();
        // Unknown sounds, in either shape.
        let err = validate_injects(&doc(json!([{"sound": "thunder"}])), &pool).unwrap_err();
        assert!(err.to_string().contains("thunder"), "{err}");
        let err = validate_injects(&doc(json!([{"stop": "thunder"}])), &pool).unwrap_err();
        assert!(err.to_string().contains("thunder"), "{err}");
        // Behaviour is not the script's to set. All three keys, by name — each
        // one is read by nobody, so a chapter would merge with a behaviour
        // nobody chose and never say so.
        for key in ["mode", "hold", "level"] {
            let mut item = json!({"sound": "blood"});
            item[key] = if key == "mode" {
                json!("trail")
            } else {
                json!(2.0)
            };
            let err = validate_injects(&doc(json!([item])), &pool).unwrap_err();
            assert!(
                err.to_string().contains("not the script's to set"),
                "{key}: {err}"
            );
        }
        // A pool entry that says `hit` on a 51 s clip is a pool bug, and the
        // message says which file to fix.
        let bad: ClipPool = [("boil", mk(51.0, "hit"))]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        let err = validate_injects(&doc(json!([{"sound": "boil"}])), &bad).unwrap_err();
        assert!(err.to_string().contains("inject-pool.json"), "{err}");
        // An item has to be one thing or the other.
        let err = validate_injects(
            &json!({"segments": [
                {"speaker": "Narrator", "text": "x"},
                {"mood": "angry"},
            ]}),
            &pool,
        )
        .unwrap_err();
        assert!(err.to_string().contains("not a line"), "{err}");
        // A sound with no line before it has no seam to fire at.
        let err = validate_injects(
            &json!({"segments": [
                {"sound": "blood"},
                {"speaker": "Narrator", "text": "x"},
            ]}),
            &pool,
        )
        .unwrap_err();
        assert!(err.to_string().contains("seam"), "{err}");
        // The rejected shape, refused by name: a sound field on a line is read
        // by nobody, so the chapter would merge without the sound and say
        // nothing — and worse, a renderer could be handed the line as speech.
        let err = validate_injects(
            &json!({"segments": [{"speaker": "Narrator", "text": "x", "sound": "blood"}]}),
            &pool,
        )
        .unwrap_err();
        assert!(err.to_string().contains("not a field on a line"), "{err}");
        // ...and so is the top-level array it used to live in.
        let err = validate_injects(
            &json!({
                "segments": [{"speaker": "Narrator", "text": "x"}],
                "injects": [{"sound": "blood"}],
            }),
            &pool,
        )
        .unwrap_err();
        assert!(err.to_string().contains("not a top-level array"), "{err}");
    }

    /// The prompt-side sound fields must not survive onto a written script.
    ///
    /// `sound_after` / `stop_after` are how the prompt asks for a sound; the
    /// pipeline lifts them into items. One reaching validation means something
    /// bypassed the expansion, and a field on a line is read by nobody — the
    /// chapter would merge with the sound missing and never say so.
    #[test]
    fn validate_injects_refuses_a_prompt_side_sound_field() {
        use crate::audio_pool::{ClipPool, Sound};
        let pool: ClipPool = [(
            "page-turn",
            Sound {
                tags: vec![],
                files: vec!["injects/page-turn.mp3".into()],
                looped: false,
                dur_s: Some(0.6),
                mode: Some("overlap".into()),
                hold: None,
                level: None,
            },
        )]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        for key in ["sound_after", "stop_after"] {
            let mut line = json!({"speaker": "Narrator", "text": "x"});
            line[key] = json!("page-turn");
            let err = validate_injects(
                &json!({"segments": [line], "roster": ["Narrator"]}),
                &pool,
            )
            .unwrap_err();
            assert!(
                err.to_string().contains("prompt-side field"),
                "{key}: {err}"
            );
        }
    }

    /// A `stop` for a sound nothing started is silence with extra steps.
    ///
    /// This is not a hypothetical: it is the exact shape the analyzer returned
    /// on the first two-round run — `{"stop": "cooking"}` and no start — and it
    /// passed validation, because a stop was only ever checked against the pool.
    /// `stop_actives` skips a sound that is not running, so the chapter merged
    /// with the bed missing and said nothing. A bed is a pair or it is nothing.
    #[test]
    fn validate_injects_refuses_a_stop_with_nothing_to_stop() {
        use crate::audio_pool::{ClipPool, Sound};
        let mk = |dur: f64, mode: &str| Sound {
            tags: vec![],
            files: vec!["injects/x.mp3".into()],
            looped: true,
            dur_s: Some(dur),
            mode: Some(mode.into()),
            hold: None,
            level: None,
        };
        let pool: ClipPool = [("cooking", mk(22.0, "overlap"))]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        let doc = |items: Value| json!({"segments": items, "roster": ["Narrator"]});
        let line = |t: &str| json!({"speaker": "Narrator", "text": t});

        // The exact failure: a stop, and no start anywhere before it.
        let err = validate_injects(
            &doc(json!([
                line("Sau một hồi cảm khái, hai người liền đi đến phòng bếp."),
                {"stop": "cooking"},
                line("Đồ nhi, tâm cảnh của tiền bối thật đáng để chúng ta học hỏi!"),
            ])),
            &pool,
        )
        .unwrap_err();
        assert!(err.to_string().contains("nothing is running"), "{err}");

        // The pair, and it passes.
        validate_injects(
            &doc(json!([
                line("Sau một hồi cảm khái, hai người liền đi đến phòng bếp."),
                {"sound": "cooking"},
                line("Đồ nhi, tâm cảnh của tiền bối thật đáng để chúng ta học hỏi!"),
                {"stop": "cooking"},
            ])),
            &pool,
        )
        .unwrap();

        // Order matters: a stop written *before* its start is the same silence.
        let err = validate_injects(
            &doc(json!([
                line("Một."),
                {"stop": "cooking"},
                {"sound": "cooking"},
                line("Hai."),
            ])),
            &pool,
        )
        .unwrap_err();
        assert!(err.to_string().contains("nothing is running"), "{err}");
    }

    #[test]
    fn validate_title_takes_a_name_and_refuses_the_crawled_headline() {
        let doc = |t: &str| {
            json!({
                "title": t,
                "segments": [{"speaker": "Narrator", "text": "Chương 9: Tê! Thật là khủng khiếp dao phay"}],
            })
        };
        validate_title(&doc("Thần Binh Dao Phay")).unwrap();
        validate_title(&doc("Thanh Sơn Kinh Hồn")).unwrap();
        // The two real headlines, refused. Both are word-for-word MT of the
        // Chinese title with the sentence punctuation still attached.
        let err = validate_title(&doc("Tê! Thật là khủng khiếp dao phay")).unwrap_err();
        assert!(err.to_string().contains("machine-translated"), "{err}");
        let err =
            validate_title(&doc("Tiền bối đối với dao phay yêu cầu đều cao như vậy?")).unwrap_err();
        assert!(err.to_string().contains("machine-translated"), "{err}");
        // Copied through unchanged is still copied through, even when the
        // headline happens to be punctuated like a name.
        let clean = |t: &str| {
            json!({
                "title": t,
                "segments": [{"speaker": "Narrator", "text": "Chương 9: Thần binh dao phay"}],
            })
        };
        let err = validate_title(&clean("Thần binh dao phay")).unwrap_err();
        assert!(err.to_string().contains("copied through"), "{err}");
        validate_title(&clean("Thần Binh Dao Phay")).unwrap();
        // Absent, empty, numbered, a single word, and a sentence.
        let err = validate_title(&json!({"segments": []})).unwrap_err();
        assert!(err.to_string().contains("no `title`"), "{err}");
        let err = validate_title(&doc("   ")).unwrap_err();
        assert!(err.to_string().contains("empty"), "{err}");
        let err = validate_title(&doc("Chương 9 Thần Binh")).unwrap_err();
        assert!(err.to_string().contains("chapter number"), "{err}");
        let err = validate_title(&doc("Dao")).unwrap_err();
        assert!(err.to_string().contains("one word"), "{err}");
        let err = validate_title(&doc(
            "một hai ba bốn năm sáu bảy tám chín mười mười một",
        ))
        .unwrap_err();
        assert!(err.to_string().contains("not a sentence"), "{err}");
    }

    #[test]
    fn vietnamese_leak_detection_ignores_known_names() {
        let bible = bible_with("Lạc Lan Tuyết", &["Tuyết"]);
        let data = json!({
            "atmosphere": "A cold morning in the courtyard.",
            "new_characters": [{"name": "Lạc Lan Tuyết", "personality": "lạnh lùng", "voice_hint": "adult female"}]
        });
        let warns = warn_vietnamese(&data, &bible);
        assert!(
            warns.iter().any(|w| w.contains("personality")),
            "expected a personality warning: {warns:?}"
        );
        // the name itself must not trip the detector
        assert!(
            !warns.iter().any(|w| w.contains("atmosphere")),
            "English atmosphere flagged: {warns:?}"
        );
    }
}
