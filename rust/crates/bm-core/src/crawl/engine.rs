//! The two script engines, behind one entry-point shape.
//!
//! An operator writes a crawler in Lua or JavaScript — the extension picks the
//! engine, exactly as it would for any other tool — and the contract is the
//! same in both: a function named `crawl` (required) and optionally one named
//! `discover`. Neither engine is reachable from the other's scripts, and
//! neither can see the environment, the filesystem or a shell:
//!
//! ```text
//! fetch(url, opts?)        -> { status, body, url }     the only way out
//! select(html, sel)        -> string                    first match, one line
//! select_all(html, sel)    -> [ { text, html, attrs } ]
//! select_text(html, sel)   -> string                    first match, as prose
//! strip_tags(html)         -> string
//! decode_entities(s)       -> string
//! sanitize(text)           -> string                    the chapter boundary
//! readable(html)           -> { title, text }           generic prose heuristic
//! abs_url(base, href)      -> string
//! chapter_url(template, n) -> string                    the host's {n}/{n:03}
//! log(msg)                 -> a line in the ledger
//! ```
//!
//! The semantics of every one of those live in [`fns`] as plain Rust; the two
//! engine modules are thin bindings, so Lua and JavaScript cannot drift apart
//! in what `select` means.
//!
//! **Primitives only, on purpose.** There is no `clean_storya` — no host
//! function that knows what a chapter of a particular site looks like. Which
//! element holds the prose, where the body starts and which lines are the
//! site's chrome are facts about a *website*, and they belong in the script the
//! operator can read and rewrite, next to the page they describe. Rust's job
//! ends at "run `crawl`, take the text": the one thing the host insists on is
//! the shared boundary ([`fns::sanitize`]) and the length guard, so a chapter is
//! the same shape however it was obtained.
//!
//! **Trust.** These run in-process, so a script is as privileged as the worker
//! that runs it apart from the ABI above — which is why a crawler script is
//! something the *profile* ships and the operator authors, not something a
//! workspace downloads from a stranger. The budget in [`super::host::Limits`]
//! bounds a runaway loop; it is not a security boundary.

use anyhow::Result;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::rc::Rc;

use super::contract::Discovered;
use super::host::{FetchOptions, Host, Page};
use super::html;

pub mod js;
pub mod lua;

/// Which interpreter runs a script. The file extension decides, unless the
/// workspace names the engine outright.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineKind {
    Lua,
    Js,
}

impl EngineKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EngineKind::Lua => "lua",
            EngineKind::Js => "js",
        }
    }

    pub fn parse(s: &str) -> Option<EngineKind> {
        match s.trim().to_ascii_lowercase().as_str() {
            "lua" | "lua5.4" | "lua54" => Some(EngineKind::Lua),
            "js" | "javascript" | "ecmascript" | "mjs" => Some(EngineKind::Js),
            _ => None,
        }
    }

    /// The engine a script path implies: `.js`/`.mjs` is JavaScript,
    /// everything else Lua. Lua is the default because that is what the
    /// bundled crawlers are written in.
    pub fn from_name(name: &str) -> EngineKind {
        match name
            .rsplit('.')
            .next()
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("js" | "mjs" | "javascript") => EngineKind::Js,
            _ => EngineKind::Lua,
        }
    }
}

/// The entry points a script may define.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    /// Required. `n -> prose`.
    Crawl,
    /// Optional. `range -> chapter list`. Its absence is the signal that this
    /// script is a single-page crawler and the host's own template supplies the
    /// URL.
    Discover,
}

impl Entry {
    pub fn name(self) -> &'static str {
        match self {
            Entry::Crawl => "crawl",
            Entry::Discover => "discover",
        }
    }
}

/// A loaded, not-yet-run crawler.
#[derive(Debug, Clone)]
pub struct Program {
    pub kind: EngineKind,
    /// The script path, for every error message and log line.
    pub name: String,
    pub source: String,
}

/// One host, shared by every host function the engine binds into a script.
///
/// `Rc<RefCell<..>>` because both interpreters' host functions must be `'static`
/// and neither is `Send` — and neither needs to be: a script runs on one
/// blocking thread for one chapter (see [`super::provider`]).
pub type SharedHost = Rc<RefCell<Host>>;

impl Program {
    pub fn new(kind: EngineKind, name: impl Into<String>, source: impl Into<String>) -> Program {
        Program {
            kind,
            name: name.into(),
            source: source.into(),
        }
    }

    /// Run one entry point with `input` as the request object.
    ///
    /// `Ok(None)` means the script does not define that function — for
    /// `discover` that is the normal case, not a failure.
    pub fn run(&self, entry: Entry, host: SharedHost, input: &Value) -> Result<Option<Value>> {
        match self.kind {
            EngineKind::Lua => lua::run(self, entry, host, input),
            EngineKind::Js => js::run(self, entry, host, input),
        }
    }
}

/// The host functions' semantics — one implementation, two bindings.
///
/// Every function takes the host so it can charge the budget: a script that
/// loops over a pure function without ever fetching must still die at the
/// deadline, and the engine hooks that also do this are a backstop rather than
/// the only guard.
pub mod fns {
    use super::*;

    /// `fetch(url, opts?)`. Never errors on an HTTP status: the status *is* the
    /// return value, because 429 and 403 are what a script classifies.
    pub fn fetch(host: &mut Host, url: &str, opts: Value) -> Result<Value> {
        host.check_budget()?;
        let opts: FetchOptions = match opts {
            Value::Null => FetchOptions::default(),
            Value::Object(_) => serde_json::from_value(opts)?,
            other => anyhow::bail!("fetch: options must be an object or nil, got {other}"),
        };
        let page: Page = host.fetch(url, Some(opts))?;
        Ok(json!({
            "status": page.status,
            "body": page.body,
            "url": page.url,
            // Headers reach the script, lowercased. A status alone cannot tell a
            // bot check from a dead link, and a crawler that has to guess is a
            // crawler that retries the wrong thing three times.
            "headers": page.headers,
        }))
    }

    /// `challenge(page)` — a string naming the Cloudflare interstitial this
    /// response is, or `nil` when it is a page.
    ///
    /// The one piece of site-shaped knowledge in the ABI, and it earns its place
    /// by being a *refusal* rather than an extraction: a status check cannot
    /// see a challenge served as `200 OK`, so a crawler that only reads the
    /// status stores the interstitial as the chapter. Detection lives here so
    /// every script gets it — see [`crate::crawl::probe::interstitial`].
    ///
    /// **`Option`, not `Value::Null`, on purpose.** mlua serialises a JSON null
    /// to a `NULL` *userdata*, which is **truthy** in Lua — so a script written
    /// the obvious way, `if challenge(r) then return blocked end`, refuses every
    /// page it is ever handed. The answer has to arrive as a real `nil`.
    ///
    pub fn challenge(host: &mut Host, page: Value) -> Result<Option<String>> {
        host.check_budget()?;
        let page: Page = serde_json::from_value(page)
            .map_err(|e| anyhow::anyhow!("challenge(): the argument is not a fetch result: {e}"))?;
        Ok(crate::crawl::probe::interstitial(&page))
    }

    /// The text of the first CSS match, squeezed. Empty when there is no match.
    pub fn select(host: &mut Host, html: &str, sel: &str) -> Result<Value> {
        host.check_budget()?;
        let text = html::select(html, sel)?;
        if text.is_empty() {
            host.note(format!("select {sel:?} matched nothing"));
        }
        Ok(Value::String(text))
    }

    /// Every CSS match: text, markup and attributes, in document order.
    pub fn select_all(host: &mut Host, html: &str, sel: &str) -> Result<Value> {
        host.check_budget()?;
        let els = html::select_all(html, sel)?;
        if els.is_empty() {
            host.note(format!("select_all {sel:?} matched nothing"));
        }
        Ok(serde_json::to_value(els)?)
    }

    /// The block text of the first CSS match: paragraphs kept apart by a blank
    /// line, site metadata out.
    ///
    /// `select` answers "what does this element say" in one line, which is what
    /// a headline or a `next` link wants and exactly what a chapter does *not*.
    /// This is the other half: point at the container, get prose.
    pub fn select_text(host: &mut Host, html: &str, sel: &str) -> Result<Value> {
        host.check_budget()?;
        let text = html::select_text(html, sel)?;
        if text.is_empty() {
            host.note(format!("select_text {sel:?} matched nothing"));
        }
        Ok(Value::String(text))
    }

    /// The generic prose heuristic, for a site nobody has read yet.
    pub fn readable(host: &mut Host, html: &str) -> Result<Value> {
        host.check_budget()?;
        Ok(serde_json::to_value(html::readable(html))?)
    }

    /// Tags out, block boundaries as line breaks. Script and style bodies go
    /// with them — the same stripper every entry point uses.
    pub fn strip_tags(html: &str) -> Value {
        Value::String(super::super::strip_tags_raw(html))
    }

    pub fn decode_entities(s: &str) -> Value {
        Value::String(super::super::decode_entities(s))
    }

    /// The shared chapter boundary: site metadata out, entities decoded. The
    /// provider runs this over whatever the script returns, so calling it
    /// inside a script is for text that needs to be measured or compared
    /// *before* it is returned.
    pub fn sanitize(text: &str) -> Value {
        Value::String(super::super::sanitize_chapter_text(text))
    }

    pub fn abs_url(base: &str, href: &str) -> Value {
        Value::String(html::abs_url(base, href))
    }

    /// The built-in mapping, exposed so a script never re-implements it.
    ///
    /// `{n}` and `{n:03}` are the host's, one implementation, and a script that
    /// wants the plain form for the chapters its `discover` did not map calls
    /// this instead of growing its own string formatting that would then need
    /// its own padding rules.
    pub fn chapter_url(template: &str, n: u32) -> Value {
        Value::String(super::super::expand_template(template, n))
    }

    pub fn log(host: &mut Host, msg: &str) {
        host.note(msg.to_string());
    }
}

/// Read a `discover()` result, tolerating the shapes a script naturally writes.
pub fn discovered_from(value: Value) -> Result<Discovered> {
    match value {
        Value::Null => Ok(Discovered::default()),
        Value::Array(items) => Ok(Discovered {
            chapters: items
                .into_iter()
                .map(discovered_chapter)
                .collect::<Result<Vec<_>>>()?,
            total: None,
        }),
        Value::Object(_) => Ok(serde_json::from_value(value)?),
        other => anyhow::bail!("discover returned {other}, expected a table/object"),
    }
}

fn discovered_chapter(v: Value) -> Result<super::contract::DiscoveredChapter> {
    match v {
        // `{ 34, "https://…" }` — the array form is what a Lua listing walk
        // naturally pushes, and refusing it would be pedantry.
        Value::Array(items) => {
            let mut it = items.into_iter();
            let n = it
                .next()
                .and_then(|v| v.as_u64())
                .ok_or_else(|| anyhow::anyhow!("discover: chapter entry needs a number"))?;
            let url = it.next().and_then(|v| match v {
                Value::String(s) => Some(s),
                _ => None,
            });
            Ok(super::contract::DiscoveredChapter {
                n: n as u32,
                url,
                title: String::new(),
                absent: false,
            })
        }
        Value::Object(_) => Ok(serde_json::from_value(v)?),
        Value::Number(n) => Ok(super::contract::DiscoveredChapter {
            n: n.as_u64().unwrap_or(0) as u32,
            ..Default::default()
        }),
        other => anyhow::bail!("discover: chapter entry is {other}"),
    }
}

/// Shared helper: what a script's `crawl` returning nothing means.
pub fn empty_result_error(entry: &str, name: &str) -> anyhow::Error {
    anyhow::anyhow!("{name}: {entry} returned nothing — return {{ text = … }}")
}
