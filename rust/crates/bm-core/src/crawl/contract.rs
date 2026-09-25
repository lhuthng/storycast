//! The crawl contract: one request object in, one response object out.
//!
//! This is the frozen half of the scripted crawl. An operator writes a crawler
//! as a **function**, not a file format:
//!
//! ```text
//! crawl(input)    -> { text, url } | { none = true, reason } | { blocked = { class } }
//! discover(input) -> { chapters = [{ n, url }], total }      (optional)
//! ```
//!
//! Everything else — which engine, where the script lives, how the URL is
//! derived — is downstream of these two shapes. `discover` is the mapping from
//! the pipeline's dense chapter index `n` to whatever the site calls a chapter
//! (`chapter-001`, a slug, `v2c15`); it is optional because a plain
//! `url_template` is a degenerate mapping the host can compute itself, and it
//! runs **once per range** on the inductor rather than once per chapter.
//!
//! The request deliberately carries `params` as an opaque object. The moment
//! Rust validates its keys, the site-specific part of crawling is hardcoded
//! again — which is the thing this module exists to undo.

use serde::{Deserialize, Serialize};

/// What one `crawl(n)` call is handed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CrawlRequest {
    /// The pipeline's chapter index — dense, 1-based, the same number
    /// `data/chapters/chNN.txt` is named after. Not necessarily the site's
    /// chapter number: `discover` exists to absorb that difference.
    pub n: u32,
    /// The URL from the manifest, already substituted. `None` when nothing
    /// computed one — a slug site with no `discover`, say — and then the
    /// script builds its own.
    #[serde(default)]
    pub url: Option<String>,
    /// The workspace's `crawl.params`, passed through verbatim.
    #[serde(default)]
    pub params: serde_json::Map<String, serde_json::Value>,
    /// 1-based attempt count, so a script can try a mirror on the second go.
    #[serde(default = "one")]
    pub attempt: u32,
}

fn one() -> u32 {
    1
}

/// What `crawl(n)` hands back, before it is interpreted.
///
/// Exactly one of the three outcomes is meaningful; a response with `text` and
/// nothing else is the normal case.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CrawlResponse {
    /// The chapter, as prose, with the headline on the first line.
    #[serde(default)]
    pub text: Option<String>,
    /// Echo only — it lands in the log so an operator can see what a chapter
    /// actually came from. Never parsed for a chapter number.
    #[serde(default)]
    pub url: Option<String>,
    /// The site does not have this chapter (a listing that ends at 380, a
    /// prologue that is not in the pipeline's index). Terminal and **not** a
    /// failure: it must not cost a strike.
    #[serde(default)]
    pub none: bool,
    /// Why the chapter is absent, for the ledger row.
    #[serde(default)]
    pub reason: String,
    /// The site refused. A different signal from a crash: the class decides
    /// whether retrying is worth a box or the chapter should be shelved with
    /// the diagnosis visible.
    #[serde(default)]
    pub blocked: Option<Blocked>,
}

impl CrawlResponse {
    /// The response as one of three outcomes, refusing the shapes that would
    /// otherwise be silently wrong — a blocked response carrying text, or an
    /// empty response that claims neither.
    pub fn into_outcome(self) -> anyhow::Result<CrawlOutcome> {
        if let Some(b) = self.blocked {
            return Ok(CrawlOutcome::Blocked(b));
        }
        if self.none {
            return Ok(CrawlOutcome::Absent {
                reason: if self.reason.trim().is_empty() {
                    "the script reported no chapter".into()
                } else {
                    self.reason.trim().to_string()
                },
            });
        }
        match self.text {
            Some(t) => Ok(CrawlOutcome::Text {
                text: t,
                url: self.url,
            }),
            None => anyhow::bail!(
                "the crawl script returned neither `text`, `none` nor `blocked` — return one of them"
            ),
        }
    }
}

/// A refusal, classified.
///
/// The distinction that matters: `rate_limit` and `challenge` are worth
/// another attempt (with backoff), while `login_required`, `js_required` and
/// `gone` are not — retrying those three times before shelving only wastes a
/// worker and an hour, and a challenge page served as HTTP 200 is exactly the
/// case a bare "3 strikes" policy cannot tell from a real chapter.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Blocked {
    #[serde(default)]
    pub class: BlockedClass,
    /// Human-readable cause, shown on the ledger row.
    #[serde(default)]
    pub detail: String,
    /// Seconds the script wants the host to wait, when it knows.
    #[serde(default)]
    pub retry_after: Option<u64>,
}

impl Blocked {
    pub fn new(class: BlockedClass, detail: impl Into<String>) -> Blocked {
        Blocked {
            class,
            detail: detail.into(),
            retry_after: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockedClass {
    /// A bot check / interstitial served instead of the chapter.
    Challenge,
    /// Explicit rate limiting — 429, or a warning page.
    RateLimit,
    /// The site wants a session or a purchase.
    LoginRequired,
    /// The body is empty on arrival and only a script can fill it.
    JsRequired,
    /// 404/410 on a URL the manifest claimed exists.
    Gone,
    /// A page that read fine but held no chapter text (wrong selector,
    /// truncated response).
    Empty,
    #[default]
    Unknown,
}

impl BlockedClass {
    pub fn as_str(self) -> &'static str {
        match self {
            BlockedClass::Challenge => "challenge",
            BlockedClass::RateLimit => "rate_limit",
            BlockedClass::LoginRequired => "login_required",
            BlockedClass::JsRequired => "js_required",
            BlockedClass::Gone => "gone",
            BlockedClass::Empty => "empty",
            BlockedClass::Unknown => "unknown",
        }
    }

    pub fn parse(s: &str) -> BlockedClass {
        match s.trim().to_ascii_lowercase().as_str() {
            "challenge" | "captcha" | "cloudflare" => BlockedClass::Challenge,
            "rate_limit" | "rate-limit" | "ratelimit" | "429" => BlockedClass::RateLimit,
            "login_required" | "login" | "paywall" => BlockedClass::LoginRequired,
            "js_required" | "js" => BlockedClass::JsRequired,
            "gone" | "404" | "410" => BlockedClass::Gone,
            "empty" => BlockedClass::Empty,
            _ => BlockedClass::Unknown,
        }
    }

    /// Whether another attempt is worth a worker.
    ///
    /// Retryable classes take the ordinary strike ladder; the rest shelve
    /// immediately **with the diagnosis on the row**, because three attempts at
    /// a login wall prove nothing a single one did not.
    pub fn retryable(self) -> bool {
        matches!(
            self,
            BlockedClass::RateLimit | BlockedClass::Challenge | BlockedClass::Unknown
        )
    }
}

/// The interpreted result of one chapter.
#[derive(Debug, Clone)]
pub enum CrawlOutcome {
    /// Prose, ready to be written to `chapter_txt(n)`.
    Text { text: String, url: Option<String> },
    /// The chapter does not exist. A terminal non-failure.
    Absent { reason: String },
    /// The site refused, with enough information to decide what next.
    Blocked(Blocked),
}

/// What `discover(input)` is handed: the range to map, never the whole book.
///
/// A pure mapping never has to walk from chapter 1 because of this, and a
/// mapping that *does* need the site is asked once for a bounded range rather
/// than per chapter.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiscoverRequest {
    #[serde(default)]
    pub params: serde_json::Map<String, serde_json::Value>,
    #[serde(default = "one")]
    pub start: u32,
    #[serde(default)]
    pub count: u32,
}

/// What `discover(input)` hands back.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Discovered {
    #[serde(default)]
    pub chapters: Vec<DiscoveredChapter>,
    /// How many chapters the book has, when the listing says. Lets a range
    /// that runs past the end be trimmed *before* twenty tasks are enqueued.
    #[serde(default)]
    pub total: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiscoveredChapter {
    pub n: u32,
    /// `None` with `absent` unset is "no URL, the script will build one".
    #[serde(default)]
    pub url: Option<String>,
    /// Display only: it is written into the index file next to the URL, so a
    /// human reading `data/crawl-index.json` sees the site's own chapter names
    /// before any digest exists. Never read for a title — that stays the first
    /// line of the chapter text, or the digest's own `title`.
    #[serde(default)]
    pub title: String,
    /// The site has no chapter at `n`.
    #[serde(default)]
    pub absent: bool,
}
