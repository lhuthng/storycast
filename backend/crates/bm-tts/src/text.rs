//! Turning raw text into phoneme chunks.

/// Slack at the usual ceiling; scaled down for small ceilings.
const CHUNK_TAIL_SLACK: usize = 15;

/// Words that read as a break point. Cutting before one is more natural than
const CONN_WORDS: &[&str] = &[
    "và", "nhưng", "hoặc", "song", "rồi", "nên", "vì", "nếu", "khi", "để", "do", "bởi",
];

/// Two-word connectors. A cut inside a pair (`sau | khi`) or right after the
const CONN_PAIRS: &[(&str, &str)] = &[
    ("sau", "khi"),
    ("trước", "khi"),
    ("trong", "khi"),
    ("mỗi", "khi"),
    ("đến", "khi"),
    ("tới", "khi"),
    ("cho", "nên"),
    ("cho", "đến"),
    ("bởi", "vì"),
    ("nếu", "như"),
    ("tuy", "nhiên"),
    ("thế", "nhưng"),
    ("vì", "vậy"),
    ("vì", "thế"),
    ("do", "đó"),
    ("sau", "đó"),
];

/// Stripped from a token before it is compared to the connector lists.
const CONN_STRIP: &str = "\"'“”‘’()[]«»…";

/// Punctuation that stays attached to the emotion token before it.
const ATTACHING_PUNCT: &[char] = &[
    '.', ',', '!', '?', ';', ':', '…', ')', ']', '}', '"', '\'', '’', '”',
];

/// Bracketed cues that mean a non-verbal sound, and which token each maps to.
pub(crate) fn emotion_token_k(tag: &str) -> Option<&'static str> {
    let t = tag.trim();
    if !t.starts_with('[') || !t.ends_with(']') {
        // An already-resolved token, or not a bracket at all. The caller passes
        return None;
    }
    let inner = t[1..t.len() - 1].trim().to_lowercase();
    match inner.as_str() {
        "chuckle" | "cười" | "cuoi" => Some("<|emotion_1|>"),
        "sigh" | "thở dài" | "tho dai" => Some("<|emotion_2|>"),
        "clear throat" | "hắng giọng" | "hang giong" => Some("<|emotion_3|>"),
        _ => None,
    }
}

/// A bracketed span or an already-resolved emotion token.
pub(crate) fn is_emotion_span(s: &str) -> bool {
    if s.starts_with("[") && s.ends_with("]") && s.len() >= 2 {
        return true;
    }
    if s.starts_with("<|emotion_") && s.ends_with("|>") {
        // `<|emotion_` is ten characters, so the digits start at 10.
        return s[10..s.len() - 2].chars().all(|c| c.is_ascii_digit());
    }
    false
}

/// Turn normalized paragraphs into one chunk per sentence, retaining the
pub(crate) fn sentence_chunks(paragraphs: Vec<Vec<String>>) -> (Vec<String>, Vec<String>) {
    let mut chunks = Vec::new();
    let mut gaps = Vec::new();
    for sentences in paragraphs {
        let mut first = true;
        for sentence in sentences {
            if sentence.trim().is_empty() {
                continue;
            }
            if !chunks.is_empty() {
                gaps.push(if first {
                    "para".into()
                } else {
                    "sentence".into()
                });
            }
            chunks.push(sentence);
            first = false;
        }
    }
    (chunks, gaps)
}

/// Split on emotion spans, keeping them — the captured-group behaviour of the
pub(crate) fn split_emotions(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '[' {
            if let Some(end) = chars[i + 1..].iter().position(|c| *c == ']') {
                out.push(std::mem::take(&mut cur));
                out.push(chars[i..i + end + 2].iter().collect());
                i += end + 2;
                continue;
            }
        } else if chars[i] == '<'
            && chars[i..].starts_with(&['<', '|', 'e', 'm', 'o', 't', 'i', 'o', 'n', '_'])
        {
            let rest: String = chars[i..].iter().collect();
            if let Some(bar) = rest.find("|>") {
                let span: String = chars[i..i + bar + 2].iter().collect();
                if is_emotion_span(&span) {
                    out.push(std::mem::take(&mut cur));
                    out.push(span);
                    i += bar + 2;
                    continue;
                }
            }
        }
        cur.push(chars[i]);
        i += 1;
    }
    out.push(cur);
    out
}

pub use chunks::{classify_gap, pack_sentences_into_chunks, split_sentences};
pub use frontend::FrontEnd;
mod chunks;
mod frontend;
