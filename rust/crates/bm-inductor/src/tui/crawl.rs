//! The crawl view (`c`): what this crawl will read, by what, and what is wrong

use bm_core::config::CrawlSettings;
use bm_core::crawl::CrawlIndex;
use bm_core::Layout;

/// Width of the `key` column. Sized by the longest key that can appear, which
pub(crate) const KEY_W: usize = 20;

/// One line of the view, before it is painted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Row {
    Blank,
    Section(String),
    /// A `key   value` line. `warn` paints it as a fault rather than as chrome.
    Field {
        key: String,
        value: String,
        warn: bool,
    },
    Note(String),
}

impl Row {
    fn field(key: &str, value: impl Into<String>) -> Self {
        Row::Field {
            key: key.to_string(),
            value: value.into(),
            warn: false,
        }
    }

    fn warn(key: &str, value: impl Into<String>) -> Self {
        Row::Field {
            key: key.to_string(),
            value: value.into(),
            warn: true,
        }
    }
}

/// What the crawl will do, and anything wrong with it.
pub(crate) fn rows(layout: &Layout, settings: &serde_json::Value) -> Vec<Row> {
    let faults = faults(&detail(layout, settings));
    let mut out = vec![Row::Section("Reading".into()), Row::Blank];
    out.extend(verdict(layout, settings));
    out.push(Row::Blank);
    out.push(Row::Section("Faults".into()));
    out.push(Row::Blank);
    // Nothing wrong is worth saying once, so "no faults" is a fact rather than
    if faults.is_empty() {
        out.push(Row::field("checks", "— none"));
    } else {
        out.extend(faults);
    }
    out
}

/// The full configuration, for when the verdict is not the whole truth.
pub(crate) fn detail(layout: &Layout, settings: &serde_json::Value) -> Vec<Row> {
    // Parsed once and passed down: the section that lists the crawlers has to
    let (crawl, fault) = effective(settings);
    // Loaded once and passed down: the section's *title* asks what kind of
    let index = CrawlIndex::load(layout);
    let title = if index.as_ref().is_some_and(is_book_index) {
        "Chapters"
    } else {
        "Chapter links"
    };
    let mut out = Vec::new();
    for (title, group) in [
        ("In force", settings_rows(layout, &crawl, fault)),
        (title, link_rows(index.as_ref())),
        ("Crawlers", script_rows(layout, &crawl)),
    ] {
        if !out.is_empty() {
            out.push(Row::Blank);
        }
        out.push(Row::Section(title.into()));
        out.push(Row::Blank);
        out.extend(group);
    }
    out
}

/// The three lines: what is read, from what, by what.
fn verdict(layout: &Layout, settings: &serde_json::Value) -> Vec<Row> {
    let (crawl, _) = effective(settings);
    let index = CrawlIndex::load(layout);
    let mut out = Vec::new();
    match index.as_ref() {
        Some(idx) => {
            let (start, count) = idx.range();
            let book = is_book_index(idx);
            let total = match (idx.total, book) {
                (Some(t), true) => format!(" · the book has {t}"),
                (Some(t), false) => format!(" · the site says {t}"),
                (None, _) => String::new(),
            };
            out.push(Row::field(
                "reading",
                format!("{count} chapters from {start}{total}"),
            ));
            let volumes = volumes_of(idx);
            if !volumes.is_empty() {
                out.push(Row::field("volumes", volume_summary(&volumes)));
            }
        }
        // No index yet is not a fault — it is the state before `:crawl` — but it
        None => out.push(Row::field(
            "reading",
            "— no chapter index yet · `:crawl` builds one",
        )),
    }
    // The crawler, **only when it resolves.** When it does not, the fault below
    match bm_core::crawl::provider::resolve_script(layout, &crawl.script) {
        Some(path) => out.push(Row::field(
            "crawler",
            format!(
                "{}  ({})",
                short(&path, layout),
                bm_core::crawl::engine::EngineKind::from_name(&crawl.script).as_str()
            ),
        )),
        None if crawl.script.trim().is_empty() => out.push(Row::field("crawler", "— none set")),
        None => {}
    }
    out
}

/// A library in one line, because the concise view has one line for it.
fn volume_summary(volumes: &[Volume]) -> String {
    // Four, then a count. The breakdown is there to be read, not audited, and a
    const SHOWN: usize = 4;
    let named: Vec<String> = volumes
        .iter()
        .take(SHOWN)
        .map(|v| {
            format!(
                "{} ch {}-{}",
                std::path::Path::new(&v.path)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy(),
                v.first,
                v.last
            )
        })
        .collect();
    let more = volumes.len().saturating_sub(named.len());
    format!(
        "{} · {}{}",
        volumes.len(),
        named.join(" · "),
        if more > 0 {
            format!(" · …{more} more")
        } else {
            String::new()
        }
    )
}

/// The faults, with the sentences that fix them.
fn faults(rows: &[Row]) -> Vec<Row> {
    let mut out = Vec::new();
    let mut under_fault = false;
    for row in rows {
        match row {
            Row::Field { warn: true, .. } => {
                under_fault = true;
                out.push(row.clone());
            }
            // An ordinary field ends the run of notes that belong to it.
            Row::Field { .. } | Row::Section(_) | Row::Blank => under_fault = false,
            Row::Note(_) => {
                if under_fault {
                    out.push(row.clone());
                }
            }
        }
    }
    out
}

/// The `crawl` block as a crawl would see it, and why it had to be guessed when
fn effective(settings: &serde_json::Value) -> (CrawlSettings, Option<String>) {
    match settings.get("crawl") {
        Some(v) => match serde_json::from_value::<CrawlSettings>(v.clone()) {
            Ok(c) => (c, None),
            Err(e) => (CrawlSettings::default(), Some(format!("unreadable — {e}"))),
        },
        None => (
            CrawlSettings::legacy_default(),
            Some("no block in settings.json — read as script mode + bundled Storya".into()),
        ),
    }
}

/// The effective `crawl` block, with defaults resolved and each trap named.
fn settings_rows(layout: &Layout, crawl: &CrawlSettings, fault: Option<String>) -> Vec<Row> {
    let mut out = Vec::new();
    if let Some(f) = fault {
        out.push(Row::warn("crawl", f));
    }
    let crawl = crawl.clone();
    // The two failures worth a red line, in the order they bite.
    let script_named = !crawl.script.trim().is_empty();
    let resolved = bm_core::crawl::provider::resolve_script(layout, &crawl.script);
    let script_mode = crawl.mode == "script";

    if script_mode && !script_named {
        out.push(Row::warn("mode", "script — but no script is set"));
        out.push(Row::Note(
            "      crawls take the built-in fetcher's path instead of failing".into(),
        ));
    } else {
        out.push(Row::field("mode", &crawl.mode));
    }
    if !script_named {
        out.push(Row::field(
            "script",
            "— none: the built-in fetcher reads no selectors",
        ));
    } else {
        match &resolved {
            Some(p) => out.push(Row::field(
                "script",
                format!(
                    "{}  ({})",
                    short(p, layout),
                    bm_core::crawl::engine::EngineKind::from_name(&crawl.script).as_str()
                ),
            )),
            // The one that has bitten: the setting names a crawler, the file is
            None => {
                out.push(Row::warn(
                    "script",
                    format!("{}  — NOT FOUND", crawl.script),
                ));
                out.push(Row::Note(
                    "      not in this book, the root or assets/ — crawls fall back to the \
                     built-in fetcher"
                        .into(),
                ));
            }
        }
    }
    // A relative script resolves against the workspace first, so the view says
    out.push(Row::field(
        "workspace",
        if layout.work == layout.root {
            "the root itself".to_string()
        } else {
            short(&layout.work, layout)
        },
    ));
    out.extend(param_rows(layout, &crawl));
    out.extend(header_rows(&crawl.headers));
    out.push(Row::field("user_agent", ua(&crawl.user_agent)));
    out.push(if crawl.pace_ms == 0 {
        Row::warn("pace_ms", "0 — pacing off")
    } else {
        Row::field(
            "pace_ms",
            format!("{} ms between fetches of one host", crawl.pace_ms),
        )
    });
    if crawl.pace_ms == 0 {
        out.push(Row::Note(
            "      a cluster pointed at one site is what gets a scraper banned".into(),
        ));
    }
    out.push(Row::field("timeout_secs", crawl.timeout_secs.to_string()));
    out.push(Row::field(
        "max_seconds",
        format!("{} s wall clock for one chapter", crawl.max_seconds),
    ));
    out.push(Row::field(
        "max_fetches",
        format!("{} round trips per chapter", crawl.max_fetches),
    ));
    out
}

/// A path as it should be *read*: relative to the root when it is inside it.
fn short(path: &std::path::Path, layout: &Layout) -> String {
    match path.strip_prefix(&layout.root) {
        Ok(p) if p.as_os_str().is_empty() => path.display().to_string(),
        Ok(p) => p.display().to_string(),
        Err(_) => path.display().to_string(),
    }
}

fn ua(user_agent: &str) -> String {
    if user_agent.trim().is_empty() {
        "— built-in default".to_string()
    } else {
        user_agent.to_string()
    }
}

/// `params` one per line — the values are per-book and are meant to be read
fn param_rows(layout: &Layout, crawl: &CrawlSettings) -> Vec<Row> {
    if crawl.params.is_empty() {
        return vec![Row::field("params", "— none")];
    }
    let mut out = vec![Row::field(
        "params",
        format!("{} to the script", crawl.params.len()),
    )];
    for (k, v) in &crawl.params {
        match (k.as_str(), v.as_str()) {
            // A folder of volumes: the shelf, counted and named.
            ("books", Some(named)) => out.extend(books_rows(layout, k, named)),
            // The single-book shape. The keys the example crawler accepts, so a
            (key @ ("epub" | "book" | "path"), Some(named)) => {
                out.extend(one_book_rows(layout, key, named))
            }
            // Every other param — a site URL, a heading pattern, junk patterns —
            _ => out.push(Row::Note(format!("      {k} = {v}"))),
        }
    }
    out
}

/// The `books` param: how many volumes, which, and a fault when it holds none.
fn books_rows(layout: &Layout, key: &str, named: &str) -> Vec<Row> {
    let dir = match bm_core::crawl::epub::confined_dir(&layout.work, named) {
        Err(why) => {
            return vec![
                Row::warn(key, format!("{named}  — NOT FOUND: {why}")),
                Row::Note(
                    "      the crawl resolves this against the workspace and refuses the rest"
                        .into(),
                ),
            ]
        }
        Ok(dir) => dir,
    };
    let books = match bm_core::crawl::epub::books_in(&dir) {
        Err(why) => return vec![Row::warn(key, format!("{named}  — unreadable: {why}"))],
        Ok(books) => books,
    };
    if books.is_empty() {
        return vec![
            Row::warn(key, format!("{named}  — no .epub in it")),
            Row::Note(
                "      name the volumes so file order is reading order: vol-01.epub, vol-02.epub"
                    .into(),
            ),
        ];
    }
    let names: Vec<String> = books
        .iter()
        .map(|b| {
            b.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    vec![
        Row::field(
            key,
            format!(
                "{} volume{} in {named}",
                names.len(),
                if names.len() == 1 { "" } else { "s" }
            ),
        ),
        Row::Note(format!("      {}", names.join(", "))),
    ]
}

/// The single-book param: a file the crawl can actually open, or a fault.
fn one_book_rows(layout: &Layout, key: &str, named: &str) -> Vec<Row> {
    let path = match bm_core::crawl::epub::confined(&layout.work, named) {
        Err(why) => {
            return vec![
                Row::warn(key, format!("{named}  — NOT FOUND: {why}")),
                Row::Note(
                    "      drop the .epub in the workspace; the crawl reads nothing outside it"
                        .into(),
                ),
            ]
        }
        Ok(path) => path,
    };
    // A folder spelled as the single book is a misconfiguration with a one-word
    if path.is_dir() {
        return vec![
            Row::warn(key, format!("{named}  — that is a folder")),
            Row::Note("      name a folder under crawl.params.books, not crawl.params.epub".into()),
        ];
    }
    let mb = path
        .metadata()
        .map(|m| m.len() / (1024 * 1024))
        .unwrap_or(0);
    vec![Row::field(key, format!("{named} ({mb} MB)"))]
}

/// Header **names only**. A crawl header is where a `cf_clearance` cookie or a
fn header_rows(headers: &std::collections::BTreeMap<String, String>) -> Vec<Row> {
    if headers.is_empty() {
        return vec![Row::field("headers", "— none")];
    }
    let names: Vec<&str> = headers.keys().map(String::as_str).collect();
    vec![Row::field("headers", names.join(", "))]
}

/// This book's frozen `n -> url` mapping — the "link" half of the question, and
fn link_rows(index: Option<&CrawlIndex>) -> Vec<Row> {
    let Some(idx) = index else {
        return vec![
            Row::field("index", "— none yet"),
            Row::Note("      `:crawl` builds one from a `{n}` template or a discover()".into()),
        ];
    };
    let (start, count) = idx.range();
    let volumes = volumes_of(idx);
    let book = is_book_index(idx);
    let mut out = vec![
        Row::field("index", format!("{} (data/crawl-index.json)", idx.source)),
        Row::field(
            "chapters",
            match (idx.total, book) {
                (Some(t), true) => format!("{count} from chapter {start} · the book has {t}"),
                (Some(t), false) => format!("{count} from chapter {start} · site says {t}"),
                (None, true) => format!(
                    "{count} from chapter {start} · the book reported no total, so the run \
                     ends where the library does"
                ),
                (None, false) => format!(
                    "{count} from chapter {start} · the site reported no total, so the run \
                     ends where the listing does"
                ),
            },
        ),
    ];
    if idx.is_hand() {
        out.push(Row::Note(
            "      hand-written — never rebuilt behind your back".into(),
        ));
    }
    // The volume breakdown, which is the whole of what a library adds over one
    if !volumes.is_empty() {
        let total: usize = volumes.iter().map(|v| v.chapters).sum();
        out.push(Row::field(
            "volumes",
            format!(
                "{} volume{} · {total} chapters",
                volumes.len(),
                if volumes.len() == 1 { "" } else { "s" }
            ),
        ));
        for (i, v) in volumes.iter().take(8).enumerate() {
            out.push(Row::Note(format!(
                "      vol {}  {} · ch {}-{}",
                i + 1,
                v.path,
                v.first,
                v.last
            )));
        }
        if volumes.len() > 8 {
            out.push(Row::Note(format!("      … and {} more", volumes.len() - 8)));
        }
    }
    // Two links, not the whole book: enough to see the site's shape without
    for n in [start, start.saturating_add(1)] {
        let Some(url) = idx.url(n) else { continue };
        let shown = match locator(url) {
            Some((path, from, to)) => match volumes.iter().position(|v| v.path == path) {
                Some(i) => format!("volume {} · spine {from}-{to}", i + 1),
                None => format!("{path} · spine {from}-{to}"),
            },
            None => url.to_string(),
        };
        out.push(Row::Note(format!("      ch {n}  {shown}")));
    }
    let absent = idx.chapters().values().filter(|c| c.absent).count();
    if absent > 0 {
        out.push(Row::field(
            "absent",
            if book {
                format!("{absent} chapters the book does not have")
            } else {
                format!("{absent} chapters the site does not have")
            },
        ));
    }
    out
}

/// An `epub:` locator as `(volume path, first spine entry, last)`.
fn locator(url: &str) -> Option<(&str, u32, u32)> {
    let (path, range) = url.strip_prefix("epub:")?.rsplit_once('#')?;
    let (from, to) = range.split_once('-')?;
    Some((path, from.parse().ok()?, to.parse().ok()?))
}

/// One volume of a library, as the index recorded it.
struct Volume {
    path: String,
    /// The chapter number this volume starts at — the whole point of numbering
    first: u32,
    last: u32,
    chapters: usize,
}

/// The library's volumes, in reading order.
fn volumes_of(idx: &CrawlIndex) -> Vec<Volume> {
    let mut out: Vec<Volume> = Vec::new();
    for (&n, chapter) in idx.chapters() {
        let Some(url) = chapter.url.as_deref() else {
            continue;
        };
        let Some((path, _, _)) = locator(url) else {
            continue;
        };
        match out.iter_mut().find(|v| v.path == path) {
            Some(v) => {
                v.first = v.first.min(n);
                v.last = n;
                v.chapters += 1;
            }
            None => out.push(Volume {
                path: path.to_string(),
                first: n,
                last: n,
                chapters: 1,
            }),
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Whether a crawler reading local files built this index.
fn is_book_index(idx: &CrawlIndex) -> bool {
    idx.chapters()
        .values()
        .filter_map(|c| c.url.as_deref())
        .any(|u| u.starts_with("epub:"))
}

/// The crawlers on disk, with the one in force marked. Three directories, listed
fn script_rows(layout: &Layout, crawl: &CrawlSettings) -> Vec<Row> {
    let in_force = crawl.script.trim().to_string();
    let mut out = Vec::new();
    for (title, dir) in [
        ("this book", layout.crawl_workspace()),
        ("known", layout.crawlers_dir().join("known")),
        ("examples", layout.crawlers_dir().join("examples")),
    ] {
        let mut files = scripts_in(&dir);
        files.sort();
        if files.is_empty() {
            // The global `known/` tree missing is not the same as a book with no
            if title == "known" && !reads_local_books(crawl) {
                out.push(Row::warn(
                    title,
                    format!("— none in {}", short(&dir, layout)),
                ));
                out.push(Row::Note(
                    "      so `crawl`.`script` resolves to nothing: crawls fall back to the \
                     built-in fetcher"
                        .into(),
                ));
                out.push(Row::Note(
                    "      they are tracked in the repo — `crawlers/known/`; a missing tree \
                     means a bad checkout, not a fetch"
                        .into(),
                ));
            } else {
                out.push(Row::field(
                    title,
                    format!("— none in {}", short(&dir, layout)),
                ));
            }
            continue;
        }
        // The names go on a note line rather than in the value: six filenames
        let names: Vec<String> = files
            .iter()
            .map(|p| {
                let name = p
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                // The in-force crawler is named by a path that may not be this
                let live = !in_force.is_empty()
                    && (p.display().to_string().ends_with(&in_force) || in_force.ends_with(&name));
                if live {
                    format!("{name}  ← in force")
                } else {
                    name
                }
            })
            .collect();
        out.push(Row::field(
            title,
            format!("{} in {}", names.len(), short(&dir, layout)),
        ));
        out.push(Row::Note(format!("      {}", names.join("  "))));
    }
    out
}

/// Whether this workspace's crawler reads files on this machine rather than a
fn reads_local_books(crawl: &CrawlSettings) -> bool {
    crawl
        .params
        .keys()
        .any(|k| matches!(k.as_str(), "epub" | "books"))
}

fn scripts_in(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<std::path::PathBuf> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e == "lua" || e == "js" || e == "mjs")
        })
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests;
