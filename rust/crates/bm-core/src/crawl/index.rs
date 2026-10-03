//! The chapter index: the frozen `n -> url` mapping a crawl run works from.
//!
//! It exists because the mapping is genuinely site-specific — `chapter-001`, a
//! slug, `v2c15` — and because **deriving it per chapter is unsafe on a
//! paginated listing**. A site that appends a chapter repaginates, and every
//! `n` after the insertion point then maps to its neighbour; per-chapter
//! discovery would silently crawl the wrong chapter for half a book, while a
//! frozen index turns that shift into a `--refresh` decision instead of a
//! corruption.
//!
//! Three ways the same file gets filled, all of them the same shape:
//!
//! | source     | filled by                                             |
//! |------------|-------------------------------------------------------|
//! | `template` | the built-in `{n}` / `{n:03}` expansion                 |
//! | `script`   | the crawler script's optional `discover()`              |
//! | `hand`     | an operator, by editing `data/crawl-index.json`         |
//!
//! `hand` is the escape hatch for a book whose URLs are arbitrary words: the
//! mapping is not computable, so generating it once (scrape, spreadsheet, an
//! LLM, a human reading the index) and keeping it beats re-deriving it.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

use super::contract::Discovered;
use crate::Layout;

/// Source tags, spelled once so a hand-written file and a generated one can be
/// told apart by eye as well as by code.
pub const SOURCE_HAND: &str = "hand";
pub const SOURCE_TEMPLATE: &str = "template";
pub const SOURCE_SCRIPT: &str = "script";

/// One chapter's place in the site.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Chapter {
    /// `None` means "the script builds its own URL" — a slug site whose
    /// mapping names the chapter but not its link.
    #[serde(default)]
    pub url: Option<String>,
    /// Display only: it is written here so the file reads like the site's own
    /// listing, and it is never a chapter's title — that stays the first line
    /// of the chapter text, or the digest's own `title`.
    #[serde(default)]
    pub title: String,
    /// The site has no chapter here. Terminal non-failure: the crawl task is
    /// recorded done-with-a-reason instead of being offered to a worker that
    /// would 404 three times and shelve.
    #[serde(default)]
    pub absent: bool,
}

/// The whole mapping, as it lands in `data/crawl-index.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CrawlIndex {
    /// One of [`SOURCE_HAND`], [`SOURCE_TEMPLATE`], [`SOURCE_SCRIPT`].
    #[serde(default)]
    pub source: String,
    /// Fingerprint of what produced it (engine + script + params + template).
    /// Only consulted for the generated sources: a hand-authored index is the
    /// operator's own work and is never rebuilt behind their back.
    #[serde(default)]
    pub hash: String,
    /// The range this index was built for. A run that asks outside it
    /// rebuilds rather than guesses.
    #[serde(default)]
    pub start: u32,
    #[serde(default)]
    pub count: u32,
    /// Total chapter count when the listing reported one.
    #[serde(default)]
    pub total: Option<u32>,
    #[serde(default)]
    pub chapters: BTreeMap<u32, Chapter>,
}

impl CrawlIndex {
    pub fn is_hand(&self) -> bool {
        self.source == SOURCE_HAND
    }

    /// The URL for `n`, if this index names one.
    pub fn url(&self, n: u32) -> Option<&str> {
        self.chapters.get(&n).and_then(|c| c.url.as_deref())
    }

    /// The site explicitly has no chapter at `n`.
    pub fn is_absent(&self, n: u32) -> bool {
        self.chapters.get(&n).map(|c| c.absent).unwrap_or(false)
    }

    /// A chapter that is neither absent nor nameless: what the crawl stage
    /// should be asked for.
    pub fn is_wanted(&self, n: u32) -> bool {
        !self.is_absent(n)
    }

    /// Whether this index can serve a run of `count` chapters from `start`.
    ///
    /// A hand-authored index is usable whenever it covers the range at all —
    /// its `hash` is empty by construction, and an operator who edits it means
    /// the edit to take effect. A generated one must also match the machinery
    /// that produced it, or a changed script would keep serving the old
    /// mapping.
    pub fn usable(&self, hash: &str, start: u32, count: u32) -> bool {
        let empty = self.chapters.is_empty();
        if empty {
            return false;
        }
        if self.is_hand() {
            return self.covers(start, count);
        }
        self.hash == hash && self.covers(start, count)
    }

    fn covers(&self, start: u32, count: u32) -> bool {
        self.start <= start && start.saturating_add(count) <= self.start.saturating_add(self.count)
    }

    /// The range this index was built for, for the ledger message.
    pub fn range(&self) -> (u32, u32) {
        (self.start, self.count)
    }

    pub fn load(layout: &Layout) -> Option<CrawlIndex> {
        crate::read_json::<CrawlIndex>(&layout.crawl_index()).ok()
    }

    /// The index in a workspace, whatever its shape — `None` when absent.
    pub fn chapters(&self) -> &BTreeMap<u32, Chapter> {
        &self.chapters
    }

    pub fn save(&self, layout: &Layout) -> Result<()> {
        crate::atomic_write(&layout.crawl_index(), &serde_json::to_string_pretty(self)?)
    }

    /// Build the mapping for `start..start+count` from a URL template — the
    /// built-in `discover`, and the reason an existing workspace needs no
    /// script to keep working.
    pub fn from_template(template: &str, start: u32, count: u32, hash: &str) -> CrawlIndex {
        let mut chapters = BTreeMap::new();
        for n in start..start.saturating_add(count) {
            chapters.insert(
                n,
                Chapter {
                    url: Some(expand_template(template, n)),
                    title: String::new(),
                    absent: false,
                },
            );
        }
        CrawlIndex {
            source: SOURCE_TEMPLATE.into(),
            hash: hash.into(),
            start,
            count,
            total: None,
            chapters,
        }
    }

    /// Build from a script's `discover()` result.
    pub fn from_discovered(d: &Discovered, start: u32, count: u32, hash: &str) -> CrawlIndex {
        let mut chapters: BTreeMap<u32, Chapter> = d
            .chapters
            .iter()
            .map(|c| {
                (
                    c.n,
                    Chapter {
                        url: c.url.clone(),
                        title: c.title.clone(),
                        absent: c.absent,
                    },
                )
            })
            .collect();
        // A listing that skips a number says nothing about it, and silence is
        // **not** absence: only an explicit `absent` marks a gap. Treating a
        // missing entry as absent would silently drop a real chapter whose
        // mapping the script merely failed to mention.
        for n in start..start.saturating_add(count) {
            chapters.entry(n).or_default();
        }
        // A listing that reports how long the book is turns the tail of an
        // over-long range into twenty *absent* rows instead of twenty crawl
        // tasks that 404 three times each and shelve. This is the whole reason
        // `total` is in the discover contract.
        if let Some(total) = d.total {
            for (n, c) in chapters.iter_mut() {
                if *n > total {
                    c.url = None;
                    c.absent = true;
                    if c.title.is_empty() {
                        c.title = format!("past the end of the book ({total} chapters)");
                    }
                }
            }
        }
        CrawlIndex {
            source: SOURCE_SCRIPT.into(),
            hash: hash.into(),
            start,
            count,
            total: d.total,
            chapters,
        }
    }
}

/// Fingerprint of everything that decides the mapping, so a changed script,
/// engine, param, template **or book** invalidates a generated index and
/// nothing else.
///
/// `books` is the digest [`books_fingerprint`] produced for the local files the
/// crawl reads, or the empty string when it reads none. It is a separate input
/// rather than a param because the params are configuration the operator wrote
/// and this is the disk those params point at: a site listing lives on the
/// server, but a local book's chapter tree lives on this machine and changes
/// with no setting to show for it.
pub fn fingerprint(
    engine: &str,
    script: &str,
    params: &serde_json::Map<String, serde_json::Value>,
    url_template: &str,
    start: u32,
    count: u32,
    books: &str,
) -> String {
    let mut h = Sha256::new();
    for part in [
        engine,
        script,
        &serde_json::to_string(params).unwrap_or_default(),
        url_template,
        &start.to_string(),
        &count.to_string(),
        books,
    ] {
        h.update(part.as_bytes());
        h.update([0]);
    }
    let digest = h.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// A digest of the local book files a crawl reads, for [`fingerprint`].
///
/// **Why this exists.** A scripted `discover()` is cached in
/// `data/crawl-index.json` and reused while its fingerprint matches. For a site
/// that fingerprint is configuration alone — the listing lives on the server.
/// For a **local book** the listing lives on this disk, and swapping
/// `book.epub` for a different file (or adding a volume to `books/`) changes
/// every chapter's place without changing a single setting. The bytes are part
/// of what decides the mapping, so they are part of the fingerprint.
///
/// **Why the rule is over-inclusive.** The host cannot know which param a
/// script treats as a book the way the script does. So every string param that
/// resolves to a readable `.epub` inside the read root — as a file, or as a
/// directory holding them — is included; anything that does not resolve, or is
/// not an EPUB, contributes nothing. Being slow to invalidate a cache costs a
/// rebuild; failing to invalidate one serves a stale chapter tree.
///
/// The empty string means "no local book", and is what a site crawler gets.
pub fn books_fingerprint(
    read_root: &std::path::Path,
    params: &serde_json::Map<String, serde_json::Value>,
) -> String {
    let Ok(base) = read_root.canonicalize() else {
        return String::new();
    };
    let mut books: Vec<std::path::PathBuf> = Vec::new();
    for value in params.values() {
        let Some(named) = value.as_str() else {
            continue;
        };
        if named.trim().is_empty() {
            continue;
        }
        let candidate = if std::path::Path::new(named).is_absolute() {
            std::path::PathBuf::from(named)
        } else {
            read_root.join(named)
        };
        // The same containment a crawl read gets: only a file inside the
        // workspace can be what the mapping is derived from.
        let Ok(real) = candidate.canonicalize() else {
            continue;
        };
        if !real.starts_with(&base) {
            continue;
        }
        if real.is_dir() {
            if let Ok(found) = super::epub::books_in(&real) {
                books.extend(found);
            }
        } else if is_epub(&real) {
            books.push(real);
        }
    }
    books.sort();
    books.dedup();
    if books.is_empty() {
        return String::new();
    }
    let mut h = Sha256::new();
    for path in &books {
        // The name decides the **order** of the volumes, so it is part of the
        // mapping; the bytes decide the chapters.
        h.update(path.to_string_lossy().as_bytes());
        h.update([0]);
        let bytes = std::fs::read(path).unwrap_or_default();
        h.update(&bytes);
        h.update([0]);
    }
    let digest = h.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn is_epub(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("epub"))
}

/// Expand `{n}` / `{n:03}` in a chapter URL template.
///
/// `{n}` **everywhere**, not just the first: `…/chuong-{n}?page={n}` is a real
/// template shape and an existing test asserts it. The padded form is the
/// degenerate end of `n -> f(n)` — `chapter-{n:03}` is 80% of the cases that
/// would otherwise need a script — and keeping it in the one place that already
/// owns the substitution means the mapping function subsumes it if a site ever
/// needs more.
pub fn expand_template(template: &str, n: u32) -> String {
    let bytes = template.as_bytes();
    let mut out = String::with_capacity(template.len() + 4);
    let mut i = 0usize;
    while i < bytes.len() {
        // A placeholder is `{n}` or `{n:0<W>}`; anything else is literal text,
        // including a stray `{`.
        if bytes[i] == b'{' {
            if let Some(close) = template[i..].find('}').map(|p| i + p) {
                let body = &template[i + 1..close];
                if let Some(width) = pad_width(body) {
                    out.push_str(&format!("{n:0width$}", width = width));
                    i = close + 1;
                    continue;
                }
                if body == "n" {
                    out.push_str(&n.to_string());
                    i = close + 1;
                    continue;
                }
            }
        }
        let ch = template[i..].chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// The zero-pad width of a `{n:03}` style placeholder, or `None`.
fn pad_width(body: &str) -> Option<usize> {
    let rest = body.strip_prefix("n:")?;
    let rest = rest.strip_prefix('0')?;
    if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    rest.parse::<usize>().ok().filter(|w| *w <= 32)
}

/// The host's built-in `discover`: a template is a mapping that needs no I/O.
///
/// Returning `None` when there is no template is deliberate — it is what makes
/// "a slug site needs a `discover`" a first-class state rather than a 404 storm.
pub fn template_mapping(template: &str, start: u32, count: u32, hash: &str) -> Option<CrawlIndex> {
    let t = template.trim();
    if t.is_empty() || !t.contains("{n") {
        return None;
    }
    Some(CrawlIndex::from_template(t, start, count, hash))
}

/// Load the index for a range, rebuilding it from `build` when the stored one
/// cannot serve the request.
///
/// `force` skips the reuse check entirely: a repaginated listing that the
/// fingerprint cannot see, because nothing about the configuration changed.
/// Nothing in the TUI passes it today — deleting `data/crawl-index.json` is the
/// equivalent, and the fingerprint already rebuilds on every change that
/// *could* be visible here.
pub fn resolve(
    layout: &Layout,
    hash: &str,
    start: u32,
    count: u32,
    force: bool,
    build: impl FnOnce() -> Result<Option<CrawlIndex>>,
) -> Result<CrawlIndex> {
    if !force {
        if let Some(existing) = CrawlIndex::load(layout) {
            if existing.usable(hash, start, count) {
                return Ok(existing);
            }
        }
    }
    let built = build().context("building the chapter index")?.ok_or_else(|| {
        anyhow::anyhow!(
            "no chapter index for ch{start}..ch{} — set a url_template, give the script a discover() function, or author {} by hand",
            start.saturating_add(count).saturating_sub(1),
            layout.crawl_index().display(),
        )
    })?;
    built.save(layout)?;
    Ok(built)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_expand_plain_padded_and_repeated() {
        assert_eq!(expand_template("https://x/{n}", 7), "https://x/7");
        assert_eq!(
            expand_template("https://x/chapter-{n:03}", 7),
            "https://x/chapter-007"
        );
        assert_eq!(
            expand_template("https://x/chapter-{n:03}", 1234),
            "https://x/chapter-1234",
            "a width is a minimum, not a truncation"
        );
        // Every occurrence, as the pre-existing `chapter_url` behaviour required.
        assert_eq!(
            expand_template("https://x/chuong-{n}?page={n}", 12),
            "https://x/chuong-12?page=12"
        );
        // Literal braces and unknown placeholders survive untouched.
        assert_eq!(
            expand_template("https://x/a{b}/{m}", 3),
            "https://x/a{b}/{m}"
        );
        assert_eq!(expand_template("no placeholder", 3), "no placeholder");
    }

    #[test]
    fn a_generated_index_is_reused_only_while_its_machinery_matches() {
        let idx = CrawlIndex::from_template("https://x/{n}", 1, 10, "abc");
        assert!(idx.usable("abc", 1, 10));
        assert!(idx.usable("abc", 3, 5), "inside the built range");
        assert!(!idx.usable("abc", 1, 20), "past the built range");
        assert!(
            !idx.usable("def", 1, 10),
            "a changed script is a new mapping"
        );
    }

    #[test]
    fn a_hand_authored_index_outlives_its_machinery() {
        // The operator wrote it; nothing about the script can invalidate it.
        let mut idx = CrawlIndex::from_template("https://x/{n}", 1, 10, "");
        idx.source = SOURCE_HAND.into();
        idx.hash = String::new();
        assert!(idx.usable("", 1, 10));
        assert!(idx.usable("whatever", 2, 3));
        assert!(!idx.usable("", 1, 50));
    }

    #[test]
    fn an_absent_chapter_is_not_wanted_but_a_gap_is() {
        let mut idx = CrawlIndex::from_template("https://x/{n}", 1, 3, "h");
        idx.chapters.insert(
            2,
            Chapter {
                url: None,
                title: String::new(),
                absent: true,
            },
        );
        assert!(idx.is_wanted(1));
        assert!(!idx.is_wanted(2), "absent means the site has no chapter 2");
        assert_eq!(idx.url(1), Some("https://x/1"));
        assert_eq!(idx.url(2), None);
        // A number with no entry at all is simply not stopped: silence is not
        // absence, and the crawl still gets a chance to fetch it.
        assert!(idx.is_wanted(99));
    }

    #[test]
    fn an_empty_index_is_never_usable() {
        assert!(!CrawlIndex::default().usable("h", 1, 1));
    }

    #[test]
    fn the_fingerprint_moves_with_every_input_that_decides_the_mapping() {
        let params = serde_json::Map::new();
        let base = fingerprint("lua", "src", &params, "https://x/{n}", 1, 5, "");
        assert_eq!(
            base,
            fingerprint("lua", "src", &params, "https://x/{n}", 1, 5, "")
        );
        assert_ne!(
            base,
            fingerprint("js", "src", &params, "https://x/{n}", 1, 5, "")
        );
        assert_ne!(
            base,
            fingerprint("lua", "src2", &params, "https://x/{n}", 1, 5, "")
        );
        assert_ne!(
            base,
            fingerprint("lua", "src", &params, "https://y/{n}", 1, 5, "")
        );
        assert_ne!(
            base,
            fingerprint("lua", "src", &params, "https://x/{n}", 2, 5, "")
        );
        let mut p2 = serde_json::Map::new();
        p2.insert("per_page".into(), serde_json::json!(50));
        assert_ne!(
            base,
            fingerprint("lua", "src", &p2, "https://x/{n}", 1, 5, "")
        );
        // The book digest is its own input: a swapped volume moves the mapping
        // with no configuration change to see it in.
        assert_ne!(
            base,
            fingerprint("lua", "src", &params, "https://x/{n}", 1, 5, "deadbeef")
        );
    }

    /// A local book's **bytes** are part of what decides the chapter tree, so
    /// they are part of the fingerprint. This is the stale-index gap: replacing
    /// a volume in place changed nothing an operator could point at.
    #[test]
    fn a_local_books_bytes_decide_the_fingerprint() {
        let dir = std::env::temp_dir().join("bm-booksfp");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let one = dir.join("vol-01.epub");
        std::fs::write(&one, b"first edition").unwrap();

        let mut params = serde_json::Map::new();
        params.insert("epub".into(), serde_json::json!("vol-01.epub"));
        let before = books_fingerprint(&dir, &params);
        assert!(!before.is_empty(), "a named book is hashed");
        assert_eq!(before, books_fingerprint(&dir, &params));

        // Same name, different bytes: the tree would change, so the digest must.
        std::fs::write(&one, b"second edition").unwrap();
        assert_ne!(before, books_fingerprint(&dir, &params));

        // A directory of volumes, and one added volume is a new tree.
        params.remove("epub");
        params.insert("books".into(), serde_json::json!("."));
        let two = books_fingerprint(&dir, &params);
        std::fs::write(dir.join("vol-02.epub"), b"another").unwrap();
        assert_ne!(two, books_fingerprint(&dir, &params));

        // A param that names no file, or a non-book file, contributes nothing —
        // so a site crawler's fingerprint is unchanged by this machinery.
        let mut site = serde_json::Map::new();
        site.insert("url_template".into(), serde_json::json!("https://x/{n}"));
        site.insert("novel".into(), serde_json::json!("apothecary"));
        assert_eq!(books_fingerprint(&dir, &site), "");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_template_makes_an_index_and_a_blank_one_does_not() {
        let hash = "h";
        assert!(template_mapping("", 1, 3, hash).is_none());
        assert!(
            template_mapping("https://x/chuong-{n}", 1, 3, hash).is_some(),
            "the existing default workspace template"
        );
        let idx = template_mapping("https://x/chapter-{n:03}", 34, 2, hash).unwrap();
        assert_eq!(idx.url(34), Some("https://x/chapter-034"));
        assert_eq!(idx.url(35), Some("https://x/chapter-035"));
        assert_eq!(idx.source, SOURCE_TEMPLATE);
    }
}
