use super::is_emotion_span;
use super::{CHUNK_TAIL_SLACK, CONN_PAIRS, CONN_STRIP, CONN_WORDS};
#[derive(Debug, Default, Clone)]
pub struct Chunks {
    pub chunks: Vec<String>,
    pub gaps: Vec<String>,
}

// ── sentence splitting ──────────────────────────────────────────────────────

/// Opening brackets and the closing bracket each expects. Single quotes are
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
            // A spaced ellipsis (". . .", "? ?") is one trailing mark, not a
            loop {
                let mut k = j;
                while k < n && chars[k].is_whitespace() {
                    k += 1;
                }
                if k < n && is_sentence_end(chars[k]) {
                    j = k + 1;
                    while j < n && is_sentence_end(chars[j]) {
                        j += 1;
                    }
                } else {
                    break;
                }
            }
            while j < n && is_trailing_close(chars[j]) {
                j += 1; // and a closing mark stuck to it
            }
            // Only a boundary when whitespace or the end follows — which is what
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
    // A punctuation-only fragment (".", "...", ". . .") is not a sentence:
    let cleaned: Vec<String> = sentences
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && bm_core::util::has_speakable_content(s))
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
pub(crate) fn merge_short_chunks(
    mut chunks: Vec<String>,
    mut gaps: Vec<String>,
    min_chars: usize,
) -> Chunks {
    while chunks.len() > 1 {
        let Some(i) = (0..chunks.len())
            .filter(|k| effective_len(&chunks[*k]) < min_chars)
            .min_by_key(|k| effective_len(&chunks[*k]))
        else {
            break;
        };
        // (is-not-a-paragraph-break, shorter-neighbour-wins) — compared as a
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
    use crate::text::{emotion_token_k, sentence_chunks, split_emotions};

    #[test]
    fn sentence_level_chunking_keeps_one_take_per_sentence() {
        let (chunks, gaps) = sentence_chunks(vec![
            vec!["First sentence.".into(), "Second sentence.".into()],
            vec!["Paragraph two.".into()],
        ]);
        assert_eq!(
            chunks,
            vec!["First sentence.", "Second sentence.", "Paragraph two."]
        );
        assert_eq!(gaps, vec!["sentence", "para"]);
    }

    #[test]
    fn sentence_level_chunking_does_not_repack_a_short_sentence() {
        let (chunks, _) = sentence_chunks(vec![vec!["Ừm!".into(), "Nước chảy mây trôi.".into()]]);
        assert_eq!(chunks, vec!["Ừm!", "Nước chảy mây trôi."]);
    }

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

    /// A spaced ellipsis is one trailing mark, not three sentences: each lone
    #[test]
    fn a_spaced_ellipsis_stays_with_its_sentence() {
        let s = split_sentences("Vậy thì. . .");
        assert_eq!(s, vec!["Vậy thì. . ."], "{s:?}");
        let s = split_sentences("Xong. . . Rồi đi.");
        assert_eq!(s, vec!["Xong. . .", "Rồi đi."], "{s:?}");
        // Ordinary boundaries are untouched.
        let s = split_sentences("Xong. Rồi đi.");
        assert_eq!(s, vec!["Xong.", "Rồi đi."], "{s:?}");
    }

    /// A punctuation-only fragment is not a sentence at all: synthesized
    #[test]
    fn a_punctuation_only_fragment_is_not_a_sentence() {
        let s = split_sentences("...");
        assert!(s.is_empty(), "{s:?}");
        let s = split_sentences(". . .");
        assert!(s.is_empty(), "{s:?}");
        let s = split_sentences("... Cứu mạng, cứu mạng...");
        assert_eq!(s, vec!["Cứu mạng, cứu mạng..."], "{s:?}");
        // A cue in brackets still counts as content.
        let s = split_sentences("[cười]. Rồi đi.");
        assert_eq!(s, vec!["[cười].", "Rồi đi."], "{s:?}");
    }

    #[test]
    fn the_ceiling_is_relative_not_hard() {
        // 100 characters, then a 10-character sentence: 100 + 1 + 10 = 111 is
        assert!(fits(100, 10, 100));
        // A 40-character addition is not, even though 141 <= 100 + 15.
        assert!(!fits(100, 40, 100));
        // An addition longer than the ceiling never fits, empty buffer or not —
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
        assert_eq!(effective_len("xin <|emotion_2|> chào"), 9);
    }

    #[test]
    fn emotion_spans_split_and_keep_their_odd_positions() {
        let parts = split_emotions("a [cười] b");
        assert_eq!(parts, vec!["a ", "[cười]", " b"]);
        // A cue at the very start keeps its odd index: the empty leading piece
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
