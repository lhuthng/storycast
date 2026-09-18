//! Turning raw text into phoneme chunks.
//!
//! A port of `vieneu_utils.phonemize_text` and the chunking half of
//! `vieneu_utils.core_utils`. This is the last stage before the generator, and
//! the one with the most accumulated judgement in it — almost every function here
//! exists because some earlier version produced bad audio.
//!
//! # The ordering rule
//!
//! **Split sentences on the raw text, then normalize.** Not the other way round.
//! sea-g2p's normalizer rewrites `"…"` and `(…)` into commas, so after
//! normalization there is no way to tell a `?` that ends a sentence from a `?`
//! inside a quotation. Doing it in the wrong order turns
//!
//! ```text
//! Có phải … kiểu như: "Rồi sao nữa? Mình phải làm đến bao giờ?", đúng không anh?
//! ```
//!
//! into three fragments, the last of which begins with a comma.
//!
//! # Two ceilings, and only one of them is hard
//!
//! `max_chars` is **relative**. A trailing piece shorter than the slack still
//! joins the open chunk even though it pushes past the ceiling — otherwise a
//! two-word fragment like `phương.` ends up alone and then gets glued to the
//! following sentence, which sounds worse than a chunk 15 characters too long.

use anyhow::{Context, Result};
use sea_g2p_rs::g2p::G2PEngine;
use sea_g2p_rs::lang::vi::Normalizer;
use sea_g2p_rs::punc::apply_punc_norm;

/// Slack at the usual ceiling; scaled down for small ceilings.
const CHUNK_TAIL_SLACK: usize = 15;

/// Words that read as a break point. Cutting before one is more natural than
/// cutting at the ceiling.
const CONN_WORDS: &[&str] = &[
    "và", "nhưng", "hoặc", "song", "rồi", "nên", "vì", "nếu", "khi", "để", "do", "bởi",
];

/// Two-word connectors. A cut inside a pair (`sau | khi`) or right after the
/// first word of one (`cho | đến khi`) reads as a stumble, so both are blocked.
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
/// The checkpoint was trained with three cues embedded in the phoneme stream, so
/// an unrecognised bracket is ordinary text, not a cue.
fn emotion_token_k(tag: &str) -> Option<&'static str> {
    let t = tag.trim();
    if !t.starts_with('[') || !t.ends_with(']') {
        // An already-resolved token, or not a bracket at all. The caller passes
        // those through; nothing here should try to read inside them.
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
fn is_emotion_span(s: &str) -> bool {
    if s.starts_with("[") && s.ends_with("]") && s.len() >= 2 {
        return true;
    }
    if s.starts_with("<|emotion_") && s.ends_with("|>") {
        // `<|emotion_` is ten characters, so the digits start at 10.
        return s[10..s.len() - 2].chars().all(|c| c.is_ascii_digit());
    }
    false
}

/// Split on emotion spans, keeping them — the captured-group behaviour of the
/// reference's `re.split`, so odd indices are the spans.
///
/// **Every piece is pushed, including empty ones.** `re.split` with a capture
/// group returns the text before the first group even when that text is empty,
/// and the trailing piece even when it is empty. Dropping the empties shifts
/// every span from an odd index to an even one, and the callers here read spans
/// by parity — so a cue at the start of a line would be treated as ordinary text
/// and normalized away instead of becoming a token.
fn split_emotions(text: &str) -> Vec<String> {
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

/// Normalize and phonemize one string, the way `SEAPipeline.run` does: `punc_norm`
/// applies at the **normalizer**, and the G2P is called with its default of off.
pub struct FrontEnd {
    normalizer: Normalizer,
    g2p: G2PEngine,
}

impl FrontEnd {
    /// `dict_path` is the 60 MB `sea_g2p.bin`. The normalizer is deliberately
    /// built **without** it — see `VENDORED.md`; passing it changes the output.
    pub fn new(dict_path: &str) -> Result<FrontEnd> {
        Ok(FrontEnd {
            normalizer: Normalizer::new("vi", None),
            g2p: G2PEngine::new(dict_path).context("loading the sea-g2p dictionary")?,
        })
    }

    /// Normalize then phonemize. `punc_norm` is applied at the normalizer only.
    pub fn phonemize(&self, text: &str, punc_norm: bool) -> String {
        let normalized = self.normalizer.normalize(text, punc_norm);
        self.g2p.phonemize(&normalized)
    }

    /// Phonemize while keeping inline non-verbal cues as emotion tokens.
    ///
    /// The fragments *between* cues are phonemized with `punc_norm` off, and the
    /// whole string gets its final mark once at the end. Normalizing each
    /// fragment would insert a full stop mid-sentence and lose the intonation the
    /// cue was there to carry.
    pub fn phonemize_with_emotions(&self, text: &str) -> String {
        if !text.contains('[') && !text.contains("<|emotion_") {
            return self.phonemize(text, true);
        }
        let parts = split_emotions(text);
        let mut out = String::new();
        for (i, part) in parts.iter().enumerate() {
            if i % 2 == 1 {
                let token = emotion_token_k(part).map(|t| t.to_string()).or_else(|| {
                    if part.starts_with("<|emotion_") && is_emotion_span(part) {
                        Some(part.trim().to_string())
                    } else {
                        None
                    }
                });
                if let Some(t) = token {
                    if out.is_empty() {
                        out = t;
                    } else {
                        out.push(' ');
                        out.push_str(&t);
                    }
                    continue;
                }
            }
            let ph = if part.trim().is_empty() {
                String::new()
            } else {
                self.phonemize(part, false)
            };
            if ph.is_empty() {
                continue;
            }
            if out.is_empty() {
                out = ph;
            } else if ph
                .chars()
                .next()
                .is_some_and(|c| ATTACHING_PUNCT.contains(&c))
            {
                out.push_str(&ph);
            } else {
                out.push(' ');
                out.push_str(&ph);
            }
        }
        apply_punc_norm(&out)
    }

    /// The chunker: `(chunks, gaps)`, where `gaps[i]` describes the boundary
    /// between `chunks[i]` and `chunks[i + 1]`.
    pub fn chunks(&self, text: &str, max_chars: usize, min_chunk_chars: usize) -> Chunks {
        if text.is_empty() {
            return Chunks::default();
        }
        let keep_cues = text.contains('[') || text.contains("<|emotion_");
        let mut chunks: Vec<String> = Vec::new();
        let mut gaps: Vec<String> = Vec::new();

        for sentences in self.normalized_sentences_by_para(text, keep_cues) {
            let para_chunks = pack_sentences_into_chunks(&sentences, max_chars);
            if para_chunks.is_empty() {
                continue;
            }
            if !chunks.is_empty() {
                gaps.push("para".into());
            }
            for (j, ch) in para_chunks.into_iter().enumerate() {
                if j > 0 {
                    gaps.push("sentence".into());
                }
                chunks.push(ch);
            }
        }

        chunks = chunks.iter().map(|c| apply_punc_norm(c)).collect();
        // A `para` boundary stays a paragraph; the rest are re-read from the
        // chunk's own final mark, because `punc_norm` may have changed it.
        gaps = gaps
            .iter()
            .enumerate()
            .map(|(i, g)| {
                if g == "para" {
                    "para".to_string()
                } else {
                    classify_gap(&chunks[i]).to_string()
                }
            })
            .collect();

        merge_short_chunks(chunks, gaps, min_chunk_chars)
    }

    /// Raw text to paragraphs of normalized sentences.
    ///
    /// Sentence splitting happens on the **raw** text and normalization after,
    /// per the module doc. Lengths for packing are measured *after*
    /// normalization, so a chunk does not grow when the normalizer expands text
    /// (`100$` becomes `một trăm u s d`).
    fn normalized_sentences_by_para(&self, text: &str, keep_cues: bool) -> Vec<Vec<String>> {
        let mut out = Vec::new();
        for para in text.split(['\r', '\n']).filter(|p| !p.trim().is_empty()) {
            let sentences = split_sentences(para);
            if sentences.is_empty() {
                continue;
            }
            if keep_cues {
                out.push(
                    sentences
                        .iter()
                        .map(|s| self.normalize_sentence_keep_cues(s))
                        .collect(),
                );
            } else {
                out.push(
                    sentences
                        .iter()
                        .map(|s| self.normalizer.normalize(s, false))
                        .collect(),
                );
            }
        }
        out
    }

    /// Normalize one sentence, keeping inline cues as tokens.
    fn normalize_sentence_keep_cues(&self, sentence: &str) -> String {
        if !sentence.contains('[') && !sentence.contains("<|emotion_") {
            return self.normalizer.normalize(sentence, false);
        }
        let parts = split_emotions(sentence);
        let mut kept: Vec<String> = Vec::new();
        for (i, part) in parts.iter().enumerate() {
            if i % 2 == 1 {
                let tok = emotion_token_k(part).map(|t| t.to_string()).or_else(|| {
                    if part.starts_with("<|emotion_") {
                        Some(part.trim().to_string())
                    } else {
                        None
                    }
                });
                kept.push(tok.unwrap_or_else(|| part.clone()));
            } else if !part.trim().is_empty() {
                kept.push(self.normalizer.normalize(part, false));
            }
        }
        kept.into_iter()
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[derive(Debug, Default, Clone)]
pub struct Chunks {
    pub chunks: Vec<String>,
    pub gaps: Vec<String>,
}

// ── sentence splitting ──────────────────────────────────────────────────────

/// Opening brackets and the closing bracket each expects. Single quotes are
/// deliberately absent: they double as apostrophes, so they would open a span
/// that never closes and swallow the rest of the text.
const OPEN_TO_CLOSE: &[(char, char)] = &[
    ('(', ')'),
    ('[', ']'),
    ('{', '}'),
    ('“', '”'),
    ('‘', '’'),
    ('«', '»'),
    ('‹', '›'),
    ('「', '」'),
    ('『', '』'),
];

fn is_opener(c: char) -> bool {
    OPEN_TO_CLOSE.iter().any(|(o, _)| *o == c)
}

fn is_closer(c: char) -> bool {
    OPEN_TO_CLOSE.iter().any(|(_, cl)| *cl == c)
}

fn is_sentence_end(c: char) -> bool {
    matches!(c, '.' | '!' | '?' | '…')
}

/// A closing mark that belongs to the sentence it follows: `bao giờ?"`, `(thế à!)`.
fn is_trailing_close(c: char) -> bool {
    is_closer(c) || matches!(c, '"' | '\'' | '’' | '”')
}

/// Split into sentences, not cutting inside brackets or quotations.
///
/// An unbalanced bracket would swallow everything after it into one enormous
/// sentence, so the scan runs again ignoring brackets if the first pass ends
/// unbalanced.
pub fn split_sentences(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let (sentences, balanced) = scan_sentences(text, true);
    if !balanced {
        return scan_sentences(text, false).0;
    }
    sentences
}

/// One pass, cutting at `.!?…` that is outside brackets and quotes.
fn scan_sentences(text: &str, quote_aware: bool) -> (Vec<String>, bool) {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut sentences: Vec<String> = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    let mut depth = 0i32;
    let mut in_quote = false;

    while i < n {
        let ch = chars[i];
        if quote_aware && ch == '"' {
            in_quote = !in_quote;
        } else if quote_aware && is_opener(ch) {
            depth += 1;
        } else if quote_aware && is_closer(ch) {
            if depth > 0 {
                depth -= 1;
            }
        } else if is_sentence_end(ch) && depth == 0 && !in_quote {
            let mut j = i + 1;
            while j < n && is_sentence_end(chars[j]) {
                j += 1; // swallow "?!", "..."
            }
            while j < n && is_trailing_close(chars[j]) {
                j += 1; // and a closing mark stuck to it
            }
            // Only a boundary when whitespace or the end follows — which is what
            // keeps "3.5 triệu" and "8.30 sáng" whole.
            if j >= n || chars[j].is_whitespace() {
                sentences.push(chars[start..j].iter().collect());
                start = j;
                i = j;
                continue;
            }
            i = j;
            continue;
        }
        i += 1;
    }
    if start < n {
        sentences.push(chars[start..].iter().collect());
    }
    let cleaned: Vec<String> = sentences
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    (cleaned, depth == 0 && !in_quote)
}

// ── packing sentences into chunks ───────────────────────────────────────────

fn tail_slack(max_chars: usize) -> usize {
    CHUNK_TAIL_SLACK.min(max_chars / 8)
}

/// Does `add_len` more characters (plus a space) fit?
fn fits(cur_len: usize, add_len: usize, max_chars: usize) -> bool {
    let total = if cur_len > 0 {
        cur_len + 1 + add_len
    } else {
        add_len
    };
    let slack = tail_slack(max_chars);
    total <= max_chars || (add_len <= slack && total <= max_chars + slack)
}

fn conn_key(token: &str) -> String {
    token
        .trim_matches(|c| CONN_STRIP.contains(c))
        .to_lowercase()
}

fn is_conn_pair(a: &str, b: &str) -> bool {
    CONN_PAIRS.iter().any(|(x, y)| *x == a && *y == b)
}

/// Split on a comma, semicolon, colon or dash followed by whitespace.
///
/// The reference uses a lookbehind; Rust's `regex` has none, and the rule is a
/// scan anyway — split at a whitespace run whose preceding character is one of
/// those marks, dropping the whitespace.
fn split_minor_punct(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            let prev = cur.chars().last();
            let is_mark = prev.is_some_and(|c| matches!(c, ',' | ';' | ':' | '-' | '–' | '—'));
            if is_mark {
                out.push(std::mem::take(&mut cur));
                while i < chars.len() && chars[i].is_whitespace() {
                    i += 1;
                }
                continue;
            }
        }
        cur.push(chars[i]);
        i += 1;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Tokens, with every `<en>...</en>` span kept whole.
///
/// The reference is `<en>.*?</en>|\S+`. Note the alternation order matters: at a
/// position that does *not* start a tag, `\S+` matches the whole run, so
/// `x<en>a</en>` is one token, not three.
fn tokenize_keep_en(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }
        if starts_en_tag(&chars, i) {
            if let Some(end) = find_close_en(&chars, i + 4) {
                out.push(chars[i..end].iter().collect());
                i = end;
                continue;
            }
        }
        let start = i;
        while i < chars.len() && !chars[i].is_whitespace() {
            i += 1;
        }
        out.push(chars[start..i].iter().collect());
    }
    out
}

fn starts_en_tag(chars: &[char], i: usize) -> bool {
    chars.len() >= i + 4
        && chars[i] == '<'
        && chars[i + 1].eq_ignore_ascii_case(&'e')
        && chars[i + 2].eq_ignore_ascii_case(&'n')
        && chars[i + 3] == '>'
}

fn find_close_en(chars: &[char], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 5 <= chars.len() {
        if chars[i] == '<'
            && chars[i + 1] == '/'
            && chars[i + 2].eq_ignore_ascii_case(&'e')
            && chars[i + 3].eq_ignore_ascii_case(&'n')
            && chars[i + 4] == '>'
        {
            return Some(i + 5);
        }
        i += 1;
    }
    None
}

/// A cut point that lands on a connector, scanned back from the ceiling.
///
/// Scanning backwards prefers the fullest chunk. It stops as soon as the left
/// piece would be shorter than `min_left`, because a chunk that is too small
/// loses the benefit of packing at all.
fn natural_cut(words: &[String], start: usize, end: usize, min_left: usize) -> Option<usize> {
    let mut left: usize = words[start..end]
        .iter()
        .map(|w| w.chars().count())
        .sum::<usize>()
        + (end - start - 1);
    for j in (start + 1..end).rev() {
        left -= words[j].chars().count() + 1;
        if left < min_left {
            return None;
        }
        let key = conn_key(&words[j]);
        let prev = conn_key(&words[j - 1]);
        if is_conn_pair(&prev, &key) || CONN_WORDS.contains(&prev.as_str()) {
            // Inside a pair, or right after another connector — keep scanning.
            continue;
        }
        let nxt = if j + 1 < words.len() {
            conn_key(&words[j + 1])
        } else {
            String::new()
        };
        if CONN_WORDS.contains(&key.as_str()) || is_conn_pair(&key, &nxt) {
            return Some(j);
        }
    }
    None
}

/// Break a piece with no punctuation left to cut on, by word.
fn split_long_part(part: &str, max_chars: usize) -> Vec<String> {
    let words = tokenize_keep_en(part);
    let min_left = max_chars / 2;
    let mut pieces: Vec<String> = Vec::new();
    let mut start = 0usize;

    while start < words.len() {
        let mut end = start;
        let mut length = 0usize;
        while end < words.len() {
            let w = words[end].chars().count();
            let add = if end > start { length + 1 + w } else { w };
            if end > start && add > max_chars {
                break;
            }
            length = add;
            end += 1;
        }
        if end < words.len() {
            let rest: usize = words[end..]
                .iter()
                .map(|w| w.chars().count())
                .sum::<usize>()
                + (words.len() - end - 1);
            if fits(length, rest, max_chars) {
                // The remainder is too short to stand alone — take it too.
                pieces.push(words[start..].join(" "));
                break;
            }
            match natural_cut(&words, start, end, min_left) {
                Some(cut) => end = cut,
                None => {
                    // Even a ceiling cut must not land inside a connector pair.
                    while end > start + 1
                        && is_conn_pair(&conn_key(&words[end - 1]), &conn_key(&words[end]))
                    {
                        end -= 1;
                    }
                }
            }
        }
        pieces.push(words[start..end].join(" "));
        start = end;
    }
    pieces
}

/// Greedy packing, preserving order.
pub fn pack_sentences_into_chunks(sentences: &[String], max_chars: usize) -> Vec<String> {
    let mut final_chunks: Vec<String> = Vec::new();
    let mut buffer = String::new();

    for sentence in sentences {
        let sentence = sentence.trim();
        if sentence.is_empty() {
            continue;
        }
        let slen = sentence.chars().count();
        if slen > max_chars {
            if !buffer.is_empty() {
                final_chunks.push(std::mem::take(&mut buffer));
            }
            for part in split_minor_punct(sentence) {
                let part = part.trim();
                if part.is_empty() {
                    continue;
                }
                let plen = part.chars().count();
                if fits(buffer.chars().count(), plen, max_chars) {
                    if !buffer.is_empty() {
                        buffer.push(' ');
                    }
                    buffer.push_str(part);
                } else {
                    if !buffer.is_empty() {
                        final_chunks.push(std::mem::take(&mut buffer));
                    }
                    buffer = part.to_string();
                    if buffer.chars().count() > max_chars {
                        let mut pieces = split_long_part(&buffer, max_chars);
                        if let Some(last) = pieces.pop() {
                            final_chunks.extend(pieces);
                            buffer = last;
                        } else {
                            buffer.clear();
                        }
                    }
                }
            }
        } else if !buffer.is_empty() && !fits(buffer.chars().count(), slen, max_chars) {
            final_chunks.push(std::mem::take(&mut buffer));
            buffer = sentence.to_string();
        } else {
            if !buffer.is_empty() {
                buffer.push(' ');
            }
            buffer.push_str(sentence);
        }
    }
    if !buffer.is_empty() {
        final_chunks.push(buffer);
    }
    final_chunks
        .into_iter()
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .collect()
}

// ── boundaries ──────────────────────────────────────────────────────────────

/// Classify the boundary after a chunk from its final mark.
pub fn classify_gap(chunk: &str) -> &'static str {
    let c = chunk.trim_end();
    match c.chars().last() {
        Some('.') | Some('!') | Some('?') => "sentence",
        _ => "minor",
    }
}

/// Readable length: an emotion token is characters but not words.
fn effective_len(chunk: &str) -> usize {
    let chars: Vec<char> = chunk.chars().collect();
    let mut out: Vec<char> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '<'
            && chars[i..].starts_with(&['<', '|', 'e', 'm', 'o', 't', 'i', 'o', 'n', '_'])
        {
            let rest: String = chars[i..].iter().collect();
            if let Some(bar) = rest.find("|>") {
                let span: String = chars[i..i + bar + 2].iter().collect();
                if is_emotion_span(&span) {
                    i += bar + 2;
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out.into_iter().collect::<String>().trim().chars().count()
}

/// Fold chunks shorter than `min_chars` into a neighbour.
///
/// A one- or two-word chunk standing alone leaves the autoregressive model
/// without enough text to condition on, the stop token misses, and it invents
/// words. Merging is the cheap fix.
///
/// Preference order: a boundary that is **not** a paragraph break, then the
/// shorter neighbour, then the right-hand one. Merging across a paragraph break
/// is allowed when nothing else is left, and the paragraph pause gives way to the
/// sentence break — a deliberate trade to avoid hallucination.
fn merge_short_chunks(mut chunks: Vec<String>, mut gaps: Vec<String>, min_chars: usize) -> Chunks {
    while chunks.len() > 1 {
        let Some(i) = (0..chunks.len())
            .filter(|k| effective_len(&chunks[*k]) < min_chars)
            .min_by_key(|k| effective_len(&chunks[*k]))
        else {
            break;
        };
        // (is-not-a-paragraph-break, shorter-neighbour-wins) — compared as a
        // tuple, exactly like the reference, so "R" wins a full tie by being first.
        let mut sides: Vec<((bool, std::cmp::Reverse<usize>), char)> = Vec::new();
        if i < chunks.len() - 1 {
            sides.push((
                (
                    !gaps[i].eq("para"),
                    std::cmp::Reverse(chunks[i + 1].chars().count()),
                ),
                'R',
            ));
        }
        if i > 0 {
            sides.push((
                (
                    !gaps[i - 1].eq("para"),
                    std::cmp::Reverse(chunks[i - 1].chars().count()),
                ),
                'L',
            ));
        }
        let side = sides
            .into_iter()
            .max_by_key(|(k, _)| *k)
            .map(|(_, s)| s)
            .expect("at least one neighbour when len > 1");

        if side == 'R' {
            let moved = chunks.remove(i + 1);
            chunks[i].push(' ');
            chunks[i].push_str(&moved);
            gaps.remove(i);
        } else {
            let moved = chunks.remove(i);
            chunks[i - 1].push(' ');
            chunks[i - 1].push_str(&moved);
            gaps.remove(i - 1);
        }
    }
    Chunks { chunks, gaps }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_question_inside_quotes_does_not_end_a_sentence() {
        let s = split_sentences(
            "Có phải kiểu như: \"Rồi sao nữa? Mình phải làm đến bao giờ?\", đúng không anh?",
        );
        assert_eq!(s.len(), 1, "{s:?}");
    }

    #[test]
    fn a_decimal_point_is_not_a_sentence_end() {
        let s = split_sentences("Giá là 4.200,5 điểm. Rồi sao nữa?");
        assert_eq!(s.len(), 2, "{s:?}");
        assert!(s[0].contains("4.200,5"));
    }

    /// An unclosed quote must not swallow the rest of the text.
    #[test]
    fn an_unbalanced_quote_falls_back_to_a_plain_scan() {
        let s = split_sentences("Ông ấy nói: \"Đi đi. Tôi đứng dậy. Rồi bỏ đi.");
        assert!(s.len() > 1, "{s:?}");
    }

    /// A mark *inside* brackets does not end a sentence — that is the whole
    /// point of tracking depth — so `(Thế à!) Ừ.` stays one sentence.
    #[test]
    fn a_mark_inside_brackets_does_not_end_a_sentence() {
        let s = split_sentences("Thật à? (Thế à!) Ừ.");
        assert_eq!(s.len(), 2, "{s:?}");
        assert_eq!(s[1], "(Thế à!) Ừ.");
    }

    /// …but a closing mark *after* the sentence mark belongs to that sentence.
    #[test]
    fn a_trailing_closing_mark_is_absorbed() {
        let s = split_sentences("Anh ấy hỏi: bao giờ?\" Rồi im.");
        assert_eq!(s[0], "Anh ấy hỏi: bao giờ?\"", "{s:?}");
    }

    #[test]
    fn the_ceiling_is_relative_not_hard() {
        // 100 characters, then a 10-character sentence: 100 + 1 + 10 = 111 is
        // past 100 but the addition is within the slack, so it joins.
        assert!(fits(100, 10, 100));
        // A 40-character addition is not, even though 141 <= 100 + 15.
        assert!(!fits(100, 40, 100));
        // An addition longer than the ceiling never fits, empty buffer or not —
        // the slack is a grace for short tails, not an escape hatch.
        assert!(!fits(0, 500, 256));
    }

    #[test]
    fn a_small_ceiling_scales_its_slack() {
        assert_eq!(tail_slack(256), 15);
        assert_eq!(tail_slack(64), 8); // 64 / 8
        assert_eq!(tail_slack(100), 12); // 100 / 8
    }

    #[test]
    fn minor_punctuation_splits_after_the_mark() {
        let parts = split_minor_punct("một, hai; ba: bốn");
        assert_eq!(parts, vec!["một,", "hai;", "ba:", "bốn"]);
        // A hyphen only counts when whitespace follows.
        assert_eq!(
            split_minor_punct("đường Nguyễn-Huệ dài"),
            vec!["đường Nguyễn-Huệ dài"]
        );
    }

    #[test]
    fn english_spans_stay_whole() {
        assert_eq!(
            tokenize_keep_en("xin chào <en>hello world</en> nhé").len(),
            4
        );
        // A tag mid-token is not a tag: `\S+` takes the whole run.
        assert_eq!(tokenize_keep_en("x<en>a</en>"), vec!["x<en>a</en>"]);
        assert_eq!(tokenize_keep_en("<EN>hi</EN>"), vec!["<EN>hi</EN>"]);
    }

    #[test]
    fn a_boundary_is_classified_by_its_final_mark() {
        assert_eq!(classify_gap("Hắn cười."), "sentence");
        assert_eq!(classify_gap("Hắn cười?"), "sentence");
        assert_eq!(classify_gap("Hắn cười,"), "minor");
        assert_eq!(classify_gap("Hắn cười"), "minor");
    }

    #[test]
    fn an_emotion_token_is_not_counted_as_text() {
        assert_eq!(effective_len("<|emotion_1|>"), 0);
        assert_eq!(effective_len("  <|emotion_1|>  "), 0);
        // Removing the token leaves "xin  chào" — the gap it occupied stays, so
        // this is 9, not 8. Only the ends are stripped.
        assert_eq!(effective_len("xin <|emotion_2|> chào"), 9);
    }

    #[test]
    fn emotion_spans_split_and_keep_their_odd_positions() {
        let parts = split_emotions("a [cười] b");
        assert_eq!(parts, vec!["a ", "[cười]", " b"]);
        // A cue at the very start keeps its odd index: the empty leading piece
        // is part of the contract, not noise.
        let parts = split_emotions("[thở dài] Thôi vậy.");
        assert_eq!(parts, vec!["", "[thở dài]", " Thôi vậy."]);
        let parts = split_emotions("<|emotion_2|> xin");
        assert_eq!(parts, vec!["", "<|emotion_2|>", " xin"]);
        // …and a trailing cue leaves an empty tail.
        let parts = split_emotions("xin <|emotion_2|>");
        assert_eq!(parts, vec!["xin ", "<|emotion_2|>", ""]);
    }

    #[test]
    fn only_the_three_trained_cues_map_to_tokens() {
        assert_eq!(emotion_token_k("[cười]"), Some("<|emotion_1|>"));
        assert_eq!(emotion_token_k("[chuckle]"), Some("<|emotion_1|>"));
        assert_eq!(emotion_token_k("[thở dài]"), Some("<|emotion_2|>"));
        assert_eq!(emotion_token_k("[hắng giọng]"), Some("<|emotion_3|>"));
        // An unknown bracket is ordinary text.
        assert_eq!(emotion_token_k("[vỗ tay]"), None);
    }

    #[test]
    fn short_chunks_merge_towards_the_shorter_neighbour() {
        let chunks = vec![
            "một câu dài đủ dài để không bị gộp".to_string(),
            "ngắn.".to_string(),
            "một câu khác cũng dài đủ".to_string(),
        ];
        let gaps = vec!["sentence".to_string(), "sentence".to_string()];
        let out = merge_short_chunks(chunks, gaps, 20);
        assert_eq!(out.chunks.len(), 2, "{:?}", out.chunks);
        // Both boundaries are non-paragraph, so the shorter *neighbour* decides —
        // and the right-hand one is shorter, so "ngắn." joins it.
        assert!(out.chunks[1].starts_with("ngắn."), "{:?}", out.chunks);
        assert_eq!(out.gaps.len(), 1);
    }

    /// A single chunk is left alone however short: the frame ceiling handles it.
    #[test]
    fn one_short_chunk_is_not_merged_into_nothing() {
        let out = merge_short_chunks(vec!["ngắn.".to_string()], vec![], 20);
        assert_eq!(out.chunks, vec!["ngắn."]);
    }
}
