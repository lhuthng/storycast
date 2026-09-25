//! What the host gives a script, and what it refuses to give it.
//!
//! One network primitive — `fetch` — with the user agent, the timeout, the
//! pacing and the size cap owned here rather than by the script. That is the
//! whole sandbox story for the in-process engines: a crawl script gets bytes
//! and pure functions, **never** the environment, the filesystem or a shell.
//! `std::env` keys are not reachable from Lua or JavaScript through this ABI at
//! all, which is why a scripted crawler is a different risk class from a
//! user-supplied *program* — and why the bundled scripts can ship in a profile
//! and be rsynced to workers.
//!
//! ## Charset
//!
//! Novel sites are not all UTF-8, and a GBK page decoded as UTF-8 is not
//! "slightly wrong" — it is unreadable prose that passes a length guard and
//! then fails every downstream stage. So the body is decoded deliberately:
//! the `Content-Type` charset when the server sends one, otherwise UTF-8, and
//! otherwise the `<meta charset>` in the document head.

use anyhow::{anyhow, Context, Result};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use super::pacing;

/// The browser-ish agent the old crawler sent, and the default here.
pub const DEFAULT_USER_AGENT: &str = "Mozilla/5.0";
/// The old `run_crawl` timeout, kept as the default.
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;
/// A page larger than this is not a chapter — it is a mis-fetch (a video, an
/// archive) and reading it into memory helps nobody.
pub const MAX_PAGE_BYTES: usize = 8 * 1024 * 1024;

/// What one task may spend. Enforced here, not asked of the script: a budget a
/// script can raise is not a budget, and an infinite loop in a listing walk
/// must cost one task rather than a worker.
#[derive(Debug, Clone)]
pub struct Limits {
    /// Network round trips per chapter. A listing walk spends several; a
    /// single-page crawl spends one. The cap is what stops a bug in a `next`
    /// link loop from walking a site all afternoon.
    pub max_fetches: u32,
    /// Wall-clock budget for the whole chapter, engine time included.
    pub max_seconds: u64,
    /// Per-request timeout.
    pub timeout_secs: u64,
    /// Minimum spacing between two fetches of the same host, in ms.
    pub pace_ms: u64,
    pub max_page_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_fetches: 64,
            max_seconds: 180,
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            pace_ms: 0,
            max_page_bytes: MAX_PAGE_BYTES,
        }
    }
}

/// One fetched page, as a script sees it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Page {
    /// The HTTP status. **Not** an error when it is not 2xx: 429 and 403 are
    /// precisely what a script needs to classify as `rate_limit` or
    /// `login_required`, and an exception would throw that information away.
    pub status: u16,
    /// Where the response came from (after redirects).
    pub url: String,
    pub body: String,
    /// The response headers, names lowercased.
    ///
    /// A script gets these because the status alone is not the whole answer:
    /// `cf-mitigated: challenge` distinguishes a bot check from a plain 403,
    /// and `content-type` says whether a body is even a page. A check reads the
    /// first of those, and so can a script.
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
}

/// Options a script may pass to `fetch`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FetchOptions {
    #[serde(default)]
    pub method: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body: Option<String>,
}

/// One chapter's host: a client, a budget, and the log the script wrote.
pub struct Host {
    /// A **blocking** client, because the engines are blocking: a script's
    /// `fetch` is a synchronous call, and driving it from an interpreter's
    /// host function means blocking the thread anyway. The provider is
    /// responsible for calling this from a blocking thread — see
    /// [`super::provider`].
    client: Client,
    limits: Limits,
    deadline: Instant,
    fetches: u32,
    /// The script's own log lines, surfaced in the ledger so a walk can say
    /// what it did without a debugger.
    pub log: Vec<String>,
}

impl Host {
    pub fn new(
        user_agent: &str,
        headers: &BTreeMap<String, String>,
        limits: Limits,
    ) -> Result<Host> {
        let ua = if user_agent.trim().is_empty() {
            DEFAULT_USER_AGENT
        } else {
            user_agent.trim()
        };
        let mut default_headers = reqwest::header::HeaderMap::new();
        for (k, v) in headers {
            let (Ok(name), Ok(value)) = (
                reqwest::header::HeaderName::from_bytes(k.as_bytes()),
                reqwest::header::HeaderValue::from_str(v),
            ) else {
                anyhow::bail!("crawl header {k:?} is not a valid header");
            };
            default_headers.insert(name, value);
        }
        let client = Client::builder()
            .timeout(Duration::from_secs(limits.timeout_secs.max(1)))
            .user_agent(ua)
            .default_headers(default_headers)
            .build()
            .context("building the crawl http client")?;
        Ok(Host {
            client,
            deadline: Instant::now() + Duration::from_secs(limits.max_seconds),
            limits,
            fetches: 0,
            log: Vec::new(),
        })
    }

    /// Log one line, for the ledger. Capped: a script that logs inside a walk
    /// loop must not grow the report without bound.
    pub fn note(&mut self, msg: impl Into<String>) {
        if self.log.len() < 64 {
            self.log.push(msg.into());
        }
    }

    /// Whether the chapter's budget is spent. Called by the engine between
    /// steps and from its own interrupt hook.
    pub fn budget_ok(&self) -> bool {
        Instant::now() < self.deadline
    }

    /// Fail if the budget is spent. The message is what the ledger shows.
    pub fn check_budget(&self) -> Result<()> {
        if self.budget_ok() {
            return Ok(());
        }
        Err(anyhow!(
            "crawl exceeded its {}s budget ({} fetch(es) so far) — a `next` loop may not be terminating",
            self.limits.max_seconds,
            self.fetches
        ))
    }

    pub fn fetches(&self) -> u32 {
        self.fetches
    }

    /// The budget in force, for an error message that names the number rather
    /// than only the symptom.
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// GET (or POST) one URL. The only way out of the sandbox.
    pub fn fetch(&mut self, url: &str, opts: Option<FetchOptions>) -> Result<Page> {
        self.check_budget()?;
        if self.fetches >= self.limits.max_fetches {
            return Err(anyhow!(
                "crawl exceeded its {} fetch budget for one chapter — a listing walk may be looping",
                self.limits.max_fetches
            ));
        }
        let url = url.trim();
        if url.is_empty() {
            return Err(anyhow!("fetch: empty URL"));
        }
        let opts = opts.unwrap_or_default();
        let method = opts.method.trim().to_ascii_uppercase();
        let method = if method.is_empty() {
            reqwest::Method::GET
        } else {
            reqwest::Method::from_bytes(method.as_bytes())
                .map_err(|_| anyhow!("fetch: {method:?} is not an HTTP method"))?
        };

        // Space requests to one host, ours and the script's alike.
        let host = pacing::host_of(url);
        let slept = pacing::wait_for(&host, Duration::from_millis(self.limits.pace_ms));
        if !slept.is_zero() {
            self.note(format!("paced {host}: waited {}ms", slept.as_millis()));
        }

        let mut req = self.client.request(method, url);
        for (k, v) in &opts.headers {
            req = req.header(k, v);
        }
        if let Some(body) = opts.body {
            req = req.body(body);
        }
        self.fetches += 1;
        let resp = req.send().with_context(|| format!("GET {url}"))?;
        let status = resp.status().as_u16();
        let final_url = resp.url().to_string();
        let ctype = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        // Read before the body: `bytes()` consumes the response, and the
        // headers are what tell a script (and a check) that a 403 was a bot
        // check rather than a dead link.
        let headers: std::collections::BTreeMap<String, String> = resp
            .headers()
            .iter()
            .filter_map(|(k, v)| {
                v.to_str()
                    .ok()
                    .map(|v| (k.as_str().to_ascii_lowercase(), v.to_string()))
            })
            .collect();
        let bytes = resp.bytes().context("reading the response body")?;
        let bytes = if bytes.len() > self.limits.max_page_bytes {
            self.note(format!(
                "{final_url}: {} bytes truncated to {}",
                bytes.len(),
                self.limits.max_page_bytes
            ));
            bytes.slice(..self.limits.max_page_bytes)
        } else {
            bytes
        };
        Ok(Page {
            status,
            url: final_url,
            body: decode_body(&bytes, ctype.as_deref()),
            headers,
        })
    }
}

/// Decode a response body: UTF-8 when the bytes validate, else the declared
/// charset, else the document's own `<meta charset>`.
///
/// **UTF-8 is asked first, and that is not the naive order.** Bytes that are
/// valid UTF-8 are UTF-8 in practice — a GBK page whose bytes happen to form a
/// valid UTF-8 sequence does not occur for real text — whereas a mis-declared
/// header is common (a site serving UTF-8 with a stale
/// `charset=gb2312`), and believing it first would turn a perfectly readable
/// chapter into mojibake. Only bytes that *cannot* be UTF-8 are handed to a
/// declared encoding, and then the header wins over `<meta>` because it is the
/// transport's own statement.
pub fn decode_body(bytes: &[u8], content_type: Option<&str>) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    let label = content_type
        .and_then(charset_of_label)
        .or_else(|| charset_from_meta(bytes));
    match label.and_then(|l| encoding_rs::Encoding::for_label(l.as_bytes())) {
        Some(enc) => {
            let (text, _, _) = enc.decode(bytes);
            text.into_owned()
        }
        None => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// The `charset=` parameter of a `Content-Type` header value.
fn charset_of_label(content_type: &str) -> Option<String> {
    let lower = content_type.to_ascii_lowercase();
    let idx = lower.find("charset")?;
    let rest = &content_type[idx + "charset".len()..];
    // Both `<meta charset="gbk">` and `Content-Type: text/html; charset=gbk`
    // reach here, and so does `charset = "gb2312"` with its spaces. The quote
    // strip is load-bearing: without it the value read is the opening quote,
    // the label lookup fails, and the page falls through to lossy UTF-8.
    let rest = rest.trim_start().strip_prefix('=')?.trim_start();
    let rest = rest
        .strip_prefix('"')
        .or_else(|| rest.strip_prefix('\''))
        .unwrap_or(rest);
    let value: String = rest
        .chars()
        .take_while(|c| !matches!(c, ';' | ',' | ' ' | '\t' | '"' | '\''))
        .collect();
    (!value.is_empty()).then_some(value)
}

/// The `<meta charset>` / `<meta content="…charset=…">` declaration of a page.
///
/// Read from a lossy-ASCII view of the first 4 KiB: the declaration is ASCII by
/// definition, and the surrounding bytes may be any encoding at all.
fn charset_from_meta(bytes: &[u8]) -> Option<String> {
    let head: String = bytes
        .iter()
        .take(4096)
        .map(|b| if b.is_ascii() { *b as char } else { ' ' })
        .collect();
    charset_of_label(&head)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_declared_charset_decodes_bytes_that_are_not_utf8() {
        // Chinese in GBK, which is not valid UTF-8 — the shape that reaches a
        // length guard as unreadable prose if nothing decodes it. (Vietnamese
        // would not do here: GBK cannot encode it, and encoding_rs falls back
        // to numeric character references, which happen to be valid UTF-8.)
        let gbk = encoding_rs::GBK.encode("第一章 试炼").0;
        assert!(std::str::from_utf8(&gbk).is_err());
        assert_eq!(
            decode_body(&gbk, Some("text/html; charset=gbk")),
            "第一章 试炼"
        );
        // And the same bytes with no header fall back to the document's own
        // declaration.
        let mut page = b"<html><head><meta charset=\"gbk\"></head><body>".to_vec();
        page.extend_from_slice(&gbk);
        page.extend_from_slice(b"</body></html>");
        let decoded = decode_body(&page, None);
        assert!(decoded.contains("第一章 试炼"), "{decoded}");
    }

    #[test]
    fn utf8_is_only_second_guessed_when_it_is_provably_not_utf8() {
        let utf8 = "Chương 34: Bí ẩn".as_bytes();
        assert_eq!(decode_body(utf8, None), "Chương 34: Bí ẩn");
        // A page that says gbk but is really utf8 stays readable: the header is
        // believed only when the bytes are not already valid UTF-8.
        assert_eq!(
            decode_body(utf8, Some("text/html; charset=gbk")),
            "Chương 34: Bí ẩn"
        );
        // Bytes that are valid UTF-8 are never re-decoded.
        assert_eq!(decode_body(b"plain ascii", None), "plain ascii");
    }

    #[test]
    fn a_charset_label_is_read_out_of_a_header_with_casing_and_quotes() {
        assert_eq!(
            charset_of_label("text/html; charset=UTF-8").as_deref(),
            Some("UTF-8")
        );
        assert_eq!(
            charset_of_label("text/html;charset=\"gb2312\"").as_deref(),
            Some("gb2312")
        );
        assert_eq!(
            charset_of_label("text/html; charset = gb18030").as_deref(),
            Some("gb18030")
        );
        assert_eq!(charset_of_label("text/html").as_deref(), None);
    }

    #[test]
    fn the_budget_is_checked_before_the_network_and_reported_in_the_error() {
        let mut host = Host::new(
            "",
            &BTreeMap::new(),
            Limits {
                max_fetches: 1,
                max_seconds: 3600,
                ..Limits::default()
            },
        )
        .unwrap();
        // A spent clock fails before any request is attempted.
        host.deadline = Instant::now() - Duration::from_secs(1);
        let err = host.fetch("https://example.invalid/", None).unwrap_err();
        assert!(err.to_string().contains("budget"), "{err}");
        assert_eq!(host.fetches(), 0, "no request was made");
    }

    #[test]
    fn the_fetch_budget_stops_a_runaway_walk_without_touching_the_network() {
        let mut host = Host::new(
            "",
            &BTreeMap::new(),
            Limits {
                max_fetches: 0,
                max_seconds: 3600,
                ..Limits::default()
            },
        )
        .unwrap();
        let err = host.fetch("https://example.invalid/", None).unwrap_err();
        assert!(err.to_string().contains("fetch budget"), "{err}");
    }

    #[test]
    fn a_bad_header_is_refused_when_the_host_is_built() {
        let mut headers = BTreeMap::new();
        headers.insert("x bad".into(), "v".into());
        assert!(Host::new("", &headers, Limits::default()).is_err());
        let mut ok = BTreeMap::new();
        ok.insert("referer".into(), "https://site.example/".into());
        assert!(Host::new("", &ok, Limits::default()).is_ok());
    }
}
