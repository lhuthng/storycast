//! `L`: LLM providers — keys, endpoints, models, and which one digests.
//!
//! The file is `.bm/llm.json` (machine-global, like `machines.json`); every
//! edit here saves it at once and mirrors the active provider into the
//! workspace's `settings.json`, so the run screen and the API preview read
//! the same model the next task offer carries. The offer is the whole sync:
//! the active key+model travel per task, and switching takes effect on the
//! next offer with no restart and no second file to keep in step.
use crate::tui::input::Flow;
use crate::tui::{
    app::App,
    input::dispatch,
    jobs::Job,
    screen::{LlmView, Screen, TextKind, TextPrompt},
    style::Level,
};
use crossterm::event::{KeyCode, KeyEvent};

/// Open the screen, remembering where the cursor was.
pub(crate) fn open_llm(app: &mut App) {
    let mut v = LlmView::new();
    v.cursor = app.llm_cursor;
    app.screen = Screen::Llm(v);
    app.set_status(
        Level::Info,
        "LLM providers — ↑↓ move · a activate · k key · u URL · m model · f fetch models · Esc close",
    );
}

/// Save one field of one provider, then mirror the active provider into the
/// workspace settings. Returns a status line; `Err` keeps the prompt open.
pub(crate) fn save_llm_field(
    app: &App,
    provider: &str,
    kind: &TextKind,
    buf: &str,
) -> Result<String, String> {
    if app.layout.root.as_os_str().is_empty() {
        return Err("no repo root — restart the TUI from a checkout".into());
    }
    let mut cfg = bm_core::config::LlmConfig::load(&app.layout.root);
    let entry = cfg.providers.entry(provider.to_string()).or_default();
    let msg = match kind {
        TextKind::LlmKey(_) => {
            let key = buf.trim();
            if key.is_empty() {
                entry.api_key.clear();
                format!("{provider} key cleared — provider off until a key is added")
            } else if key.contains(char::is_whitespace) {
                return Err("a key has no spaces — paste the whole key".into());
            } else {
                entry.api_key = key.to_string();
                format!("{provider} key saved — activate it with `a` to digest with it")
            }
        }
        TextKind::LlmUrl(_) => {
            let url = buf.trim().trim_end_matches('/').to_string();
            if url.is_empty() {
                entry.base_url.clear();
                format!("{provider} URL cleared")
            } else if !(url.starts_with("http://") || url.starts_with("https://")) {
                return Err("a URL starts with http:// or https://".into());
            } else {
                entry.base_url = url.clone();
                format!("{provider} URL saved: {url}")
            }
        }
        TextKind::LlmModel(_) => {
            let model = buf.trim().to_string();
            if model.is_empty() {
                return Err("a model is empty — pick one from `f`, or type its name".into());
            }
            if model.contains(char::is_whitespace) {
                return Err("a model name has no spaces".into());
            }
            entry.model = model.clone();
            format!("{provider} model saved: {model}")
        }
        _ => return Err("not an LLM prompt".into()),
    };
    cfg.save(&app.layout.root)
        .map_err(|e| format!("saving llm.json: {e:#}"))?;
    mirror_settings(app, &cfg);
    Ok(msg)
}

/// Activate one provider: key (except Ollama) and model must be set, or the
/// digest would be switched onto a provider that cannot answer.
pub(crate) fn activate(app: &App, provider: &str) -> Result<String, String> {
    if app.layout.root.as_os_str().is_empty() {
        return Err("no repo root — restart the TUI from a checkout".into());
    }
    let mut cfg = bm_core::config::LlmConfig::load(&app.layout.root);
    // The keyless kind is read off the entry, not the id: any local service
    // marked `ollama` activates without a key.
    let keyless = cfg.kind_of(provider) == bm_core::config::LlmKind::Ollama;
    let entry = cfg.providers.entry(provider.to_string()).or_default();
    if entry.model.trim().is_empty() {
        return Err(format!(
            "{provider} has no model — set one with `m` (or `f` to list) first"
        ));
    }
    if !keyless && !entry.has_key() {
        return Err(format!("{provider} has no key — add one with `k` first"));
    }
    cfg.active = provider.to_string();
    cfg.save(&app.layout.root)
        .map_err(|e| format!("saving llm.json: {e:#}"))?;
    mirror_settings(app, &cfg);
    Ok(format!(
        "{provider} active — next digest offers carry its key + model"
    ))
}

/// Save the fetched-pick: the highlighted model becomes the provider's model.
pub(crate) fn pick_model(app: &App, provider: &str, model: &str) -> Result<String, String> {
    if app.layout.root.as_os_str().is_empty() {
        return Err("no repo root — restart the TUI from a checkout".into());
    }
    let mut cfg = bm_core::config::LlmConfig::load(&app.layout.root);
    cfg.providers.entry(provider.to_string()).or_default().model = model.to_string();
    cfg.save(&app.layout.root)
        .map_err(|e| format!("saving llm.json: {e:#}"))?;
    mirror_settings(app, &cfg);
    Ok(format!("{provider} model saved: {model}"))
}

/// Mirror the active provider into the workspace settings so the run screen,
/// the footer and the headless CLI read the same model the offers carry. One
/// direction only: `llm.json` wins, and a failure here never fails the save.
fn mirror_settings(app: &App, cfg: &bm_core::config::LlmConfig) {
    let path = app.layout.settings();
    let mut settings = bm_core::config::Settings::load(&path);
    cfg.sync_settings(&mut settings);
    let _ = settings.save(&path);
}

pub(crate) async fn key_llm(
    app: &mut App,
    view: LlmView,
    key: KeyEvent,
    _http: &reqwest::Client,
    job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
) -> Flow {
    let cfg = bm_core::config::LlmConfig::load(&app.layout.root);
    let ids = LlmView::ids(&cfg);
    if ids.is_empty() {
        app.screen = Screen::Normal;
        return Flow::KeepRunning;
    }
    let mut v = view;
    v.cursor = v.cursor.min(ids.len().saturating_sub(1));
    let id = ids[v.cursor].clone();
    // The fetched list belongs to the provider it was fetched for; moving
    // off it leaves picking rather than offering another row's Enter.
    if v.picking && app.llm_models_for != id {
        v.picking = false;
        v.model_cursor = 0;
    }
    match key.code {
        KeyCode::Esc => {
            if v.picking {
                // The fetched list is a step of this screen, like the voice
                // picker's second stage: Esc leaves the list, and only the
                // next Esc closes the screen. It used to walk out of both.
                v.picking = false;
                v.model_cursor = 0;
                v.note.clear();
                app.screen = Screen::Llm(v);
                app.set_status(Level::Info, "model list closed — `f` fetches it again");
            } else {
                app.screen = Screen::Normal;
                app.set_status(Level::Info, "closed the LLM screen");
            }
        }
        KeyCode::Up | KeyCode::Char('k') if v.picking => {
            v.model_cursor = v.model_cursor.saturating_sub(1);
            app.screen = Screen::Llm(v);
        }
        KeyCode::Down | KeyCode::Char('j') if v.picking => {
            v.model_cursor = (v.model_cursor + 1).min(app.llm_models.len().saturating_sub(1));
            app.screen = Screen::Llm(v);
        }
        KeyCode::Enter if v.picking => match app.llm_models.get(v.model_cursor).cloned() {
            Some(model) => match pick_model(app, &id, &model) {
                Ok(msg) => {
                    app.llm_cursor = v.cursor;
                    let mut nv = LlmView::new();
                    nv.cursor = app.llm_cursor;
                    app.screen = Screen::Llm(nv);
                    app.set_status(Level::Ok, msg);
                }
                Err(msg) => {
                    app.screen = Screen::Llm(v);
                    app.set_status(Level::Error, msg);
                }
            },
            None => {
                app.screen = Screen::Llm(v);
                app.set_status(Level::Warn, "no models listed yet — `f` fetches them");
            }
        },
        KeyCode::Up => {
            v.cursor = v.cursor.saturating_sub(1);
            app.screen = Screen::Llm(v);
        }
        KeyCode::Down => {
            v.cursor = (v.cursor + 1).min(ids.len().saturating_sub(1));
            app.screen = Screen::Llm(v);
        }
        KeyCode::Char('a') => {
            app.llm_cursor = v.cursor;
            match activate(app, &id) {
                Ok(msg) => app.set_status(Level::Ok, msg),
                Err(msg) => app.set_status(Level::Error, msg),
            }
            app.screen = Screen::Llm(v);
        }
        KeyCode::Char('k') if !v.picking => {
            app.llm_cursor = v.cursor;
            let e = cfg.providers.get(&id).cloned().unwrap_or_default();
            let hint = if e.has_key() {
                "a key is set (never shown) — pasting replaces it, empty clears it"
            } else {
                "paste the provider's API key — empty leaves it off"
            };
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::LlmKey(id),
                "Provider key",
                hint,
                "",
            ));
        }
        KeyCode::Char('u') => {
            app.llm_cursor = v.cursor;
            let cur = cfg
                .providers
                .get(&id)
                .map(|e| e.base_url.clone())
                .unwrap_or_default();
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::LlmUrl(id),
                "Provider base URL",
                "where the API lives — empty clears it",
                &cur,
            ));
        }
        KeyCode::Char('m') => {
            app.llm_cursor = v.cursor;
            let cur = cfg
                .providers
                .get(&id)
                .map(|e| e.model.clone())
                .unwrap_or_default();
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::LlmModel(id),
                "Provider model",
                "exact model name — or `f` to list what the API serves and pick",
                &cur,
            ));
        }
        KeyCode::Char('f') => {
            let e = cfg.providers.get(&id).cloned().unwrap_or_default();
            if e.base_url.trim().is_empty() {
                app.screen = Screen::Llm(v);
                app.set_status(
                    Level::Warn,
                    format!("{id} has no URL — set one with `u` first"),
                );
                return Flow::KeepRunning;
            }
            if !e.has_key() && cfg.kind_of(&id) != bm_core::config::LlmKind::Ollama {
                app.screen = Screen::Llm(v);
                app.set_status(
                    Level::Warn,
                    format!("{id} has no key — add one with `k` first"),
                );
                return Flow::KeepRunning;
            }
            app.llm_models.clear();
            app.llm_models_for = id.clone();
            v.picking = true;
            v.model_cursor = 0;
            v.note = format!("fetching {id} models…");
            app.screen = Screen::Llm(v);
            dispatch(
                app,
                job_tx,
                Job::LlmModels {
                    provider: id.clone(),
                    kind: cfg.kind_of(&id).as_backend().to_string(),
                    base_url: e.base_url.clone(),
                    key: e.api_key.clone(),
                },
            );
            app.set_status(Level::Info, format!("fetching {id} models…"));
        }
        _ => {
            app.screen = Screen::Llm(v);
        }
    }
    Flow::KeepRunning
}
