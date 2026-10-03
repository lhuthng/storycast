use super::*;

/// **The gate for the templates.** Each one, run over a page captured from the
/// site it is written for, must produce exactly the text recorded beside it.
///
/// Both halves of each pair are checked for *absence* as well as presence,
/// because a crawler that returns the right prose plus the site's navigation is
/// not a crawler that works — the extra lines become audio.
#[test]
fn the_site_templates_reproduce_their_captured_goldens() {
    let base = fixture::start(vec![
        (
            "/chuong-1.html".into(),
            200,
            site("truyencom-chapter").to_string(),
        ),
        ("/wn-ch1".into(), 200, site("webnovel-chapter").to_string()),
        (
            "/the-sword-god-of-the-universe/chapter-1-genius-of-the-jing-clan.html".into(),
            200,
            site("readnovelfull-chapter").to_string(),
        ),
    ]);

    // (template, chapter URL, golden, the headline form the site writes in)
    for (file, path, golden, headline) in [
        (
            "truyencom.lua",
            format!("{base}/chuong-1.html"),
            "truyencom-chapter",
            "Chương ",
        ),
        (
            "webnovel.lua",
            format!("{base}/wn-ch1"),
            "webnovel-chapter",
            "Chương ",
        ),
        (
            "readnovelfull.lua",
            format!("{base}/the-sword-god-of-the-universe/chapter-1-genius-of-the-jing-clan.html"),
            "readnovelfull-chapter",
            "Chapter ",
        ),
    ] {
        let mut s = spec("lua", file, &template(file));
        s.url_template = path.clone();
        let got = Provider::new(&s).crawl(1, Some(&path), 1).expect(file);
        let text = text_of(got.outcome);
        assert_eq!(text, site_golden(golden), "{file} diverged on {golden}");

        // The chrome that is on the page and must not be in the chapter.
        for chrome in [
            "Chương trước",   // truyencom's prev-chapter button
            "Báo lỗi chương", // …and its report button
            "chapter-nav",    // the nav element itself
            "Next Chapter",   // readnovelfull's next link
            "Prev Chapter",   // …and its previous one
        ] {
            assert!(
                !text.contains(chrome),
                "{file} leaked the site's {chrome:?} into the chapter"
            );
        }
        // The prose is paragraphs, not one breath. This is the assertion the
        // whole truyencom template exists to satisfy: without the CR split the
        // output is a single 10,000-character line that still "passes".
        let paras = text.matches("\n\n").count() + 1;
        assert!(
            paras > 20,
            "{file} produced {paras} paragraph(s) — the container's own separator was not honoured"
        );
        assert!(!text.contains('\r'), "{file} left a raw carriage return");
        // And the headline leads, because the pipeline speaks the first line.
        assert!(
            text.starts_with(headline),
            "{file} does not open with the chapter headline: {:?}",
            &text[..text.len().min(60)]
        );
    }
}

/// The webnovel template's whole reason for existing: `div.chapter_content` is
/// the container everyone reaches for, and it is not the chapter. Asserting the
/// absence of the chrome that lives in it is the test.
#[test]
fn the_webnovel_template_does_not_return_the_chapter_shell() {
    let base = fixture::start(vec![(
        "/wn-ch1".into(),
        200,
        site("webnovel-chapter").to_string(),
    )]);
    let mut s = spec("lua", "webnovel.lua", &template("webnovel.lua"));
    s.url_template = format!("{base}/wn-ch1");
    let text = text_of(Provider::new(&s).crawl(1, None, 1).unwrap().outcome);
    for shell in [
        "Tác giả:",                // the author byline
        "Legend of the Paladin",   // …and the name beside it
        "Tu Chân Liêu Thiên Quần", // the book title
        "©",                       // the WebNovel copyright line
        "Biên tập viên",           // the editor credit
        "cha-words",               // the selector itself
        "appendClass",             // the EJS template behind the page
        "isLock",
        "j_chapter",
    ] {
        assert!(
            !text.contains(shell),
            "the chapter shell leaked {shell:?} into the prose"
        );
    }
    // The 39 KB client-side template that follows the prose on the live page
    // must not appear either: `strip_tags` drops <script> contents, and this is
    // what proves it on a page where not dropping it would be spectacular.
    assert!(!text.contains("ejs"), "the EJS template leaked");
    assert!(!text.contains('<'), "a tag survived into the prose");
}

/// The truyencom tail artifact is dropped in the *script*, because the host's
/// artifact list knows about Storya's markers and not about this site's.
#[test]
fn the_truyencom_template_drops_the_sites_own_end_marker() {
    let base = fixture::start(vec![(
        "/chuong-1.html".into(),
        200,
        site("truyencom-chapter").to_string(),
    )]);
    let mut s = spec("lua", "truyencom.lua", &template("truyencom.lua"));
    s.url_template = format!("{base}/chuong-{{n}}.html");
    let text = text_of(Provider::new(&s).crawl(1, None, 1).unwrap().outcome);
    assert!(
        !text.contains("bản chương xong"),
        "the site's end-of-draft marker survived: {:?}",
        &text[text.len().saturating_sub(80)..]
    );
    // …and the real last paragraph is still there, so the cut was at the
    // marker and not somewhere earlier in the chapter.
    assert!(
        text.contains("Khâu Bình lại lần nữa tại trong lòng an ủi"),
        "the cut took more than the marker"
    );
}

/// `discover` is mandatory on webnovel (no url_template can produce a slug URL)
/// and optional on truyencom. Both paths, on the real captured listings.
#[test]
fn the_templates_discover_a_whole_index_from_one_page() {
    // ── truyencom: a regular listing, numbers straight out of the href ──
    let base = fixture::start(vec![
        ("/index".into(), 200, site("truyencom-index").to_string()),
        (
            "/chuong-1.html".into(),
            200,
            site("truyencom-chapter").to_string(),
        ),
    ]);
    let mut s = spec("lua", "truyencom.lua", &template("truyencom.lua"));
    s.url_template = format!("{base}/chuong-{{n}}.html");
    s.params
        .insert("index".into(), serde_json::json!(format!("{base}/index")));
    // One page, deliberately. The captured fixture's pagination links are
    // absolute to the *real* truyencom.com, so a walk past page 1 would go out
    // to the live internet — right behaviour for a crawl, wrong for a test that
    // must be the same on an aeroplane. The walk itself is covered below, on
    // synthetic pages that stay on the fixture server.
    s.params.insert("max_pages".into(), serde_json::json!(1));
    let found = Provider::new(&s)
        .discover(1, 60)
        .expect("discover")
        .expect("a listing was found");
    assert_eq!(
        found.total,
        Some(50),
        "page 1 of the listing holds 50 chapters"
    );
    let ch1 = found.chapters.iter().find(|c| c.n == 1).expect("ch1");
    // The captured hrefs are absolute to the real site, and `abs_url` leaves an
    // absolute URL alone — which is the whole contract for the two of the three
    // kinds a listing mixes. The fixture stays verbatim rather than being
    // rewritten to point at the test server, so the golden is the real page.
    assert_eq!(
        ch1.url.as_deref(),
        Some("https://truyencom.com/nga-thi-nhan-gian-tinh-long-vuong/chuong-1.html")
    ); // The title comes from the `title` attribute with the book prefix cut off —
       // the link text is only the number.
    assert_eq!(
        ch1.title,
        // U+00A0, not a space — the separator the site actually uses, and the
        // character that has to survive from the `title` attribute all the way
        // to what the pipeline speaks.
        "Chương 1: \u{a0}Thần giếng Khâu Bình",
        "the title is the attribute, minus the book name"
    );
    // The sidebar of *other* books' chapters is not in the fixture, and this is
    // the assertion that would notice if the selector ever widened to it.
    assert_eq!(found.chapters.len(), 50);
    // The five pagination <li> in that same container are NOT chapters, and
    // their link text is a bare number exactly like a chapter's is.
    for bogus in ["Last", "2", "3", "4", "5"] {
        assert!(
            !found.chapters.iter().any(|c| c.title == bogus
                || c.n == bogus.parse().unwrap_or(0) && found.chapters.len() > 50),
            "the pagination was indexed as a chapter: {bogus:?}"
        );
    }
    assert!(
        found
            .chapters
            .iter()
            .all(|c| c.url.as_deref().unwrap_or("").contains("/chuong-")),
        "only chapter links may be indexed"
    );
    // Without params.index it declines rather than failing: a site whose URLs
    // are a function of n does not need a listing walk, and the host has to be
    // able to hear "fall back to the template".
    let mut s2 = spec("lua", "truyencom.lua", &template("truyencom.lua"));
    s2.url_template = format!("{base}/chuong-{{n}}.html");
    assert!(
        Provider::new(&s2)
            .discover(1, 60)
            .expect("discover")
            .is_none(),
        "no params.index means no discover, not an error"
    );

    // ── webnovel: fourteen <ol>s, root-relative hrefs, the number in an <i> ──
    let base = fixture::start(vec![(
        "/catalog".into(),
        200,
        site("webnovel-catalog").to_string(),
    )]);
    let mut s = spec("lua", "webnovel.lua", &template("webnovel.lua"));
    s.params.insert(
        "catalog".into(),
        serde_json::json!(format!("{base}/catalog")),
    );
    let found = Provider::new(&s)
        .discover(1, 20)
        .expect("discover")
        .expect("a catalog was found");
    assert!(
        found.chapters.len() >= 12,
        "the selector must span every <ol>, not the first: got {}",
        found.chapters.len()
    );
    let ch1 = found.chapters.iter().find(|c| c.n == 1).expect("ch1");
    assert_eq!(
        ch1.title, "Chương 1: Hoàng Sơn Chân Quân và Nhóm Cửu Châu Số 1",
        "the title is the a[title], not the link text with its timestamp"
    );
    // The href is `/vi/book/…` on the live site: root-relative, and getting this
    // wrong is the classic listing-walk bug.
    let url = ch1.url.as_deref().expect("a URL");
    assert!(
        url.starts_with(&format!("{base}/vi/book/")),
        "the root-relative href was not resolved: {url}"
    );
    // No `url_template` on this one, by design — and the error says so.
    let bare = spec("lua", "webnovel.lua", &template("webnovel.lua"));
    let err = Provider::new(&bare)
        .crawl(1, None, 1)
        .unwrap_err()
        .to_string();
    assert!(err.contains("url_template"), "{err}");
}

/// An absent `input.url` must reach a script as a real `nil`.
///
/// **This is a contract test for a trap, not for a feature.** mlua serialises a
/// JSON `null` to a `NULL` *userdata* so that nulls round-trip, and in Lua that
/// sentinel is **truthy**. So `if not input.url then error("no URL for ch" …)`
/// — the guard every template writes, and the one that keeps a chapter with no
/// URL from being fetched as garbage — silently did nothing, and the userdata
/// went into `fetch` instead. Caught by the webnovel template refusing to fail
/// on a chapter it should have refused.
#[test]
fn an_absent_url_reaches_a_lua_script_as_nil() {
    let base = fixture::start(vec![("/x".into(), 200, "z".repeat(400))]);
    // The guard, and the observable consequence of it not firing.
    let source = format!(
        r#"
        function crawl(input)
          if input.url == nil then
            return {{ none = true, reason = "no url, as documented" }}
          end
          if input.url == "" then
            return {{ none = true, reason = "empty url" }}
          end
          return {{ text = "{}" }}
        end
    "#,
        quoted_long()
    );
    let source = source.as_str();
    let mut s = spec("lua", "nil-url.lua", source);
    s.url_template = String::new();
    let outcome = Provider::new(&s).crawl(1, None, 1).unwrap().outcome;
    match outcome {
        CrawlOutcome::Absent { reason } => assert_eq!(reason, "no url, as documented"),
        other => panic!("a null url must arrive as nil, not as a truthy userdata: {other:?}"),
    }

    // …and a URL that *is* there is still a plain string, so the guard did not
    // break the happy path.
    let mut s = spec("lua", "nil-url.lua", source);
    s.url_template = format!("{base}/x");
    let got = Provider::new(&s).crawl(1, None, 1).unwrap().outcome;
    assert!(
        matches!(got, CrawlOutcome::Text { .. }),
        "a real url must still arrive: {got:?}"
    );

    // JavaScript gets `null`, which is falsy, so the same guard works there with
    // no special case — the asymmetry is Lua's alone.
    let mut s = spec(
        "js",
        "nil-url.js",
        "function crawl(input) { if (input.url === null || input.url === undefined) { return { none: true, reason: 'no url' }; } return { text: 'reached, and long enough to clear the two-hundred-byte chapter floor here too' }; }",
    );
    s.url_template = String::new();
    match Provider::new(&s).crawl(1, None, 1).unwrap().outcome {
        CrawlOutcome::Absent { .. } => {}
        other => panic!("js: {other:?}"),
    }
}

/// Long enough to clear the 200-byte chapter floor, so these tests are about the
/// verdict and not about the length guard.
const LONG: &str = "no challenge here, and comfortably long enough to clear the \
                    two-hundred-byte chapter floor, so that what is under test \
                    is the verdict rather than the length guard that every crawl \
                    crosses on its way out of the script and into the boundary \
                    where a short answer becomes an empty block and an empty \
                    block becomes three strikes and a shelved chapter nobody \
                    asked for";

/// The same sentence, quoted into a script, which has to escape it itself.
fn quoted_long() -> String {
    LONG.replace('"', "'")
}

/// `challenge(page)` must answer with a falsy value when there is no challenge.
///
/// The same `NULL`-is-truthy trap as above, in the other direction: a host
/// function that returned mlua's null sentinel would make
/// `if challenge(r) then return blocked end` refuse **every** page. Found by the
/// truyencom template refusing the real chapter it was written for.
#[test]
fn challenge_answers_falsy_on_a_real_page_in_both_engines() {
    let base = fixture::start(vec![(
        "/x".into(),
        200,
        "<html><body><article><p>Hắn bước vào phòng và nhìn quanh một lượt, \
         không thấy một ai cả, và cửa vẫn khóa.</p></article></body></html>"
            .into(),
    )]);
    let body = quoted_long();
    let lua = format!(
        r#"
        function crawl(input)
          local r = fetch(input.url)
          local why = challenge(r)
          if why then
            return {{ blocked = {{ class = "challenge", detail = why }} }}
          end
          return {{ text = "{body}" }}
        end
    "#
    );
    let js = format!(
        r#"
        function crawl(input) {{
          const r = fetch(input.url);
          const why = challenge(r);
          if (why) {{ return {{ blocked: {{ class: "challenge", detail: why }} }}; }}
          return {{ text: "{body}" }};
        }}
    "#
    );
    let lua = lua.as_str();
    let js = js.as_str();
    for (engine, file, source) in [("lua", "ch.lua", lua), ("js", "ch.js", js)] {
        let mut s = spec(engine, file, source);
        s.url_template = format!("{base}/x");
        let text = text_of(Provider::new(&s).crawl(1, None, 1).unwrap().outcome);
        assert_eq!(
            text.trim_end(),
            LONG,
            "{engine}: a real page must not read as a challenge"
        );
    }

    // …and truthy on one that is a challenge, so the function is not simply
    // always returning nothing.
    let base = fixture::start(vec![(
        "/x".into(),
        200,
        "<html><head><title>Just a moment...</title></head><body>x</body></html>".into(),
    )]);
    let mut s = spec("lua", "ch.lua", lua);
    s.url_template = format!("{base}/x");
    match Provider::new(&s).crawl(1, None, 1).unwrap().outcome {
        CrawlOutcome::Blocked(b) => assert_eq!(b.class, BlockedClass::Challenge),
        other => panic!("expected a challenge, got {other:?}"),
    }
}

/// ReadNovelFull writes the chapter title **twice** into the body, glued to the
/// first sentence, while the headline carries the same text without the colon.
///
/// Without cutting it every chapter opens with its own title three times over
/// (twice from the body, once prepended) and the first real sentence is
/// unreachable. The site's own capture shows the shape, and the golden proves
/// the cut — but the assertion here is on the *rule*: the prose must start at
/// the first sentence, and the title must appear exactly once.
#[test]
fn the_readnovelfull_template_cuts_the_title_the_site_writes_into_the_body() {
    let base = fixture::start(vec![(
        "/ch1".into(),
        200,
        site("readnovelfull-chapter").to_string(),
    )]);
    let mut s = spec("lua", "readnovelfull.lua", &template("readnovelfull.lua"));
    s.url_template = format!("{base}/ch1");
    let text = text_of(Provider::new(&s).crawl(1, None, 1).unwrap().outcome);

    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines[0], "Chapter 1 Genius of the Jing Clan",
        "the headline leads"
    );
    assert_eq!(
        lines[2], "Dong Lin City was a city located in the far west corner of the Lan Qu Province.",
        "the prose starts at the first real sentence, not at the site's doubled title"
    );
    assert_eq!(
        text.matches("Chapter 1: Genius of the Jing Clan").count(),
        0,
        "the body's copy of the title was not cut"
    );
    assert_eq!(
        text.matches("Chapter 1 Genius of the Jing Clan").count(),
        1,
        "the title is spoken once, as the headline"
    );
    // The site's other shape: a title that appears *inside* the prose is not an
    // artifact and must survive. The cut is anchored at the start, not anywhere.
    let quoted = r#"<div id="chr-content" class="chr-c"><p>Chapter 9: Echo Echo
           The sign read Chapter 9: Echo and then nothing happened for a while, which
           is the sort of thing that happens in this book often enough to be boring
           to anyone reading it and worth a paragraph of explanation regardless.</p>
           <p>A second paragraph, long enough that the chapter clears the floor and the
           assertions below are about the title rather than the length guard, which is
           a two-hundred-byte floor that everything on the way out of a script crosses.</p></div>
           <h2><a class="chr-title">Chapter 9 Echo</a></h2>"#;
    let base = fixture::start(vec![("/q".into(), 200, quoted.to_string())]);
    let mut s = spec("lua", "readnovelfull.lua", &template("readnovelfull.lua"));
    s.url_template = format!("{base}/q");
    let text = text_of(Provider::new(&s).crawl(1, None, 1).unwrap().outcome);
    assert!(
        text.contains("The sign read Chapter 9: Echo"),
        "a title quoted inside the prose must not be cut: {text:.200}"
    );
}

/// The rule that makes this site safe: **`total` is only ever the end of the
/// book**.
///
/// ReadNovelFull's book page lists only the first ~30 chapters — a 2730-chapter
/// book lists 2..30 and stops, with no pagination and no full-list endpoint. So a
/// `discover` that trusted it would report `total = 30`, the host would mark
/// chapters 31..2730 **absent** (a terminal verdict), never crawl them, and every
/// ledger row would look healthy. A long novel becomes a short one silently.
///
/// The template walks `next_chap` instead, and this is the test for the
/// difference between "we reached the end" and "we stopped for another reason".
#[test]
fn the_readnovelfull_index_withholds_total_until_the_book_really_ends() {
    // A book page that lists 3 chapters, and a chain that keeps going.
    let book = r#"<div class="col-xs-12" id="list-chapter"><ul class="list-chapter">
        <li><a href="/b/chapter-1-one.html" title="Chapter 1 One">Chapter 1 One</a></li>
        <li><a href="/b/chapter-2-two.html" title="Chapter 2 Two">Chapter 2 Two</a></li>
        <li><a href="/b/chapter-3-three.html" title="Chapter 3 Three">Chapter 3 Three</a></li>
        </ul></div>"#;
    let page = |n: u32, next: Option<u32>| {
        let nav = match next {
            Some(k) => format!(r#"<a id="next_chap" href="/b/chapter-{k}-t{k}.html">Next</a>"#),
            None => String::new(),
        };
        format!(
            r#"<div id="chr-content" class="chr-c"><p>Body {n}.</p></div>
               <h2><a class="chr-title">Chapter {n} T{n}</a></h2>{nav}"#
        )
    };

    // Case 1: the chain is not routed past chapter 3, so the walk stops on an
    // HTTP error with the book possibly unfinished. `total` must be withheld.
    //
    // Chapter 4 *is* still listed: the site published the `next_chap` link that
    // names it, and hiding it would be us second-guessing the site. It comes with
    // no title, because nothing ever fetched its page — and crawl() will go and
    // get the 404 and mark it absent, which is the truthful outcome.
    let base = fixture::start(vec![
        ("/book".into(), 200, book.into()),
        ("/b/chapter-3-three.html".into(), 200, page(3, Some(4))),
    ]);
    let mut s = spec("lua", "readnovelfull.lua", &template("readnovelfull.lua"));
    s.params
        .insert("book".into(), serde_json::json!(format!("{base}/book")));
    s.max_fetches = 100;
    s.max_seconds = 30;
    let found = Provider::new(&s).discover(1, 20).unwrap().unwrap();
    assert_eq!(
        found.total, None,
        "the chain stopped early, so the end of the book is unknown — reporting a \
         total here is what marks every later chapter absent"
    );
    assert_eq!(
        found.chapters.iter().map(|c| c.n).collect::<Vec<_>>(),
        vec![1, 2, 3, 4],
        "the walk stops where the site stopped speaking, and invents nothing past it"
    );
    let c4 = found.chapters.iter().find(|c| c.n == 4).expect("ch4");
    assert_eq!(
        c4.title, "",
        "a chapter the walk announced but never fetched has no title to claim"
    );
    // And the listed ones kept the titles the book page gave them — the walk must
    // not overwrite a real title with a headline read off a neighbouring page.
    let c2 = found.chapters.iter().find(|c| c.n == 2).expect("ch2");
    assert_eq!(c2.title, "Chapter 2 Two");

    // Case 2: the chain runs to a chapter with no `next_chap` at all, which IS
    // the site saying the book is finished. Now, and only now, `total`.
    let base = fixture::start(vec![
        ("/book".into(), 200, book.into()),
        ("/b/chapter-3-three.html".into(), 200, page(3, Some(4))),
        ("/b/chapter-4-t4.html".into(), 200, page(4, Some(5))),
        ("/b/chapter-5-t5.html".into(), 200, page(5, None)),
    ]);
    let mut s = spec("lua", "readnovelfull.lua", &template("readnovelfull.lua"));
    s.params
        .insert("book".into(), serde_json::json!(format!("{base}/book")));
    s.max_fetches = 100;
    s.max_seconds = 30;
    let found = Provider::new(&s).discover(1, 20).unwrap().unwrap();
    assert_eq!(
        found.total,
        Some(5),
        "a chain that reached a chapter with no next_chap has found the end"
    );
    assert_eq!(
        found.chapters.iter().map(|c| c.n).collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5],
        "the walk extends past what the book page listed, in order"
    );
    // The chapters the walk found took their titles from their *own* pages, not
    // from the list — which never mentioned them — and not from the neighbour
    // the link was read off, which is the off-by-one this rule exists to catch.
    for (n, title) in [(4, "Chapter 4 T4"), (5, "Chapter 5 T5")] {
        let c = found.chapters.iter().find(|c| c.n == n).expect("ch");
        assert_eq!(c.title, title, "chapter {n} carries the wrong headline");
    }
    let c5 = found.chapters.iter().find(|c| c.n == 5).expect("ch5");
    assert!(c5.url.as_deref().unwrap().ends_with("/b/chapter-5-t5.html"));

    // Case 3: the range is satisfied before the end of the book, so the walk
    // stops on purpose and `total` is still withheld. A later range walks on.
    let found = Provider::new(&s).discover(1, 4).unwrap().unwrap();
    assert_eq!(
        found.total, None,
        "a range that stopped short of the end must not claim to have found it"
    );
    assert!(found.chapters.iter().any(|c| c.n == 4));
    assert!(!found.chapters.iter().any(|c| c.n == 5));
}

/// The chain walk spends a fetch per chapter, so the defaults sized for one
/// chapter are not enough — and the template has to be able to say so rather than
/// dying mid-walk with a budget error.
#[test]
fn the_readnovelfull_walk_is_bounded_by_a_hop_ceiling() {
    let book = r#"<div id="list-chapter"><ul class="list-chapter">
        <li><a href="/b/chapter-1-one.html" title="One">One</a></li>
        </ul></div>"#;
    // A `next_chap` that points at itself: the loop a real site has and a real
    // walk must not follow for ever.
    let looping = r#"<div id="chr-content" class="chr-c"><p>Body.</p></div>
        <h2><a class="chr-title">Chapter 1 One</a></h2>
        <a id="next_chap" href="/b/chapter-1-one.html">Next</a>"#;
    let base = fixture::start(vec![
        ("/book".into(), 200, book.into()),
        ("/b/chapter-1-one.html".into(), 200, looping.into()),
    ]);
    let mut s = spec("lua", "readnovelfull.lua", &template("readnovelfull.lua"));
    s.params
        .insert("book".into(), serde_json::json!(format!("{base}/book")));
    s.params.insert("max_hops".into(), serde_json::json!(5));
    s.max_fetches = 100;
    s.max_seconds = 30;
    // The loop makes the number go nowhere, so the walk stops on the ceiling
    // rather than spinning, and `total` stays withheld.
    let found = Provider::new(&s).discover(1, 500).unwrap().unwrap();
    assert_eq!(found.total, None, "a looping chain has not found the end");
    assert_eq!(found.chapters.len(), 1, "and it did not invent chapters");
}

/// A paginated listing is the quietest data-loss bug in the whole stage.
///
/// The truyencom book page really is five pages of 50, and a `discover` that
/// reads page 1 and stops reports `total = 50`. The host believes that: it marks
/// everything past ch50 **absent**, which is a terminal verdict, so a 250-chapter
/// book silently becomes a 50-chapter one and every ledger row looks healthy.
/// This is the test that says the walk follows the page links.
#[test]
fn a_paginated_listing_is_walked_to_the_end_not_truncated_at_one_page() {
    // Two pages, 2 chapters each, with the page links shaped like the real ones.
    let page = |first: u32, last: u32, next: Option<u32>| {
        let mut lis = String::new();
        for c in first..=last {
            lis.push_str(&format!(
                r#"<li><a href="/book/chuong-{c}.html" title="Sách - Chương {c}: Tên {c}">{c}</a></li>"#
            ));
        }
        let mut pager = String::new();
        if let Some(p) = next {
            pager.push_str(&format!(
                r#"<li><a href="/book/trang-{p}/#chapter-list">{p}</a></li>"#
            ));
        }
        format!(
            r#"<div class="col-xs-12" id="list-chapter">
                 <ul class="list-chapter">{lis}</ul>
                 <ul class="pagination">{pager}</ul>
               </div>"#
        )
    };
    let base = fixture::start(vec![
        ("/book".into(), 200, page(1, 2, Some(2))),
        ("/book/trang-2/".into(), 200, page(3, 4, None)),
    ]);
    let mut s = spec("lua", "truyencom.lua", &template("truyencom.lua"));
    s.params
        .insert("index".into(), serde_json::json!(format!("{base}/book")));
    let found = Provider::new(&s)
        .discover(1, 10)
        .expect("discover")
        .expect("a listing was found");

    assert_eq!(
        found.chapters.len(),
        4,
        "both pages were read: {:?}",
        found.chapters.iter().map(|c| c.n).collect::<Vec<_>>()
    );
    assert_eq!(
        found.total,
        Some(4),
        "`total` must be the last chapter of the BOOK, not of the last page read — \
         it is what marks a range past the end as absent"
    );
    let ch4 = found.chapters.iter().find(|c| c.n == 4).expect("ch4");
    assert!(
        ch4.url.as_deref().unwrap_or("").ends_with("/chuong-4.html"),
        "a page-2 chapter is present: {:?}",
        ch4.url
    );
    // The page-2 link is root-relative and was resolved against the page it was
    // found on — the classic listing-walk bug, caught on a URL rather than in a
    // comment.
    assert!(
        found.chapters.iter().filter(|c| c.n >= 3).all(|c| c
            .url
            .as_deref()
            .unwrap_or("")
            .starts_with(&format!("{base}/book/"))),
        "page-2 links resolved against the page they were found on: {:?}",
        found.chapters.iter().map(|c| &c.url).collect::<Vec<_>>()
    );
}

/// The real captured Cloudflare challenge, served both ways it is really served.
///
///
/// This is the whole argument for the link check, in test form: the same body
/// is a `403` with `cf-mitigated: challenge` over one protocol and a plain `200`
/// over the other, and a crawler has to name both rather than writing an
/// interstitial into `chNN.txt`.
#[test]
fn a_real_cloudflare_challenge_is_a_challenge_either_way_it_arrives() {
    let file = "webnovel.lua";
    // As served over HTTP/2: 403, header present.
    let base = fixture::start_with(vec![(
        "/wn-ch1".into(),
        403,
        CLOUDFLARE_403.to_string(),
        vec![("cf-mitigated".into(), "challenge".into())],
    )]);
    let mut s = spec("lua", file, &template(file));
    s.url_template = format!("{base}/wn-ch1");
    match Provider::new(&s).crawl(1, None, 1).unwrap().outcome {
        CrawlOutcome::Blocked(b) => {
            assert_eq!(b.class, BlockedClass::Challenge, "{}", b.detail);
            assert!(
                b.detail.contains("cookie"),
                "the detail names the only route there is: {}",
                b.detail
            );
        }
        other => panic!("expected a challenge, got {other:?}"),
    }

    // As served over HTTP/1.1 by the same site: 200, no header, challenge body.
    // Nothing in the status says no — only the body does.
    let base = fixture::start(vec![("/wn-ch1".into(), 200, CLOUDFLARE_403.to_string())]);
    let mut s = spec("lua", file, &template(file));
    s.url_template = format!("{base}/wn-ch1");
    match Provider::new(&s).crawl(1, None, 1).unwrap().outcome {
        CrawlOutcome::Blocked(b) => {
            assert_eq!(
                b.class,
                BlockedClass::Challenge,
                "a 200 interstitial is still a challenge: {}",
                b.detail
            );
        }
        other => panic!(
            "expected a challenge, got {other:?} — a 200 interstitial was crawled as a chapter"
        ),
    }

    // And the host classifies it too, so a workspace with no script at all
    // still refuses it rather than storing it.
    assert_eq!(
        crate::crawl::provider::block_for_status(403).unwrap().class,
        BlockedClass::Challenge
    );
}
