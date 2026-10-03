use super::*;

#[test]
fn the_import_prompt_takes_a_number_and_a_path_but_never_guesses_the_number() {
    let mut app = App::new("http://x");
    let p = |buf: &str| TextPrompt::new(TextKind::Import, "t", "h", buf);

    // `<chapter> <path>` → the op carries both.
    match submit_text(&mut app, &p("34 /tmp/ch34.txt")).unwrap() {
        Job::Op { req, .. } => {
            assert_eq!(req.op, Op::Import);
            assert_eq!(req.chapter, Some(34));
            assert_eq!(req.paths, vec!["/tmp/ch34.txt".to_string()]);
        }
        other => panic!("{other:?}"),
    }
    // A path alone: the number comes from the filename, later, by the importer
    match submit_text(&mut app, &p("/tmp/ch217.txt")).unwrap() {
        Job::Op { req, .. } => {
            assert_eq!(req.chapter, None);
            assert_eq!(req.paths, vec!["/tmp/ch217.txt".to_string()]);
        }
        other => panic!("{other:?}"),
    }
    // Several files, comma-separated: every one must carry its own number or be
    match submit_text(&mut app, &p("34 a.txt, b.txt")).unwrap() {
        Job::Op { req, .. } => assert_eq!(req.paths.len(), 2),
        other => panic!("{other:?}"),
    }

    // Refused, with the reason, so the prompt stays open.
    assert!(submit_text(&mut app, &p("  "))
        .unwrap_err()
        .contains("nothing to import"));
    assert!(submit_text(&mut app, &p("34"))
        .unwrap_err()
        .contains("give the path"));
    assert!(submit_text(&mut app, &p("0 /tmp/ch0.txt"))
        .unwrap_err()
        .contains("chapter 0"));
}

#[test]
fn crawl_template_takes_a_placeholder_or_nothing_at_all() {
    let mut app = App::new("http://x");
    let p = TextPrompt::new(TextKind::CrawlTemplate, "t", "h", "https://x/chuong");
    assert!(submit_text(&mut app, &p).unwrap_err().contains("{n}"));
    let p = TextPrompt::new(TextKind::CrawlTemplate, "t", "h", "https://x/chuong-{n}");
    assert!(submit_text(&mut app, &p).is_ok());
    // Empty probes without saving a template: a crawler with a `discover()` has
    let p = TextPrompt::new(TextKind::CrawlTemplate, "t", "h", "   ");
    assert!(submit_text(&mut app, &p).is_ok());
}

/// Pasting a URL we have a crawler for should say which one, and how to get
#[test]
fn a_known_site_url_names_its_crawler_instead_of_only_refusing() {
    let mut app = App::new("http://x");
    let p = TextPrompt::new(
        TextKind::CrawlTemplate,
        "t",
        "h",
        "https://readnovelfull.com/the-sword-god-of-the-universe.html",
    );
    let err = submit_text(&mut app, &p).unwrap_err();
    assert!(err.contains("readnovelfull.com"), "{err}");
    assert!(
        err.contains("crawlers/known/readnovelfull.lua"),
        "the refusal must name the crawler, not just refuse: {err}"
    );

    // And the live note under the prompt, which is what saves the round trip.
    let note = p.known_note().expect("a recognised URL gets a note");
    assert!(note.contains("readnovelfull.com"), "{note}");
    assert!(note.contains("readnovelfull.lua"), "{note}");
    // It has to say what to do, or the note is only a label.
    assert!(note.contains("no {n} in its URLs"), "{note}");
}

/// A site on the list that is blocked must say so in the dialog, not offer a
#[test]
fn a_blocked_known_site_says_so_rather_than_offering_a_crawler() {
    let mut app = App::new("http://x");
    let p = TextPrompt::new(
        TextKind::CrawlTemplate,
        "t",
        "h",
        "https://novelfull.com/a-book/chapter-1",
    );
    let note = p.known_note().expect("a recognised URL gets a note");
    assert!(note.contains("no bundled crawler"), "{note}");
    let err = submit_text(&mut app, &p).unwrap_err();
    assert!(err.contains("no crawler for it"), "{err}");
    assert!(err.contains("Cloudflare"), "and why: {err}");
}

/// The note is a function of what is on screen, so it must not stick.
#[test]
fn the_known_site_note_tracks_the_buffer_and_only_where_it_belongs() {
    let p = TextPrompt::new(
        TextKind::CrawlTemplate,
        "t",
        "h",
        "https://storya.click/truyen/a/chuong-{n}",
    );
    // A template with `{n}` in it is already a mapping; matching it against the
    assert_eq!(p.known_site(), None);
    // An unknown site says nothing, rather than guessing.
    let p = TextPrompt::new(TextKind::CrawlTemplate, "t", "h", "https://example.com/c/1");
    assert_eq!(p.known_site(), None);
    // A prompt that is not about URLs never shows one, whatever it holds.
    let p = TextPrompt::new(
        TextKind::Import,
        "t",
        "h",
        "https://readnovelfull.com/the-sword-god.html",
    );
    assert_eq!(p.known_site(), None);
    assert_eq!(p.known_note(), None);
    // And a known one resolves from a bare host, scheme and all.
    let p = TextPrompt::new(TextKind::CrawlTemplate, "t", "h", "storya.click");
    assert_eq!(p.known_site().map(|s| s.host), Some("storya.click"));
    // Templatable, so no "no {n}" instruction, that would be a lie here.
    let note = p.known_note().unwrap();
    assert!(!note.contains("no {n}"), "{note}");
}
