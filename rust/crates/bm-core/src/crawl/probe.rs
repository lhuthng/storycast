//! The quick check: **one request against a pasted link, before a workspace
//! commits to a site.**
//!
//! The problem it exists for is arithmetic. A cluster does not notice that a
//! site is refusing it — it notices one worker at a time, sixty seconds apart,
//! for a whole afternoon. Ten workers leasing crawl tasks against a
//! `403 cf-mitigated: challenge` is ten workers × three strikes × a 60s timeout
//! before a single word reaches the operator's screen, and the first honest
//! signal is a ledger full of identical rows.
//!
//! A check costs one request and answers the only question that matters before
//! any of that runs: *would a crawl of this page produce a chapter?* It is
//! deliberately not a crawler. It fetches, classifies, and reports, and the
//! answer it gives is the answer a crawl would give for the same URL — because
//! it goes through the same host, the same status classification and the same
//! length guard a real crawl does.
//!
//! **What it cannot tell you.** A check reads one page. A site can serve
//! chapter 1 and challenge chapter 200, or pass the first hour and rate-limit
//! the second, and a check will happily say yes to the first of those. It is a
//! floor, not a proof: it removes the failure modes that are decidable from one
//! request, and says so rather than implying more.
//!
//! It is also the cheapest place to find the two settings that need a human: a
//! `crawl.user_agent` that reads as a browser rather than as `Mozilla/5.0`, and
//! a `crawl.headers` cookie when the site is behind a bot check. Both are
//! per-site facts a person has to supply, and both are things a check can
//! confirm or refute in one request.
//!
//! **What a check cannot fix, stated plainly.** There is no TLS-fingerprint
//! spoofing, no browser engine and no challenge solver here, and this crate's
//! `reqwest` is built without the `http2` feature — so a crawl speaks HTTP/1.1
//! with rustls and a header-shaped request, full stop. Some Cloudflare-fronted
//! sites refuse that on the TLS fingerprint alone, and for those the only route
//! is a session cookie a human obtained in a real browser. Finding that out from
//! a check takes a second; finding it out from a cluster takes an afternoon.
//!
//! Nothing here writes anything. A check is a read of one URL.

use anyhow::Result;
use serde::Serialize;

use super::contract::BlockedClass;
use super::host::{Host, Limits};
use super::provider::{block_for_status, MAX_CHAPTER_BYTES, MIN_CHAPTER_BYTES};

/// Why a page is not a chapter, in the order the questions are worth asking.
///
/// The order matters and is the diagnosis: `Cloudflare` explains a `TooShort`,
/// and `TooShort` on its own explains nothing you can act on.
///
///
/// A single enum rather than a struct of flags, because the operator needs one
/// answer and a fix, not a scoreboard — and because the ordering is a
/// diagnosis: `Cloudflare` explains a `TooShort`, and `Empty` explains nothing
/// on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// A chapter came back and cleared the length guard. Crawlable.
    Ok,
    /// The request never completed. Not a site problem, and worth saying so
    /// rather than reporting it as one.
    Unreachable,
    /// A non-2xx status, classified the way a crawl classifies it.
    Refused,
    /// A Cloudflare interstitial — either a 403/503 with the marker, or a
    /// **200 that is not the page you asked for**.
    ///
    /// The second case is the one that costs an afternoon, and the reason this
    /// type exists rather than a status check. Cloudflare's managed challenge
    /// is frequently served as `200 OK` with a challenge body: no status to
    /// trip on, so a crawl that only watched the status sailed straight through
    /// it and wrote the interstitial to `chNN.txt`. Detecting it is a check's
    /// real job.
    Cloudflare,
    /// Fetched, and there is no chapter on it.
    ///
    /// Subdivided in the report because the two have nothing in common: `short`
    /// is usually a challenge page or a selector that missed, `no_container` is
    /// a selector that matched nothing at all, and neither is fixed by retrying.
    TooShort,
    NoContainer,
}

impl Verdict {
    /// Whether a crawl of this page is worth starting.
    pub fn crawlable(&self) -> bool {
        matches!(self, Verdict::Ok)
    }
}

/// What a check found. Cheap to print, cheap to serialize, and safe to log: it
/// carries no page body and no header values, only lengths and names.
#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub url: String,
    /// Where the request ended up, which is not always where it was aimed —
    /// a redirect to a login page is a *finding*, not a detail.
    pub final_url: String,
    pub status: u16,
    pub verdict: Verdict,
    /// One line an operator can act on. Deliberately not a wall of text: this
    /// prints into a terminal next to a settings file they are editing.
    pub detail: String,
    /// Bytes of body received.
    pub bytes: usize,
    /// Bytes that survived the chapter boundary, or 0 when there were none.
    pub text_bytes: usize,
    /// The container the generic heuristic would take, when one stood out. The
    /// first thing to paste into `crawl.params.extract` or a copied template.
    pub guess: String,
    /// Classified refusal, when the failure was an HTTP one — so a check and a
    /// crawl agree on what the status means.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class: Option<BlockedClass>,
}

impl Check {
    /// Whether another attempt later could plausibly work.
    ///
    /// **Delegates to [`BlockedClass::retryable`] rather than keeping its own
    /// opinion, and that is the whole point.** A check that called a Cloudflare
    /// challenge "not worth retrying" while the crawl ladder retries it three
    /// times would be sending the operator to wait on one answer and then
    /// watching another happen. The two agree by construction, so they cannot
    /// drift — which is why this reads the very class a crawl would have
    /// recorded, rather than re-deriving a verdict from the verdict.
    ///
    /// So a challenge *is* retryable, because the crawl treats it as one: bot
    /// scoring is often transient, and three attempts an hour apart is
    /// reasonable to spend on a site that is merely suspicious. What it is not
    /// is something retrying reliably fixes — where there is a fix at all it is
    /// a session cookie in `crawl.headers`, and the detail line says so.
    pub fn retryable(&self) -> bool {
        match self.verdict {
            Verdict::Ok | Verdict::NoContainer => false,
            // No class means the failure was not an HTTP one, so the honest
            // answer is a generic "maybe": a dead host and a too-short page both
            // often come back on their own.
            _ => self.class.is_none_or(|c| c.retryable()),
        }
    }
}

/// Markers that identify a Cloudflare interstitial in a body served as `200`.
///
/// Chosen from what Cloudflare actually ships, and checked against real
/// captures: `cf-mitigated` (the header, but it leaks into some challenge
/// pages' markup), the `cf_chl_opt` script hook, and the two title strings a
/// challenged document has carried for years. The title match is deliberately
/// last and deliberately narrow — `Just a moment...` in a `<title>` is not
/// something a novel page has.
const CF_BODY_MARKERS: [&str; 4] = [
    "cf_chl_opt",
    "challenges.cloudflare.com",
    "cf-mitigated",
    "Just a moment",
];

/// The `cf-mitigated` response header, whose value is the site saying so
/// outright rather than the body being guessed at.
const CF_HEADER: &str = "cf-mitigated";

/// What a check should send, and what it may spend.
#[derive(Debug, Clone)]
pub struct Options {
    pub user_agent: String,
    pub headers: std::collections::BTreeMap<String, String>,
    pub timeout_secs: u64,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            user_agent: String::new(),
            headers: Default::default(),
            timeout_secs: 30,
        }
    }
}

/// Check one URL, and say whether a crawl of it would produce a chapter.
///
/// The second half of the contract with the operator: this runs the *same*
/// [`Host`] a crawl does, applies the *same* status classification and the
/// *same* length guard, so a `Ok` here means the crawl will produce that text,
/// not that the host answered. The difference from a crawl is everything else:
/// one request, no script, no index, nothing written.
pub fn probe(url: &str, opts: &Options) -> Result<Check> {
    once(url, opts)
}

/// One request, one verdict.
fn once(url: &str, opts: &Options) -> Result<Check> {
    let limits = Limits {
        max_fetches: 1,
        max_seconds: 30,
        timeout_secs: opts.timeout_secs.max(1),
        ..Limits::default()
    };
    let mut host = Host::new(&opts.user_agent, &opts.headers, limits)?;
    let page = match host.fetch(url, None) {
        Ok(p) => p,
        Err(e) => {
            return Ok(Check {
                url: url.to_string(),
                final_url: url.to_string(),
                status: 0,
                verdict: Verdict::Unreachable,
                detail: format!("the request did not complete: {e}"),
                bytes: 0,
                text_bytes: 0,
                guess: String::new(),
                class: None,
            })
        }
    };

    let mut check = Check {
        url: url.to_string(),
        final_url: page.url.clone(),
        status: page.status,
        verdict: Verdict::Ok,
        detail: String::new(),
        bytes: page.body.len(),
        text_bytes: 0,
        guess: String::new(),
        class: None,
    };

    // Cloudflare first, and *before* the status: a challenge is a challenge
    // whether it arrived as a 403 or as a 200, and the status check below
    // would otherwise report the second one as a perfectly good page. The same
    // call a script makes through the `challenge()` ABI function, so a check and
    // a crawl can never disagree about what a challenge is.
    if let Some(why) = interstitial(&page) {
        check.verdict = Verdict::Cloudflare;
        check.class = Some(BlockedClass::Challenge);
        check.detail = why;
        return Ok(check);
    }

    if let Some(blocked) = block_for_status(page.status) {
        check.verdict = Verdict::Refused;
        check.class = Some(blocked.class);
        check.detail = format!("HTTP {} — {}", page.status, blocked.detail);
        return Ok(check);
    }

    // The body, through the same boundary a crawl's text crosses. Note this is
    // the *generic* path: a check has no script, so it cannot know the site's
    // container, and what it measures is what the built-in fetcher would get.
    // A `TooShort` here is therefore a statement about `readable()`'s guess —
    // which is exactly the "the site changed under us" signal worth having
    // before a cluster finds it the expensive way.
    let read = super::html::readable(&page.body);
    let text = super::sanitize_chapter_text(&read.text);
    check.text_bytes = text.len();
    let title = read.title;
    check.guess = title.clone();

    if text.is_empty() {
        check.verdict = Verdict::NoContainer;
        check.detail = "the page has no container with any text in it".into();
    } else if text.len() < MIN_CHAPTER_BYTES {
        check.verdict = Verdict::TooShort;
        check.detail = format!(
            "the best container holds {} bytes, under the {}-byte chapter floor",
            text.len(),
            MIN_CHAPTER_BYTES
        );
    } else if text.len() > MAX_CHAPTER_BYTES {
        check.verdict = Verdict::TooShort;
        check.detail = format!(
            "the best container holds {} bytes, over the {}-byte chapter cap — \
             it is matching the whole page, not the chapter",
            text.len(),
            MAX_CHAPTER_BYTES
        );
    } else {
        let paras = text.matches("\n\n").count() + 1;
        check.detail = format!(
            "{} bytes of prose in {paras} paragraph(s) under {title:?}",
            text.len()
        );
    }
    Ok(check)
}

/// Say whether this page is a Cloudflare interstitial, and why we think so.
///
/// **In the host ABI, not in a template.** Two reasons, and the second is the
/// one that decided it:
///
///   1. A body-marker check is a *detection heuristic*, and heuristics belong
///      where they can be fixed once. Two templates each carrying their own
///      copy is how the webnovel template ended up missing it while the
///      truyencom one had it — caught by a test, not by review.
///   2. Every operator's own crawler needs this too, and the failure it prevents
///      is the worst one there is: a *successful* crawl of a challenge page,
///      stored as `chNN.txt`, digested as if it were prose. A `blocked` verdict
///      is visible on a ledger row; that is not.
///
/// Two independent signals, because either alone has a false side. The header
/// is the site stating it outright; the body markers catch the interstitial
/// served as `200 OK`, which has no status left to read. A body match is only
/// believed for a `200` — a 403 whose error page merely links to
/// `challenges.cloudflare.com` is still a challenge, but naming that is the
/// status check's job, not this one's.
pub fn interstitial(page: &super::host::Page) -> Option<String> {
    if page.headers.get(CF_HEADER).is_some_and(|v| !v.is_empty()) {
        return Some(format!(
            "HTTP {} with cf-mitigated: challenge — Cloudflare is challenging this client",
            page.status
        ));
    }
    if page.status != 200 {
        return None;
    }
    // **Comments are stripped first, and that is not a detail.** A marker inside
    // an HTML comment is documentation, not behaviour — a page that merely
    // *mentions* `cf-mitigated` (this repo's own fixtures do, in their
    // provenance notes; so would a blog post, a Stack Overflow answer, or a
    // chapter of a novel about web security) is a real page, and refusing it
    // would turn a detector into a denial-of-service. Script *bodies* are not
    // stripped, because that is exactly where `cf_chl_opt` lives.
    let body = without_comments(&page.body);
    let hit = CF_BODY_MARKERS
        .iter()
        .find(|m| body.contains(**m))
        .copied()?;
    Some(format!(
        "HTTP 200 but the body is a Cloudflare interstitial ({hit:?}) — \
         a crawl would store this page as the chapter"
    ))
}

/// Drop `<!-- … -->` spans, so a marker can only match where it is live.
///
/// A hand-rolled scan rather than a parse: this runs on every fetched page
/// before a decision, and the only question it has to answer is "does this
/// string appear outside a comment and outside markup", which does not need a
/// document model.
///
/// **Script and style bodies are copied through whole**, and that is the part
/// that decides it: `cf_chl_opt` and the challenge's own scripts live inside
/// `<script>`, so a stripper that also ate script bodies would delete the very
/// evidence it is looking for. It is the safe direction too — a page whose
/// JavaScript contains the literal `<!--` (a string, a nested template) can no
/// longer swallow the rest of the document and hide a real challenge behind it.
/// The
//  unterminated case (`<!--` with no `-->`) consumes the rest of the page,
///  which is the conservative direction — a comment that never closes really
///  does mean nothing after it is markup.
fn without_comments(html: &str) -> String {
    if !html.contains("<!--") {
        return html.to_string();
    }
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len());
    let mut pos = 0usize;
    loop {
        // Whichever comes first decides: a comment is removed, a raw-text
        // element is copied whole. Checking them in that order matters — a
        // `<script>` that opens *before* the next `<!--` means the `<!--` is
        // JavaScript, not a comment.
        let comment = lower[pos..].find("<!--").map(|i| pos + i);
        let raw = raw_text_open(&lower[pos..]).map(|i| pos + i);
        let raw_first = match (comment, raw) {
            (_, Some(open)) => comment.is_none_or(|c| open < c),
            (None, None) => false,
            _ => false,
        };
        if raw_first {
            let open = raw.expect("raw_first implies a raw element");
            // Copy the element up to its closing tag; its body is not markup
            // and must not be interpreted as such.
            match lower[open..].find("</") {
                Some(rel_close) => {
                    let end = open + rel_close;
                    out.push_str(&html[pos..end]);
                    pos = end;
                }
                None => {
                    out.push_str(&html[pos..]);
                    return out;
                }
            }
        } else if let Some(start) = comment {
            out.push_str(&html[pos..start]);
            let Some(rel_end) = lower[start..].find("-->") else {
                return out;
            };
            pos = start + rel_end + 3;
        } else {
            out.push_str(&html[pos..]);
            return out;
        }
    }
}

/// Where the next `<script` or `<style` opens in `rest`, if one does.
fn raw_text_open(rest: &str) -> Option<usize> {
    match (rest.find("<script"), rest.find("<style")) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> Options {
        Options {
            timeout_secs: 5,
            ..Options::default()
        }
    }

    /// A page with a real chapter on it, big enough to clear the floor.
    fn chapter_page() -> String {
        let para = "Hắn bước vào phòng và nhìn quanh một lượt, không thấy một ai cả. \
                    Gió đêm thổi qua khe cửa, lay một chút đèn lồng trên bàn.";
        format!(
            "<html><head><title>Chương 1: Mở đầu</title></head><body>\
             <nav><a href=\"/a\">Mục lục</a><a href=\"/b\">Chương trước</a></nav>\
             <div id=\"chapter-c\"><p>{para}</p><p>{para}</p><p>{para}</p></div>\
             </body></html>"
        )
    }

    /// The 403 Cloudflare actually served, header and all, reduced to the parts
    /// a check reads. The shape is the point: `cf-mitigated: challenge`.
    fn cf_403() -> String {
        r#"<html><head><title>Just a moment...</title></head><body>
           <script src="/cdn-cgi/chl-platform/z.js"></script>
           <script>window._cf_chl_opt={}</script>
           </body></html>"#
            .to_string()
    }

    fn server(routes: Vec<(&str, u16, &str)>) -> String {
        super::super::script_tests::fixture::start(
            routes
                .into_iter()
                .map(|(p, s, b)| (p.to_string(), s, b.to_string()))
                .collect(),
        )
    }

    /// As [`server`], but a route may send response headers — path, status,
    /// body, headers, all borrowed from the fixture.
    type BorrowedRoute<'a> = (&'a str, u16, &'a str, Vec<(&'a str, &'a str)>);

    fn server_with(routes: Vec<BorrowedRoute<'_>>) -> String {
        let routes: Vec<super::super::script_tests::fixture::Route> = routes
            .into_iter()
            .map(|(p, s, b, h)| {
                (
                    p.to_string(),
                    s,
                    b.to_string(),
                    h.into_iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                )
            })
            .collect();
        super::super::script_tests::fixture::start_with(routes)
    }

    #[test]
    fn a_page_with_a_chapter_in_it_is_ok() {
        let base = server(vec![("/chuong-1", 200, &chapter_page())]);
        let check = probe(&format!("{base}/chuong-1"), &opts()).unwrap();
        assert_eq!(check.verdict, Verdict::Ok, "{}", check.detail);
        assert!(check.text_bytes > MIN_CHAPTER_BYTES);
        assert!(check.detail.contains("paragraph"), "{}", check.detail);
        assert!(check.verdict.crawlable());
        assert!(!check.retryable(), "a chapter is not a thing to retry");
    }

    #[test]
    fn a_cloudflare_403_is_named_rather_than_called_a_plain_refusal() {
        let base = server_with(vec![(
            "/chuong-1",
            403,
            &cf_403(),
            vec![("cf-mitigated", "challenge"), ("server", "cloudflare")],
        )]);
        let check = probe(&format!("{base}/chuong-1"), &opts()).unwrap();
        assert_eq!(check.status, 403);
        assert_eq!(check.verdict, Verdict::Cloudflare, "{}", check.detail);
        assert_eq!(check.class, Some(BlockedClass::Challenge));
        assert!(check.detail.contains("Cloudflare"), "{}", check.detail);
        // Retryable — because the crawl's own ladder says a challenge is, and
        // this check is not allowed a better opinion than the thing it predicts.
        assert!(check.retryable(), "a challenge takes the 3-strike ladder");
    }

    /// A 403 with no Cloudflare on it is a plain refusal, and the two must not
    /// be confused: one wants a cookie, the other wants a working link.
    #[test]
    fn a_403_without_cloudflare_on_it_is_just_a_refusal() {
        let base = server(vec![("/x", 403, "<html><body>forbidden</body></html>")]);
        let check = probe(&format!("{base}/x"), &opts()).unwrap();
        assert_eq!(check.verdict, Verdict::Refused, "{}", check.detail);
        assert_eq!(check.class, Some(BlockedClass::Challenge));
        assert!(!check.detail.contains("Cloudflare"), "{}", check.detail);
    }

    /// The failure mode a status check alone cannot see: Cloudflare's managed
    /// challenge is routinely served as `200 OK`, and a crawl that only watched
    /// the status wrote the interstitial straight to `chNN.txt`.
    #[test]
    fn a_challenge_served_as_200_is_still_a_challenge() {
        let base = server(vec![("/chuong-1", 200, &cf_403())]);
        let check = probe(&format!("{base}/chuong-1"), &opts()).unwrap();
        assert_eq!(check.status, 200, "the status really is 200");
        assert_eq!(check.verdict, Verdict::Cloudflare, "{}", check.detail);
        assert!(check.detail.contains("interstitial"), "{}", check.detail);
    }

    /// And the plain one, for contrast: a 200 with no challenge in it and no
    /// chapter either is a site problem, not a bot check.
    #[test]
    fn a_200_with_nothing_on_it_is_too_short_not_cloudflare() {
        let base = server(vec![("/x", 200, "<html><body><p>ok</p></body></html>")]);
        let check = probe(&format!("{base}/x"), &opts()).unwrap();
        assert_eq!(check.status, 200);
        assert_eq!(check.verdict, Verdict::TooShort, "{}", check.detail);
        assert!(check.detail.contains("floor"), "{}", check.detail);
    }

    /// The webnovel shape exactly: same URL, same IP, same second — a 403 with
    /// `cf-mitigated: challenge` and then, once a session cookie is added, the
    /// chapter. That second half is the only route past this site's check, and
    /// this is the assertion that says so.
    #[test]
    fn a_session_cookie_is_what_turns_a_challenge_into_a_chapter() {
        let base = super::super::script_tests::fixture::start_sequence(
            "/chuong-1",
            vec![
                (
                    403,
                    cf_403(),
                    vec![("cf-mitigated".into(), "challenge".into())],
                ),
                (200, chapter_page(), Vec::new()),
            ],
        );
        let mut o = opts();
        assert_eq!(
            probe(&format!("{base}/chuong-1"), &o).unwrap().verdict,
            Verdict::Cloudflare,
            "first, with no cookie"
        );
        o.headers
            .insert("Cookie".into(), "cf_clearance=pasted".into());
        let check = probe(&format!("{base}/chuong-1"), &o).unwrap();
        assert_eq!(check.verdict, Verdict::Ok, "{}", check.detail);
        assert!(check.text_bytes > MIN_CHAPTER_BYTES);
    }

    #[test]
    fn a_refusal_carries_the_class_a_crawl_would_use() {
        let base = server(vec![("/x", 429, "slow down")]);
        let check = probe(&format!("{base}/x"), &opts()).unwrap();
        assert_eq!(check.verdict, Verdict::Refused);
        assert_eq!(check.class, Some(BlockedClass::RateLimit));
        assert!(check.retryable(), "429 is worth another attempt");
    }

    #[test]
    fn a_dead_host_is_unreachable_not_refused() {
        // Port 1 on loopback: nothing listens, so this is a connection failure
        // rather than a site with something to say.
        let check = probe("http://127.0.0.1:1/x", &opts()).unwrap();
        assert_eq!(check.verdict, Verdict::Unreachable, "{}", check.detail);
        assert_eq!(check.status, 0);
        assert!(
            !check.detail.contains("Cloudflare"),
            "a dead host is not a bot check: {}",
            check.detail
        );
    }

    /// A page that *mentions* `cf-mitigated` is a page. This one was written the
    /// hard way: the webnovel fixture's own provenance comment quotes the header,
    /// and the detector refused the real chapter because of it. A detector that
    /// trips on the word is a detector that will eventually refuse a novel about
    /// web security.
    #[test]
    fn a_marker_inside_an_html_comment_is_not_a_challenge() {
        let page = format!(
            "<!-- captured while answering 403 with cf-mitigated: challenge; \
             also see challenges.cloudflare.com and 'Just a moment...' -->\n{}",
            chapter_page()
        );
        let base = server(vec![("/x", 200, &page)]);
        let check = probe(&format!("{base}/x"), &opts()).unwrap();
        assert_eq!(check.status, 200);
        assert_eq!(check.verdict, Verdict::Ok, "{}", check.detail);

        // …and the same markers *live* in the document are still caught, which is
        // what stops the comment stripper from neutering the check.
        let live = format!(
            "<html><head><title>Just a moment...</title></head><body>{}</body></html>",
            chapter_page()
        );
        let base = server(vec![("/x", 200, &live)]);
        let check = probe(&format!("{base}/x"), &opts()).unwrap();
        assert_eq!(check.verdict, Verdict::Cloudflare, "{}", check.detail);
    }

    /// The stripper itself, at the two edges that matter: an unterminated
    /// comment swallows the rest, and script bodies are left alone — because
    /// that is where `cf_chl_opt` lives, and stripping scripts would blind the
    /// detector it exists to feed.
    #[test]
    fn comment_stripping_handles_the_edges() {
        assert_eq!(without_comments("no comments here"), "no comments here");
        assert_eq!(without_comments("a<!-- x -->b"), "ab");
        assert_eq!(without_comments("a<!-- x -->b<!-- y -->c"), "abc");
        assert_eq!(without_comments("keep<!-- swallowed"), "keep");
        // A script body is copied whole — `<!--` inside it is JavaScript, not a
        // comment, and the challenge markers live in exactly such a place.
        let js = "<script>/* <!-- not a comment --> */</script>";
        assert!(without_comments(js).contains("not a comment"));
        assert_eq!(without_comments(js), js);
        // A comment before a script, and one inside it, both survive correctly.
        let mixed = "a<!-- c --><script>var s = '<!--';</script>b";
        assert!(
            without_comments(mixed).contains("var s = '<!--'"),
            "{}",
            without_comments(mixed)
        );
        assert!(without_comments(mixed).ends_with("b"));
    }

    /// The check's retry advice is the crawl's advice, read off the same field.
    /// A second opinion kept here would be one more thing to keep in step.
    #[test]
    fn the_check_and_the_crawl_agree_about_what_is_worth_retrying() {
        for status in [404u16, 410, 429, 401, 403, 503, 500] {
            let base = server(vec![("/x", status, "no")]);
            let check = probe(&format!("{base}/x"), &opts()).unwrap();
            let crawl_says = crate::crawl::provider::block_for_status(status).unwrap();
            assert_eq!(check.class, Some(crawl_says.class), "HTTP {status}");
            assert_eq!(
                check.retryable(),
                crawl_says.class.retryable(),
                "HTTP {status}: the check and the crawl disagree"
            );
        }
        let base = server(vec![("/x", 200, &chapter_page())]);
        let check = probe(&format!("{base}/x"), &opts()).unwrap();
        assert!(!check.retryable(), "a success is not a thing to retry");
    }
}
