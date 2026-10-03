//! Keys for the guided `workspace new`: name → profile → crawler → create.
use crate::tui::input::Flow;
use crate::tui::{
    app::App,
    input::dispatch,
    jobs::{Job, WorkspaceReq},
    screen::{Screen, WorkspaceNew, WsItem, WsStep},
    style::Level,
};
use bm_core::preset::{read_presets, CrawlerSetup};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::path::{Path, PathBuf};

/// Byte offset of a character index (the data is Vietnamese).
fn byte_at(buf: &str, char_idx: usize) -> usize {
    buf.char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or(buf.len())
}

/// The single-line editor the two text steps share: insertion at the caret,
/// delete/backspace, and the readline kills `Ctrl-U` / `Ctrl-W`.
fn edit(buf: &mut String, cursor: &mut usize, key: &KeyEvent, ctrl: bool, alt: bool) {
    match key.code {
        KeyCode::Backspace => {
            if *cursor > 0 {
                buf.remove(byte_at(buf, *cursor - 1));
                *cursor -= 1;
            }
        }
        KeyCode::Delete => {
            if *cursor < buf.chars().count() {
                buf.remove(byte_at(buf, *cursor));
            }
        }
        KeyCode::Left => *cursor = cursor.saturating_sub(1),
        KeyCode::Right => {
            if *cursor < buf.chars().count() {
                *cursor += 1;
            }
        }
        KeyCode::Home => *cursor = 0,
        KeyCode::End => *cursor = buf.chars().count(),
        KeyCode::Char(c) if ctrl => match c.to_ascii_lowercase() {
            'u' => {
                buf.clear();
                *cursor = 0;
            }
            'w' => {
                while *cursor > 0 && buf.chars().nth(*cursor - 1) == Some(' ') {
                    buf.remove(byte_at(buf, *cursor - 1));
                    *cursor -= 1;
                }
                while *cursor > 0 && buf.chars().nth(*cursor - 1) != Some(' ') {
                    buf.remove(byte_at(buf, *cursor - 1));
                    *cursor -= 1;
                }
            }
            'a' => *cursor = 0,
            'e' => *cursor = buf.chars().count(),
            _ => {}
        },
        KeyCode::Char(c) if !alt => {
            buf.insert(byte_at(buf, *cursor), c);
            *cursor += 1;
        }
        _ => {}
    }
}

/// Every preset, keyed for the picker: the label is what a human reads, the id
/// is what `workspace new --profile` takes.
pub(crate) fn preset_items(root: &Path) -> Vec<WsItem> {
    match read_presets(root) {
        Ok(map) => map
            .into_iter()
            .map(|(id, p)| WsItem {
                label: p.label,
                note: format!("{id} · {} · {}", p.adapter, p.engine),
                value: id,
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// A script's real path: the preset's adapter home first, then the checkout
/// root (which holds the global `crawlers/…` tree), then the pre-split
/// `assets/` — the same scopes the crawler resolver walks, so what the picker
/// offers is what a run will find.
fn script_path(root: &Path, adapter: &str, script: &str) -> Option<PathBuf> {
    for base in [
        root.join(bm_core::paths::ADAPTERS_DIR).join(adapter),
        root.to_path_buf(),
        root.join("assets"),
    ] {
        let p = base.join(script);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// The crawler choices for one profile's adapter.
///
/// Three kinds, in the order an operator meets them: **none** (a book that will
/// say later), **local** (an EPUB, no network), the **known sites** whose bundle
/// is actually on this checkout, and **custom** (a site nobody has written a
/// crawler for yet).
fn crawler_items(root: &Path, adapter: &str) -> Vec<WsItem> {
    let mut out = vec![
        WsItem {
            label: "none".into(),
            note: "no crawler yet — set it up in the crawl view".into(),
            value: "none".into(),
        },
        WsItem {
            label: "Local file (EPUB)".into(),
            note: "reads a local .epub, no network".into(),
            value: "local-epub".into(),
        },
    ];
    for site in bm_core::crawl::known_sites() {
        if !site.is_crawlable() || script_path(root, adapter, site.script).is_none() {
            continue;
        }
        out.push(WsItem {
            label: site.host.to_string(),
            note: format!("known site · {}", site.shape.lines().next().unwrap_or("")),
            value: format!("site:{}", site.host),
        });
    }
    out.push(WsItem {
        label: "Custom site".into(),
        note: "type a chapter URL template".into(),
        value: "custom".into(),
    });
    out
}

/// Turn the wizard's choice into the crawler the workspace is created with.
fn build_crawler(
    root: &Path,
    adapter: &str,
    kind: &str,
    custom_url: &str,
    epub: &str,
) -> Option<CrawlerSetup> {
    match kind {
        "none" => None,
        "local-epub" => {
            // The EPUB example is global (`crawlers/examples/epub.lua`): the
            // book references it in place rather than carrying a copy, so a fix
            // to the unknown-structure crawler reaches every epub import.
            // Workspace-scoped, and the workspace's own scratch tree: `tmp/`
            // is `Layout::scratch()` for a workspace (the checkout root keeps
            // the legacy `.bm/tmp`). `.bm/tmp` inside a book would be a second
            // `.bm/`, reading as machine-global state the book does not own.
            //
            // The operator's `epub` is the file itself: it is copied to
            // `tmp/book.epub` at create time (`CrawlerSetup::book`), because
            // the crawl's read root is the workspace and a path outside it is
            // refused — a reference would not survive the confinement.
            // A **folder** is the multi-volume shape: one `.epub` per volume,
            // copied into the workspace's own `books/` and read via
            // `crawl.params.books`. A file stays the single-book shape. Which
            // one it is, is a property of what the operator typed, so the same
            // step takes either.
            let named = epub.trim();
            let dir = Path::new(named).is_dir();
            let mut params = serde_json::Map::new();
            let (key, value) = if dir {
                ("books", "books")
            } else {
                ("epub", "tmp/book.epub")
            };
            params.insert(key.into(), serde_json::Value::String(value.into()));
            Some(CrawlerSetup {
                script: "crawlers/examples/epub.lua".into(),
                params,
                book: if dir {
                    PathBuf::new()
                } else {
                    PathBuf::from(named)
                },
                books: if dir {
                    PathBuf::from(named)
                } else {
                    PathBuf::new()
                },
                ..Default::default()
            })
        }
        "custom" => {
            // A site nobody has written a crawler for: seed the workspace with a
            // copy of the easy-shape template for the operator to edit. The
            // book owns it (`crawl/…`), which is what `custom` means.
            Some(CrawlerSetup {
                source: script_path(root, adapter, bm_core::crawl::DEFAULT_SCRIPT)
                    .unwrap_or_default(),
                url_template: custom_url.to_string(),
                ..Default::default()
            })
        }
        kind if kind.starts_with("site:") => {
            let host = kind.trim_start_matches("site:");
            let site = bm_core::crawl::known_sites()
                .iter()
                .find(|s| s.host == host)?;
            let mut params = serde_json::Map::new();
            for (k, v) in site.params {
                params.insert(
                    (*k).to_string(),
                    serde_json::Value::String((*v).to_string()),
                );
            }
            // Global reference: the resolved path is checked by
            // `crawler_items`, and `apply_preset` writes the shared spelling.
            Some(CrawlerSetup {
                script: site.script.to_string(),
                url_template: site.url_template.to_string(),
                params,
                max_fetches: site.max_fetches,
                max_seconds: site.max_seconds,
                source: PathBuf::new(),
                book: PathBuf::new(),
                books: PathBuf::new(),
            })
        }
        _ => None,
    }
}

/// The adapter a preset id names, or the checkout's own.
fn preset_adapter(root: &Path, id: &str) -> String {
    read_presets(root)
        .ok()
        .and_then(|m| m.get(id).map(|p| p.adapter.clone()))
        .unwrap_or_default()
}

pub(crate) async fn key_workspace_new(
    app: &mut App,
    mut ws: WorkspaceNew,
    key: KeyEvent,
    job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
) -> Flow {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    ws.error = None;
    let mut created = false;

    match ws.step {
        WsStep::Name => match key.code {
            KeyCode::Esc => {
                app.screen = app.back_out();
                app.set_status(Level::Info, "cancelled — no workspace was created");
            }
            KeyCode::Enter => match super::submit::workspace_name(&ws.name) {
                Ok(name) => {
                    if ws.profiles.is_empty() {
                        ws.error = Some(
                            "no presets in profiles/presets.json — the guided flow needs one"
                                .into(),
                        );
                    } else {
                        ws.name = name;
                        ws.step = WsStep::Profile;
                        ws.cursor = 0;
                        ws.scroll = 0;
                    }
                }
                Err(e) => ws.error = Some(e),
            },
            _ => edit(&mut ws.name, &mut ws.name_cursor, &key, ctrl, alt),
        },
        WsStep::Profile => match key.code {
            KeyCode::Esc => {
                ws.step = WsStep::Name;
                ws.cursor = ws.name_cursor;
            }
            KeyCode::Enter => match ws.profiles.get(ws.cursor) {
                Some(item) => {
                    let id = item.value.clone();
                    ws.profile = Some(ws.cursor);
                    let adapter = preset_adapter(&app.layout.root, &id);
                    ws.crawlers = crawler_items(&app.layout.root, &adapter);
                    ws.step = WsStep::Crawler;
                    ws.cursor = 0;
                    ws.scroll = 0;
                }
                None => ws.error = Some("no profile to choose".into()),
            },
            KeyCode::Up | KeyCode::Char('k') => ws.move_cursor(false),
            KeyCode::Down | KeyCode::Char('j') => ws.move_cursor(true),
            _ => {}
        },
        WsStep::Crawler => match key.code {
            KeyCode::Esc => {
                ws.step = WsStep::Profile;
                ws.cursor = ws.profile.unwrap_or(0);
            }
            KeyCode::Enter => {
                let Some(item) = ws.crawlers.get(ws.cursor).cloned() else {
                    return Flow::KeepRunning;
                };
                if item.value == "custom" {
                    ws.step = WsStep::CustomUrl;
                    ws.url.clear();
                    ws.url_cursor = 0;
                } else if item.value == "local-epub" {
                    // The EPUB is a file, so this step asks for it before the
                    // create job runs — the TUI's "add epub".
                    ws.step = WsStep::Epub;
                    ws.epub.clear();
                    ws.epub_cursor = 0;
                } else {
                    created = dispatch_new(app, job_tx, &ws, &item.value, "", "");
                }
            }
            KeyCode::Up | KeyCode::Char('k') => ws.move_cursor(false),
            KeyCode::Down | KeyCode::Char('j') => ws.move_cursor(true),
            _ => {}
        },
        WsStep::Epub => match key.code {
            KeyCode::Esc => ws.step = WsStep::Crawler,
            KeyCode::Enter => {
                let path = ws.epub.trim().to_string();
                if path.is_empty() {
                    ws.error = Some(
                        "give the path to the .epub, or a folder of volumes — \
                         e.g. /Users/you/Books/apothecary.epub"
                            .into(),
                    );
                } else if !Path::new(&path).is_file() && !Path::new(&path).is_dir() {
                    ws.error = Some(format!("no file or folder at {path}"));
                } else {
                    created = dispatch_new(app, job_tx, &ws, "local-epub", "", &path);
                }
            }
            _ => edit(&mut ws.epub, &mut ws.epub_cursor, &key, ctrl, alt),
        },
        WsStep::CustomUrl => match key.code {
            KeyCode::Esc => ws.step = WsStep::Crawler,
            KeyCode::Enter => {
                if !ws.url.contains("{n}") {
                    ws.error = Some(
                        "template must contain {n} — that is where the chapter number goes".into(),
                    );
                } else {
                    created = dispatch_new(app, job_tx, &ws, "custom", &ws.url.clone(), "");
                }
            }
            _ => edit(&mut ws.url, &mut ws.url_cursor, &key, ctrl, alt),
        },
    }

    if !created && matches!(app.screen, Screen::WorkspaceNew(_)) {
        // Keep the highlight on screen: a list longer than the dialog scrolls
        // rather than moving the highlight off the bottom edge.
        let rows = 9usize;
        if ws.cursor < ws.scroll {
            ws.scroll = ws.cursor;
        } else if ws.cursor >= ws.scroll + rows {
            ws.scroll = ws.cursor + 1 - rows;
        }
        app.screen = Screen::WorkspaceNew(ws);
    }
    Flow::KeepRunning
}

/// Dispatch the create job and close the wizard. Returns whether it ran.
fn dispatch_new(
    app: &mut App,
    job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
    ws: &WorkspaceNew,
    kind: &str,
    custom_url: &str,
    epub: &str,
) -> bool {
    let Some(profile) = ws.profile.and_then(|i| ws.profiles.get(i)) else {
        return false;
    };
    let id = profile.value.clone();
    let adapter = preset_adapter(&app.layout.root, &id);
    let crawler = build_crawler(&app.layout.root, &adapter, kind, custom_url, epub);
    app.screen = Screen::Normal;
    app.set_status(Level::Ok, format!("creating workspace {}", ws.name));
    dispatch(
        app,
        job_tx,
        Job::Workspace {
            layout: app.layout.clone(),
            api: app.api.clone(),
            req: WorkspaceReq::New {
                name: ws.name.clone(),
                profile: Some(id),
                crawler,
            },
        },
    )
}
