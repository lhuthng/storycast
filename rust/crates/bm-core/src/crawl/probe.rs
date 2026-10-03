//! The quick check: **one request against a pasted link, before a workspace

use anyhow::Result;
use serde::Serialize;

use super::contract::BlockedClass;
use super::host::{Host, Limits};
use super::provider::{block_for_status, MAX_CHAPTER_BYTES, MIN_CHAPTER_BYTES};

/// Why a page is not a chapter, in the order the questions are worth asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// A chapter came back and cleared the length guard. Crawlable.
    Ok,
    /// The request never completed. Not a site problem, and worth saying so
    Unreachable,
    /// A non-2xx status, classified the way a crawl classifies it.
    Refused,
    /// A Cloudflare interstitial — either a 403/503 with the marker, or a
    Cloudflare,
    /// Fetched, and there is no chapter on it.
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
#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub url: String,
    /// Where the request ended up, which is not always where it was aimed —
    pub final_url: String,
    pub status: u16,
    pub verdict: Verdict,
    /// One line an operator can act on. Deliberately not a wall of text: this
    pub detail: String,
    /// Bytes of body received.
    pub bytes: usize,
    /// Bytes that survived the chapter boundary, or 0 when there were none.
    pub text_bytes: usize,
    /// The container the generic heuristic would take, when one stood out. The
    pub guess: String,
    /// Classified refusal, when the failure was an HTTP one — so a check and a
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class: Option<BlockedClass>,
}

impl Check {
    /// Whether another attempt later could plausibly work.
    pub fn retryable(&self) -> bool {
        match self.verdict {
            Verdict::Ok | Verdict::NoContainer => false,
            // No class means the failure was not an HTTP one, so the honest
            _ => self.class.is_none_or(|c| c.retryable()),
        }
    }
}

/// Markers that identify a Cloudflare interstitial in a body served as `200`.
const CF_BODY_MARKERS: [&str; 4] = [
    "cf_chl_opt",
    "challenges.cloudflare.com",
    "cf-mitigated",
    "Just a moment",
];

/// The `cf-mitigated` response header, whose value is the site saying so
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
/// before a decision, and the only question it has to answer is "does this
/// string appear outside a comment and outside markup", which does not need a
fn without_comments(html: &str) -> String {
    if !html.contains("<!--") {
        return html.to_string();
    }
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len());
    let mut pos = 0usize;
    loop {
        // Whichever comes first decides: a comment is removed, a raw-text
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
        assert!(check.retryable(), "a challenge takes the 3-strike ladder");
    }

    /// A 403 with no Cloudflare on it is a plain refusal, and the two must not
    #[test]
    fn a_403_without_cloudflare_on_it_is_just_a_refusal() {
        let base = server(vec![("/x", 403, "<html><body>forbidden</body></html>")]);
        let check = probe(&format!("{base}/x"), &opts()).unwrap();
        assert_eq!(check.verdict, Verdict::Refused, "{}", check.detail);
        assert_eq!(check.class, Some(BlockedClass::Challenge));
        assert!(!check.detail.contains("Cloudflare"), "{}", check.detail);
    }

    /// The failure mode a status check alone cannot see: Cloudflare's managed
    #[test]
    fn a_challenge_served_as_200_is_still_a_challenge() {
        let base = server(vec![("/chuong-1", 200, &cf_403())]);
        let check = probe(&format!("{base}/chuong-1"), &opts()).unwrap();
        assert_eq!(check.status, 200, "the status really is 200");
        assert_eq!(check.verdict, Verdict::Cloudflare, "{}", check.detail);
        assert!(check.detail.contains("interstitial"), "{}", check.detail);
    }

    /// And the plain one, for contrast: a 200 with no challenge in it and no
    #[test]
    fn a_200_with_nothing_on_it_is_too_short_not_cloudflare() {
        let base = server(vec![("/x", 200, "<html><body><p>ok</p></body></html>")]);
        let check = probe(&format!("{base}/x"), &opts()).unwrap();
        assert_eq!(check.status, 200);
        assert_eq!(check.verdict, Verdict::TooShort, "{}", check.detail);
        assert!(check.detail.contains("floor"), "{}", check.detail);
    }

    /// The webnovel shape exactly: same URL, same IP, same second — a 403 with
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
        let live = format!(
            "<html><head><title>Just a moment...</title></head><body>{}</body></html>",
            chapter_page()
        );
        let base = server(vec![("/x", 200, &live)]);
        let check = probe(&format!("{base}/x"), &opts()).unwrap();
        assert_eq!(check.verdict, Verdict::Cloudflare, "{}", check.detail);
    }

    /// The stripper itself, at the two edges that matter: an unterminated
    #[test]
    fn comment_stripping_handles_the_edges() {
        assert_eq!(without_comments("no comments here"), "no comments here");
        assert_eq!(without_comments("a<!-- x -->b"), "ab");
        assert_eq!(without_comments("a<!-- x -->b<!-- y -->c"), "abc");
        assert_eq!(without_comments("keep<!-- swallowed"), "keep");
        // A script body is copied whole — `<!--` inside it is JavaScript, not a
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
