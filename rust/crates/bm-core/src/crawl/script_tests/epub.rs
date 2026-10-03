use super::*;

/// A two-chapter EPUB, written as a real ZIP so the container walk is the
fn epub_fixture(path: &std::path::Path) {
    epub_fixture_named(
        path,
        &[
            ("mot-chuong", "Một câu trong chương đầu tiên của cuốn sách."),
            (
                "hai-chuong",
                "Câu thứ hai nằm ở chương thứ hai của cuốn sách.",
            ),
        ],
    );
}

/// The same builder for an arbitrary spine. A multi-volume test needs volumes
fn epub_fixture_named(path: &std::path::Path, chapters: &[(&str, &str)]) {
    use std::io::Write;
    let file = std::fs::File::create(path).unwrap();
    let mut w = zip::ZipWriter::new(file);
    let o: zip::write::FileOptions<()> = zip::write::FileOptions::default();
    w.start_file("mimetype", o).unwrap();
    w.write_all(b"application/epub+zip").unwrap();
    w.start_file("META-INF/container.xml", o).unwrap();
    w.write_all(
        br#"<container version="1.0"><rootfiles>
<rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
</rootfiles></container>"#,
    )
    .unwrap();
    let mut items = String::new();
    let mut refs = String::new();
    // Long enough to clear the provider's own length guard (200 bytes), which
    let filler = "Câu văn nối tiếp trong chương, đủ dài để qua ngưỡng kiểm tra. ";
    for (i, (name, body)) in chapters.iter().enumerate() {
        let id = format!("c{i}");
        items.push_str(&format!(r#"<item id="{id}" href="text/{name}.xhtml"/>"#));
        refs.push_str(&format!(r#"<itemref idref="{id}"/>"#));
        w.start_file(format!("OEBPS/text/{name}.xhtml"), o).unwrap();
        w.write_all(
            format!(
                "<html><head><title>{name}</title></head><body>\
                 <h1>Chương {name}</h1><p>{body} {}</p></body></html>",
                filler.repeat(5)
            )
            .as_bytes(),
        )
        .unwrap();
    }
    w.start_file("OEBPS/content.opf", o).unwrap();
    w.write_all(
        format!(
            r#"<package version="3.0"><manifest>{items}</manifest><spine>{refs}</spine></package>"#
        )
        .as_bytes(),
    )
    .unwrap();
    w.finish().unwrap();
}

/// A workspace holding a book, and the spec that crawls it.
fn epub_spec(workspace: &std::path::Path, name: &str) -> CrawlSpec {
    let mut s = spec("lua", "epub.lua", &sample(name));
    // The read root, which is the whole confinement story: a book inside it is
    s.read_root = workspace.to_path_buf();
    s.params
        .insert("epub".into(), serde_json::json!("book.epub"));
    s
}

/// The shipped EPUB crawler, over a real book, through the real Lua engine.
#[test]
fn the_epub_template_reads_a_local_book_and_stops_at_its_end() {
    let dir = std::env::temp_dir().join(format!("bm-epub-tpl-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    epub_fixture(&dir.join("book.epub"));
    let s = epub_spec(&dir, "epub.lua");

    // Chapter one: prose, through the shared boundary.
    let got = Provider::new(&s).crawl(1, None, 1).expect("chapter 1");
    let text = text_of(got.outcome);
    assert!(text.contains("Chương mot-chuong"), "{text}");
    assert!(text.contains("chương đầu tiên"), "{text}");
    assert!(!text.contains("<h1>"), "markup leaked: {text}");
    assert!(
        !text.contains("<title>"),
        "the book's title is not prose: {text}"
    );

    // The second chapter is the second spine item, not the second file on
    let text = text_of(Provider::new(&s).crawl(2, None, 1).expect("ch 2").outcome);
    assert!(text.contains("chương thứ hai"), "{text}");

    // Past the end: **absent, not a failure.** This is the assertion the whole
    match Provider::new(&s)
        .crawl(3, None, 1)
        .expect("ch 3 is not a crash")
        .outcome
    {
        CrawlOutcome::Absent { reason } => assert!(!reason.is_empty(), "{reason}"),
        other => panic!("a chapter past the end must be absent, not {other:?}"),
    }

    // `discover` knows the book's own length, so a range can be trimmed before
    let found = Provider::new(&s)
        .discover(1, 5)
        .expect("discover")
        .expect("has one");
    assert_eq!(found.total, Some(2));
    assert_eq!(found.chapters.len(), 2, "the range is trimmed to the book");

    // No book at all is a refusal by name, not an empty chapter.
    let mut missing = epub_spec(&dir, "epub.lua");
    missing
        .params
        .insert("epub".into(), serde_json::json!("nope.epub"));
    let err = Provider::new(&missing)
        .crawl(1, None, 1)
        .expect_err("a book that is not there")
        .to_string();
    assert!(err.contains("nope.epub"), "{err}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Several volumes are numbered as **one book**: the running count continues
#[test]
fn volumes_are_numbered_as_one_book_and_carry_their_own_locator() {
    let dir = std::env::temp_dir().join(format!("bm-epub-multi-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("books")).unwrap();
    // Two chapters each, with prose that names its volume, and written out of
    epub_fixture_named(
        &dir.join("books/vol-02.epub"),
        &[
            ("v2-mot", "Một câu trong chương đầu của tập hai."),
            ("v2-hai", "Một câu trong chương cuối của tập hai."),
        ],
    );
    epub_fixture_named(
        &dir.join("books/vol-01.epub"),
        &[
            ("v1-mot", "Một câu trong chương đầu của tập một."),
            ("v1-hai", "Một câu trong chương cuối của tập một."),
        ],
    );

    let mut s = spec("lua", "epub.lua", &sample("epub.lua"));
    s.read_root = dir.clone();
    s.params.insert("books".into(), serde_json::json!("books"));

    let found = Provider::new(&s)
        .discover(1, 10)
        .expect("discover")
        .expect("has chapters");
    assert_eq!(found.total, Some(4), "two volumes of two chapters");
    let ns: Vec<u32> = found.chapters.iter().map(|c| c.n).collect();
    assert_eq!(
        ns,
        vec![1, 2, 3, 4],
        "the numbering is dense across volumes"
    );

    // Every chapter names its volume and spine range, and volume 1's chapters
    let first = found.chapters[0].url.clone().expect("locator");
    let third = found.chapters[2].url.clone().expect("locator");
    assert!(first.starts_with("epub:books/vol-01.epub#"), "{first}");
    assert!(third.starts_with("epub:books/vol-02.epub#"), "{third}");

    // The locator is what `crawl` reads back, and reading it is a *lookup*:
    let text = text_of(
        Provider::new(&s)
            .crawl(3, Some(third.as_str()), 1)
            .expect("ch 3")
            .outcome,
    );
    assert!(text.contains("v2-mot"), "{text}");
    assert!(text.contains("tập hai"), "{text}");
    let text = text_of(
        Provider::new(&s)
            .crawl(1, Some(first.as_str()), 1)
            .expect("ch 1")
            .outcome,
    );
    assert!(text.contains("tập một"), "{text}");
    assert!(text.contains("v1-mot"), "{text}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// A book outside the workspace is refused, even though the crawler is right
#[test]
fn a_book_outside_the_workspace_is_refused_by_the_crawler_too() {
    let dir = std::env::temp_dir().join(format!("bm-epub-out-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    epub_fixture(&dir.join("book.epub"));
    let outside = std::env::temp_dir().join(format!("bm-epub-far-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&outside);
    std::fs::create_dir_all(&outside).unwrap();
    epub_fixture(&outside.join("book.epub"));

    let mut s = epub_spec(&dir, "epub.lua");
    s.params.insert(
        "epub".into(),
        serde_json::json!(outside.join("book.epub").display().to_string()),
    );
    let err = Provider::new(&s)
        .crawl(1, None, 1)
        .expect_err("a book outside the workspace")
        .to_string();
    assert!(err.contains("outside the workspace"), "{err}");

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&outside);
}
