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
use super::*;
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

/// The known-site templates, read from the repo rather than inlined: these tests
/// are the gate for those *files*, so reading them is the point.
///
/// The directory is tracked (`crawlers/known/` ships in the repo), so these run
/// on a fresh clone. While the crawlers rode an ignored tree they read
/// machine-local state, and passed or failed depending on whether somebody had
/// fetched something.
///
/// They are the **global** tree now — one set every workspace selects from —
/// rather than a language's or the pack's.
fn template(file: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/../../../crawlers/known/{file}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap_or_else(|e| panic!("reading the template {file}: {e}"))
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
        "{}/../../../crawlers/known/{file}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap_or_else(|e| panic!("reading the bundled {file}: {e}"))
}

/// An **example** crawler, read from the repo rather than inlined.
///
/// Not the known sites: `crawlers/known/` is knowledge of particular *sites*,
/// and an EPUB is a format that no site owns. The examples live in
/// `crawlers/examples/` — the unknown-structure shapes — and this is the gate
/// for the files there.
fn sample(file: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/../../../crawlers/examples/{file}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap_or_else(|e| panic!("reading the sample {file}: {e}"))
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
mod epub;
mod parity;
mod sites;
