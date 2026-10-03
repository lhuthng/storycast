use super::prompts::prepared_event;
use super::prompts::PreparedChapter;
use super::prompts::PreparedEvent;
use super::*;
/// Split source paragraphs into dialogue and narration spans without changing
pub fn preview_split(text: &str) -> String {
    prepare_chapter(text).split_summary()
}

/// A quoted span too small to be a spoken line: `“rear palace”`, `“flower
fn is_quoted_term(span: &str) -> bool {
    let t = span.trim();
    if t.is_empty() || t.contains('\n') {
        return false;
    }
    if t.chars().any(|c| matches!(c, '.' | '!' | '?' | '…')) {
        return false;
    }
    if t.chars().count() > 24 {
        return false;
    }
    t.split_whitespace().count() <= 4
}

/// Prose glued into running text on the same line: `the “rear palace”:
/// `"Cacao," she replied` after `?"`) is speech changing hands, and so is a
/// quote with nothing before it on the line (`"Just leave it there."
/// Within, …`). Only a letter or digit touching the opener means the quote
/// never left the sentence.
///
/// A headline glued straight onto the quote (`Chapter 25: Wine "What
/// terrible news,"`) hands over too: without this the merged event would
/// start with the headline and the headline filter would drop the line with
/// it. Headlines further back don't count — sentence punctuation or a closed
/// quote since means real prose intervenes, and the merged event starts after
/// it.
fn embedded_in_prose(text: &str, opener_at: usize, after_closer: usize) -> bool {
    let line_start = text[..opener_at].rfind('\n').map(|p| p + 1).unwrap_or(0);
    let line_end = text[after_closer..]
        .find('\n')
        .map(|p| after_closer + p)
        .unwrap_or(text.len());
    let before = text[line_start..opener_at].trim_end();
    if before.is_empty() {
        return false;
    }
    if crate::assemble::is_headline(before)
        && !before.chars().any(|c| matches!(c, '.' | '!' | '?' | '…'))
        && !before.chars().any(|c| matches!(c, '"' | '”' | '」'))
    {
        return false;
    }
    if !before
        .chars()
        .next_back()
        .is_some_and(|c| c.is_alphanumeric())
    {
        return false;
    }
    !text[after_closer..line_end].trim().is_empty()
}

/// First-person markers and English second person: the `I`/`my`/`you` voice
const THOUGHT_MARKERS: &[&str] = &[
    "i",
    "i'd",
    "i'll",
    "i'm",
    "i've",
    "my",
    "me",
    "mine",
    "myself",
    "we",
    "us",
    "our",
    "ours",
    "let's",
    "you",
    "your",
    "yours",
    "yourself",
    "yourselves",
    "tôi",
    "tao",
    "tớ",
];

/// The markers that make a thought the thinker's own voice: first person
const FIRST_PERSON_SINGULAR: &[&str] = &[
    "i", "i'd", "i'll", "i'm", "i've", "my", "me", "mine", "myself",
];

/// Whether a passage is voiced first person singular, the same word-boundaried
pub(crate) fn first_person_singular(text: &str) -> bool {
    text.split_whitespace().any(|word| {
        let folded = word
            .trim_matches(|c: char| !(c.is_alphanumeric() || c == '\'' || c == '’'))
            .to_lowercase()
            .replace('’', "'");
        FIRST_PERSON_SINGULAR.contains(&folded.as_str())
    })
}

/// Whether a narration sentence is voiced `I`/`you`: an unquoted
fn is_thought_sentence(sentence: &str) -> bool {
    let mut commas = 0usize;
    for word in sentence.split_whitespace() {
        let folded = word
            .trim_matches(|c: char| !(c.is_alphanumeric() || c == '\'' || c == '’'))
            .to_lowercase()
            .replace('’', "'");
        if commas < 2 && THOUGHT_MARKERS.contains(&folded.as_str()) {
            return true;
        }
        commas += word.matches(',').count();
    }
    false
}

/// Split a narration run into sentences at `. ! ? …`, keeping the mark.
fn narration_sentences(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let (at, ch) = chars[i];
        if matches!(ch, '.' | '!' | '?' | '…') {
            let mut end = at + ch.len_utf8();
            while text[end..]
                .chars()
                .next()
                .is_some_and(|c| matches!(c, '.' | '!' | '?' | '…'))
            {
                end += text[end..].chars().next().unwrap().len_utf8();
            }
            // Only a boundary when whitespace or the end follows, so `3.5`
            if end >= text.len()
                || text[end..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_whitespace())
            {
                out.push((start, end));
                start = end;
            }
        }
        i += 1;
    }
    if start < text.len() {
        out.push((start, text.len()));
    }
    out
}

/// Carve unquoted first-person sentences out of narration as thought events
fn carve_thoughts(events: Vec<PreparedEvent>) -> Vec<PreparedEvent> {
    let mut sentences = 0usize;
    let mut marked = 0usize;
    for event in events.iter().filter(|e| e.kind == "narration") {
        for (from, to) in narration_sentences(&event.text) {
            sentences += 1;
            if is_thought_sentence(&event.text[from..to]) {
                marked += 1;
            }
        }
    }
    if sentences == 0 || (marked >= 3 && marked * 5 >= sentences) {
        return events;
    }
    let mut out: Vec<PreparedEvent> = Vec::with_capacity(events.len());
    for event in events {
        if event.kind != "narration" {
            out.push(event);
            continue;
        }
        let mut run_from: Option<usize> = None;
        for (from, to) in narration_sentences(&event.text) {
            if is_thought_sentence(&event.text[from..to]) {
                if let Some(rs) = run_from.take() {
                    if let Some(nar) = prepared_event(
                        out.len() + 1,
                        "narration",
                        &event.text[rs..from],
                        event.at + rs,
                        event.at + from,
                    ) {
                        out.push(nar);
                    }
                }
                if let Some(thought) = prepared_event(
                    out.len() + 1,
                    "thought",
                    &event.text[from..to],
                    event.at + from,
                    event.at + to,
                ) {
                    out.push(thought);
                }
            } else if run_from.is_none() {
                run_from = Some(from);
            }
        }
        if let Some(rs) = run_from {
            if let Some(nar) = prepared_event(
                out.len() + 1,
                "narration",
                &event.text[rs..],
                event.at + rs,
                event.end,
            ) {
                out.push(nar);
            }
        }
    }
    out
}

pub(crate) fn prepare_chapter(text: &str) -> PreparedChapter {
    // Older workspaces can contain raw HTML entities and Storya's promo/footer
    let text = crate::crawl::sanitize_chapter_text(text);
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut events = Vec::new();
    let mut quote: Option<(char, usize)> = None;
    let mut start = 0usize;
    let mut kind = "narration";

    let push = |from: usize, to: usize, kind: &str, events: &mut Vec<PreparedEvent>| {
        if from >= to {
            return;
        }
        let raw = &text[from..to];
        if let Some(event) = prepared_event(events.len() + 1, kind, raw, from, to) {
            events.push(event);
        }
    };

    let mut i = 0usize;
    // A pending opener whose span may be a quoted term: the narration before
    let mut pending: Option<(usize, usize)> = None;
    while i < chars.len() {
        let (at, ch) = chars[i];
        let is_open = ch == '"' || ch == '“' || ch == '「';
        let is_close = ch == '"' || ch == '”' || ch == '」';
        if is_open && quote.is_none() {
            quote = Some((ch, at));
            pending = Some((at, at + ch.len_utf8()));
            kind = "dialogue";
        } else if is_close && quote.is_some() {
            let (opener_at, inner_start) = pending.unwrap_or((at, at));
            let span = &text[inner_start..at];
            if is_quoted_term(span) && embedded_in_prose(&text, opener_at, at + ch.len_utf8()) {
                // A translated term, not speech: the delimiters stay in the
                kind = "narration";
            } else {
                push(start, opener_at, "narration", &mut events);
                push(inner_start, at, "dialogue", &mut events);
                start = at + ch.len_utf8();
                kind = "narration";
            }
            quote = None;
            pending = None;
        } else if quote.is_none() && ch == '\n' {
            push(start, at, kind, &mut events);
            start = at + ch.len_utf8();
        }
        i += 1;
    }
    // An unclosed opener splits like before: prose before it is narration,
    if let Some((opener_at, inner_start)) = pending {
        push(start, opener_at, "narration", &mut events);
        start = inner_start;
    }
    if start < text.len() {
        push(start, text.len(), kind, &mut events);
    }

    // Chapter headlines are spoken by the title renderer, not by the digest.
    let mut content = Vec::with_capacity(events.len());
    for event in events {
        if !crate::assemble::is_headline(&event.text) {
            content.push(event);
        }
    }
    // Thoughts carve after the headline filter: a heading can itself carry a
    let events = carve_thoughts(content);
    let mut events = events;
    for (i, event) in events.iter_mut().enumerate() {
        event.id = format!("e{:04}", i + 1);
    }

    let value: Vec<Value> = events
        .iter()
        .map(|e| json!({"id": e.id, "kind": e.kind, "text": e.text}))
        .collect();
    PreparedChapter {
        prompt_json: serde_json::to_string_pretty(&value).unwrap_or_else(|_| "[]".into()),
        events,
        // Decided before the headline filter, because it is a fact about the
        unbalanced_at: quote.map(|(_, at)| at),
    }
}
