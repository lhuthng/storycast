use super::*;

pub(crate) fn strip_fences(raw: &str) -> &str {
    let s = raw.trim().trim_start_matches('\u{feff}');
    let s = s.strip_prefix("```json").unwrap_or(s);
    let s = s.strip_suffix("```").unwrap_or(s);
    s.trim()
}

/// Parse model-produced JSON, repairing only a small, well-understood set of
/// `"` inside a `text` value puts the scanner outside the string and every
/// later newline is emitted raw; escaping quotes afterwards moves that
/// boundary again. Alternating the repairs until neither changes anything is
/// the only order that converges on such input.
pub(crate) fn parse_json_repaired(input: &str) -> Result<Value> {
    // The two shapes a model produces for one object: the object itself with a
    // few raw marks in it, and the object escaped a level too deep (its own
    // closing quote escaped with it, so the last string never ends).
    if let Some(value) = repair_once(input) {
        return Ok(value);
    }
    let unescaped = unescape_json_string(input);
    if unescaped != input {
        if let Some(value) = repair_once(&unescaped) {
            return Ok(value);
        }
    }
    let error = serde_json::from_str::<Value>(input).expect_err("the fast path above rejected it");
    Err(json_failure(&error))
}

/// The repair ladder for one candidate: quotes and control characters, each
/// fixing what the other needs to see, until serde is satisfied.
fn repair_once(input: &str) -> Option<Value> {
    // **Order matters, and it is quotes first, control characters second.**
    //
    // Escaping a control character needs to know which quotes open a string, so
    // an unescaped `"` inside a value puts that scanner outside the string and
    // each quote repair is worse than useless: an escaped quote reads as "a
    // literal quote inside a string", so the scanner never sees the string
    let mut candidate = input.to_string();
    for _ in 0..256 {
        let error = match serde_json::from_str(&candidate) {
            Ok(value) => return Some(value),
            Err(error) => error,
        };
        // Each complaint is handed to the repair that owns it, and the loop
        // re-reads the fresh error the previous repair produced. Breaking on
        // the first control character is what lost a chapter: its answer also
        // carried raw marks, and the quote pass never got to see them.
        let complaint = error.to_string();
        let repaired = if complaint.contains("control character") {
            escape_json_control_chars(&candidate)
        } else if complaint.contains("trailing comma") {
            remove_json_trailing_commas(&candidate)
        } else {
            String::new()
        };
        if !repaired.is_empty() {
            if repaired == candidate {
                return None;
            }
            candidate = repaired;
            continue;
        }
        // A **fresh** parse every time, because serde's line and column belong
        let error_offset = json_error_offset(&candidate, &error)?;
        let quote = nearest_unescaped_quote(&candidate, error_offset)?;
        candidate.insert(quote, '\\');
    }

    // The scanner treats paired literal quotes as balanced, which is also how
    let candidate = remove_json_trailing_commas(&candidate);
    serde_json::from_str(&candidate).ok()
}

/// The text a JSON string's own escapes stand for: the object a model returned
/// escaped one level too deep.
fn unescape_json_string(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('/') => out.push('/'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// The parse error a model can act on.
pub(crate) fn json_failure(error: &serde_json::Error) -> anyhow::Error {
    let raw = error.to_string();
    let remedy = if raw.contains("control character") {
        " — a raw newline, tab or carriage return was written inside a string value; \
         write them escaped as \\n, \\t and \\r"
    } else if raw.contains("expected value") {
        " — the output is not a JSON object at all; answer with the object alone, no prose \
         and no code fence"
    } else if raw.contains("trailing comma") {
        " — a comma was left before a closing brace or bracket"
    } else if raw.contains("EOF") || raw.contains("end of file") {
        " — the answer was cut off; return the whole object"
    } else {
        ""
    };
    anyhow::anyhow!("{raw}{remedy}")
}

/// Byte offset reported by serde_json (line and column are one-based; column is
fn json_error_offset(input: &str, error: &serde_json::Error) -> Option<usize> {
    let line_start = if error.line() == 1 {
        0
    } else {
        input.match_indices('\n').nth(error.line() - 2)?.0 + 1
    };
    let offset = line_start.checked_add(error.column().checked_sub(1)?)?;
    (offset <= input.len()).then_some(offset)
}

/// The nearest quote before `end` that is worth escaping.
/// a value holds a raw `"` *and* a raw newline: the scanner is out of sync, so
/// the first complaint can land anywhere, and the nearest quote is then often
/// the wrong one.
fn nearest_unescaped_quote(input: &str, end: usize) -> Option<usize> {
    let mut end = end.min(input.len());
    while let Some(quote) = input[..end].rfind('"') {
        let backslashes = input[..quote]
            .bytes()
            .rev()
            .take_while(|byte| *byte == b'\\')
            .count();
        if backslashes % 2 == 0 {
            let closes_a_key = input[quote + 1..]
                .chars()
                .find(|c| !c.is_whitespace())
                .is_some_and(|c| c == ':');
            if !closes_a_key {
                return Some(quote);
            }
        }
        end = quote;
    }
    None
}

fn escape_json_control_chars(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut in_string = false;
    let mut escaped = false;
    for ch in input.chars() {
        if !in_string {
            output.push(ch);
            in_string = ch == '"';
            continue;
        }
        if escaped {
            output.push(ch);
            escaped = false;
        } else if ch == '\\' {
            output.push(ch);
            escaped = true;
        } else if ch == '"' {
            output.push(ch);
            in_string = false;
        } else if ch == '\n' {
            output.push_str("\\n");
        } else if ch == '\r' {
            output.push_str("\\r");
        } else if ch == '\t' {
            output.push_str("\\t");
        } else if ch == '\u{08}' {
            output.push_str("\\b");
        } else if ch == '\u{0c}' {
            output.push_str("\\f");
        } else if (ch as u32) < 0x20 {
            use std::fmt::Write as _;
            let _ = write!(output, "\\u{:04x}", ch as u32);
        } else {
            output.push(ch);
        }
    }
    output
}

fn remove_json_trailing_commas(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut in_string = false;
    let mut escaped = false;
    for (index, ch) in input.char_indices() {
        if in_string {
            output.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        if ch == '"' {
            in_string = true;
            output.push(ch);
        } else if ch == ',' {
            let next = input[index + ch.len_utf8()..]
                .chars()
                .find(|next| !next.is_whitespace());
            if matches!(next, Some(']') | Some('}')) {
                continue;
            }
            output.push(ch);
        } else {
            output.push(ch);
        }
    }
    output
}

// ---------------------------------------------------------------------------
