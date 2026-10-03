//! The two script engines, behind one entry-point shape.

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
    pub fn run(&self, entry: Entry, host: SharedHost, input: &Value) -> Result<Option<Value>> {
        match self.kind {
            EngineKind::Lua => lua::run(self, entry, host, input),
            EngineKind::Js => js::run(self, entry, host, input),
        }
    }
}

/// The host functions' semantics — one implementation, two bindings.
pub mod fns {
    use super::*;

    /// `fetch(url, opts?)`. Never errors on an HTTP status: the status *is* the
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
            "headers": page.headers,
        }))
    }

    /// `challenge(page)` — a string naming the Cloudflare interstitial this
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
    pub fn strip_tags(html: &str) -> Value {
        Value::String(super::super::strip_tags_raw(html))
    }

    pub fn decode_entities(s: &str) -> Value {
        Value::String(super::super::decode_entities(s))
    }

    /// The shared chapter boundary: site metadata out, entities decoded. The
    pub fn sanitize(text: &str) -> Value {
        Value::String(super::super::sanitize_chapter_text(text))
    }

    pub fn abs_url(base: &str, href: &str) -> Value {
        Value::String(html::abs_url(base, href))
    }

    /// The built-in mapping, exposed so a script never re-implements it.
    pub fn chapter_url(template: &str, n: u32) -> Value {
        Value::String(super::super::expand_template(template, n))
    }

    pub fn log(host: &mut Host, msg: &str) {
        host.note(msg.to_string());
    }

    /// `epub_chapter(path, n)` — chapter `n` of a local `.epub`, or `nil`.
    pub fn epub_chapter(host: &mut Host, path: &str, n: u32) -> Result<Option<Value>> {
        host.check_budget()?;
        let real = super::super::epub::confined(host.read_root()?, path)?;
        let mut book = super::super::epub::open(&real)?;
        Ok(book.chapter(n)?.map(|c| {
            json!({
                "n": c.n,
                "text": c.text,
                "title": c.title,
                "chapters": book_chapters(&mut book),
            })
        }))
    }

    /// `epub_total(path)` — how many chapters the spine declares, or `nil`.
    pub fn epub_total(host: &mut Host, path: &str) -> Result<Option<u32>> {
        host.check_budget()?;
        let real = super::super::epub::confined(host.read_root()?, path)?;
        Ok(Some(super::super::epub::open(&real)?.chapters() as u32))
    }

    /// `epub_index(path)` — every spine entry as `{n, title, chars, head}`.
    pub fn epub_index(host: &mut Host, path: &str) -> Result<Option<Value>> {
        host.check_budget()?;
        let real = super::super::epub::confined(host.read_root()?, path)?;
        let mut book = super::super::epub::open(&real)?;
        let items = book.index()?;
        Ok(Some(Value::Array(
            items
                .into_iter()
                .map(|i| json!({"n": i.n, "title": i.title, "chars": i.chars, "head": i.head}))
                .collect(),
        )))
    }

    /// `epub_text(path, from, to)` — spine entries `from..=to` as one chapter.
    pub fn epub_text(host: &mut Host, path: &str, from: u32, to: u32) -> Result<String> {
        host.check_budget()?;
        let real = super::super::epub::confined(host.read_root()?, path)?;
        super::super::epub::open(&real)?.text(from, to)
    }

    /// `epub_books(dir)` — every `.epub` in a workspace directory, as paths
    pub fn epub_books(host: &mut Host, dir: &str) -> Result<Value> {
        host.check_budget()?;
        let root = host.read_root()?.to_path_buf();
        let real = super::super::epub::confined_dir(&root, dir)?;
        let base = root.canonicalize().unwrap_or_else(|_| root.clone());
        let mut out: Vec<Value> = Vec::new();
        for path in super::super::epub::books_in(&real)? {
            // The path the *script* spells back into `epub_index`/`epub_text`:
            let rel = path
                .canonicalize()
                .ok()
                .and_then(|p| p.strip_prefix(&base).ok().map(|p| p.to_path_buf()))
                .unwrap_or_else(|| path.clone());
            out.push(Value::String(rel.display().to_string()));
        }
        Ok(Value::Array(out))
    }

    fn book_chapters(book: &mut super::super::epub::Epub) -> u32 {
        book.chapters() as u32
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
