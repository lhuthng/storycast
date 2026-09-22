//! The sound-design editor's keys.
//!
//! One screen, three tabs, and one rule that shapes all of it: **remove is
//! unavailable for anything the mix still reaches**. Every letter here is an
//! action rather than a filter — the pools are 11, 6 and 21 rows, so a filter
//! would cost more than it saves and would take the letters the actions need.
//!
//! Nothing here writes on a single keystroke: `a`, `e` and `l` open a prompt
//! that has to be submitted, and `d` opens a confirmation. The prompt is
//! prefilled with the values actually in force, so a mistake is visible before
//! it is saved rather than after.
use crate::tui::{
    app::App,
    jobs::Job,
    screen::{Confirm, ConfirmAction, Screen, SoundRemoval, TextKind, TextPrompt},
    sound::{self, SoundView},
    style::Level,
};
use bm_core::audio_pool::PoolKind;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// The rows the cursor is on, or a status explaining why there are none.
///
/// Every action goes through this, so "nothing selected" is stated once instead
/// of once per key.
fn selected(app: &App, view: &SoundView) -> Result<sound::SoundRow, String> {
    loaded(app)?;
    let data = app.sound.as_ref().expect("checked by `loaded`");
    let rows = sound::rows(data, view.layer);
    match rows.get(view.cursor) {
        Some(r) => Ok(r.clone()),
        None => Err(format!(
            "nothing selected in the {} pool — a adds the first sound",
            view.layer.label()
        )),
    }
}

/// Whether the pools are here at all. Separate from [`selected`] because `a`
/// needs no row — an empty pool is exactly when you want to add one.
fn loaded(app: &App) -> Result<(), String> {
    if app.sound.is_some() {
        return Ok(());
    }
    Err(match &app.sound_error {
        Some(e) => format!("pools not loaded: {e} — R retries"),
        None => "loading the pools…".into(),
    })
}

pub(crate) async fn key_sound(
    app: &mut App,
    view: SoundView,
    key: KeyEvent,
    _http: &reqwest::Client,
    job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
) -> bool {
    let mut v = view;
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let layer = v.layer;
    let n_rows = app
        .sound
        .as_ref()
        .map(|d| sound::rows(d, layer).len())
        .unwrap_or(0);
    // Tab / Shift-Tab and the arrows move between layers. The rows are a single
    // column, so left/right are free and are the obvious spelling of "the next
    // tab" on a keyboard that has them.
    let step = match key.code {
        KeyCode::Tab | KeyCode::Right | KeyCode::Char(']') => Some(1i32),
        KeyCode::BackTab | KeyCode::Left | KeyCode::Char('[') => Some(-1),
        _ => None,
    };
    if let Some(step) = step {
        let all = PoolKind::ALL;
        let at = all.iter().position(|k| *k == layer).unwrap_or(0) as i32;
        v.layer = all[(at + step).rem_euclid(all.len() as i32) as usize];
        // The cursor is a row index into a different list now: keeping it would
        // point at an arbitrary entry on the new tab.
        v.cursor = 0;
        v.scroll = 0;
        app.screen = Screen::Sound(v);
        return false;
    }

    match key.code {
        KeyCode::Esc => app.screen = Screen::Normal,
        KeyCode::Char('R') => app.load_sound(job_tx),
        KeyCode::Up | KeyCode::Char('k') if !ctrl && !alt => {
            v.cursor = v.cursor.saturating_sub(1);
            app.screen = Screen::Sound(v);
        }
        KeyCode::Down | KeyCode::Char('j') if !ctrl && !alt => {
            v.cursor = (v.cursor + 1).min(n_rows.saturating_sub(1));
            app.screen = Screen::Sound(v);
        }
        KeyCode::PageUp => {
            v.cursor = v.cursor.saturating_sub(8);
            app.screen = Screen::Sound(v);
        }
        KeyCode::PageDown => {
            v.cursor = (v.cursor + 8).min(n_rows.saturating_sub(1));
            app.screen = Screen::Sound(v);
        }
        KeyCode::Home => {
            v.cursor = 0;
            app.screen = Screen::Sound(v);
        }
        KeyCode::End => {
            v.cursor = n_rows.saturating_sub(1);
            app.screen = Screen::Sound(v);
        }
        // `a` — a new sound. The name is required; everything else has the
        // default the mix would use, stated in the hint.
        KeyCode::Char('a') if !ctrl && !alt => {
            // The add prompt needs no row selected — an empty pool is exactly
            // when you want one — but it does need the pools to have loaded: an
            // entry written against an unread registry would drop whatever that
            // registry holds.
            if let Err(e) = loaded(app) {
                app.set_status(Level::Warn, e);
            } else {
                app.screen = Screen::Text(TextPrompt::new(
                    TextKind::SoundAdd(layer),
                    &format!("Add {} sound", layer.label()),
                    &sound::fields_hint(layer),
                    "",
                ));
            }
        }
        // `e` — edit the highlighted entry, prefilled with what is in force.
        KeyCode::Char('e') if !ctrl && !alt => match selected(app, &v) {
            Err(e) => app.set_status(Level::Warn, e),
            Ok(row) => {
                app.screen = Screen::Text(TextPrompt::new(
                    TextKind::SoundEdit(layer, row.name.clone()),
                    &format!("Edit {} “{}”", layer.label(), row.name),
                    "the whole entry; the name is the key and cannot change here",
                    &sound::describe(&row.name, &row.sound, layer),
                ));
            }
        },
        // `l` — the sound's own trim. Empty clears it.
        KeyCode::Char('l') if !ctrl && !alt => match selected(app, &v) {
            Err(e) => app.set_status(Level::Warn, e),
            Ok(row) => {
                let current = row.sound.level.map(|l| l.to_string()).unwrap_or_default();
                app.screen = Screen::Text(TextPrompt::new(
                    TextKind::SoundLevel(layer, row.name.clone()),
                    &format!("Level of “{}”", row.name),
                    "trim for this sound alone, 0.01–4.0 — empty clears it back to 1.0",
                    &current,
                ));
            }
        },
        // `d` — remove. The guard is read here, at the keystroke, and again at
        // the confirmation: between the two the screen may have reloaded.
        KeyCode::Char('d') if !ctrl && !alt => match selected(app, &v) {
            Err(e) => app.set_status(Level::Warn, e),
            Ok(row) if row.in_use() => app.set_status(
                Level::Warn,
                format!(
                    "“{}” cannot be removed — in use by {}; the scene would go quiet, not fail",
                    row.name,
                    sound::summarise(&row.uses, 3)
                ),
            ),
            Ok(row) => {
                app.screen = Screen::Confirm(Confirm {
                    title: format!("Remove “{}” from the {} pool?", row.name, layer.label()),
                    danger: true,
                    body: vec![
                        format!(
                            "The entry goes from assets/{}; its {} clip(s) stay on disk.",
                            layer.registry(),
                            row.takes()
                        ),
                        "Nothing reaches it today, so no scene or script loses a sound.".into(),
                        String::new(),
                        "Re-adding the same name brings the same takes back, and the".into(),
                        "registry is in git, so this is recoverable by hand.".into(),
                    ],
                    action: ConfirmAction::SoundRemove(SoundRemoval {
                        layer,
                        name: row.name.clone(),
                        view: v.clone(),
                    }),
                });
            }
        },
        _ => {}
    }
    false
}

/// Submit one of the editor's three prompts: the whole-entry line, or a level.
///
/// Returns the status line to report and the view to come back to — the tab it
/// was asked from, with the cursor on the entry that was just edited, so the
/// result of the edit is what you are looking at. `Err` keeps the prompt open.
pub(crate) fn submit(
    app: &mut App,
    kind: &TextKind,
    buf: &str,
) -> Result<(String, SoundView), String> {
    let (layer, target) = match kind {
        TextKind::SoundAdd(layer) => (*layer, None),
        TextKind::SoundEdit(layer, name) | TextKind::SoundLevel(layer, name) => {
            (*layer, Some(name.as_str()))
        }
        _ => return Err("not a sound-design prompt".into()),
    };
    loaded(app)?;
    let data = app.sound.as_mut().expect("checked by `loaded`");

    let (msg, focus) = match kind {
        TextKind::SoundLevel(..) => {
            let name = target.expect("a level prompt always names its sound");
            let level = sound::parse_level_prompt(buf)?;
            (
                sound::set_level(data, layer, name, level)?,
                name.to_string(),
            )
        }
        _ => {
            // The layer's own parser, then the filesystem: a name and a shape
            // that parse but point at a clip that is not there would write a
            // registry line the merge can only warn about. Only the takes this
            // edit *adds* are checked — see `sound::check_files`.
            let (name, mut entry) = sound::parse_entry(layer, buf, target)?;
            let old = target.and_then(|n| data.pools[&layer].get(n));
            sound::check_files(&data.root, layer, &sound::introduced(old, &entry))?;
            if layer == PoolKind::Inject {
                // `dur_s` is a fact about the clip, so it is re-probed on every
                // inject edit rather than carried along from whatever take set
                // was there before. A failed probe (no ffprobe) leaves the
                // value alone rather than replacing it with a guess.
                if let Some(d) = sound::probe_longest(&data.root, &entry) {
                    entry.dur_s = Some(d);
                }
            }
            let msg = sound::commit(data, layer, name.clone(), entry)?;
            (msg, name)
        }
    };

    // Cursor onto the entry that was just written, so the change is visible
    // rather than something the operator has to hunt for.
    let mut view = SoundView::new();
    view.layer = layer;
    view.cursor = app
        .sound
        .as_ref()
        .and_then(|d| sound::rows(d, layer).iter().position(|r| r.name == focus))
        .unwrap_or(0);
    Ok((msg, view))
}

/// Carry out a confirmed removal, on the screen it came from.
///
/// Returns whether a registry was actually written — the caller uses it to
/// decide whether to tell the inductor the sound design changed. A refused
/// removal is not a design change, and a notification for it would be noise.
pub(crate) fn apply_removal(
    app: &mut App,
    layer: PoolKind,
    name: &str,
    view: SoundView,
) -> bool {
    if let Err(e) = loaded(app) {
        app.set_status(Level::Warn, e);
        return false;
    }
    // Re-checked, not assumed: the confirmation is a dialog and the screen
    // behind it can have been reloaded (or the scene map edited) while it was
    // open. A guard that only holds at the keystroke is a guard with a hole.
    let in_use = app
        .sound
        .as_ref()
        .and_then(|d| d.usage.get(&layer))
        .map(|u| u.contains_key(name))
        .unwrap_or(false);
    if in_use {
        app.set_status(
            Level::Warn,
            format!("“{name}” is in use — nothing was removed"),
        );
        app.screen = Screen::Sound(view);
        return false;
    }
    let outcome = sound::remove(
        app.sound.as_mut().expect("checked by `loaded`"),
        layer,
        name,
    );
    match outcome {
        Ok(msg) => {
            app.log_at(Level::Ok, msg.clone());
            app.set_status(Level::Ok, msg);
            let mut v = view;
            let n = app
                .sound
                .as_ref()
                .map(|d| sound::rows(d, layer).len())
                .unwrap_or(0);
            v.cursor = v.cursor.min(n.saturating_sub(1));
            app.screen = Screen::Sound(v);
            true
        }
        Err(e) => {
            app.set_status(Level::Error, e);
            app.screen = Screen::Sound(view);
            false
        }
    }
}
