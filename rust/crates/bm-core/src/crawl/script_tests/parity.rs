use super::*;

#[test]
fn the_bundled_crawlers_reproduce_the_rust_extractors_goldens() {
    let routes: Vec<(String, u16, String)> = FIXTURES
        .iter()
        .map(|(name, html, _)| (format!("/{name}"), 200, html.to_string()))
        .collect();
    let base = fixture::start(routes);

    for (engine, file) in [("lua", "storya.lua"), ("js", "storya.js")] {
        let source = bundled(file);
        for (name, _, golden) in FIXTURES {
            let s = spec(engine, file, &source);
            let got = Provider::new(&s)
                .crawl(1, Some(&format!("{base}/{name}")), 1)
                .expect("crawl");
            assert_eq!(
                text_of(got.outcome),
                *golden,
                "{file} through the {engine} engine diverged on {name}"
            );
        }
    }
}

/// Storya leaves a one-letter stub at the end of some paragraphs, and it must
/// never reach the digest.
///
/// The stub is in the page, not in our extraction: `…cảnh tượng đó. m.` is what
/// storya.click serves for ch386. The digest's source gate demands every word be
/// spoken exactly once, a model drops a meaningless fragment on sight, and the
/// chapter is then refused on every racer for ever — ch386 burned 15 attempts
/// and was shelved over this one fragment. So the templates strip it, both of
/// them: the two engines are one behaviour, and a rule in only one of them is a
/// chapter that crawls clean or not depending on `crawl.engine`.
///
/// The page below is the shape the site serves, cut to the parts the extractor
/// reads. The prose is the real ch386 paragraph, verbatim, so the assertion
/// about what survives is about the corpus and not about this fixture.
#[test]
fn the_storya_engines_drop_the_translators_letter_stub() {
    const PAGE: &str = r#"<html><body><article>
<h1>Chương 386: Liền như vậy một đống lạt kê?</h1>
<p>Gần như chỉ trong chớp mắt, phi thuyền đã bị đánh nát thành từng mảnh.</p>
<p>Những vị Võ Đế theo sau này, đa số là công nhân mà Hám Thiên Khuyết và những người khác đã tuyển dụng sau này, cũng đều kinh hãi khi chứng kiến cảnh tượng đó. m.</p>
<p>Trong lòng hắn lại thở dài một tiếng.</p>
<p>Chương sau</p>
</article></body></html>"#;
    let base = fixture::start(vec![("/stub".into(), 200, PAGE.to_string())]);

    for (engine, file) in [("lua", "storya.lua"), ("js", "storya.js")] {
        let s = spec(engine, file, &template(file));
        let got = Provider::new(&s)
            .crawl(1, Some(&format!("{base}/stub")), 1)
            .expect("crawl");
        let text = text_of(got.outcome);
        assert!(
            !text.contains("m."),
            "{file} handed the stub to the digest: {text:?}"
        );
        assert!(
            text.contains("kinh hãi khi chứng kiến cảnh tượng đó."),
            "{file} lost the sentence with the stub: {text:?}"
        );
        // A paragraph that ends in a real one-letter Vietnamese word keeps it.
        assert!(
            text.contains("thở dài một tiếng."),
            "{file} is not the file being changed here: {text:?}"
        );
    }
}

/// A workspace's own crawler shadows the profile's: `crawl/` in the active
/// workspace is searched before the root and before `assets/`, so a book whose
/// site needs a different script wins without touching anything shared — and
/// without the name in `crawl.script` having to change.
#[test]
fn the_workspaces_crawler_shadows_the_profiles() {
    let dir = std::env::temp_dir().join(format!("bm-ws-crawl-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("assets/crawl")).unwrap();
    std::fs::create_dir_all(dir.join("workspaces/book/crawl")).unwrap();
    std::fs::create_dir_all(dir.join("crawl")).unwrap();
    std::fs::create_dir_all(dir.join(".bm")).unwrap();
    std::fs::write(dir.join("assets/crawl/site.lua"), "profile copy").unwrap();
    std::fs::write(dir.join("crawl/site.lua"), "root copy").unwrap();
    std::fs::write(dir.join("workspaces/book/crawl/site.lua"), "workspace copy").unwrap();
    std::fs::write(dir.join(".bm/active-workspace"), "book\n").unwrap();
    let layout = Layout::resolve(&dir).unwrap();
    let picked =
        resolve_script(&layout, "crawl/site.lua").expect("the workspace copy must resolve");
    assert!(
        picked.starts_with(dir.join("workspaces/book")),
        "the workspace copy wins: {}",
        picked.display()
    );
    // Without the workspace copy the same name falls through to the root.
    std::fs::remove_file(dir.join("workspaces/book/crawl/site.lua")).unwrap();
    let picked = resolve_script(&layout, "crawl/site.lua").unwrap();
    assert_eq!(picked, dir.join("crawl/site.lua"), "then the root");
    // …and with no pointer at all, `crawl/` at the root IS the legacy workspace.
    std::fs::remove_file(dir.join(".bm/active-workspace")).unwrap();
    let layout = Layout::resolve(&dir).unwrap();
    let picked = resolve_script(&layout, "crawl/site.lua").unwrap();
    assert_eq!(picked, dir.join("crawl/site.lua"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Every crawler now lives in the global `crawlers/` tree, so a `settings.json`
/// written before the move spells an old path (`assets/crawl/templates/…`,
/// `crawl/templates/…`) that no longer exists. It has to keep working: the
/// failure mode otherwise is "no such file" on the first crawl of a book that
/// was fine yesterday.
///
/// The fallback is by **basename only**, into `crawlers/known/` and
/// `crawlers/examples/`, and only for a path that missed — so it cannot shadow a
/// real file and cannot turn a bare name into a bundled one.
#[test]
fn a_settings_file_naming_the_pre_move_path_still_finds_its_crawler() {
    let dir = std::env::temp_dir().join(format!("bm-move-{}-{}", std::process::id(), line!()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("crawlers/known")).unwrap();
    std::fs::create_dir_all(dir.join("workspaces/book")).unwrap();
    std::fs::create_dir_all(dir.join(".bm")).unwrap();
    std::fs::write(dir.join(".bm/active-workspace"), "book\n").unwrap();
    std::fs::write(dir.join("crawlers/known/storya.lua"), "moved").unwrap();
    let layout = Layout::resolve(&dir).unwrap();

    // The old spellings resolve to where the file went…
    for old in [
        "assets/crawl/templates/storya.lua",
        "crawl/templates/storya.lua",
    ] {
        let picked = resolve_script(&layout, old).unwrap_or_else(|| {
            panic!("the pre-move path {old} must still find the bundled crawler")
        });
        assert_eq!(picked, dir.join("crawlers/known/storya.lua"), "{old}");
    }
    // A basename alone is still not a lookup: it must not silently become a
    // bundled crawler, because a typo would then run someone else's script.
    assert_eq!(resolve_script(&layout, "storya.lua"), None);
    // And a path whose basename is nowhere stays missing rather than guessing.
    assert_eq!(resolve_script(&layout, "assets/crawl/nosuchsite.lua"), None);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The crawlers are **global** now: `crawlers/` at the checkout root, the same
/// for every adapter and every workspace. A book's own `crawl/` still shadows
/// them — that is where a site nobody has written down yet lives.
#[test]
fn the_global_crawlers_resolve_from_the_root_and_a_book_can_shadow_them() {
    let dir = std::env::temp_dir().join(format!("bm-global-crawl-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("crawlers/known")).unwrap();
    std::fs::write(dir.join("crawlers/known/storya.lua"), "global copy").unwrap();

    let layout = Layout::new(&dir);
    assert_eq!(layout.crawl_scripts(), dir.join("crawlers"));
    // The registry spelling resolves straight out of the root…
    assert_eq!(
        resolve_script(&layout, "crawlers/known/storya.lua").unwrap(),
        dir.join("crawlers/known/storya.lua")
    );

    // …and a book's own crawler shadows a global one of the same name, because
    // `work` is searched before the root.
    std::fs::create_dir_all(dir.join("crawl")).unwrap();
    std::fs::write(dir.join("crawl/site.lua"), "book copy").unwrap();
    assert_eq!(
        resolve_script(&layout, "crawl/site.lua").unwrap(),
        dir.join("crawl/site.lua")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A page that is served happily and holds no chapter: the bundled crawler
/// hands back nothing, and the length guard is what turns that into an `empty`
/// block rather than a stub three stages downstream.
#[test]
fn the_default_crawler_refuses_a_page_with_no_chapter_on_it() {
    let base = fixture::start(vec![("/x".into(), 200, EMPTY_PAGE.to_string())]);
    let mut s = spec("lua", "storya.lua", &bundled("storya.lua"));
    s.url_template = format!("{base}/x");
    match Provider::new(&s).crawl(1, None, 1).unwrap().outcome {
        CrawlOutcome::Blocked(b) => {
            assert_eq!(b.class, BlockedClass::Empty);
            assert!(b.detail.contains("short"), "{}", b.detail);
        }
        other => panic!("expected an empty block, got {other:?}"),
    }
}

/// A script sees the manifest's URL, its own opaque params, and the attempt
/// count — the three things that are supposed to reach it and nothing else.
#[test]
fn a_script_receives_the_url_params_and_attempt() {
    let html = "x".repeat(400);
    let base = fixture::start(vec![("/chuong-7".into(), 200, html)]);
    let mut s = spec(
        "lua",
        "echo.lua",
        r#"
        function crawl(input)
          local seen = table.concat({
            tostring(input.url),
            tostring(input.params.site_tag),
            tostring(input.attempt),
          }, "|")
          return { text = string.rep(seen .. "\n", 40) }
        end
        "#,
    );
    s.url_template = format!("{base}/chuong-{{n}}");
    s.params
        .insert("site_tag".into(), serde_json::json!("opaque-value"));
    let got = Provider::new(&s).crawl(7, None, 2).expect("crawl");
    let text = text_of(got.outcome);
    assert!(
        text.starts_with(&format!("{base}/chuong-7|opaque-value|2\n")),
        "{text}"
    );
}

/// `none` is a terminal non-failure: a book that ends at 380 must not shelve
/// twenty rows for the range a careless operator typed.
#[test]
fn a_missing_chapter_is_absent_and_a_bot_check_is_a_retryable_block() {
    let base = fixture::start(vec![]);
    let mut s = spec(
        "lua",
        "verdict.lua",
        r#"
        function crawl(input)
          local r = fetch(input.url)
          if r.status == 404 then return { none = true, reason = "HTTP 404" } end
          if r.status == 403 then return { blocked = { class = "challenge", detail = "bot check" } } end
          if r.status == 429 then return { blocked = { class = "rate_limit", retry_after = 30 } } end
          return { text = r.body }
        end
        "#,
    );
    s.url_template = format!("{base}/chuong-{{n}}");
    // Nothing is routed, so every path is a 404.
    let absent = Provider::new(&s)
        .crawl(381, None, 1)
        .expect("crawl")
        .outcome;
    match absent {
        CrawlOutcome::Absent { reason } => assert!(reason.contains("404"), "{reason}"),
        other => panic!("expected absent, got {other:?}"),
    }
    // And the built-in path classifies HTTP the same way, with the classes that
    // decide whether another attempt is worth a worker.
    let absent_report = bm_proto::CrawlReport {
        verdict: bm_proto::CrawlVerdict::Absent,
        class: String::new(),
        detail: "site ends at ch380".into(),
        retry_after: None,
        fetches: 1,
    };
    assert!(
        !absent_report.retryable(),
        "an absent chapter is never retried"
    );
    for (status, want, retryable) in [
        (404, BlockedClass::Gone, false),
        (410, BlockedClass::Gone, false),
        (429, BlockedClass::RateLimit, true),
        (403, BlockedClass::Challenge, true),
        (401, BlockedClass::LoginRequired, false),
        (503, BlockedClass::Challenge, true),
    ] {
        let b = super::provider::block_for_status(status).expect("non-2xx is a block");
        assert_eq!(b.class, want, "HTTP {status}");
        assert_eq!(b.class.retryable(), retryable, "HTTP {status}");
    }
    assert!(
        super::provider::block_for_status(200).is_none(),
        "a 200 is not a refusal"
    );
}

/// A page that arrives but is not a chapter fails **at the crawl**, with the
/// class that says how to treat it, rather than writing a stub the digest then
/// chokes on.
#[test]
fn a_short_page_is_an_empty_block_with_the_length_in_it() {
    let base = fixture::start(vec![(
        "/x".into(),
        200,
        "<html><body>nope</body></html>".into(),
    )]);
    let s = spec("", "", "");
    let mut s = s;
    s.url_template = format!("{base}/x");
    let outcome = Provider::new(&s).crawl(1, None, 1).unwrap().outcome;
    match outcome {
        CrawlOutcome::Blocked(b) => {
            assert_eq!(b.class, BlockedClass::Empty);
            assert!(b.detail.contains("short"), "{}", b.detail);
        }
        other => panic!("expected a block, got {other:?}"),
    }
}

/// The host has no idea what a Storya chapter is. There is no `clean_storya` to
/// call, because which element holds the prose is the script's business — and
/// this is the test that keeps it that way.
#[test]
fn the_host_offers_primitives_and_no_site_knowledge() {
    for (engine, file) in [("lua", "nosites.lua"), ("js", "nosites.js")] {
        let call = "clean_storya(\"<html><body><p>hi</p></body></html>\")";
        let source = if engine == "lua" {
            format!("function crawl(input) return {{ text = {call} }} end")
        } else {
            format!("function crawl(input) {{ return {{ text: {call} }}; }}")
        };
        let mut s = spec(engine, file, &source);
        s.url_template = "http://127.0.0.1:1/{n}".into();
        let err = Provider::new(&s).crawl(1, None, 1).unwrap_err().to_string();
        assert!(err.contains("clean_storya"), "{engine}: {err}");
    }
}

/// `select_text` is the "point at the container" primitive: point a script at
/// an element and get prose back, paragraphs and all — as opposed to `select`,
/// which squeezes the same element onto one line.
#[test]
fn a_script_can_take_a_container_as_prose() {
    let long = "Hắn bước vào phòng và nhìn quanh một lượt, không thấy một ai cả. ";
    let html = format!(
        "<html><body><div class=\"junk\">menu menu menu</div>\
         <div class=\"body\"><p>{long}</p><p>{long}{long}</p></div></body></html>"
    );
    let base = fixture::start(vec![("/x".into(), 200, html)]);
    for (engine, file) in [("lua", "prose.lua"), ("js", "prose.js")] {
        let source = if engine == "lua" {
            r#"
            function crawl(input)
              local r = fetch(input.url)
              return { text = select_text(r.body, "div.body") }
            end
            "#
        } else {
            r#"
            function crawl(input) {
              const r = fetch(input.url);
              return { text: select_text(r.body, "div.body") };
            }
            "#
        };
        let mut s = spec(engine, file, source);
        s.url_template = format!("{base}/x");
        let text = text_of(Provider::new(&s).crawl(1, None, 1).unwrap().outcome);
        assert!(
            text.contains("\n\n"),
            "{engine}: paragraphs survived: {text:.80}"
        );
        assert!(
            !text.contains("menu"),
            "{engine}: the container is the body: {text:.80}"
        );
    }
}

/// A workspace with no script at all can still say which element holds the
/// chapter. `extract` is the one key the host reads out of `params`, and it is
/// what makes "the built-in fetcher" something other than a fixed guess.
#[test]
fn the_builtin_path_takes_the_container_crawl_params_names() {
    let para = "Hắn bước vào phòng và nhìn quanh một lượt, không thấy một ai cả. ".repeat(2);
    let html = format!(
        "<html><body><div class=\"chrome\">navigation and other noise</div>\
         <div class=\"body\"><p>{para}</p><p>{para}</p></div></body></html>"
    );
    let base = fixture::start(vec![("/x".into(), 200, html)]);

    let mut s = spec("", "", "");
    s.url_template = format!("{base}/x");
    s.params.insert(
        "extract".into(),
        serde_json::json!({ "selector": "div.body" }),
    );
    let text = text_of(Provider::new(&s).crawl(1, None, 1).unwrap().outcome);
    assert!(text.contains("Hắn bước vào phòng"), "{text:.80}");
    assert!(text.contains("\n\n"), "paragraphs survive: {text:.80}");
    assert!(!text.contains("navigation"), "{text:.80}");

    // A selector that matches nothing falls back to the generic heuristic
    // rather than failing, and says what it tried on the ledger row.
    let mut s = spec("", "", "");
    s.url_template = format!("{base}/x");
    s.params
        .insert("extract".into(), serde_json::json!("div.not-here"));
    let got = Provider::new(&s).crawl(1, None, 1).unwrap();
    assert!(text_of(got.outcome).contains("Hắn bước vào phòng"));
    assert!(
        got.log.iter().any(|l| l.contains("matched nothing")),
        "{:?}",
        got.log
    );

    // A selector that is not valid CSS is a configuration error and says so,
    // instead of quietly guessing.
    let mut s = spec("", "", "");
    s.url_template = format!("{base}/x");
    s.params.insert("extract".into(), serde_json::json!("div["));
    let err = Provider::new(&s).crawl(1, None, 1).unwrap_err().to_string();
    assert!(err.contains("not valid CSS"), "{err}");
}

/// The listing site: `discover` runs once for the range, the manifest comes out
/// of it, and a range that runs past the end of the book is marked absent
/// instead of enqueued as twenty doomed fetches.
#[test]
fn discover_builds_the_index_and_marks_the_tail_absent() {
    let listing = r#"<html><body><div class="content">
        <a class="chapter-link" href="/truyen/x/chuong-1">Chương 1: Mở đầu</a>
        <a class="chapter-link" href="/truyen/x/chuong-2">Chương 2: Tiếp</a>
        <a class="chapter-link" href="/truyen/x/chuong-3">Chương 3: Nữa</a>
        </div></body></html>"#
        .to_string();
    let base = fixture::start(vec![("/book".into(), 200, listing)]);

    let root = std::env::temp_dir().join("bm-crawl-discover");
    let _ = std::fs::remove_dir_all(&root);
    let layout = Layout::new(&root);
    layout.ensure().unwrap();
    std::fs::create_dir_all(layout.crawlers_dir().join("known")).unwrap();
    std::fs::write(
        layout.crawlers_dir().join("known/site.lua"),
        r#"
        function discover(input)
          local r = fetch(input.params.entry)
          local out = {}
          for _, l in ipairs(select_all(r.body, "a.chapter-link")) do
            local n = tonumber(string.match(l.attrs.href, "(%d+)$"))
            out[#out + 1] = { n = n, url = abs_url(input.params.entry, l.attrs.href), title = l.text }
          end
          return { chapters = out, total = #out }
        end
        function crawl(input)
          local r = fetch(input.url)
          return { text = r.body, url = r.url }
        end
        "#,
    )
    .unwrap();

    let settings = Settings {
        url_template: String::new(),
        crawl: CrawlSettings {
            script: "crawlers/known/site.lua".into(),
            params: [(
                "entry".to_string(),
                serde_json::json!(format!("{base}/book")),
            )]
            .into_iter()
            .collect(),
            ..CrawlSettings::default()
        },
        ..Settings::default()
    };

    let index = chapter_index(&layout, &settings, 1, 5, false).expect("index");
    assert_eq!(index.source, super::index::SOURCE_SCRIPT);
    assert_eq!(
        index.url(2).unwrap(),
        format!("{base}/truyen/x/chuong-2"),
        "relative hrefs are resolved against the page"
    );
    assert_eq!(index.chapters().get(&2).unwrap().title, "Chương 2: Tiếp");
    // 4 and 5 are past the end the listing reported: absent, not queued.
    assert!(
        index.is_absent(4) && index.is_absent(5),
        "{:?}",
        index.chapters()
    );
    assert!(!index.is_absent(3));
    // A second call reuses the frozen index rather than walking the listing
    // again — the shift-proofing this file exists for.
    let again = chapter_index(&layout, &settings, 1, 5, false).expect("index");
    assert_eq!(again.url(2), index.url(2));

    // A workspace with neither a template nor a discover has no mapping, and
    // the error names all three ways to get one.
    let bare = Settings::default();
    let mut bare = bare;
    bare.url_template = String::new();
    let root2 = root.join("bare");
    let layout2 = Layout::new(&root2);
    layout2.ensure().unwrap();
    let err = chapter_index(&layout2, &bare, 1, 2, false)
        .unwrap_err()
        .to_string();
    assert!(err.contains("discover"), "{err}");
    assert!(err.contains("by hand"), "{err}");
}

/// `async function crawl` is allowed, and a promise that needs a real event loop
/// is refused with a message saying so — rather than hanging a worker.
#[test]
fn an_async_js_crawl_resolves_and_a_never_settling_one_is_refused() {
    let html = "y".repeat(400);
    let base = fixture::start(vec![("/ok".into(), 200, html)]);
    let mut s = spec(
        "js",
        "async.js",
        r#"
        async function crawl(input) {
          const r = fetch(input.url);
          return { text: r.body, url: r.url };
        }
        "#,
    );
    s.url_template = format!("{base}/ok");
    let got = Provider::new(&s).crawl(1, None, 1).expect("async crawl");
    // The boundary appends one newline, so compare the body, not the length.
    assert_eq!(text_of(got.outcome).trim_end().len(), 400);

    let mut s = spec(
        "js",
        "hang.js",
        r#"
        async function crawl(input) {
          await new Promise(function () {});
          return { text: "never" };
        }
        "#,
    );
    s.url_template = format!("{base}/ok");
    let err = Provider::new(&s).crawl(1, None, 1).unwrap_err().to_string();
    assert!(err.contains("never settled"), "{err}");
}

/// A script that loops forever costs one chapter's budget, not a worker.
#[test]
fn a_runaway_loop_dies_at_its_budget() {
    for (engine, file) in [("lua", "spin.lua"), ("js", "spin.js")] {
        let source = if engine == "lua" {
            "function crawl(input) while true do end end"
        } else {
            "function crawl(input) { while (true) {} }"
        };
        let mut s = spec(engine, file, source);
        s.url_template = "http://127.0.0.1:1/{n}".into();
        s.max_seconds = 1;
        let err = Provider::new(&s).crawl(1, None, 1).unwrap_err().to_string();
        // Both engines must *say* it was the budget: QuickJS reports an
        // interrupt with no message at all, so the host names it.
        assert!(err.contains("budget"), "{engine}: {err}");
        assert!(err.contains(file), "{engine}: {err}");
    }
}

/// A script that returns nothing, or nonsense, is a script error naming the
/// file — not a chapter, and not a silent success.
#[test]
fn a_script_that_returns_nothing_or_nonsense_is_refused_by_name() {
    let mut s = spec("lua", "bad.lua", "function crawl(input) end");
    s.url_template = "http://127.0.0.1:1/{n}".into();
    let err = Provider::new(&s).crawl(1, None, 1).unwrap_err().to_string();
    assert!(err.contains("bad.lua"), "{err}");
    assert!(err.contains("returned nothing"), "{err}");

    let mut s = spec("js", "bad.js", "function crawl(input) { return 42; }");
    s.url_template = "http://127.0.0.1:1/{n}".into();
    let err = Provider::new(&s).crawl(1, None, 1).unwrap_err().to_string();
    assert!(err.contains("bad.js"), "{err}");
}

/// The host ABI is the whole sandbox, and this is the test that says so: a
/// script cannot see the environment, the filesystem or a shell.
#[test]
fn scripts_cannot_reach_the_environment_or_the_filesystem() {
    let mut s = spec(
        "lua",
        "nosy.lua",
        r#"
        function crawl(input)
          local out = {}
          out[#out + 1] = tostring(os)
          out[#out + 1] = tostring(io)
          out[#out + 1] = tostring(require)
          out[#out + 1] = tostring(dofile)
          out[#out + 1] = tostring(loadfile)
          out[#out + 1] = tostring(package)
          return { text = table.concat(out, " ") .. string.rep("x", 300) }
        end
        "#,
    );
    s.url_template = "http://127.0.0.1:1/{n}".into();
    let text = text_of(Provider::new(&s).crawl(1, None, 1).unwrap().outcome);
    for banned in ["table:", "function", "userdata"] {
        assert!(
            !text.starts_with(banned),
            "a script reached {banned}: {text:.60}"
        );
    }
    assert!(text.starts_with("nil nil nil nil nil nil"), "{text:.60}");
}
