use super::*;
use serde_json::json;

fn layout_with_settings(crawl: serde_json::Value) -> (tempfile::TempDir, Layout) {
    let dir = tempfile::tempdir().expect("tmp");
    let layout = Layout::new(dir.path());
    let s = json!({ "crawl": crawl });
    let path = layout.settings();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_json::to_string_pretty(&s).unwrap()).unwrap();
    (dir, layout)
}

fn value_of(rows: &[Row], key: &str) -> String {
    rows.iter()
        .find_map(|r| match r {
            Row::Field { key: k, value, .. } if k == key => Some(value.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no field {key} in {rows:#?}"))
}

/// Everything the view says, flattened — for the assertions that care about
fn said(rows: &[Row]) -> String {
    rows.iter()
        .map(|r| match r {
            Row::Field { key, value, .. } => format!("{key}: {value}"),
            Row::Note(n) => n.clone(),
            Row::Section(s) => s.clone(),
            Row::Blank => String::new(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_warn(rows: &[Row], key: &str) -> bool {
    rows.iter()
        .any(|r| matches!(r, Row::Field { key: k, warn: true, .. } if k == key))
}

#[test]
fn the_view_answers_what_is_in_force_with_defaults_resolved() {
    let (_tmp, layout) = layout_with_settings(json!({ "mode": "manual" }));
    let settings = bm_core::read_json::<serde_json::Value>(&layout.settings()).unwrap();
    let rows = detail(&layout, &settings);
    // `pace_ms`, `timeout_secs` and the rest are absent from the file and
    assert_eq!(value_of(&rows, "mode"), "manual");
    assert!(value_of(&rows, "pace_ms").contains("750 ms"));
    assert!(value_of(&rows, "max_fetches").contains("64 round trips"));
    assert!(value_of(&rows, "user_agent").contains("built-in default"));
}

#[test]
fn a_script_that_is_not_there_is_the_line_that_matters() {
    // The silent-failure case: the setting names a crawler, the file does
    let (_tmp, layout) = layout_with_settings(json!({
        "mode": "script",
        "script": "crawl/nosuchsite.lua",
    }));
    let settings = bm_core::read_json::<serde_json::Value>(&layout.settings()).unwrap();
    let rows = rows(&layout, &settings);
    assert!(
        is_warn(&rows, "script"),
        "the miss must be a fault, not a value"
    );
    let said = said(&rows);
    assert!(value_of(&rows, "script").contains("NOT FOUND"), "{said}");
    assert!(said.contains("built-in fetcher"), "{said}");
}

#[test]
fn script_mode_with_no_script_says_itself_rather_than_looking_configured() {
    let (_tmp, layout) = layout_with_settings(json!({ "mode": "script", "script": "" }));
    let settings = bm_core::read_json::<serde_json::Value>(&layout.settings()).unwrap();
    let rows = rows(&layout, &settings);
    assert!(is_warn(&rows, "mode"));
    assert!(said(&rows).contains("built-in fetcher"));
}

#[test]
fn a_resolved_crawler_is_named_with_its_engine() {
    let (_tmp, layout) = layout_with_settings(json!({}));
    std::fs::create_dir_all(layout.crawl_workspace()).unwrap();
    std::fs::write(layout.crawl_workspace().join("truyencom.lua"), "-- x").unwrap();
    let settings = bm_core::read_json::<serde_json::Value>(&layout.settings()).unwrap();
    let mut s = settings.clone();
    s["crawl"]["mode"] = json!("script");
    s["crawl"]["script"] = json!("crawl/truyencom.lua");
    let rows = detail(&layout, &s);
    let v = value_of(&rows, "script");
    assert!(v.contains("truyencom.lua") && v.contains("lua"), "{v}");
    assert!(!is_warn(&rows, "script"));
    // And the same file is listed as the one in force.
    assert!(
        said(&rows).contains("in force"),
        "the in-force crawler must be marked: {rows:#?}"
    );
}

#[test]
fn headers_are_named_and_never_valued() {
    let (_tmp, layout) = layout_with_settings(json!({
        "headers": { "Cookie": "cf_clearance=SECRET", "Referer": "https://x/" }
    }));
    let settings = bm_core::read_json::<serde_json::Value>(&layout.settings()).unwrap();
    let rows = detail(&layout, &settings);
    let all: Vec<String> = rows
        .iter()
        .map(|r| match r {
            Row::Field { value, .. } => value.clone(),
            Row::Note(n) => n.clone(),
            _ => String::new(),
        })
        .collect();
    let joined = all.join("\n");
    assert!(joined.contains("Cookie"), "{joined}");
    assert!(
        !joined.contains("SECRET"),
        "a session cookie must never be painted for a screen-share: {joined}"
    );
}

#[test]
fn no_pace_is_flagged_because_a_cluster_is_the_thing_that_gets_banned() {
    let (_tmp, layout) = layout_with_settings(json!({ "pace_ms": 0 }));
    let settings = bm_core::read_json::<serde_json::Value>(&layout.settings()).unwrap();
    let rows = rows(&layout, &settings);
    assert!(
        is_warn(&rows, "pace_ms"),
        "the pace line must stand out: {rows:#?}"
    );
    assert!(said(&rows).contains("scraper banned"));
}

#[test]
fn the_book_links_come_from_the_frozen_index() {
    let (_tmp, layout) = layout_with_settings(json!({}));
    let mut idx = CrawlIndex::from_template("https://s/chuong-{n}", 1, 3, "hash");
    idx.total = Some(1200);
    idx.save(&layout).unwrap();
    let settings = bm_core::read_json::<serde_json::Value>(&layout.settings()).unwrap();
    let rows = detail(&layout, &settings);
    let v = value_of(&rows, "index");
    assert!(v.contains("template"), "{v}");
    let chapters = value_of(&rows, "chapters");
    assert!(chapters.contains("1200"), "{chapters}");
    // The link itself: the shape of the site's URLs, in the view.
    assert!(
        rows.iter()
            .any(|r| matches!(r, Row::Note(n) if n.contains("https://s/chuong-1"))),
        "{rows:#?}"
    );
}

#[test]
fn a_missing_bundled_tree_is_not_the_same_as_a_book_with_no_crawler() {
    // A workspace with no crawler of its own is normal. The *bundled* tree
    let (_tmp, layout) = layout_with_settings(json!({ "mode": "script" }));
    let settings = bm_core::read_json::<serde_json::Value>(&layout.settings()).unwrap();
    let rows = rows(&layout, &settings);
    let said = said(&rows);
    assert!(is_warn(&rows, "known"), "{said}");
    assert!(said.contains("built-in fetcher"), "{said}");
    assert!(
        said.contains("crawlers/known/"),
        "and where to fix it: {said}"
    );
    // The per-book line stays quiet: an empty workspace dir is expected.
    assert!(!is_warn(&rows, "this book"), "{said}");
}

#[test]
fn a_books_param_is_counted_and_its_volumes_named() {
    let (_tmp, layout) = layout_with_settings(json!({
        "params": { "books": "books" }
    }));
    let shelf = layout.work.join("books");
    std::fs::create_dir_all(&shelf).unwrap();
    std::fs::write(shelf.join("vol-01.epub"), b"one").unwrap();
    std::fs::write(shelf.join("vol-02.epub"), b"two").unwrap();
    let settings = bm_core::read_json::<serde_json::Value>(&layout.settings()).unwrap();
    let rows = detail(&layout, &settings);
    assert!(
        !is_warn(&rows, "books"),
        "a full shelf is not a fault: {rows:#?}"
    );
    let v = value_of(&rows, "books");
    assert!(v.contains("2 volumes"), "{v}");
    let said = said(&rows);
    assert!(
        said.contains("vol-01.epub") && said.contains("vol-02.epub"),
        "{said}"
    );
}

#[test]
fn a_books_param_that_is_missing_or_empty_is_a_fault() {
    // The quiet misconfiguration: a shelf that is not there fails at the
    let (_tmp, layout) = layout_with_settings(json!({
        "params": { "books": "books" }
    }));
    let settings = bm_core::read_json::<serde_json::Value>(&layout.settings()).unwrap();
    let missing = rows(&layout, &settings);
    assert!(is_warn(&missing, "books"), "{missing:#?}");
    assert!(
        value_of(&missing, "books").contains("NOT FOUND"),
        "{missing:#?}"
    );

    // And one that exists but holds nothing, which looks even more like a
    std::fs::create_dir_all(layout.work.join("books")).unwrap();
    let empty = rows(&layout, &settings);
    assert!(is_warn(&empty, "books"), "{empty:#?}");
    assert!(value_of(&empty, "books").contains("no .epub"), "{empty:#?}");
    assert!(
        said(&empty).contains("vol-01.epub"),
        "and how to fix it: {empty:#?}"
    );
}

#[test]
fn a_single_book_param_is_opened_or_flagged() {
    let (_tmp, layout) = layout_with_settings(json!({
        "params": { "epub": "tmp/book.epub" }
    }));
    let settings = bm_core::read_json::<serde_json::Value>(&layout.settings()).unwrap();
    let missing = rows(&layout, &settings);
    assert!(is_warn(&missing, "epub"), "{missing:#?}");

    std::fs::create_dir_all(layout.work.join("tmp")).unwrap();
    std::fs::write(layout.work.join("tmp/book.epub"), b"PK\x03\x04 x").unwrap();
    let found = detail(&layout, &settings);
    assert!(!is_warn(&found, "epub"), "{found:#?}");
    assert!(
        value_of(&found, "epub").contains("tmp/book.epub"),
        "{found:#?}"
    );
}

#[test]
fn a_folder_spelled_as_the_single_book_says_where_it_belongs() {
    let (_tmp, layout) = layout_with_settings(json!({
        "params": { "epub": "books" }
    }));
    std::fs::create_dir_all(layout.work.join("books")).unwrap();
    let settings = bm_core::read_json::<serde_json::Value>(&layout.settings()).unwrap();
    let rows = rows(&layout, &settings);
    assert!(is_warn(&rows, "epub"), "{rows:#?}");
    assert!(
        said(&rows).contains("crawl.params.books"),
        "the fix is one word away: {rows:#?}"
    );
}

#[test]
fn a_book_index_reads_as_volumes_rather_than_as_site_urls() {
    let (_tmp, layout) = layout_with_settings(json!({}));
    let mut idx = CrawlIndex::from_template("https://s/{n}", 1, 4, "hash");
    idx.total = Some(4);
    for (n, url) in [
        (1, "epub:books/vol-01.epub#1-16"),
        (2, "epub:books/vol-01.epub#17-32"),
        (3, "epub:books/vol-02.epub#1-16"),
        (4, "epub:books/vol-02.epub#17-32"),
    ] {
        idx.chapters.get_mut(&n).unwrap().url = Some(url.into());
    }
    idx.save(&layout).unwrap();
    let settings = bm_core::read_json::<serde_json::Value>(&layout.settings()).unwrap();
    let rows = detail(&layout, &settings);
    let said = said(&rows);
    // A book has no links, and saying so in the heading is half the point.
    assert!(
        rows.iter()
            .any(|r| matches!(r, Row::Section(s) if s == "Chapters")),
        "{said}"
    );
    // The book's own length, not a site's: a book has no site to ask.
    assert!(
        value_of(&rows, "chapters").contains("the book has 4"),
        "{said}"
    );
    // The breakdown, and where each volume starts.
    assert!(value_of(&rows, "volumes").contains("2 volumes"), "{said}");
    // The chapter numbers, which are what a run is ranged by — the spine
    assert!(said.contains("vol 1  books/vol-01.epub · ch 1-2"), "{said}");
    assert!(said.contains("vol 2  books/vol-02.epub · ch 3-4"), "{said}");
    // And the locator decoded: an encoding nobody typed should not be the
    assert!(said.contains("ch 1  volume 1 · spine 1-16"), "{said}");
    assert!(
        !said.contains("epub:books/"),
        "the raw locator is internal: {said}"
    );
}

#[test]
fn a_site_index_still_reads_as_a_site() {
    let (_tmp, layout) = layout_with_settings(json!({}));
    let mut idx = CrawlIndex::from_template("https://s/chuong-{n}", 1, 3, "hash");
    idx.total = Some(1200);
    idx.save(&layout).unwrap();
    let settings = bm_core::read_json::<serde_json::Value>(&layout.settings()).unwrap();
    let rows = detail(&layout, &settings);
    let said = said(&rows);
    assert!(
        rows.iter()
            .any(|r| matches!(r, Row::Section(s) if s == "Chapter links")),
        "a site is still a site: {said}"
    );
    assert!(
        value_of(&rows, "chapters").contains("site says 1200"),
        "{said}"
    );
    assert!(value_of(&rows, "index").contains("template"), "{said}");
    // No volume block for a site, and the URL is still the thing to print.
    assert!(said.contains("ch 1  https://s/chuong-1"), "{said}");
    assert!(
        !rows
            .iter()
            .any(|r| matches!(r, Row::Field { key, .. } if key == "volumes")),
        "{said}"
    );
}

#[test]
fn a_locator_that_is_not_one_is_not_a_crash() {
    // A hand-edited index can hold anything. The decoder is the only place
    assert_eq!(
        locator("epub:books/vol-01.epub#1-1"),
        Some(("books/vol-01.epub", 1, 1))
    );
    assert_eq!(locator("https://s/1"), None);
    assert_eq!(locator("epub:books/vol-01.epub"), None);
    assert_eq!(locator("epub:books/a#1-x"), None);
}

#[test]
fn a_book_with_no_index_is_told_how_to_get_one() {
    let (_tmp, layout) = layout_with_settings(json!({}));
    let settings = bm_core::read_json::<serde_json::Value>(&layout.settings()).unwrap();
    // The concise view leads with it, because nothing else can be counted
    let concise = rows(&layout, &settings);
    assert!(
        value_of(&concise, "reading").contains(":crawl"),
        "{concise:#?}"
    );
    // And the detail still names the absent file and the command that
    let rows = detail(&layout, &settings);
    assert!(value_of(&rows, "index").contains("none"));
    assert!(rows
        .iter()
        .any(|r| matches!(r, Row::Note(n) if n.contains(":crawl"))));
}

/// **The screen is a screenful shorter.** This is the whole change, so it is
#[test]
fn the_default_view_is_the_verdict_and_nothing_else() {
    let (_tmp, layout) = layout_with_settings(json!({}));
    let settings = bm_core::read_json::<serde_json::Value>(&layout.settings()).unwrap();
    let concise = rows(&layout, &settings);
    let said = said(&concise);
    for noise in [
        "pace_ms",
        "timeout_secs",
        "max_seconds",
        "max_fetches",
        "user_agent",
        "headers",
    ] {
        assert!(
            !said.contains(noise),
            "{noise} is not an answer, so it is not on the default screen:\n{said}"
        );
    }
    // What is left is the verdict, and the two headings that organise it.
    assert!(said.contains("Reading"), "{said}");
    assert!(said.contains("Faults"), "{said}");
    assert!(said.contains("no chapter index yet"), "{said}");
    // Counted in *facts*, not rows: a heading and a blank are layout, and
    let facts = concise
        .iter()
        .filter(|r| matches!(r, Row::Field { .. }))
        .count();
    assert_eq!(
        facts, 3,
        "a verdict is three facts, not a screen: {concise:#?}"
    );
}

#[test]
fn nothing_wrong_is_said_once_rather_than_left_to_be_inferred() {
    // A workspace with a real book, a real crawler and a real index.
    let (_tmp, layout) = layout_with_settings(json!({
        "mode": "script",
        "script": "crawlers/examples/epub.lua",
        "params": { "books": "books" },
    }));
    std::fs::create_dir_all(layout.crawlers_dir().join("examples")).unwrap();
    std::fs::write(layout.crawlers_dir().join("examples/epub.lua"), "-- crawl").unwrap();
    let shelf = layout.work.join("books");
    std::fs::create_dir_all(&shelf).unwrap();
    std::fs::write(shelf.join("vol-01.epub"), b"one").unwrap();
    let mut idx = CrawlIndex::from_template("x", 1, 3, "hash");
    idx.total = Some(3);
    for (n, url) in [
        (1, "epub:books/vol-01.epub#1-2"),
        (2, "epub:books/vol-01.epub#3-4"),
        (3, "epub:books/vol-01.epub#5-6"),
    ] {
        idx.chapters.get_mut(&n).unwrap().url = Some(url.into());
    }
    idx.save(&layout).unwrap();
    let settings = bm_core::read_json::<serde_json::Value>(&layout.settings()).unwrap();
    let concise = rows(&layout, &settings);
    assert_eq!(
        value_of(&concise, "checks"),
        "— none",
        "a working crawl says so: {concise:#?}"
    );
    let said = said(&concise);
    assert!(said.contains("3 chapters from 1"), "{said}");
    assert!(said.contains("epub.lua"), "{said}");
    // The library's whole breakdown, on one line.
    assert!(said.contains("1 · vol-01.epub ch 1-3"), "{said}");
}

#[test]
fn a_volume_list_longer_than_a_line_is_capped_rather_than_scrolled_away() {
    let (_tmp, layout) = layout_with_settings(json!({}));
    let mut idx = CrawlIndex::from_template("x", 1, 9, "hash");
    idx.total = Some(9);
    for n in 1..=9u32 {
        idx.chapters.get_mut(&n).unwrap().url = Some(format!("epub:books/vol-{n:02}.epub#1-2"));
    }
    idx.save(&layout).unwrap();
    let settings = bm_core::read_json::<serde_json::Value>(&layout.settings()).unwrap();
    let said = said(&rows(&layout, &settings));
    assert!(
        said.contains("…5 more"),
        "nine volumes are not one line: {said}"
    );
    assert!(said.contains("vol-01.epub"), "{said}");
    assert!(
        !said.contains("vol-09.epub"),
        "and the tail is a count: {said}"
    );
}

#[test]
fn the_missing_bundled_tree_is_not_a_fault_for_a_book_that_reads_no_website() {
    // `crawlers/known/` is empty in this checkout, and the EPUB example
    let (_tmp, layout) = layout_with_settings(json!({
        "mode": "script",
        "script": "crawlers/examples/epub.lua",
        "params": { "epub": "tmp/book.epub" },
    }));
    // The example crawler is present; the site tree is not, and cannot be
    std::fs::create_dir_all(layout.crawlers_dir().join("examples")).unwrap();
    std::fs::write(layout.crawlers_dir().join("examples/epub.lua"), "-- crawl").unwrap();
    let settings = bm_core::read_json::<serde_json::Value>(&layout.settings()).unwrap();
    let said = said(&rows(&layout, &settings));
    assert!(
        !said.contains("crawlers/known"),
        "the site tree is invisible to a book: {said}"
    );
    assert!(
        !said.contains("built-in fetcher"),
        "a local book has no website to fall back from: {said}"
    );
    // The book itself is still the one thing reported, and it is still
    assert!(said.contains("NOT FOUND"), "{said}");
}
