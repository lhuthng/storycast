use super::validate::has_diacritic;
use super::*;

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
fn needs_company(w: &str) -> bool {
    matches!(w, "hô" | "hắc" | "khà")
}

/// Rewrite written-out non-verbal sounds into the engine's three tags.
pub fn retag_text(text: &str) -> Option<String> {
    // A tag already present: only trim a matching literal run immediately
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
                1 => ["một hơi rồi", "một tiếng", "một hơi"]
                    .iter()
                    .find_map(|suffix| rest[k..].strip_prefix(suffix).map(|_| k + suffix.len()))
                    .or_else(|| match_sound_run(&rest[k..], Sound::Sigh)),
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
fn match_sound_run(s: &str, kind: Sound) -> Option<usize> {
    if kind == Sound::Sigh {
        // The corpus spells a sigh both as the engine tag's own name and as a
        for phrase in [
            "thở dài một hơi rồi",
            "thở dài một tiếng",
            "thở dài một hơi",
            "thở dài",
        ] {
            let matches = s
                .get(..phrase.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(phrase));
            let boundary = s
                .get(phrase.len()..)
                .and_then(|rest| rest.chars().next())
                .map(|ch| !ch.is_alphabetic())
                .unwrap_or(true);
            if matches && boundary {
                return Some(phrase.len());
            }
        }
    }

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
