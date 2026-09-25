//! The provider: one chapter in, one outcome out.
//!
//! Two implementations behind one call, and they differ only in *who* decides
//! where the bytes come from:
//!
//! * a **script** — the operator's Lua or JavaScript, run for one chapter. This
//!   is where every site-specific rule lives, including the default crawler's;
//! * the **built-in** path — GET the manifest's URL and take the text of the
//!   container `crawl.params.extract` names, or `readable()`'s guess when it
//!   names none. Site-agnostic by construction, and a fallback for a workspace
//!   whose script cannot be read at all rather than a crawler in its own right.
//!
//! What both share is the part that must not vary: the chapter boundary
//! ([`super::sanitize_chapter_text`]) and the length guard. A script decides
//! what the text *is*; the host decides what counts as a chapter at all, so
//! "the selector missed" fails at the crawl rather than at the digest.
//!
//! **Blocking.** Everything here is synchronous and must run on a blocking
//! thread (`spawn_blocking`), never directly inside a tokio task: the HTTP
//! client is `reqwest::blocking`, whose runtime cannot be built inside one.

use anyhow::{anyhow, Context, Result};
use bm_proto::CrawlSpec;
use serde_json::json;
use std::cell::RefCell;
use std::rc::Rc;

use super::contract::{CrawlOutcome, CrawlRequest, CrawlResponse, DiscoverRequest, Discovered};
use super::engine::{discovered_from, Entry, Program, SharedHost};
use super::host::{Host, Limits, DEFAULT_TIMEOUT_SECS};
use super::index::expand_template;
use crate::config::Settings;
use crate::Layout;

/// The shortest body that could be a chapter.
///
/// **Bytes, not characters**, and that is not a detail: Vietnamese prose is
/// two bytes per accented letter, so a 150-character chapter is over 200 bytes
/// and passed the pre-existing guard. Switching to `chars().count()` here would
/// silently start refusing short chapters that had always worked.
pub const MIN_CHAPTER_BYTES: usize = 200;

/// Bound on what a script may hand back, so a runaway selector cannot write a
/// gigabyte into the ledger.
pub const MAX_CHAPTER_BYTES: usize = 4 * 1024 * 1024;

/// One chapter's crawl, plus what the script said while doing it.
#[derive(Debug, Clone)]
pub struct Crawled {
    pub outcome: CrawlOutcome,
    /// The script's own `log()` lines and the host's own notes (pacing, a
    /// selector that matched nothing). Carried into the ledger row.
    pub log: Vec<String>,
    pub fetches: u32,
}

/// A crawler ready to run: the spec, plus its script parsed into an engine.
pub struct Provider {
    spec: CrawlSpec,
    program: Option<Program>,
}

impl Provider {
    /// Build from a spec. A spec with no engine, or one whose script cannot be
    /// parsed, is the built-in path — parsing is deferred to the first call, so
    /// this never fails.
    pub fn new(spec: &CrawlSpec) -> Provider {
        let program = match engine_kind(&spec.engine, &spec.script) {
            Some(kind) if !spec.source.trim().is_empty() => Some(Program::new(
                kind,
                if spec.script.trim().is_empty() {
                    format!("crawl.{}", kind.as_str())
                } else {
                    spec.script.clone()
                },
                spec.source.clone(),
            )),
            _ => None,
        };
        Provider {
            spec: spec.clone(),
            program,
        }
    }

    pub fn spec(&self) -> &CrawlSpec {
        &self.spec
    }

    /// Whether this provider runs an operator's script (as opposed to the
    /// built-in fetcher).
    pub fn is_scripted(&self) -> bool {
        self.program.is_some()
    }

    fn limits(&self) -> Limits {
        let s = &self.spec;
        Limits {
            max_fetches: if s.max_fetches == 0 {
                Limits::default().max_fetches
            } else {
                s.max_fetches
            },
            max_seconds: if s.max_seconds == 0 {
                Limits::default().max_seconds
            } else {
                s.max_seconds
            },
            timeout_secs: if s.timeout_secs == 0 {
                DEFAULT_TIMEOUT_SECS
            } else {
                s.timeout_secs
            },
            pace_ms: s.pace_ms,
            ..Limits::default()
        }
    }

    fn host(&self) -> Result<SharedHost> {
        let host = Host::new(&self.spec.user_agent, &self.spec.headers, self.limits())?;
        Ok(Rc::new(RefCell::new(host)))
    }

    /// Crawl one chapter.
    ///
    /// `url` is the manifest's answer for this `n`; `None` means nothing
    /// computed one, and then the script (or the template) has to.
    pub fn crawl(&self, n: u32, url: Option<&str>, attempt: u32) -> Result<Crawled> {
        let host = self.host()?;
        let outcome = match &self.program {
            Some(program) => {
                let request = CrawlRequest {
                    n,
                    url: url.map(str::to_string).or_else(|| self.templated(n)),
                    params: self.spec.params.clone(),
                    attempt,
                };
                let value = program
                    .run(Entry::Crawl, host.clone(), &serde_json::to_value(&request)?)?
                    .ok_or_else(|| {
                        anyhow!(
                            "{}: crawl(input) returned nothing — return {{ text = … }}, {{ none = true }} or {{ blocked = {{ … }} }}",
                            program.name
                        )
                    })?;
                let response: CrawlResponse = serde_json::from_value(value).with_context(|| {
                    format!("{}: crawl(input) returned an unusable value", program.name)
                })?;
                response.into_outcome()?
            }
            None => {
                let mut host = host
                    .try_borrow_mut()
                    .map_err(|_| anyhow!("the crawl host is already in use"))?;
                self.builtin(&mut host, n, url)?
            }
        };
        let outcome = finish(outcome);
        let host = host.borrow();
        Ok(Crawled {
            outcome,
            log: host.log.clone(),
            fetches: host.fetches(),
        })
    }

    /// Run `discover` once for a range. `Ok(None)` when the script has none —
    /// the normal single-page case, and the signal to fall back to the
    /// template.
    pub fn discover(&self, start: u32, count: u32) -> Result<Option<Discovered>> {
        let Some(program) = &self.program else {
            return Ok(None);
        };
        let host = self.host()?;
        let request = DiscoverRequest {
            params: self.spec.params.clone(),
            start,
            count,
        };
        let Some(value) = program.run(
            Entry::Discover,
            host.clone(),
            &serde_json::to_value(&request)?,
        )?
        else {
            return Ok(None);
        };
        let found = discovered_from(value)?;
        Ok(Some(found))
    }

    /// The URL for `n` from `crawl.params.url_template` — the built-in mapping,
    /// and the convenience that keeps the bundled script three lines.
    fn templated(&self, n: u32) -> Option<String> {
        let template = self
            .spec
            .params
            .get("url_template")
            .and_then(|v| v.as_str())
            .unwrap_or(&self.spec.url_template);
        if template.trim().is_empty() {
            return None;
        }
        Some(expand_template(template, n))
    }

    /// The path with no script at all: one fetch, then the element the workspace
    /// pointed at — or the generic prose heuristic when it pointed at nothing.
    ///
    /// Deliberately site-agnostic. The only thing it knows about a site is what
    /// `crawl.params.extract` says; when that says nothing it falls back to
    /// [`super::html::readable`], which is a heuristic and labelled as one. A
    /// site whose markup needs more than "this container" wants a script — which
    /// is the whole point of this module.
    ///
    /// A selector that is not valid CSS is a configuration error and fails here,
    /// named, rather than silently falling through to a guess.
    fn builtin(&self, host: &mut Host, n: u32, url: Option<&str>) -> Result<CrawlOutcome> {
        let url = url
            .map(str::to_string)
            .or_else(|| self.templated(n))
            .ok_or_else(|| {
                anyhow!(
                    "no URL for ch{n}: set a url_template, give the crawl script a discover() function, or author the chapter index by hand"
                )
            })?;
        let page = host.fetch(&url, None)?;
        if let Some(blocked) = block_for_status(page.status) {
            return Ok(CrawlOutcome::Blocked(blocked));
        }
        let mut text = String::new();
        for sel in extract_selectors(&self.spec) {
            text = super::html::select_text(&page.body, &sel)?;
            if !text.trim().is_empty() {
                break;
            }
            host.note(format!("extract selector {sel:?} matched nothing"));
        }
        if text.trim().is_empty() {
            let read = super::html::readable(&page.body);
            if !read.title.is_empty() {
                host.note(format!("readable() took {:?} for the title", read.title));
            }
            text = read.text;
        }
        Ok(CrawlOutcome::Text {
            text,
            url: Some(page.url),
        })
    }
}

/// The element(s) `crawl.params.extract` points at, most preferred first.
///
/// The **one** key the host reads out of an otherwise opaque `params`, and it
/// exists for the workspace with no script at all: "the chapter is in this
/// container" should not require writing one. It accepts a bare selector, a list
/// of them, or an object carrying a `selector` of either shape.
///
/// Nothing here is validated or interpreted further — a script is free to ignore
/// the key entirely, which is what the bundled crawlers do.
fn extract_selectors(spec: &CrawlSpec) -> Vec<String> {
    fn strings(value: &serde_json::Value) -> Vec<String> {
        match value {
            serde_json::Value::String(s) => vec![s.clone()],
            serde_json::Value::Array(items) => items.iter().flat_map(strings).collect(),
            serde_json::Value::Object(obj) => obj.get("selector").map(strings).unwrap_or_default(),
            _ => Vec::new(),
        }
    }
    spec.params
        .get("extract")
        .map(strings)
        .unwrap_or_default()
        .into_iter()
        .filter(|s| !s.trim().is_empty())
        .collect()
}

/// The report a crawl's outcome travels home in.
///
/// The script's own `log()` lines ride along in `detail`, because they are the
/// only thing that explains a verdict to an operator: "blocked[empty]" is a
/// diagnosis, and "select \"div.text-left\" matched nothing" is an answer.
pub fn report_of(crawled: &Crawled) -> bm_proto::CrawlReport {
    use bm_proto::CrawlVerdict as V;
    let (verdict, class, mut detail) = match &crawled.outcome {
        CrawlOutcome::Text { .. } => (V::Text, String::new(), String::new()),
        CrawlOutcome::Absent { reason } => (V::Absent, String::new(), reason.clone()),
        CrawlOutcome::Blocked(b) => (V::Blocked, b.class.as_str().to_string(), b.detail.clone()),
    };
    if !crawled.log.is_empty() {
        let log = crawled.log.join("; ");
        detail = if detail.is_empty() {
            log
        } else {
            format!("{detail} — {log}")
        };
    }
    bm_proto::CrawlReport {
        verdict,
        detail,
        class,
        retry_after: match &crawled.outcome {
            CrawlOutcome::Blocked(b) => b.retry_after,
            _ => None,
        },
        fetches: crawled.fetches,
    }
}

/// Classify a non-success status, or `None` when it is a success.
///
/// A 404 on a URL the index claimed exists is terminal: retrying it three
/// times proves nothing. A 429 or a 403 bot check is not — those are the two
/// that come back on their own.
pub fn block_for_status(status: u16) -> Option<super::contract::Blocked> {
    use super::contract::BlockedClass::*;
    let class = match status {
        200..=299 => return None,
        404 | 410 => Gone,
        429 => RateLimit,
        401 | 402 | 407 => LoginRequired,
        403 | 503 => Challenge,
        500..=599 => Unknown,
        _ => Unknown,
    };
    Some(super::contract::Blocked {
        class,
        detail: format!("HTTP {status}"),
        retry_after: if class == RateLimit { Some(30) } else { None },
    })
}

/// The boundary every text passes on its way to `chapter_txt(n)`.
///
/// One function for the script path and the built-in path, and it is the same
/// boundary manual import uses: whatever produced the prose, site metadata is
/// out, entities are decoded, and a body too short to be a chapter is refused
/// **here** rather than three stages downstream.
fn finish(outcome: CrawlOutcome) -> CrawlOutcome {
    match outcome {
        CrawlOutcome::Text { text, url } => {
            let clean = super::sanitize_chapter_text(&text);
            if clean.len() > MAX_CHAPTER_BYTES {
                return CrawlOutcome::Blocked(super::contract::Blocked {
                    class: super::contract::BlockedClass::Unknown,
                    detail: format!(
                        "crawl returned {} bytes for one chapter (cap {}) — the selector is matching the whole page",
                        clean.len(),
                        MAX_CHAPTER_BYTES
                    ),
                    retry_after: None,
                });
            }
            // `Empty` on the length guard takes the ordinary strike ladder
            // rather than shelving at once: a page that comes back short is
            // usually a transient challenge or a half-written upstream, both
            // of which come back on their own. A *terminal* verdict is the
            // script's to make, explicitly, by returning `blocked`.
            if clean.len() < MIN_CHAPTER_BYTES {
                return CrawlOutcome::Blocked(super::contract::Blocked {
                    class: super::contract::BlockedClass::Empty,
                    detail: format!(
                        "text suspiciously short ({} bytes, {} chars) — the selector may have missed",
                        clean.len(),
                        clean.chars().count()
                    ),
                    retry_after: None,
                });
            }
            CrawlOutcome::Text { text: clean, url }
        }
        other => other,
    }
}

/// The engine a spec asks for, or `None` for the built-in path.
fn engine_kind(engine: &str, script: &str) -> Option<super::engine::EngineKind> {
    if !engine.trim().is_empty() {
        return super::engine::EngineKind::parse(engine);
    }
    // An empty engine with a named script means "pick by extension": that is
    // what makes `script = "assets/crawl/site.js"` work without a second field
    // to keep in step.
    if script.trim().is_empty() {
        None
    } else {
        Some(super::engine::EngineKind::from_name(script))
    }
}

/// The spec a workspace's settings describe, with the script's source read in.
///
/// The source travels with the offer rather than being read on the worker, so a
/// box that has not been re-provisioned still runs the crawler the operator
/// edited — and so a worker never needs the profile tree to crawl.
pub fn spec_from_settings(layout: &Layout, s: &Settings) -> CrawlSpec {
    let crawl = &s.crawl;
    let mut spec = CrawlSpec {
        engine: String::new(),
        script: String::new(),
        source: String::new(),
        params: crawl.params.clone(),
        url_template: s.url_template.clone(),
        headers: crawl.headers.clone(),
        user_agent: crawl.user_agent.clone(),
        pace_ms: crawl.pace_ms,
        timeout_secs: crawl.timeout_secs,
        max_seconds: crawl.max_seconds,
        max_fetches: crawl.max_fetches,
    };
    // The template is handed to scripts as a param as well: a script that maps
    // slugs for most chapters may still want the plain form for the rest.
    spec.params
        .entry("url_template".to_string())
        .or_insert_with(|| json!(s.url_template));
    if let Some(path) = resolve_script(layout, &crawl.script) {
        // A script that cannot be read leaves the spec unscripted, which is the
        // built-in path rather than an error: see `builtin`.
        if let Ok(source) = std::fs::read_to_string(&path) {
            spec.engine = super::engine::EngineKind::from_name(&crawl.script)
                .as_str()
                .to_string();
            spec.script = path.display().to_string();
            spec.source = source;
        }
    }
    spec
}

/// Resolve a script path contained in the workspace.
///
/// Containment is deliberate: a crawl script runs with the worker's own
/// privileges, so `crawl.script` naming `/etc/…` or a path outside the root is
/// not a configuration to honour. A relative name is tried against the active
/// workspace first — `crawl/mysite.lua` there is the per-book crawler, and it
/// shadows anything the profile ships — then the root (`assets/crawl/…`, the
/// profile's), then `assets/`. The workspace base is `layout.work` rather than
/// the crawl directory itself so one spelling (`crawl/x.lua`) means the same
/// file on the inductor and on a worker, where provision pushed the dir to
/// `~/bm-worker/crawl`.
///
/// **Last resort: a bundled crawler by bare filename.** Every bundled crawler
/// now lives in `assets/crawl/templates/`, and a `settings.json` written before
/// that move spells it `assets/crawl/storya.lua` — a path that no longer
/// exists. Falling back to the basename keeps those files working instead of
/// failing at the first crawl with "script not found", which is a confusing way
/// to learn about a directory move. An exact hit always wins, so this cannot
/// shadow an operator's own crawler, and a bare name is left alone: it has no
/// directory to have moved out from under it.
pub fn resolve_script(layout: &Layout, name: &str) -> Option<std::path::PathBuf> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let candidate = std::path::Path::new(name);
    if candidate.is_absolute() {
        let inside = candidate.starts_with(&layout.root);
        return (inside && candidate.is_file()).then(|| candidate.to_path_buf());
    }
    for base in [layout.work.clone(), layout.root.clone(), layout.assets()] {
        let p = base.join(candidate);
        if p.is_file() {
            return Some(p);
        }
    }
    if candidate.components().count() < 2 {
        return None;
    }
    let moved = layout
        .assets()
        .join("crawl")
        .join("templates")
        .join(candidate.file_name()?);
    moved.is_file().then_some(moved)
}
