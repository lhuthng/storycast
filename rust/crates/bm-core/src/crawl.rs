//! Stage 1 — fetch a chapter and reduce it to plain prose.
//!
//! **The crawling is not here.** Rust's part is to run an operator's script —
//! [`engine`], [`host`], [`provider`] — and to hold on to the two things that
//! must not vary: the chapter boundary ([`sanitize_chapter_text`]) and the
//! length guard. *Which element of which page holds the prose* is a fact about a
//! website rather than about this program, so it lives in
//! `assets/crawl/templates/storya.lua`, beside the operator who can read the page.
//!
//! What is left in this file is the part that is genuinely the host's:
//!
//! * [`chapter_index`] — the frozen `n -> url` mapping a run works from;
//! * [`strip_tags_raw`], [`decode_entities`], [`sanitize_chapter_text`] — the
//!   text primitives the ABI exposes, and the boundary every chapter crosses
//!   whichever stage produced it (a crawl, a manual import, digest preparation).
//!
//! The bundled crawlers are byte-for-byte parity tested against the Rust
//! extractor they replaced: the pages and its exact output live in
//! `rust/fixtures/crawl/`, and the test is in [`script_tests`].

pub mod contract;
pub mod engine;
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
/// written before the `crawl` block existed.**
///
/// It is **not** a default. `Settings::default()` is `manual` with an empty
/// `script` — a workspace created now names no crawler and fetches nothing, and
/// that is the design rather than an oversight. This constant is only what a
/// *deserialized* file with no `crawl` key falls back to, because such a file
/// means "an old workspace", and that one used to hardcode this path — see
/// [`CrawlSettings::legacy_default`](crate::config::CrawlSettings::legacy_default).
///
/// A path rather than a `include_str!` on purpose. The script is *profile
/// content* — it ships in `assets/`, is rsynced to workers with the rest of the
/// tree, and an operator is expected to copy it and edit the copy for their own
/// site. Compiling it in would make the first thing they must do (read it) the
/// hardest.
pub const DEFAULT_SCRIPT: &str = "assets/crawl/templates/storya.lua";

/// The index a run works from: `data/crawl-index.json`.
///
/// Built once per range rather than derived per chapter, because a paginated
/// listing shifts under an insertion and would then map half a book to the
/// wrong chapters.
///
/// A changed script, engine, param, template or range rebuilds it by itself:
/// the fingerprint is what decides reuse, so editing a crawler is enough. `force`
/// is for the other direction — a listing that moved **under an unchanged
/// config** — and the operator's route to that today is deleting
/// `data/crawl-index.json` (or editing it, which marks it `hand` and stops it
/// being rebuilt behind them at all).
pub fn chapter_index(
    layout: &crate::Layout,
    settings: &crate::config::Settings,
    start: u32,
    count: u32,
    force: bool,
) -> Result<CrawlIndex> {
    let spec = provider::spec_from_settings(layout, settings);
    let hash = index::fingerprint(
        &spec.engine,
        &spec.source,
        &spec.params,
        &settings.url_template,
        start,
        count,
    );
    index::resolve(layout, &hash, start, count, force, || {
        // A script's own `discover()` first: it is the only thing that can map
        // a slug, and it runs here — on the inductor, once per range — rather
        // than on ten workers, once per chapter.
        let provider = provider::Provider::new(&spec);
        if let Some(found) = provider.discover(start, count)? {
            return Ok(Some(CrawlIndex::from_discovered(
                &found, start, count, &hash,
            )));
        }
        // Otherwise the built-in mapping, which needs no network and is what
        // keeps a template-only workspace working with no script at all.
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
/// breaks. No entity decoding — that is a separate step, and a caller that
/// wants both composes them (or calls [`sanitize_chapter_text`]).
///
/// One implementation for every caller that needs it: the host ABI's
/// `strip_tags`, the generic [`html::readable`] fallback, and a crawl script
/// that reduces a container to prose. They must agree about where a line ends,
/// or the same page would read differently depending on which one produced it.
pub(crate) fn strip_tags_raw(html: &str) -> String {
    let html = remove_block(html, "script");
    let html = remove_block(&html, "style");
    strip_tags(&html)
}

/// Remove `<tag>...</tag>` blocks entirely, content included.
/// Case-insensitive; used to drop `<script>` and `<style>` before stripping tags.
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
///
/// `pub(crate)`, not private: the digest's `prepare_chapter` runs the same
/// decoder over every chapter it reads. Chapters crawled before the
/// generic numeric-entity support landed (the `&#x27;két&#x27;` shape) keep the
/// raw forms on disk, and the digest is the stage where the mismatch bites —
/// the model reads `&#x27;` and answers `'`, then the source gate refuses the
/// one-character difference for every racer and the chapter can never digest.
/// One decoder, run at the same boundary the crawler decodes at, so prepared
/// text and the model's natural output agree again.
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
            // leaves hex variants like &#x27; raw in source, model decodes to '
            // and source gate then fails verbatim compare. The body spans the
            // whole entity INCLUDING the leading `&` (tail starts at it): slicing
            // from 1 dropped the `&`, every `strip_prefix("&#…")` missed, and the
            // branch silently passed numeric entities through raw — which is how
            // `&#x27;két&#x27;` reached disk after this code existed.
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

fn is_chapter_heading(line: &str) -> bool {
    match line.strip_prefix("Chương ") {
        Some(rest) => rest
            .chars()
            .next()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false),
        None => false,
    }
}

/// True for a standalone line copied from Storya rather than the novel.
///
/// These markers are deliberately conjunctive. A story may legitimately say
/// that a task was completed, include an author's `PS:`, or mention a platform;
/// only the site-shaped combinations are removed. In particular, the false
/// completion footer needs all three fingerprints (`Hệ thống`, `chiếc đỉnh`,
/// `hậu cung`, and `Truyện đã hoàn thành`) so ordinary prose is never mistaken
/// for site metadata.
fn is_storya_artifact(line: &str) -> bool {
    let line = line.trim();
    let lower = line.to_lowercase();
    let normalized = lower.split_whitespace().collect::<Vec<_>>().join(" ");

    normalized == "cài đặt đọc"
        || normalized == "người trên vạn người"
        || ((lower.contains("đọc online")
            || lower.contains("cập nhật nhanh nhất")
            || lower.contains("nền tảng đọc truyện"))
            && lower.contains("storya"))
        || (lower.contains("hệ thống")
            && lower.contains("chiếc đỉnh")
            && lower.contains("hậu cung")
            && lower.contains("truyện đã hoàn thành"))
        || lower.starts_with("ps:")
        || lower.starts_with("p/s:")
}

/// Remove known site metadata and decode entities at the chapter boundary.
///
/// This runs for freshly crawled pages, existing local chapter files, and the
/// digest preparer. Keeping one function at all three boundaries is what makes
/// a workspace created by an older binary safe: bad source data is repaired on
/// read instead of being blessed by the source-alignment gate and spoken.
pub(crate) fn sanitize_chapter_text(text: &str) -> String {
    let decoded = decode_entities(text);
    let mut paragraphs = Vec::new();
    for line in decoded.lines() {
        let line = line.trim();
        if line.is_empty() || is_storya_artifact(line) {
            continue;
        }
        // Storya repeats the chapter as `81. Chương 81: ...` beside the real
        // `Chương 81: ...` headline. The numbered copy is metadata; the clean
        // headline is retained and spoken once by the title renderer.
        let bytes = line.as_bytes();
        let numbered_heading = bytes.first().is_some_and(u8::is_ascii_digit)
            && line
                .split_once(". ")
                .map(|(_, rest)| is_chapter_heading(rest))
                .unwrap_or(false);
        if numbered_heading {
            continue;
        }
        paragraphs.push(line);
    }
    if paragraphs.is_empty() {
        return String::new();
    }
    format!("{}\n", paragraphs.join("\n\n"))
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
        // leading `&`, so every numeric entity passed through raw — and the
        // digest's source gate then refused the decoded answer the model
        // naturally gives, stranding the chapter on every racer.
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
    fn chapter_heading_detection() {
        assert!(is_chapter_heading("Chương 12"));
        assert!(is_chapter_heading("Chương 12: Tên"));
        assert!(!is_chapter_heading("Chương trước"));
        assert!(!is_chapter_heading("Mở đầu"));
    }

    #[test]
    fn sanitizes_storya_metadata_without_touching_story_prose() {
        let raw = "Chương 81: Liền phòng ngự\n\n81. Chương 81: Liền phòng ngự\n\nCài đặt đọc\n\nNgười Trên Vạn Người\n\nNgười Trên Vạn Người thuộc thể loại Xuyên Không, chương 81 tiếp tục diễn biến hấp dẫn của câu chuyện. Đọc online miễn phí, cập nhật nhanh nhất tại Storya - nền tảng đọc truyện chất lượng cao.\n\nHắn đã hoàn thành nhiệm vụ.\n\nHệ thống thực thể dưới dạng chiếc đỉnh. Main bá, không hậu cung. Truyện đã hoàn thành\n\nPS: sẽ cập nhật sau.\n\nCánh cửa k&#x27;két&#x27; một tiếng.";
        let out = sanitize_chapter_text(raw);

        assert!(out.starts_with("Chương 81: Liền phòng ngự\n\nHắn đã hoàn thành nhiệm vụ."));
        assert!(out.ends_with("Cánh cửa k'két' một tiếng.\n"));
        assert!(!out.contains("81. Chương"));
        assert!(!out.contains("Storya"));
        assert!(!out.contains("Truyện đã hoàn thành"));
        assert!(!out.contains("PS:"));
        assert!(!out.contains("&#"));
    }
}
