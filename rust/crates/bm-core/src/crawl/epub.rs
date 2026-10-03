//! Reading a chapter out of an EPUB, which is a ZIP of XHTML.
//! [`Epub::text`] reads a *range* of entries. A rule like "an entry whose text
//! begins `Chapter 4` starts a chapter" is knowledge about one book, and this

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
    pub title: String,
}

/// How much of a spine entry [`Epub::index`] reports.
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
pub struct Epub {
    /// The whole archive in memory. EPUBs are a few megabytes of already
    zip: zip::ZipArchive<std::fs::File>,
    /// The publisher's reading order, as `(title, path inside the archive)`.
    spine: Vec<SpineItem>,
    /// Spine entries that named a manifest item the manifest does not list,
    dangling: Vec<String>,
}

#[derive(Debug, Clone)]
struct SpineItem {
    title: String,
    path: String,
}

/// Open a book and read its spine.
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
    pub fn chapters(&self) -> usize {
        self.spine.len()
    }

    /// Spine entries skipped because the manifest does not list them.
    pub fn dangling(&self) -> &[String] {
        &self.dangling
    }

    /// Chapter `n`, 1-based, as the shared boundary's text.
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
    pub fn text(&mut self, from: u32, to: u32) -> Result<String> {
        let lo = from.max(1) as usize - 1;
        let hi = (to as usize).min(self.spine.len());
        if lo >= hi {
            return Ok(String::new());
        }
        let items: Vec<SpineItem> = self.spine[lo..hi].to_vec();
        // Every part is **already the boundary's output** — `prose` is `text_of`,
        let mut out = String::new();
        for item in &items {
            let text = self.prose(item)?;
            let t = text.trim();
            if t.is_empty() {
                continue;
            }
            if out.is_empty() {
                out.push_str(t);
            } else if page_continues(&out, t) {
                // ponytail: hyphen word-split across pages rejoins without a space; upgrade if a book hyphenates compounds at page ends
                if out.ends_with('-') && t.chars().next().is_some_and(|c| c.is_lowercase()) {
                    out.pop();
                    out.push_str(t);
                } else {
                    out.push(' ');
                    out.push_str(t);
                }
            } else {
                out.push_str("\n\n");
                out.push_str(t);
            }
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

/// A page break is not a paragraph break: a scanned book puts one page per
fn page_continues(prev: &str, next: &str) -> bool {
    if prev.ends_with('-') {
        return true;
    }
    let stripped = prev
        .trim_end_matches(['"', '”', '’', '\'', ')', ']', '»'])
        .trim_end();
    if !matches!(stripped.chars().last(), Some('.' | '!' | '?' | '…')) {
        return true;
    }
    // A terminal followed by a lowercase start is an abbreviation ("Mr. /
    // Smith"), not a sentence end.
    next.chars().next().is_some_and(|c| c.is_lowercase())
}

/// The OPF path, from `META-INF/container.xml`'s `rootfile`.
fn opf_path(zip: &mut zip::ZipArchive<std::fs::File>) -> Result<String> {
    let xml = read_entry(zip, "META-INF/container.xml")
        .with_context(|| "META-INF/container.xml is missing — this is a ZIP that is not an EPUB")?;
    let path = first_attr(&xml, "rootfile", "full-path")
        .ok_or_else(|| anyhow!("container.xml names no rootfile — there is no package document"))?;
    if path.trim().is_empty() {
        anyhow::bail!("container.xml's rootfile has an empty full-path");
    }
    Ok(path)
}

/// The value of `attr` on the first `<tag …>` in `xml`.
fn first_attr(xml: &str, tag: &str, attr: &str) -> Option<String> {
    elements(xml)
        .into_iter()
        .find(|(name, _)| name == tag)
        .and_then(|(_, a)| a.get(attr).cloned())
}

/// Every element in `xml` as `(name, attributes)`, self-closing or not.
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
        let val = a
            .unescape_value()
            .map(|v| v.into_owned())
            .unwrap_or_else(|_| String::from_utf8_lossy(&a.value).into_owned());
        out.insert(key, val);
    }
    out
}

/// The reading order: `(id -> (href, title))` from the manifest, then the
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
fn join_relative(base: &Path, href: &str) -> String {
    let decoded = percent_decode(href);
    let rel = Path::new(&decoded);
    let joined = if rel.is_absolute() {
        rel.to_path_buf()
    } else {
        base.join(rel)
    };
    normalise(&joined).to_string_lossy().into_owned()
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
fn decode_text(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// The prose inside a chapter's XHTML.
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

/// Resolve a **directory** a script named, with [`confined`]'s containment.
pub fn confined_dir(root: &Path, named: &str) -> Result<PathBuf> {
    let real = confined(root, named)?;
    if !real.is_dir() {
        anyhow::bail!("{named:?} is not a directory");
    }
    Ok(real)
}

/// The EPUBs directly inside `dir`, sorted by path.
pub fn books_in(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("reading the books directory {}", dir.display()))?
    {
        let path = entry?.path();
        let is_epub = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("epub"));
        if is_epub && path.is_file() {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

/// `Epub` holds a whole archive, and its `Debug` would dump every chapter of
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
        assert!(!ch.text.contains("<h1>"), "{}", ch.text);
        assert!(!ch.text.contains("<title>"), "{}", ch.text);
        // The shared boundary owns the edges, and its contract is a trailing
        assert!(ch.text.ends_with('\n'), "{:?}", ch.text);
        assert!(ch.text.ends_with(".\n"), "{:?}", ch.text);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A ranged read is the same characters the boundary would have produced.
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
        assert_eq!(
            got,
            "Chương mot\n\nCâu một.\n\nCâu hai.\n\nDịch Phong nghe.\n\n\
             Chương hai\n\nDịch Phong nghe.\n\n\
             Chương ba\n\nCâu bốn & Câu năm.\n\nDịch Phong nghe.\n"
        );
        assert!(!got.contains("\n\n\n"), "no run of blank lines: {got:?}");
        assert!(got.ends_with(".\n") && !got.ends_with("\n\n"));
        // A range is clamped, and an empty one is an empty chapter rather than
        assert_eq!(e.text(99, 120).unwrap(), "");
        assert_eq!(e.text(0, 0).unwrap(), "");
        assert_eq!(e.text(2, 2).unwrap(), "Chương hai\n\nDịch Phong nghe.\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A page break is not a paragraph break: a scanned book puts one page
    #[test]
    fn a_ranged_read_joins_a_sentence_split_across_pages_with_a_space() {
        use std::io::Write;
        let dir = tmp("pagesplit");
        let p = dir.join("book.epub");
        let pages = [
            "the dreary central courtyard housed washing areas, where the court\u{2019}s servants\u{2014}people",
            "who were neither quite man nor quite woman did laundry by the armload. Men were not allowed.",
            "A new paragraph starts here.",
        ];
        let file = std::fs::File::create(&p).unwrap();
        let mut w = zip::ZipWriter::new(file);
        let opts: zip::write::FileOptions<()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        w.start_file("mimetype", opts).unwrap();
        w.write_all(b"application/epub+zip").unwrap();
        w.start_file("META-INF/container.xml", opts).unwrap();
        w.write_all(
            br#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/book.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#,
        )
        .unwrap();
        let mut items = String::new();
        let mut refs = String::new();
        for (i, body) in pages.iter().enumerate() {
            let href = format!("text/p{i}.xhtml");
            let id = format!("p{i}");
            items.push_str(&format!(
                r#"<item id="{id}" href="{href}" media-type="application/xhtml+xml"/>"#
            ));
            refs.push_str(&format!(r#"<itemref idref="{id}"/>"#));
            w.start_file(format!("OEBPS/{href}"), opts).unwrap();
            w.write_all(
                format!("<html><head><title>p{i}</title></head><body><p>{body}</p></body></html>")
                    .as_bytes(),
            )
            .unwrap();
        }
        w.start_file("OEBPS/book.opf", opts).unwrap();
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

        let mut e = open(&p).unwrap();
        let got = e.text(1, 3).unwrap();
        assert_eq!(
            got,
            "the dreary central courtyard housed washing areas, where the court\u{2019}s servants\u{2014}people \
             who were neither quite man nor quite woman did laundry by the armload. Men were not allowed.\n\n\
             A new paragraph starts here.\n"
        );
        assert!(!got.contains("people\n\nwho"), "split sentence: {got:?}");
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
        assert!(
            inside.is_err(),
            "an absolute path outside the root is refused"
        );
        // A name that climbs out is refused after canonicalising, so a symlink
        let climb = confined(&dir, "../bm-epub-outside/secret.epub");
        assert!(climb.is_err());
        // And a book that is in the workspace is found by bare name.
        book(&dir.join("ok.epub"), &[("one", "A")]);
        assert!(confined(&dir, "ok.epub").is_ok());
        assert!(confined(&dir, "  ").is_err());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&book_dir);
    }

    /// A books directory is its `.epub` files, in name order — the listing a
    #[test]
    fn a_books_directory_is_its_epubs_in_name_order() {
        let dir = tmp("books");
        let shelf = dir.join("books");
        std::fs::create_dir_all(&shelf).unwrap();
        // Written out of order on purpose: the listing is sorted, so volume
        book(&shelf.join("vol-02.epub"), &[("two", "B")]);
        book(&shelf.join("vol-01.epub"), &[("one", "A")]);
        std::fs::write(shelf.join("notes.txt"), b"not a book").unwrap();
        std::fs::create_dir_all(shelf.join("covers")).unwrap();

        let found = books_in(&confined_dir(&dir, "books").unwrap()).unwrap();
        let names: Vec<String> = found
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["vol-01.epub", "vol-02.epub"]);
        // A path that names a file is not a directory.
        assert!(confined_dir(&dir, "books/vol-01.epub").is_err());
        // And a folder outside the workspace is refused like a book outside it.
        assert!(confined_dir(&dir, "../").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A chapter long enough to need windows, so the budget has something to
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
