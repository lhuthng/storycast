//! End-to-end tests for the scripted crawl, against a real (tiny) HTTP server.
//!
//! A fixture server rather than a mocked `fetch`: the parts worth testing are
//! exactly the ones a mock would replace — status classification, the charset
//! decode, the pacing clock, and the interpreter actually reaching a host
//! function. About forty lines of `TcpListener` buys all of that with no
//! dependency and no network.

use super::chapter_index;
use super::contract::{BlockedClass, CrawlOutcome};
use super::provider::resolve_script;
use super::provider::Provider;
use crate::config::{CrawlSettings, Settings};
use crate::Layout;
use bm_proto::CrawlSpec;
use std::io::{Read, Write};
use std::net::TcpListener;

/// A one-shot HTTP server: routes matched by path, everything else 404.
///
/// `pub(crate)` because the link check's own tests need one too, and a second
/// copy of a `TcpListener` harness is a second copy of its bugs.
pub(crate) mod fixture {
    use super::{Read, TcpListener, Write};

    /// A canned response: status, body, and the headers to send with it.
    ///
    /// Named because the header half is not optional in spirit — one of the
    /// things under test is a page that says `cf-mitigated: challenge` in a
    /// header, and a fixture server that cannot send headers cannot reproduce it.
    pub type Reply = (u16, String, Vec<(String, String)>);
    /// A route: path, status, body, headers.
    pub type Route = (String, u16, String, Vec<(String, String)>);

    pub fn start(routes: Vec<(String, u16, String)>) -> String {
        start_with(
            routes
                .into_iter()
                .map(|(p, s, b)| (p, s, b, Vec::new()))
                .collect(),
        )
    }

    /// As [`start`], but a route may carry response headers.
    ///
    /// Needed because one of the things being tested is a *header*: a Cloudflare
    /// challenge says what it is in `cf-mitigated`, and a fixture server that
    /// cannot send headers cannot reproduce the case at all.
    pub fn start_with(routes: Vec<Route>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a fixture port");
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let mut buf = [0u8; 8192];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                let (status, body, extra) = routes
                    .iter()
                    .find(|(p, _, _, _)| *p == path)
                    .map(|(_, st, b, h)| (*st, b.clone(), h.clone()))
                    .unwrap_or((
                        404,
                        "<html><body>not found</body></html>".into(),
                        Vec::new(),
                    ));
                let mut head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n",
                    body.len()
                );
                for (k, v) in &extra {
                    head.push_str(&format!("{k}: {v}\r\n"));
                }
                head.push_str("\r\n");
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(body.as_bytes());
                let _ = s.flush();
            }
        });
        format!("http://{addr}")
    }

    /// Serves a **different response per hit** on one path, then repeats the
    /// last. For "ask it twice and see which answer changes" — the shape of the
    /// HTTP/2-versus-HTTP/1.1 question, and of any site whose answer depends on
    /// what it has served you already.
    pub fn start_sequence(path: &str, responses: Vec<Reply>) -> String {
        let path = path.to_string();
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a fixture port");
        let addr = listener.local_addr().unwrap();
        // The last response is held onto and repeated, because "the site cleared
        // us" only means anything if the *second* request is the one that
        // succeeds — a sequence that 404s at the end would prove nothing.
        let last = responses
            .last()
            .cloned()
            .unwrap_or((404, String::new(), Vec::new()));
        let served = std::sync::Arc::new(std::sync::Mutex::new((responses.into_iter(), 0usize)));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let mut buf = [0u8; 8192];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let got = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                let (status, body, extra) = if got == path {
                    let mut guard = served.lock().unwrap();
                    let next = guard.0.next();
                    let _ = guard.1;
                    next.unwrap_or_else(|| last.clone())
                } else {
                    (404, "no route".into(), Vec::new())
                };
                let mut head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n",
                    body.len()
                );
                for (k, v) in &extra {
                    head.push_str(&format!("{k}: {v}\r\n"));
                }
                head.push_str("\r\n");
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(body.as_bytes());
                let _ = s.flush();
            }
        });
        format!("http://{addr}")
    }
}

/// The pages the bundled crawlers are tested against, each paired with the bytes
/// the Rust extractor they replaced produced for it — captured before that
/// extractor was deleted, so the pairing is a *record* and not a re-derivation.
///
/// The pairs are wired at compile time, so a golden that goes missing is a build
/// error rather than a test that silently checks nothing. Between them they
/// cover every branch of the old extractor: each container, the hint and the
/// trigger, the 120-character hint guard, the junk and byline skips, the
/// artifact and entity handling, and the stop markers.
macro_rules! fixtures {
    ($($name:literal),* $(,)?) => {
        &[$( (
            $name,
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/crawl/",
                $name,
                ".html"
            )),
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/crawl/",
                $name,
                ".txt"
            )),
        )),*]
    };
}

const FIXTURES: &[(&str, &str, &str)] = fixtures![
    "article",
    "main",
    "body",
    "trigger",
    "long-start",
    "artifacts",
    "entities",
    "late-paragraph",
];

/// A page that is served happily and holds no chapter at all.
const EMPTY_PAGE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/crawl/empty.html"
));

// ───────────────────────── the site templates ─────────────────────────
//
// A second class of fixture, and a different kind of claim from the ones above.
// The Storya goldens are a *parity* record: bytes a deleted Rust extractor
// produced, proving the port did not change anything. These are *correctness*
// records: pages captured from two live sites, paired with the text a template
// should get out of them — so "the template works" is checked against a real
// page rather than against the template author's own idea of one.
//
// Wired at compile time like the others, so a missing golden is a build error.

macro_rules! site_fixtures {
    ($($name:literal),* $(,)?) => {
        &[$( (
            $name,
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/crawl/",
                $name,
                ".html"
            )),
        )),*]
    };
}

const SITES: &[(&str, &str)] = site_fixtures![
    "truyencom-chapter",
    "truyencom-index",
    "webnovel-chapter",
    "webnovel-catalog",
    "readnovelfull-book",
    "readnovelfull-chapter",
];

/// A real Cloudflare challenge, kept as served.
const CLOUDFLARE_403: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/crawl/cloudflare-403.html"
));

fn site(name: &str) -> &'static str {
    SITES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, h)| *h)
        .unwrap_or_else(|| panic!("no site fixture named {name}"))
}

/// The expected text for a site fixture, from the paired golden.
///
/// Read at runtime rather than `include_str!`'d: the `.html` half is the part
/// whose absence must break the build, and a golden that cannot be found fails
/// the assertion that names it, which is a better error than a macro one.
fn site_golden(name: &str) -> String {
    let path = format!(
        "{}/../../fixtures/crawl/{name}.txt",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {path}: {e}"))
}

/// The two templates, read from the repo rather than inlined: these tests are
/// the gate for those *files*, so reading them is the point.
///
/// The directory is tracked (`.gitignore` excludes the rest of the live profile
/// tree and un-ignores `assets/crawl/templates/`), so these run on a fresh
/// clone. While the whole of `assets/` was ignored they read machine-local
/// state, and passed or failed depending on whether somebody had fetched a
/// profile.
fn template(file: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/../../../assets/crawl/templates/{file}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap_or_else(|e| panic!("reading the template {file}: {e}"))
}

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

fn spec(engine: &str, name: &str, source: &str) -> CrawlSpec {
    CrawlSpec {
        engine: engine.into(),
        script: name.into(),
        source: source.into(),
        params: serde_json::Map::new(),
        ..Default::default()
    }
}

/// The shipped crawler, read from the repo rather than inlined: these tests are
/// the parity gate for that *file*, so reading it is the point.
fn bundled(file: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/../../../assets/crawl/templates/{file}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap_or_else(|e| panic!("reading the bundled {file}: {e}"))
}

fn text_of(outcome: CrawlOutcome) -> String {
    match outcome {
        CrawlOutcome::Text { text, .. } => text,
        other => panic!("expected text, got {other:?}"),
    }
}

/// **The gate that makes the port a port.** The bundled crawler, run through
/// each engine, must produce exactly the bytes the Rust extractor it replaced
/// produced for the same page — otherwise "the default crawler is the old
/// behaviour" is a claim rather than a fact, and every existing workspace's
/// output changes on upgrade.
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

/// Every bundled crawler now lives in `assets/crawl/templates/`, which is what
/// `templates/` means. A `settings.json` written before the move still spells
/// the old path, and it has to keep working: the failure mode otherwise is "no
/// such file" on the first crawl of a book that was fine yesterday.
///
/// The fallback is by **basename only** and only for a path that missed, so it
/// cannot shadow a real file and cannot turn a bare name into a bundled one.
#[test]
fn a_settings_file_naming_the_pre_move_path_still_finds_its_crawler() {
    let dir = std::env::temp_dir().join(format!("bm-move-{}-{}", std::process::id(), line!()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("assets/crawl/templates")).unwrap();
    std::fs::create_dir_all(dir.join("workspaces/book")).unwrap();
    std::fs::create_dir_all(dir.join(".bm")).unwrap();
    std::fs::write(dir.join(".bm/active-workspace"), "book\n").unwrap();
    std::fs::write(dir.join("assets/crawl/templates/storya.lua"), "moved").unwrap();
    let layout = Layout::resolve(&dir).unwrap();

    // The old spelling resolves to where the file went…
    let picked = resolve_script(&layout, "assets/crawl/templates/storya.lua")
        .expect("the pre-move path must still find the bundled crawler");
    assert_eq!(picked, dir.join("assets/crawl/templates/storya.lua"));
    // …and so does the current one.
    assert_eq!(
        resolve_script(&layout, "assets/crawl/templates/storya.lua").unwrap(),
        picked
    );
    // A basename alone is still not a lookup: it must not silently become a
    // bundled crawler, because a typo would then run someone else's script.
    assert_eq!(resolve_script(&layout, "storya.lua"), None);
    // And a path that moved nowhere stays missing rather than guessing.
    assert_eq!(resolve_script(&layout, "assets/crawl/nosuchsite.lua"), None);
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
    std::fs::create_dir_all(layout.crawl_scripts()).unwrap();
    std::fs::write(
        layout.crawl_scripts().join("site.lua"),
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
            script: "assets/crawl/site.lua".into(),
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
