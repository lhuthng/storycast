//! Stage 1 — fetch a chapter and reduce it to plain prose.

pub mod contract;
pub mod engine;
pub mod epub;
pub mod host;
pub mod html;
pub mod import;
pub mod index;
pub mod known;
pub mod pacing;
pub mod probe;
pub mod provider;

#[cfg(test)]
mod script_tests;

pub use contract::{
    Blocked, BlockedClass, CrawlOutcome, CrawlRequest, CrawlResponse, DiscoverRequest, Discovered,
    DiscoveredChapter,
};
pub use index::{expand_template, CrawlIndex};
pub use known::{for_host, for_url, known_sites, KnownSite};
pub use provider::{report_of, spec_from_settings, Provider};

use anyhow::Result;

/// The Storya/LN crawler, kept for one narrow reason: **a `settings.json`
pub const DEFAULT_SCRIPT: &str = "crawlers/known/storya.lua";

/// The index a run works from: `data/crawl-index.json`.
pub fn chapter_index(
    layout: &crate::Layout,
    settings: &crate::config::Settings,
    start: u32,
    count: u32,
    force: bool,
) -> Result<CrawlIndex> {
    let spec = provider::spec_from_settings(layout, settings);
    // A local book's bytes decide the chapter tree, so they are part of what
    let books = index::books_fingerprint(&spec.read_root, &spec.params);
    let hash = index::fingerprint(
        &spec.engine,
        &spec.source,
        &spec.params,
        &settings.url_template,
        start,
        count,
        &books,
    );
    index::resolve(layout, &hash, start, count, force, || {
        // A script's own `discover()` first: it is the only thing that can map
        let provider = provider::Provider::new(&spec);
        if let Some(found) = provider.discover(start, count)? {
            return Ok(Some(CrawlIndex::from_discovered(
                &found, start, count, &hash,
            )));
        }
        // Otherwise the built-in mapping, which needs no network and is what
        Ok(index::template_mapping(
            &settings.url_template,
            start,
            count,
            &hash,
        ))
    })
}

/// Tags whose closing boundary should become a line break.
const BLOCK_TAGS: [&str; 15] = [
    "br",
    "p",
    "div",
    "li",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "tr",
    "section",
    "article",
    "main",
    "blockquote",
];

/// Tags out, script and style bodies with them, block boundaries as line
pub(crate) fn strip_tags_raw(html: &str) -> String {
    let html = remove_block(html, "script");
    let html = remove_block(&html, "style");
    strip_tags(&html)
}

/// Remove `<tag>...</tag>` blocks entirely, content included.
fn remove_block(html: &str, tag: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}");
    let mut out = String::with_capacity(html.len());
    let mut pos = 0usize;
    while let Some(rel) = lower[pos..].find(&open) {
        let start = pos + rel;
        out.push_str(&html[pos..start]);
        match lower[start..].find(&close) {
            Some(rel_end) => {
                let end = start + rel_end;
                match html[end..].find('>') {
                    Some(rel_gt) => pos = end + rel_gt + 1,
                    None => {
                        pos = html.len();
                    }
                }
            }
            None => {
                pos = html.len();
            }
        }
    }
    out.push_str(&html[pos..]);
    out
}

/// Replace tags with whitespace, emitting line breaks at block boundaries.
fn strip_tags(html: &str) -> String {
    let bytes = html.as_bytes();
    let mut out = String::with_capacity(html.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() && bytes[j] != b'>' {
                j += 1;
            }
            if start <= html.len() && j <= html.len() {
                let inner = &html[start..j];
                let name: String = inner
                    .trim_start_matches('/')
                    .split(|c: char| c.is_whitespace() || c == '/')
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if BLOCK_TAGS.contains(&name.as_str()) {
                    out.push('\n');
                }
            }
            i = if j < bytes.len() { j + 1 } else { j };
        } else {
            let ch = html[i..].chars().next().unwrap_or(' ');
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// Decode the handful of entities that actually show up in story pages.
pub(crate) fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(idx) = rest.find('&') {
        out.push_str(&rest[..idx]);
        let tail = &rest[idx..];
        let mut matched = false;
        for (entity, repl) in [
            ("&nbsp;", " "),
            ("&quot;", "\""),
            ("&apos;", "'"),
            ("&#39;", "'"),
            ("&lt;", "<"),
            ("&gt;", ">"),
            ("&amp;", "&"),
        ] {
            if let Some(t) = tail.strip_prefix(entity) {
                out.push_str(repl);
                rest = t;
                matched = true;
                break;
            }
        }
        if !matched {
            // ponytail: generic numeric entities (&#39; &#x27;) — named list alone
            if let Some(semi) = tail.find(';').filter(|&p| p < 10) {
                let body = &tail[..semi];
                let code = if let Some(hex) = body
                    .strip_prefix("&#x")
                    .or_else(|| body.strip_prefix("&#X"))
                {
                    u32::from_str_radix(hex, 16).ok()
                } else if let Some(dec) = body.strip_prefix("&#") {
                    dec.parse::<u32>().ok()
                } else {
                    None
                };
                if let Some(c) = code.and_then(char::from_u32) {
                    out.push(c);
                    rest = &tail[semi + 1..];
                    continue;
                }
            }
            out.push('&');
            rest = &tail[1..];
        }
    }
    out.push_str(rest);
    out
}

/// Decode entities and normalise the shape of a chapter at the boundary.
pub(crate) fn sanitize_chapter_text(text: &str) -> String {
    let decoded = decode_entities(text);
    let mut paragraphs: Vec<String> = Vec::new();
    for line in decoded.lines() {
        let line = fold_end_mark_runs(line.trim());
        let line = line.trim();
        // A paragraph whose only content is end marks is spacing, not prose —
        if line.is_empty() || line.chars().all(|c| is_end_mark(c) || c.is_whitespace()) {
            continue;
        }
        paragraphs.push(line.to_string());
    }
    if paragraphs.is_empty() {
        return String::new();
    }
    format!("{}\n", paragraphs.join("\n\n"))
}

/// `.`, `!`, `?` and the typographic ellipsis — the marks a spaced run is built
fn is_end_mark(c: char) -> bool {
    matches!(c, '.' | '!' | '?' | '…')
}

/// Whitespace-separated end-mark runs fold into one compact mark: dots render
fn fold_end_mark_runs(line: &str) -> String {
    fn kind(c: char) -> char {
        if c == '.' {
            '…'
        } else {
            c
        }
    }
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(line.len());
    let mut i = 0usize;
    while i < n {
        if !is_end_mark(chars[i]) {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let mut kinds = vec![kind(chars[i])];
        let mut marks = 1usize;
        let mut spaced = false;
        let mut j = i + 1;
        loop {
            let mut k = j;
            let mut saw_ws = false;
            while k < n && chars[k].is_whitespace() {
                saw_ws = true;
                k += 1;
            }
            if k < n && is_end_mark(chars[k]) {
                if saw_ws {
                    spaced = true;
                }
                let kd = kind(chars[k]);
                if !kinds.contains(&kd) {
                    kinds.push(kd);
                }
                marks += 1;
                j = k + 1;
            } else {
                break;
            }
        }
        if marks >= 2 && spaced {
            for kd in &kinds {
                out.push(*kd);
            }
        } else {
            out.extend(chars[i..j].iter());
        }
        i = j;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_entities() {
        assert_eq!(
            decode_entities("a &amp; b &lt;c&gt; &quot;d&quot;"),
            "a & b <c> \"d\""
        );
        assert_eq!(decode_entities("100% &nbsp;ok"), "100%  ok");
        assert_eq!(decode_entities("bare & ampersand"), "bare & ampersand");
    }

    #[test]
    fn decodes_numeric_entities_in_both_radixes() {
        // The ch79 shape, verbatim: the generic branch used to slice off the
        assert_eq!(
            decode_entities("&#x27;két&#x27; một tiếng"),
            "'két' một tiếng"
        );
        assert_eq!(decode_entities("&#39;hắn&#39; nói"), "'hắn' nói");
        assert_eq!(decode_entities("&quot;Ừm&quot;"), "\"Ừm\"");
        // A semicolon nearby but no entity body stays raw, not mangled.
        assert_eq!(decode_entities("a & b; c"), "a & b; c");
    }

    #[test]
    fn the_boundary_tidies_shape_and_never_vocabulary() {
        // The host's whole remaining job, on the messiest input there is.
        let raw =
            "  Chương 81: Liền phòng ngự  \r\n\r\n\nCánh cửa k&#x27;két&#x27; một tiếng.\r\n\r\n";
        let out = sanitize_chapter_text(raw);
        assert_eq!(
            out,
            "Chương 81: Liền phòng ngự\n\nCánh cửa k'két' một tiếng.\n"
        );
        assert!(!out.contains('\r'), "a stray CR becomes a line of its own");
    }

    #[test]
    fn site_words_are_the_scripts_business_and_the_host_leaves_them_alone() {
        // Every line here is Storya's furniture, and every one of them used to
        let furniture = [
            "81. Chương 81: Liền phòng ngự",
            "Cài đặt đọc",
            "Truyện đã hoàn thành",
            "PS: sẽ cập nhật sau.",
            "Đọc online tại Storya",
        ];
        let out = sanitize_chapter_text(&furniture.join("\n\n"));
        for line in furniture {
            assert!(
                out.contains(line),
                "the host must not know site words: {line:?} was dropped from {out:?}"
            );
        }
    }

    /// Machine-translated prose separates its trailing punctuation with
    #[test]
    fn a_spaced_end_mark_run_folds_into_one_mark() {
        let out = sanitize_chapter_text("Vậy thì. . .");
        assert_eq!(out, "Vậy thì…\n", "{out}");
        // Dots render as the ellipsis; a question mark in the run survives as
        let out = sanitize_chapter_text("ngươi đây là. . . ?");
        assert_eq!(out, "ngươi đây là…?\n", "{out}");
        // Each non-dot kind once, first-appearance order: `! ! ! ?` is one
        let out = sanitize_chapter_text("Rắn. . . Xà Vương! ! ! ?");
        assert_eq!(out, "Rắn… Xà Vương!?\n", "{out}");
        // Two dots are already an artifact run.
        assert_eq!(sanitize_chapter_text("hắn đảo mắt. . ."), "hắn đảo mắt…\n");
    }

    /// Adjacent marks are a writer's style and a decimal point is a number:
    #[test]
    fn adjacent_marks_and_lone_marks_pass_through_untouched() {
        let text = "Hắn dừng... thật?! 3.5 triệu đồng. Xong!";
        assert_eq!(sanitize_chapter_text(text), format!("{text}\n"), "{text}");
        let twice = sanitize_chapter_text(&sanitize_chapter_text("là. . . ?"));
        assert_eq!(twice, "là…?\n", "the fold must be idempotent");
    }

    /// A paragraph made only of end marks is spacing standing where a scene
    #[test]
    fn a_paragraph_of_only_end_marks_is_not_prose() {
        let out = sanitize_chapter_text("Câu một.\n\n. . .\n\nCâu hai.");
        assert_eq!(out, "Câu một.\n\nCâu hai.\n", "{out}");
        let out = sanitize_chapter_text("Câu một.\n\n...\n\nCâu hai.");
        assert_eq!(out, "Câu một.\n\nCâu hai.\n", "{out}");
        let out = sanitize_chapter_text("Câu một.\n\n───\n\nCâu hai.");
        assert_eq!(out, "Câu một.\n\n───\n\nCâu hai.\n", "{out}");
    }
}
