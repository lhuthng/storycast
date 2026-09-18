//! Voice picker: two-stage filter, arrows-only movement.
//!
//! Auditioning lives on the `:` command line (`:current` / `:try` /
//! `:another`, see `input/audition.rs`) — every letter types into the
//! filter, on both steps. `Enter` is the only key that changes the cast,
//! and it locks the held line rather than picking a new one.
use crate::tui::{
    app::App,
    jobs::Job,
    model::{filtered_characters, filtered_voices},
    screen::{Confirm, ConfirmAction, PickStage, Picker, Screen, TextKind, TextPrompt},
    style::Level,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Last valid cursor for the rows on screen right now, per stage.
fn list_len(app: &App, p: &Picker) -> usize {
    let n = match p.stage {
        PickStage::Character => filtered_characters(app, &p.filter).len(),
        PickStage::Voice => filtered_voices(app, &p.filter).len(),
    };
    n.saturating_sub(1)
}

pub(crate) async fn key_picker(
    app: &mut App,
    picker: Picker,
    key: KeyEvent,
    http: &reqwest::Client,
    job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
) -> bool {
    let mut p = picker;
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Esc => match p.stage {
            PickStage::Voice => {
                p.stage = PickStage::Character;
                p.filter.clear();
                p.cursor = 0;
                p.scroll = 0;
                app.screen = Screen::Pick(p);
            }
            PickStage::Character => {
                app.screen = Screen::Normal;
                app.set_status(Level::Info, "cancelled — nothing changed");
            }
        },
        KeyCode::Char('R') => {
            app.load_roster(job_tx, http);
        }
        KeyCode::Char(':') => {
            // The command line works here too — this is where the
            // `:current` / `:try` / `:another` audition words run, now
            // that every letter types into the filter.
            app.command_return = Some(Screen::Pick(p));
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::Command,
                ":",
                "command — :current :try :another audition, or any word (:help)",
                "",
            ));
            app.set_status(Level::Info, "command mode — Enter runs it, Esc closes");
        }
        KeyCode::Enter => {
            match p.stage {
                PickStage::Character => {
                    let list = filtered_characters(app, &p.filter);
                    let chosen = list
                        .get(p.cursor)
                        .cloned()
                        .unwrap_or_else(|| p.filter.trim().to_string());
                    if chosen.is_empty() {
                        app.set_status(Level::Error, "pick a character, or type a new name first");
                    } else {
                        // Resume on the locked sentence when this character has
                        // one — the voice was picked on it once, so A/B starts
                        // there instead of on another random pick.
                        p.line = app.locked_lines.get(&chosen).cloned();
                        p.character = chosen;
                        p.stage = PickStage::Voice;
                        p.filter.clear();
                        p.cursor = 0;
                        p.scroll = 0;
                        // Step 2 is where auditions run from, and building the
                        // line index is a hundred file opens. Start it here rather
                        // than on the word, so the wait happens while the
                        // operator is reading the list. Deliberately *not* on every
                        // keystroke: a filter key must never dispatch work.
                        app.ensure_lines(job_tx);
                        app.screen = Screen::Pick(p);
                    }
                }
                PickStage::Voice => {
                    let list = filtered_voices(app, &p.filter);
                    match list.get(p.cursor) {
                        None => app.set_status(Level::Error, "no voice selected"),
                        Some(v) => {
                            if !v.allowed {
                                app.set_status(
                                    Level::Warn,
                                    format!(
                                        "{} is an accent policy concern — pick another",
                                        v.name
                                    ),
                                );
                            } else {
                                // Lock the speech the voice is picked on: the
                                // confirm — and every later audition — keeps
                                // this sentence instead of another random pick.
                                if let Some(l) = &p.line {
                                    app.locked_lines.insert(p.character.clone(), l.clone());
                                }
                                app.screen = Screen::Confirm(Confirm {
                                    title: "Confirm voice swap".into(),
                                    danger: true,
                                    body: vec![
                                        format!("Repoint “{}” from its current voice to “{}”.", p.character, v.name),
                                        String::new(),
                                        "This deletes only that speaker's cached segments, drops the".into(),
                                        "stale mp3s for the affected chapters and requeues render +".into(),
                                        "merge. Every other character keeps its cache.".into(),
                                    ],
                                    action: ConfirmAction::SwapVoice {
                                        character: p.character.clone(),
                                        voice: v.name.clone(),
                                    },
                                });
                            }
                        }
                    }
                }
            }
        }
        // Movement is arrows only. `j`/`k` used to move too, which meant a
        // filter for a speaker called "Kiên" silently moved the cursor
        // instead of typing — and nothing on screen said why.
        KeyCode::Up => {
            p.cursor = p.cursor.saturating_sub(1);
            app.screen = Screen::Pick(p);
        }
        KeyCode::Down => {
            // Clamped: the highlight must never leave the list, or Down
            // past the end silently selects nothing.
            let last = list_len(app, &p);
            p.cursor = (p.cursor + 1).min(last);
            app.screen = Screen::Pick(p);
        }
        KeyCode::PageUp => {
            p.cursor = p.cursor.saturating_sub(8);
            app.screen = Screen::Pick(p);
        }
        KeyCode::PageDown => {
            let last = list_len(app, &p);
            p.cursor = (p.cursor + 8).min(last);
            app.screen = Screen::Pick(p);
        }
        KeyCode::Backspace => {
            p.filter.pop();
            p.cursor = 0;
            p.scroll = 0;
            app.screen = Screen::Pick(p);
        }
        KeyCode::Char(c) if ctrl => {
            match c {
                'u' => {
                    p.filter.clear();
                    p.cursor = 0;
                    p.scroll = 0;
                }
                'r' => {
                    app.load_roster(job_tx, http);
                }
                _ => {}
            }
            app.screen = Screen::Pick(p);
        }
        KeyCode::Char(c) if !alt => {
            p.filter.push(c);
            p.cursor = 0;
            p.scroll = 0;
            app.screen = Screen::Pick(p);
        }
        _ => {}
    }
    false
}
