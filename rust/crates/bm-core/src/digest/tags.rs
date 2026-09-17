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

/// `palette` is the closed music vocabulary from the scene map
/// (`ambience::palette_names`). Pass it empty to skip the music check — a map
/// that declares no palette cannot be used to judge a value.
pub fn validate(data: &Value, bible: &Value, palette: &[String]) -> Result<()> {
    if !data.is_object() {
        anyhow::bail!("top-level must be a JSON object");
    }
    let segments = data
        .get("segments")
        .and_then(|s| s.as_array())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("no segments"))?;

    let mut names: Vec<String> = data
        .get("roster")
        .and_then(|r| r.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    names.push("Narrator".to_string());
    let mut known = names.clone();
    if let Some(chars) = bible.get("characters").and_then(|c| c.as_array()) {
        for c in chars {
            if let Some(n) = c.get("name").and_then(|n| n.as_str()) {
                known.push(n.to_string());
            }
        }
    }

    for (i, s) in segments.iter().enumerate() {
        let speaker = s.get("speaker").and_then(|v| v.as_str()).unwrap_or("");
        // A variant spelling that resolves to a known character is fine — the
        // inductor canonicalizes the script on completion.
        if !names.iter().any(|n| n == speaker)
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
    let music: Vec<&str> = segments
        .iter()
        .map(|s| s.get("music").and_then(|m| m.as_str()).unwrap_or("").trim())
        .collect();
    if music.iter().any(|m| !m.is_empty()) && !palette.is_empty() {
        for (i, m) in music.iter().enumerate() {
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

fn has_diacritic(word: &str) -> bool {
    word.chars().any(|c| VI_DIACRITICS.contains(c))
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
        validate(&tagged("Hắn [cười]."), &json!({"characters": []}), &pal()).unwrap();
        validate(&tagged("Nàng [CƯỜI]."), &json!({"characters": []}), &pal()).unwrap();
        validate(&tagged("Hắn [sigh]."), &json!({"characters": []}), &pal()).unwrap();
        let err = validate(
            &tagged("Dừng [pause] lại."),
            &json!({"characters": []}),
            &pal(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("[pause]"), "{err}");
        // Invented tags are read aloud downstream — that is why they fail here.
        let err = validate(&tagged("Hắn [khóc]."), &json!({"characters": []}), &pal()).unwrap_err();
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
        let err = validate(&data, &json!({"characters": []}), &pal()).unwrap_err();
        assert!(err.to_string().contains("unknown speaker"), "{err}");
    }

    #[test]
    fn validate_ignores_direction_and_rejects_a_bad_voice_hint() {
        // `direction` used to be required ("Say ..."); nothing consumes it, so
        // it is neither required nor checked now — old scripts keep passing.
        let no_dir = json!({
            "segments": [{"speaker": "Narrator", "text": "hi"}],
            "roster": ["Narrator"]
        });
        validate(&no_dir, &json!({"characters": []}), &pal()).unwrap();

        let bad_hint = json!({
            "segments": [{"speaker": "Narrator", "text": "hi"}],
            "roster": ["Narrator"],
            "new_characters": [{"name": "X", "voice_hint": "mysterious"}]
        });
        let err = validate(&bad_hint, &json!({"characters": []}), &pal()).unwrap_err();
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
        validate(&data, &json!({"characters": []}), &pal()).unwrap();
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
        assert!(validate(&no_tags, &json!({"characters": []}), &pal()).is_err());

        // A sentence is not a tag.
        let mut sloppy = base();
        sloppy["new_characters"] =
            json!([{"name": "X", "voice_hint": "adult male, gruff", "tags": ["old man"]}]);
        let err = validate(&sloppy, &json!({"characters": []}), &pal()).unwrap_err();
        assert!(err.to_string().contains("single tokens"), "{err}");

        // `[]` is the honest answer for the ageless — and it validates.
        let mut ageless = base();
        ageless["new_characters"] =
            json!([{"name": "X", "voice_hint": "elderly male, flat", "tags": []}]);
        validate(&ageless, &json!({"characters": []}), &pal()).unwrap();
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

        validate(&one("quiet"), &bible, &pal()).unwrap();
        // `none` is a value, not an absence.
        validate(&one("none"), &bible, &pal()).unwrap();

        // Out of the palette: rejected, and the message names it so the repair
        // round has something to repair *to*.
        let err = validate(&one("melancholy"), &bible, &pal()).unwrap_err();
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
        let err = validate(&mixed, &bible, &pal()).unwrap_err();
        assert!(err.to_string().contains("missing `music`"), "{err}");

        // No value anywhere: a script from before the field existed. It still
        // validates, because the scene map's legacy shim gives it a mood.
        let legacy = json!({
            "segments": [{"speaker": "Narrator", "text": "x", "scene": "street-day"}],
            "roster": ["Narrator"]
        });
        validate(&legacy, &bible, &pal()).unwrap();

        // A map with no palette cannot judge a value, so it does not try.
        validate(&one("melancholy"), &bible, &[]).unwrap();
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
