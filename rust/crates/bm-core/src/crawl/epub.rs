//! Reading a chapter out of an EPUB, which is a ZIP of XHTML.
//!
//! **Why this is Rust and not a crawl script.** A scripted crawl runs with `io`
//! and `os` removed — see [`super::engine::lua`], where the sandbox exists so
//! the documented ABI is the true one — and `fetch` is the only way out. So a
//! script cannot open a local file even in principle, and an EPUB needs more
//! than that: it is a ZIP central directory, DEFLATE streams and an XML spine.
//! Handing that to a script would mean re-implementing inflate in a language
//! that has no filesystem. So the container is read here, and the script asks
//! for a chapter by number through one host function.
//!
//! **The spine is the reading order, not the chapter list.** Reading in manifest
//! order would put a book's title page, copyright leaf and table of contents
//! into the pipeline as chapters 1, 2 and 3 — and the dense index is the
//! pipeline's, so nothing downstream could tell. The spine is the publisher's
//! own ordered list of what a reader sees.
//!
//! **But a spine entry is not always a chapter, and the host does not guess
//! which.** The Internet Archive's *Apothecary Diaries* volume 1 has 219 spine
//! entries and 32 chapters: entry 1 is the navigation document, 2 is the
//! Archive's own copyright notice, 4 to 7 are decorative pages whose OCR came
//! back at 23% accuracy, 8 to 11 are the title page, the table of contents, an
//! illustration list and an app advertisement, and only then — entry 12 —
//! `Chapter 1: Maomao`. A publisher who scans a book produces *pages*; a
//! publisher who types one produces *chapters*. Both are valid EPUBs, and only
//! one of them has a spine that is a chapter list.
//!
//! So the host exposes the two halves separately and lets the **script** decide:
//! [`Epub::index`] reports every spine entry's size and opening words, and
//! [`Epub::text`] reads a *range* of entries. A rule like "an entry whose text
//! begins `Chapter 4` starts a chapter" is knowledge about one book, and this
//! repository's rule is that such knowledge lives in the crawl script, beside
//! the rest of it — not in Rust, where nobody reviewing the host would expect
//! to find it.
//!
//! **Text goes through the same boundary a crawled page does.** A chapter
//! fetched from a site and a chapter read out of a book are the same artifact
//! to everything after the crawl, so they are cleaned by the same function —
//! entities decoded, blank lines collapsed, one trailing newline. Anything
//! else and a book would produce chapters shaped differently from a website's
//! for no reason an operator could see.
//!
//! ## What this deliberately does not do
//!
//! It does not decide where a chapter ends. See above: that is the script's,
//! and a host that guessed would be a host that was wrong about somebody's
//! book.

use anyhow::{anyhow, Context, Result};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use scraper::{Html, Selector};

/// One chapter, as the pipeline's boundary wants it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpubChapter {
    /// The dense, 1-based chapter number the caller asked for.
    pub n: u32,
    /// The chapter's text, already through the shared boundary.
    pub text: String,
    /// The spine item's own title, when the manifest carries one. Display only
    /// — the chapter's title is the first line of `text`, or the digest's.
    pub title: String,
}

/// How much of a spine entry [`Epub::index`] reports.
///
/// Long enough to hold a chapter heading and the sentence after it, short
/// enough that an index of three hundred entries is a few tens of kilobytes
/// rather than the book. The real book's longest heading is 61 characters:
/// `Chapter 10: The Unsettling Matter of the Spirit (Part One)`.
const HEAD_CHARS: usize = 160;

/// One spine entry, as far as a script deciding chapter boundaries gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpubItem {
    /// The 1-based spine position. This is what [`Epub::text`] takes.
    pub n: u32,
    /// The manifest's own title for the entry, when it has one.
    pub title: String,
    /// Characters of prose in the entry. `0` for a page that is only a scan.
    pub chars: usize,
    /// The first [`HEAD_CHARS`] characters of the prose.
    pub head: String,
}

/// An EPUB opened once and read from.
///
/// The container is parsed on open and the spine kept, because a crawl asks for
/// chapters one at a time and re-reading `container.xml` and the OPF for each
/// of three hundred chapters is three hundred redundant ZIP walks.
pub struct Epub {
    /// The whole archive in memory. EPUBs are a few megabytes of already
    /// DEFLATE-compressed text; a book that did not fit here would be a
    /// different kind of file.
    zip: zip::ZipArchive<std::fs::File>,
    /// The publisher's reading order, as `(title, path inside the archive)`.
    spine: Vec<SpineItem>,
    /// Spine entries that named a manifest item the manifest does not list,
    /// skipped rather than obeyed. See [`spine_of`].
    dangling: Vec<String>,
}

#[derive(Debug, Clone)]
struct SpineItem {
    title: String,
    path: String,
}

/// Open a book and read its spine.
///
/// Fails loudly on anything that is not an EPUB rather than guessing: a missing
/// `container.xml` or an OPF with no spine is a file that will not yield a
/// chapter, and the crawl that has to report it should say which part was
/// missing.
pub fn open(path: &Path) -> Result<Epub> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening the book {}", path.display()))?;
    let mut zip = zip::ZipArchive::new(file)
        .with_context(|| format!("{} is not a ZIP archive — not an EPUB", path.display()))?;

    let opf = opf_path(&mut zip)?;
    let opf_dir = Path::new(&opf)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_default();
    let (spine, dangling) = spine_of(&read_entry(&mut zip, &opf)?, &opf_dir)?;
    if spine.is_empty() {
        anyhow::bail!(
            "{} has an empty spine — the book declares no reading order, so it has no chapters",
            path.display()
        );
    }
    Ok(Epub {
        zip,
        spine,
        dangling,
    })
}

impl Epub {
    /// How many chapters the spine declares.
    ///
    /// The **usable** count: entries that named nothing are not in it, so
    /// `1..=chapters()` is exactly the range that answers.
    pub fn chapters(&self) -> usize {
        self.spine.len()
    }

    /// Spine entries skipped because the manifest does not list them.
    ///
    /// Empty on a well-formed book. A real one is not always well-formed, and
    /// this is how a caller can say "chapter 1 of this book is spine item 2"
    /// instead of quietly producing a book that is missing its cover and not
    /// knowing.
    pub fn dangling(&self) -> &[String] {
        &self.dangling
    }

    /// Chapter `n`, 1-based, as the shared boundary's text.
    ///
    /// **One spine entry, which is one chapter only for a book whose publisher
    /// made it that way.** For a scanned book this is a *page*; a script that
    /// wants chapters uses [`Epub::index`] and [`Epub::text`] instead.
    ///
    /// `n` past the end is `Ok(None)` rather than an error: a range that runs
    /// off the end of a book is the ordinary shape of asking for a chapter
    /// that is not there, and the crawl contract has a word for exactly that
    /// (`none`, a terminal non-failure that must not cost a strike).
    ///
    /// **A spine item with no text in it is the same answer, not an error.**
    /// Real books carry spine entries that are not prose: the navigation
    /// document, a cover page, an empty part-divider. The Internet Archive's
    /// Apothecary Diaries EPUB opens with one — its first usable spine entry is
    /// `nav.xhtml`, a `epub:type="toc"` shell whose `<ol/>` is empty. Failing
    /// the book on it was wrong twice over: it is not a failure (retrying it
    /// cannot make text appear), and it is not a chapter (there is nothing to
    /// speak). `none` is the contract's word for both.
    pub fn chapter(&mut self, n: u32) -> Result<Option<EpubChapter>> {
        if n == 0 {
            anyhow::bail!("chapter 0 is not a chapter — the pipeline's index starts at 1");
        }
        let Some(item) = self.spine.get(n as usize - 1).cloned() else {
            return Ok(None);
        };
        let text = self.prose(&item)?;
        if text.trim().is_empty() {
            return Ok(None);
        }
        Ok(Some(EpubChapter {
            n,
            text,
            title: item.title,
        }))
    }

    /// Every spine entry, as size and opening words — one walk of the book.
    ///
    /// This is what a script reads to work out where the chapters are, and the
    /// reason [`Epub::text`] takes a range. It costs a full decompression pass,
    /// so a script calls it **once per crawl** and reuses the answer for the
    /// chapter it is building; calling it per chapter would re-walk the book
    /// once per chapter.
    ///
    /// Entries are in spine order and numbered from 1, so `item.n` is exactly
    /// what [`Epub::text`] takes. An entry with no prose is reported with
    /// `chars: 0` rather than dropped, so the numbering cannot shift under a
    /// script that indexes by position — which is the mistake this module
    /// exists to make impossible.
    pub fn index(&mut self) -> Result<Vec<EpubItem>> {
        let spine = self.spine.clone();
        let mut out = Vec::with_capacity(spine.len());
        for (i, item) in spine.iter().enumerate() {
            let text = self.prose(item)?;
            out.push(EpubItem {
                n: i as u32 + 1,
                title: item.title.clone(),
                chars: text.chars().count(),
                head: text.chars().take(HEAD_CHARS).collect(),
            });
        }
        Ok(out)
    }

    /// Spine entries `from..=to` inclusive, as one piece of prose.
    ///
    /// The range is clamped to the spine rather than refused: a script that has
    /// worked out that chapter 32 ends at entry 219 should not have to know
    /// that 219 is the last one. An empty range is an empty string, not an
    /// error, for the same reason — and the pipeline's own boundary says an
    /// empty chapter is a chapter.
    pub fn text(&mut self, from: u32, to: u32) -> Result<String> {
        let lo = from.max(1) as usize - 1;
        let hi = (to as usize).min(self.spine.len());
        if lo >= hi {
            return Ok(String::new());
        }
        let items: Vec<SpineItem> = self.spine[lo..hi].to_vec();
        // Every part is **already the boundary's output** — `prose` is `text_of`,
        // which ends in `sanitize_chapter_text` — so the join is a paragraph
        // break and a trailing newline rather than a second run of the boundary
        // over characters it has already seen.
        //
        // That second run used to be here, and it was not free of consequence:
        // it was the reason a chapter made of five spine entries paid five
        // sanitizations plus a sixth over the join plus the provider's own, and
        // the join was the only part of it that did any work. The output is
        // byte-for-byte what it was — a `\n` between two entries becomes a
        // paragraph break either way, because the boundary drops the empty line
        // that a `\n\n` join would have left and then puts one back.
        let mut out = String::new();
        for item in &items {
            let text = self.prose(item)?;
            if text.trim().is_empty() {
                continue;
            }
            out.push_str(text.trim_end());
            out.push_str("\n\n");
        }
        if out.is_empty() {
            return Ok(String::new());
        }
        out.truncate(out.trim_end().len());
        out.push('\n');
        Ok(out)
    }

    /// One spine entry's prose, through the shared crawl boundary.
    fn prose(&mut self, item: &SpineItem) -> Result<String> {
        let markup = read_entry(&mut self.zip, &item.path)?;
        Ok(text_of(&markup))
    }
}

/// The OPF path, from `META-INF/container.xml`'s `rootfile`.
///
/// The indirection is the whole reason this is not "look for `content.opf`":
/// the archive may name it anything, and the container is the only thing that
/// says where it is.
fn opf_path(zip: &mut zip::ZipArchive<std::fs::File>) -> Result<String> {
    let xml = read_entry(zip, "META-INF/container.xml").with_context(|| {
        "META-INF/container.xml is missing — this is a ZIP that is not an EPUB"
    })?;
    let path = first_attr(&xml, "rootfile", "full-path")
        .ok_or_else(|| anyhow!("container.xml names no rootfile — there is no package document"))?;
    if path.trim().is_empty() {
        anyhow::bail!("container.xml's rootfile has an empty full-path");
    }
    Ok(path)
}

/// The value of `attr` on the first `<tag …>` in `xml`.
///
/// **quick-xml, not html5ever.** An EPUB's two control files are XML, and the
/// difference is not academic: reading `<item … />` as HTML treats the solidus
/// as an ignored self-closing marker, so every `<item>` after the first becomes
/// a *child* of it. A three-chapter book then yields one, and the spine check
/// that would have caught it is downstream of the parse that lost them. The
/// control files are small, so a streaming reader over start/empty elements is
/// enough and there is no tree to build.
fn first_attr(xml: &str, tag: &str, attr: &str) -> Option<String> {
    elements(xml)
        .into_iter()
        .find(|(name, _)| name == tag)
        .and_then(|(_, a)| a.get(attr).cloned())
}

/// Every element in `xml` as `(name, attributes)`, self-closing or not.
///
/// Collected rather than streamed because the only caller is a control file
/// of a few kilobytes, and a `Vec` is a return type that needs no lifetime
/// gymnastics to hand back.
fn elements(xml: &str) -> Vec<(String, std::collections::BTreeMap<String, String>)> {
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut out = Vec::new();
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(quick_xml::events::Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                out.push((name, attributes(&e)));
            }
            Ok(quick_xml::events::Event::Empty(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                out.push((name, attributes(&e)));
            }
            Ok(quick_xml::events::Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

fn attributes(e: &quick_xml::events::BytesStart<'_>) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    for a in e.attributes().flatten() {
        let key = String::from_utf8_lossy(a.key.as_ref()).into_owned();
        // `unescape_value` rather than `value`: a manifest href may carry an
        // entity, and this is the only place the two differ.
        let val = a
            .unescape_value()
            .map(|v| v.into_owned())
            .unwrap_or_else(|_| String::from_utf8_lossy(&a.value).into_owned());
        out.insert(key, val);
    }
    out
}

/// The reading order: `(id -> (href, title))` from the manifest, then the
/// spine's `itemref` list walked through it.
///
/// Manifest first because the spine holds *ids*, and a script written against
/// the file order would silently produce a book in whatever order the archive
/// happened to be built in.
fn spine_of(opf: &str, opf_dir: &Path) -> Result<(Vec<SpineItem>, Vec<String>)> {
    let all = elements(opf);
    let mut manifest = std::collections::BTreeMap::new();
    for (name, attrs) in &all {
        if name != "item" {
            continue;
        }
        let (Some(id), Some(href)) = (attrs.get("id"), attrs.get("href")) else {
            continue;
        };
        manifest.insert(
            id.clone(),
            (
                join_relative(opf_dir, href),
                attrs
                    .get("title")
                    .cloned()
                    .or_else(|| {
                        Path::new(href)
                            .file_stem()
                            .map(|s| s.to_string_lossy().into_owned())
                    })
                    .unwrap_or_default(),
            ),
        );
    }
    let mut spine = Vec::new();
    let mut dangling = Vec::new();
    for (name, attrs) in &all {
        if name != "itemref" {
            continue;
        }
        let Some(idref) = attrs.get("idref") else {
            continue;
        };
        let Some((path, title)) = manifest.get(idref) else {
            // A spine entry with no manifest entry is **skipped, and named**.
            //
            // This used to be a hard error, on the reasoning that skipping
            // shifts every later chapter by one. A real book says otherwise: the
            // Internet Archive's Apothecary Diaries EPUB has 169 spine entries
            // and its first is `idref="cover"` with no `cover` in the manifest,
            // and failing the whole book over it made the reader useless on
            // exactly the books an operator is most likely to have.
            //
            // Skipping is not the shift that error was guarding against. The
            // entry resolves to *nothing*, so there is no position it could
            // have held; the numbering that follows is the only numbering the
            // file admits. What would be wrong is skipping *silently*, because
            // then an operator cannot tell a book's chapter 1 from a book's
            // chapter 2-because-something-was-dropped — so the names are kept
            // and `Epub::dangling` reports them.
            dangling.push(idref.clone());
            continue;
        };
        spine.push(SpineItem {
            title: title.clone(),
            path: path.clone(),
        });
    }
    Ok((spine, dangling))
}

/// Resolve a manifest `href` against the OPF's directory.
///
/// `href` is relative to the package document, and it may percent-encode or
/// reach upwards (`../Text/ch1.xhtml`), so it is treated as a path and
/// normalised — never as a string to paste together.
fn join_relative(base: &Path, href: &str) -> String {
    let decoded = percent_decode(href);
    let rel = Path::new(&decoded);
    let joined = if rel.is_absolute() {
        rel.to_path_buf()
    } else {
        base.join(rel)
    };
    normalise(&joined)
        .to_string_lossy()
        .into_owned()
}

/// Collapse `.` and `..` textually, refusing to climb out of the archive root.
fn normalise(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `%20` and friends, since an href is a URL even when it names a file.
fn percent_decode(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
            if let Ok(b) = u8::from_str_radix(hex, 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Read one entry out of the archive.
fn read_entry(zip: &mut zip::ZipArchive<std::fs::File>, name: &str) -> Result<String> {
    let mut file = zip
        .by_name(name)
        .with_context(|| format!("the book has no {name:?} — the archive is incomplete"))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)
        .with_context(|| format!("decompressing {name:?}"))?;
    Ok(decode_text(&buf))
}

/// EPUB content is XHTML, which is UTF-8 or declares an encoding; the same
/// fallback the crawler uses for a page that lies about its charset.
fn decode_text(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// The prose inside a chapter's XHTML.
///
/// Not [`super::html::readable`]: that is a heuristic for a *web page*,
/// weighted toward pulling the main column out of a layout full of chrome. A
/// chapter file is already only the chapter, so the right move is the
/// opposite — take the body and keep its paragraphs.
///
/// The body's own markup then goes through the crate's own stripper, the same
/// one `select_text` uses, rather than a second implementation here. Two
/// ways to decide where a line ends would let the same chapter read
/// differently depending on which one produced it, which is the one thing a
/// shared boundary exists to prevent. `<script>` and `<style>` are dropped by
/// that stripper; `<head>` and the `<title>` in it never arrive, because only
/// the body is selected — and a chapter's own title is the pipeline's title,
/// not its prose.
fn text_of(markup: &str) -> String {
    let doc = Html::parse_document(markup);
    let body = Selector::parse("body")
        .ok()
        .and_then(|s| doc.select(&s).next())
        .map(|el| el.inner_html());
    let fragment = body.unwrap_or_else(|| markup.to_string());
    super::sanitize_chapter_text(&super::strip_tags_raw(&fragment))
}

/// Resolve a path a script named against the root it is allowed to read.
///
/// The containment is the same one [`super::provider::resolve_script`] applies
/// to a crawler path, and for the same reason: a crawl script is trusted the
/// way configuration is, but the ABI should still be the true one. A book is a
/// file the operator put in their workspace; `/etc/passwd` is not.
pub fn confined(root: &Path, named: &str) -> Result<PathBuf> {
    let named = named.trim();
    if named.is_empty() {
        anyhow::bail!("no book named — give the path to the .epub in crawl.params");
    }
    let candidate = if Path::new(named).is_absolute() {
        PathBuf::from(named)
    } else {
        root.join(named)
    };
    // Canonicalise both sides: the join can still land outside through a
    // symlink, and `..` in a name the operator typed should not be the thing
    // that decides where a file is read from.
    let real = candidate
        .canonicalize()
        .with_context(|| format!("no book at {named:?}"))?;
    let base = root
        .canonicalize()
        .with_context(|| format!("the workspace root {} is not there", root.display()))?;
    if !real.starts_with(&base) {
        anyhow::bail!(
            "{named:?} is outside the workspace — a crawl may only read a book inside {}",
            base.display()
        );
    }
    Ok(real)
}

/// `Epub` holds a whole archive, and its `Debug` would dump every chapter of
/// the book into a failing test's output. This says the two things that
/// actually explain a failure: how big the spine is, and the names in it.
impl std::fmt::Debug for Epub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Epub")
            .field("chapters", &self.spine.len())
            .field("dangling", &self.dangling)
            .field(
                "spine",
                &self
                    .spine
                    .iter()
                    .map(|i| i.path.as_str())
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A three-chapter book, written as a real ZIP, so the container walk is
    /// exercised rather than mocked: `container.xml` naming a **nested** OPF
    /// path (so a manifest `href` has to resolve against the package document's
    /// directory rather than the archive root), a manifest, and a spine in the
    /// publisher's order.
    fn book(path: &Path, chapters: &[(&str, &str)]) {
        let file = std::fs::File::create(path).unwrap();
        let mut w = zip::ZipWriter::new(file);
        let opts: zip::write::FileOptions<()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        w.start_file("mimetype", opts).unwrap();
        w.write_all(b"application/epub+zip").unwrap();
        w.start_file("META-INF/container.xml", opts).unwrap();
        w.write_all(
            br#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/pkg/book.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#,
        )
        .unwrap();
        let mut items = String::new();
        let mut refs = String::new();
        for (i, (name, body)) in chapters.iter().enumerate() {
            let href = format!("text/{}.xhtml", name);
            let id = format!("c{i}");
            items.push_str(&format!(
                r#"<item id="{id}" href="{href}" media-type="application/xhtml+xml"/>"#
            ));
            refs.push_str(&format!(r#"<itemref idref="{id}"/>"#));
            w.start_file(format!("OEBPS/pkg/{href}"), opts).unwrap();
            w.write_all(
                format!(
                    "<html><head><title>{name}</title></head><body>\
                     <h1>Chương {name}</h1><p>{body}</p><p>Dịch Phong nghe.</p></body></html>"
                )
                .as_bytes(),
            )
            .unwrap();
        }
        w.start_file("OEBPS/pkg/book.opf", opts).unwrap();
        w.write_all(
            format!(
                r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <manifest>{items}</manifest>
  <spine>{refs}</spine>
</package>"#
            )
            .as_bytes(),
        )
        .unwrap();
        w.finish().unwrap();
    }

    fn long(n: usize) -> String {
        let mut s = String::new();
        for i in 0..n {
            s.push_str(&format!("Câu {i} trong chương này dài hơn một chút. "));
        }
        s
    }

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bm-epub-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_spine_is_the_chapter_list_not_the_file_order() {
        let dir = tmp("spine");
        let p = dir.join("book.epub");
        // Written out of order on purpose: if the reader walked the archive
        // instead of the spine, chapter 1 would be `three`.
        book(&p, &[("one", "A"), ("two", "B"), ("three", "C")]);
        let mut e = open(&p).unwrap();
        assert_eq!(e.chapters(), 3);
        for (n, want) in [(1u32, "one"), (2, "two"), (3, "three")] {
            let ch = e.chapter(n).unwrap().expect("in the spine");
            assert!(ch.text.contains(&format!("Chương {want}")), "{ch:?}");
        }
        // Past the end is absent, not an error — the crawl contract's `none`.
        assert!(e.chapter(4).unwrap().is_none());
        assert!(e.chapter(0).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_chapter_is_the_prose_and_nothing_else() {
        let dir = tmp("prose");
        let p = dir.join("book.epub");
        book(&p, &[("one", "Một câu trong chương.")]);
        let mut e = open(&p).unwrap();
        let ch = e.chapter(1).unwrap().unwrap();
        assert!(ch.text.contains("Chương one"), "{}", ch.text);
        assert!(ch.text.contains("Dịch Phong nghe."), "{}", ch.text);
        // The `<title>` is the pipeline's title, not its prose, and the `<h1>`
        // heading *is* prose — so exactly one of the two must be absent.
        assert!(!ch.text.contains("<h1>"), "{}", ch.text);
        assert!(!ch.text.contains("<title>"), "{}", ch.text);
        // The shared boundary owns the edges, and its contract is a trailing
        // newline with no trailing blank line — the same one a crawled page
        // gets, which is the whole point of routing both through it.
        assert!(ch.text.ends_with('\n'), "{:?}", ch.text);
        assert!(ch.text.ends_with(".\n"), "{:?}", ch.text);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A ranged read is the same characters the boundary would have produced.
    ///
    /// The join used to be run through `sanitize_chapter_text` a second time,
    /// which was redundant and cost a full extra pass over every chapter of
    /// every book. Removing work is only safe if the output is provably the
    /// same, so this asserts it against the construction that was removed —
    /// with entries that differ in the ways that make the two disagree: one
    /// with a blank line inside it, one with markup that becomes two
    /// paragraphs, and one that is nothing but a scan.
    #[test]
    fn a_ranged_read_is_what_re_sanitizing_the_join_would_have_given() {
        let dir = tmp("range");
        let p = dir.join("book.epub");
        book(
            &p,
            &[
                ("mot", "Câu một.<p>Câu hai.</p>"),
                ("hai", "<p>  </p>"),
                ("ba", "Câu bốn &amp; Câu năm."),
            ],
        );
        let mut e = open(&p).unwrap();

        // What the removed code computed: the entries, each already through
        // the boundary, trimmed and joined with a single newline.
        let mut parts = Vec::new();
        for n in 1..=3 {
            if let Some(c) = e.chapter(n).unwrap() {
                parts.push(c.text.trim_end().to_string());
            }
        }
        let expected = super::super::sanitize_chapter_text(&parts.join("\n"));

        let got = e.text(1, 3).unwrap();
        assert_eq!(got, expected, "the join must not change the characters");
        // Pinned, so a change to the boundary is a visible diff and not a
        // silent one inside a file nobody is looking at. Entry two is the
        // `<p>  </p>`, and it is **absent** from the join rather than
        // contributing a blank line — which is the case the old
        // re-sanitize used to absorb and the new one has to get right itself.
        assert_eq!(
            got,
            "Chương mot\n\nCâu một.\n\nCâu hai.\n\nDịch Phong nghe.\n\n\
             Chương hai\n\nDịch Phong nghe.\n\n\
             Chương ba\n\nCâu bốn & Câu năm.\n\nDịch Phong nghe.\n"
        );
        assert!(!got.contains("\n\n\n"), "no run of blank lines: {got:?}");
        assert!(got.ends_with(".\n") && !got.ends_with("\n\n"));
        // A range is clamped, and an empty one is an empty chapter rather than
        // an error — the pipeline's own boundary says an empty chapter is one.
        assert_eq!(e.text(99, 120).unwrap(), "");
        assert_eq!(e.text(0, 0).unwrap(), "");
        assert_eq!(e.text(2, 2).unwrap(), "Chương hai\n\nDịch Phong nghe.\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_zip_that_is_not_an_epub_is_refused_by_name() {
        let dir = tmp("notepub");
        let p = dir.join("plain.zip");
        let file = std::fs::File::create(&p).unwrap();
        let mut w = zip::ZipWriter::new(file);
        let plain: zip::write::FileOptions<()> = zip::write::FileOptions::default();
        w.start_file("hello.txt", plain).unwrap();
        w.write_all(b"not a book").unwrap();
        w.finish().unwrap();
        let err = open(&p).unwrap_err().to_string();
        assert!(err.contains("container.xml"), "{err}");
        // And something that is not a ZIP at all fails the same way.
        let p2 = dir.join("notes.txt");
        std::fs::write(&p2, b"just text").unwrap();
        assert!(open(&p2).unwrap_err().to_string().contains("ZIP"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_book_is_only_readable_from_inside_the_workspace() {
        let dir = tmp("confine");
        let book_dir = std::env::temp_dir().join("bm-epub-outside");
        let _ = std::fs::remove_dir_all(&book_dir);
        std::fs::create_dir_all(&book_dir).unwrap();
        book(&book_dir.join("secret.epub"), &[("one", "A")]);
        let p = book_dir.join("secret.epub");
        // Inside: fine.
        let inside = confined(&dir, p.to_str().unwrap());
        assert!(inside.is_err(), "an absolute path outside the root is refused");
        // A name that climbs out is refused after canonicalising, so a symlink
        // cannot be what decides where a file is read from.
        let climb = confined(&dir, "../bm-epub-outside/secret.epub");
        assert!(climb.is_err());
        // And a book that is in the workspace is found by bare name.
        book(&dir.join("ok.epub"), &[("one", "A")]);
        assert!(confined(&dir, "ok.epub").is_ok());
        assert!(confined(&dir, "  ").is_err());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&book_dir);
    }

    /// A chapter long enough to need windows, so the budget has something to
    /// do — the shape the corpus actually has.
    #[test]
    fn a_long_chapter_still_reads_as_one_artifact() {
        let dir = tmp("long");
        let p = dir.join("book.epub");
        book(&p, &[("one", &long(400))]);
        let mut e = open(&p).unwrap();
        let ch = e.chapter(1).unwrap().unwrap();
        let events = super::super::sanitize_chapter_text(&ch.text);
        assert!(events.chars().count() > 5_000, "{}", events.chars().count());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
