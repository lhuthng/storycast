//! The crawl view (`c`): what is actually in force, what this book's links
//! are, which crawlers are on this machine, and which sites are known.
//!
//! **Why a screen for settings at all.** A misconfigured crawler does not fail
//! loudly. A `crawl.script` that resolves to nothing leaves the crawl spec
//! unscripted, which is the built-in fetcher's path rather than an error (see
//! `crawl::provider`), so the run proceeds, the page comes back, and the
//! selectors simply do not match. Until this screen existed the only way to see
//! any of it was `bm-inductor check <url>` in another terminal — which needs a
//! URL you may not have — or reading `settings.json` by hand and hoping.
//!
//! So the warnings here are the point, not decoration: each one names a
//! configuration that *looks* right and behaves otherwise.
//!
//! Everything is pure. [`rows`] takes a layout and the settings value the
//! dashboard already holds and answers with rows, so what the screen says is
//! testable without a terminal — the same bargain `tui/sound.rs` keeps.

use bm_core::config::CrawlSettings;
use bm_core::crawl::CrawlIndex;
use bm_core::Layout;

/// Width of the `key` column. Sized by the longest key that can appear, which
/// is a **site host** (`readnovelfull.com`) rather than a field name — a fixed
/// 12 ran `timeout_secs` straight into its value and `storya.click` into
/// `crawler`, which reads as one word.
pub(crate) const KEY_W: usize = 20;

/// One line of the view, before it is painted.
///
/// The variants exist so the painter does not have to guess: a [`Row::Field`]
/// is a `key  value` pair, and only `warn` decides its colour. Prose that is
/// not a field is [`Row::Note`], so nothing that must stand out gets lost in a
/// paragraph.
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

/// Everything the view says, in the order a person asks for it.
///
/// A blank line after every title, and one between the sections: this is a
/// screen of short facts read by scanning, and a wall of `key value` pairs is
/// harder to scan than the same facts in four named groups.
pub(crate) fn rows(layout: &Layout, settings: &serde_json::Value) -> Vec<Row> {
    // Parsed once and passed down: the section that lists the crawlers has to
    // mark the one **in force**, and the in-force one is the *resolved* setting
    // — a workspace with no `crawl` block still has one, by `legacy_default`.
    let (crawl, fault) = effective(settings);
    let mut out = Vec::new();
    for (title, group) in [
        ("In force", settings_rows(layout, &crawl, fault)),
        ("Chapter links", link_rows(layout)),
        ("Crawlers", script_rows(layout, &crawl)),
        ("Known sites", known_rows()),
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

/// The `crawl` block as a crawl would see it, and why it had to be guessed when
/// it had to be guessed.
///
/// A missing `crawl` key is *not* an empty one: a settings file written before
/// the block existed deserializes into `legacy_default()` — script mode with the
/// bundled Storya crawler — so that is what this workspace would crawl with, and
/// printing `manual` instead would say the opposite of the truth. An inductor
/// running hands the dashboard its *migrated* settings, where the block is
/// present, so the guess is the cold-start and no-inductor case.
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
///
/// A missing `crawl` key is *not* the same as an empty one: `CrawlSettings`
/// deserializes absent fields to its own defaults, and a settings file written
/// before the block existed is migrated by the inductor into `legacy_default()`
/// (script mode, the bundled Storya crawler). The dashboard reads the live
/// settings when the inductor is up, so what is printed here is what a crawl
/// would actually use — not what the file on disk says.
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
            // not there, and the crawl still runs — against nothing.
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
    // which workspace that is: on a worker the same spelling means
    // `~/bm-worker/crawl`, and that is not a detail.
    out.push(Row::field(
        "workspace",
        if layout.work == layout.root {
            "the root itself".to_string()
        } else {
            short(&layout.work, layout)
        },
    ));
    out.extend(param_rows(&crawl));
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
///
/// An absolute path in a 94-column dialog is a path whose tail — the part that
/// says which file — is the part that falls off the right-hand edge. The root
/// itself is named once, in the section, so everything under it is relative.
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
/// (`book` is a URL), unlike headers below.
fn param_rows(crawl: &CrawlSettings) -> Vec<Row> {
    if crawl.params.is_empty() {
        return vec![Row::field("params", "— none")];
    }
    let mut out = vec![Row::field(
        "params",
        format!("{} to the script", crawl.params.len()),
    )];
    for (k, v) in &crawl.params {
        out.push(Row::Note(format!("      {k} = {v}")));
    }
    out
}

/// Header **names only**. A crawl header is where a `cf_clearance` cookie or a
/// session token lives, and this view is a thing people screen-share. The
/// values are in `settings.json` under the same names.
fn header_rows(headers: &std::collections::BTreeMap<String, String>) -> Vec<Row> {
    if headers.is_empty() {
        return vec![Row::field("headers", "— none")];
    }
    let names: Vec<&str> = headers.keys().map(String::as_str).collect();
    vec![Row::field("headers", names.join(", "))]
}

/// This book's frozen `n -> url` mapping — the "link" half of the question, and
/// the only place the stored range and total are visible.
fn link_rows(layout: &Layout) -> Vec<Row> {
    let Some(idx) = CrawlIndex::load(layout) else {
        return vec![
            Row::field("index", "— none yet"),
            Row::Note("      `:crawl` builds one from a `{n}` template or a discover()".into()),
        ];
    };
    let (start, count) = idx.range();
    let mut out = vec![
        Row::field("index", format!("{} (data/crawl-index.json)", idx.source)),
        Row::field(
            "chapters",
            match idx.total {
                Some(t) => format!("{count} from chapter {start} · site says {t}"),
                None => format!(
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
    // Two links, not the whole book: enough to see the site's shape without
    // turning the view into a dump.
    for n in [start, start.saturating_add(1)] {
        if let Some(url) = idx.url(n) {
            out.push(Row::Note(format!("      ch {n}  {url}")));
        }
    }
    let absent = idx.chapters().values().filter(|c| c.absent).count();
    if absent > 0 {
        out.push(Row::field(
            "absent",
            format!("{absent} chapters the site does not have"),
        ));
    }
    out
}

/// The crawlers on disk, with the one in force marked. Two directories, listed
/// separately because they behave differently: the workspace's own are this
/// book's and shadow the profile's, and `:profile load` cannot reach them.
fn script_rows(layout: &Layout, crawl: &CrawlSettings) -> Vec<Row> {
    let in_force = crawl.script.trim().to_string();
    let mut out = Vec::new();
    for (title, dir) in [
        ("this book", layout.crawl_workspace()),
        ("bundled", layout.crawl_scripts().join("templates")),
    ] {
        let mut files = scripts_in(&dir);
        files.sort();
        if files.is_empty() {
            // The bundled tree missing is not the same as a book with no crawler
            // of its own: it means `DEFAULT_SCRIPT` resolves to nothing and
            // every crawl of this workspace is quietly running without
            // selectors. Say which one it is, and how to fix it.
            if title == "bundled" {
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
                    "      they come with a profile — `tools/profile.sh fetch <name>`, or \
                     `git pull` if this checkout should already have them"
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
        // do not fit a 72-column value, and a clipped list is a wrong list.
        let names: Vec<String> = files
            .iter()
            .map(|p| {
                let name = p
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                // The in-force crawler is named by a path that may not be this
                // file's path (a pre-move spelling, a bare name), so the match
                // is on the tail.
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

/// The registry [`bm_core::crawl::known_sites`] holds — the list `:crawl` and
/// `check` both read.
///
/// **One line per site, and no prose.** The question this section answers is
/// "which sites can this thing crawl, and which are refused" — a list, and a
/// list is ruined by paragraphs. The long form (the URL shape, the full caveat,
/// the language warning) is what `bm-inductor check <url>` and the note under
/// the `:crawl` prompt are for; this is the index, and an index that explains
/// itself is an index nobody can scan.
fn known_rows() -> Vec<Row> {
    let sites = bm_core::crawl::known_sites();
    let mut out = Vec::new();
    for site in sites {
        out.push(if site.is_crawlable() {
            Row::field(
                site.host,
                // The language is a word here. Where it carries a clause on top
                // ("English — but `/vi/` is the Vietnamese edition"), that
                // clause is the caveat's business, not this row's.
                format!(
                    "{}  ·  {}",
                    file_of(site.script),
                    site.language.split(" — ").next().unwrap_or(site.language)
                ),
            )
        } else {
            // A refused entry is worth more than a missing one: it saves
            // reading a 403 as a puzzle.
            Row::warn(site.host, "refused")
        });
        // The cause, on its own line, only where there is one. A caveat is
        // written for `check` and the `:crawl` prompt, so it is cut to its
        // first sentence here rather than trimmed by hand in two places.
        if let Some(c) = site.caveat {
            out.push(Row::Note(format!("      {}", first_sentence(c))));
        }
    }
    out
}

/// The last path segment: a crawler is named by its file, and the directory it
/// lives in is already the row it is listed under.
fn file_of(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// The first sentence of a caveat, capped — a cause in one line, not a page.
/// A caveat with no sentence break inside the cap is cut where the cap falls.
fn first_sentence(text: &str) -> String {
    const CAP: usize = 72;
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let cut = match flat.find(". ").filter(|i| *i <= CAP) {
        Some(i) => &flat[..i + 1],
        None if flat.chars().count() <= CAP => &flat[..],
        None => {
            let head: String = flat.chars().take(CAP - 1).collect();
            return format!("{head}…");
        }
    };
    cut.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn layout_with_settings(crawl: serde_json::Value) -> (tempfile::TempDir, Layout) {
        let dir = tempfile::tempdir().expect("tmp");
        let layout = Layout::new(dir.path());
        let s = json!({ "crawl": crawl });
        let path = layout.settings();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_string_pretty(&s).unwrap()).unwrap();
        (dir, layout)
    }

    fn value_of(rows: &[Row], key: &str) -> String {
        rows.iter()
            .find_map(|r| match r {
                Row::Field { key: k, value, .. } if k == key => Some(value.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no field {key} in {rows:#?}"))
    }

    /// Everything the view says, flattened — for the assertions that care about
    /// an explanation rather than about which line it landed on.
    fn said(rows: &[Row]) -> String {
        rows.iter()
            .map(|r| match r {
                Row::Field { key, value, .. } => format!("{key}: {value}"),
                Row::Note(n) => n.clone(),
                Row::Section(s) => s.clone(),
                Row::Blank => String::new(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn is_warn(rows: &[Row], key: &str) -> bool {
        rows.iter()
            .any(|r| matches!(r, Row::Field { key: k, warn: true, .. } if k == key))
    }

    #[test]
    fn the_view_answers_what_is_in_force_with_defaults_resolved() {
        let (_tmp, layout) = layout_with_settings(json!({ "mode": "manual" }));
        let settings = bm_core::read_json::<serde_json::Value>(&layout.settings()).unwrap();
        let rows = rows(&layout, &settings);
        // `pace_ms`, `timeout_secs` and the rest are absent from the file and
        // still answered: this is the *effective* block, not the file's echo.
        assert_eq!(value_of(&rows, "mode"), "manual");
        assert!(value_of(&rows, "pace_ms").contains("750 ms"));
        assert!(value_of(&rows, "max_fetches").contains("64 round trips"));
        assert!(value_of(&rows, "user_agent").contains("built-in default"));
    }

    #[test]
    fn a_script_that_is_not_there_is_the_line_that_matters() {
        // The silent-failure case: the setting names a crawler, the file does
        // not exist, and the crawl still runs — against the built-in fetcher.
        let (_tmp, layout) = layout_with_settings(json!({
            "mode": "script",
            "script": "crawl/nosuchsite.lua",
        }));
        let settings = bm_core::read_json::<serde_json::Value>(&layout.settings()).unwrap();
        let rows = rows(&layout, &settings);
        assert!(
            is_warn(&rows, "script"),
            "the miss must be a fault, not a value"
        );
        let said = said(&rows);
        assert!(value_of(&rows, "script").contains("NOT FOUND"), "{said}");
        assert!(said.contains("built-in fetcher"), "{said}");
    }

    #[test]
    fn script_mode_with_no_script_says_itself_rather_than_looking_configured() {
        let (_tmp, layout) = layout_with_settings(json!({ "mode": "script", "script": "" }));
        let settings = bm_core::read_json::<serde_json::Value>(&layout.settings()).unwrap();
        let rows = rows(&layout, &settings);
        assert!(is_warn(&rows, "mode"));
        assert!(said(&rows).contains("built-in fetcher"));
    }

    #[test]
    fn a_resolved_crawler_is_named_with_its_engine() {
        let (_tmp, layout) = layout_with_settings(json!({}));
        std::fs::create_dir_all(layout.crawl_workspace()).unwrap();
        std::fs::write(layout.crawl_workspace().join("truyencom.lua"), "-- x").unwrap();
        let settings = bm_core::read_json::<serde_json::Value>(&layout.settings()).unwrap();
        let mut s = settings.clone();
        s["crawl"]["mode"] = json!("script");
        s["crawl"]["script"] = json!("crawl/truyencom.lua");
        let rows = rows(&layout, &s);
        let v = value_of(&rows, "script");
        assert!(v.contains("truyencom.lua") && v.contains("lua"), "{v}");
        assert!(!is_warn(&rows, "script"));
        // And the same file is listed as the one in force.
        assert!(
            said(&rows).contains("in force"),
            "the in-force crawler must be marked: {rows:#?}"
        );
    }

    #[test]
    fn headers_are_named_and_never_valued() {
        let (_tmp, layout) = layout_with_settings(json!({
            "headers": { "Cookie": "cf_clearance=SECRET", "Referer": "https://x/" }
        }));
        let settings = bm_core::read_json::<serde_json::Value>(&layout.settings()).unwrap();
        let rows = rows(&layout, &settings);
        let all: Vec<String> = rows
            .iter()
            .map(|r| match r {
                Row::Field { value, .. } => value.clone(),
                Row::Note(n) => n.clone(),
                _ => String::new(),
            })
            .collect();
        let joined = all.join("\n");
        assert!(joined.contains("Cookie"), "{joined}");
        assert!(
            !joined.contains("SECRET"),
            "a session cookie must never be painted for a screen-share: {joined}"
        );
    }

    #[test]
    fn no_pace_is_flagged_because_a_cluster_is_the_thing_that_gets_banned() {
        let (_tmp, layout) = layout_with_settings(json!({ "pace_ms": 0 }));
        let settings = bm_core::read_json::<serde_json::Value>(&layout.settings()).unwrap();
        let rows = rows(&layout, &settings);
        assert!(
            is_warn(&rows, "pace_ms"),
            "the pace line must stand out: {rows:#?}"
        );
        assert!(said(&rows).contains("scraper banned"));
    }

    #[test]
    fn the_book_links_come_from_the_frozen_index() {
        let (_tmp, layout) = layout_with_settings(json!({}));
        let mut idx = CrawlIndex::from_template("https://s/chuong-{n}", 1, 3, "hash");
        idx.total = Some(1200);
        idx.save(&layout).unwrap();
        let settings = bm_core::read_json::<serde_json::Value>(&layout.settings()).unwrap();
        let rows = rows(&layout, &settings);
        let v = value_of(&rows, "index");
        assert!(v.contains("template"), "{v}");
        let chapters = value_of(&rows, "chapters");
        assert!(chapters.contains("1200"), "{chapters}");
        // The link itself: the shape of the site's URLs, in the view.
        assert!(
            rows.iter()
                .any(|r| matches!(r, Row::Note(n) if n.contains("https://s/chuong-1"))),
            "{rows:#?}"
        );
    }

    #[test]
    fn a_missing_bundled_tree_is_not_the_same_as_a_book_with_no_crawler() {
        // A workspace with no crawler of its own is normal. The *bundled* tree
        // being empty is not: `crawl`.`script` then resolves to nothing, and
        // every crawl quietly runs without selectors. The view has to say which
        // of the two it is looking at.
        let (_tmp, layout) = layout_with_settings(json!({ "mode": "script" }));
        let settings = bm_core::read_json::<serde_json::Value>(&layout.settings()).unwrap();
        let rows = rows(&layout, &settings);
        let said = said(&rows);
        assert!(is_warn(&rows, "bundled"), "{said}");
        assert!(said.contains("built-in fetcher"), "{said}");
        assert!(
            said.contains("profile.sh fetch"),
            "and how to fix it: {said}"
        );
        // The per-book line stays quiet: an empty workspace dir is expected.
        assert!(!is_warn(&rows, "this book"), "{said}");
    }

    #[test]
    fn a_book_with_no_index_is_told_how_to_get_one() {
        let (_tmp, layout) = layout_with_settings(json!({}));
        let settings = bm_core::read_json::<serde_json::Value>(&layout.settings()).unwrap();
        let rows = rows(&layout, &settings);
        assert!(value_of(&rows, "index").contains("none"));
        assert!(rows
            .iter()
            .any(|r| matches!(r, Row::Note(n) if n.contains(":crawl"))));
    }

    #[test]
    fn the_known_sites_are_all_there_with_their_refusals_kept() {
        let (_tmp, layout) = layout_with_settings(json!({}));
        let settings = bm_core::read_json::<serde_json::Value>(&layout.settings()).unwrap();
        let rows = rows(&layout, &settings);
        let sites = bm_core::crawl::known_sites();
        assert!(!sites.is_empty());
        for site in sites {
            assert!(
                rows.iter()
                    .any(|r| matches!(r, Row::Field { key, .. } if key == site.host)),
                "{} is missing from the view",
                site.host
            );
        }
        // A site we cannot crawl is listed as refused, not hidden: that entry
        // is what saves reading a 403 as a puzzle.
        for site in sites.iter().filter(|s| !s.is_crawlable()) {
            assert!(
                is_warn(&rows, site.host),
                "{} must be marked refused",
                site.host
            );
        }
    }
}
