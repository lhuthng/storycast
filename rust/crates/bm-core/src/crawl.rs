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

/// Decode entities and normalise the shape of a chapter at the boundary.
///
/// **Nothing here is about any one website.** Not which element holds the prose,
/// not which lines are the site's furniture, not what a chapter headline looks
/// like in the language the novel is written in: those are facts about a site,
/// and they live in the crawler script — `SITE.artifact` in
/// `assets/crawl/templates/storya.lua` is where Storya's own lines are listed,
/// and a new site adds its own.
///
/// This function used to hold a list of Storya's junk lines and drop them. That
/// was the one piece of site knowledge the host had, and it was a lie of the
/// documented contract ("the host offers primitives and no site knowledge"),
/// not a feature: it made every *other* site's chapters depend on a Vietnamese
/// word list, and a site that needs its own filter would have had to be added
/// here — in Rust, unreviewable by whoever wrote the crawler — to get one.
///
/// What is left is what cannot be wrong whatever the site: entities decoded so a
/// chapter reads the same as it was crawled, carriage returns gone, one line per
/// paragraph with blank lines between, and a trailing newline. The length guard
/// then refuses a body too short to be a chapter, so a miss fails at the crawl
/// rather than three stages downstream.
pub(crate) fn sanitize_chapter_text(text: &str) -> String {
    let decoded = decode_entities(text);
    let mut paragraphs = Vec::new();
    for line in decoded.lines() {
        let line = line.trim();
        if line.is_empty() {
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
        // be dropped by this file. The crawlers drop them instead — that is what
        // `SITE.artifact` in `storya.lua` / `storya.js` is for, and
        // `script_tests::the_bundled_crawlers_reproduce_the_rust_extractors_goldens`
        // is what proves they still land on the same bytes.
        //
        // Pinned deliberately: a host that quietly deletes words is a host that
        // can delete a *novel's* words, on a language nobody wrote a list for.
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
}
