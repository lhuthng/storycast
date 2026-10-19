//! Cast overview: filterable table, read-only.
use crate::tui::input::Flow;
use crate::tui::{
    app::App,
    input::audition::{cast_current, cast_pointed},
    jobs::Job,
    model::filtered_cast_rows,
    screen::{CastView, Confirm, ConfirmAction, Screen, TextKind, TextPrompt},
    style::Level,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub(crate) async fn key_cast(
    app: &mut App,
    view: CastView,
    key: KeyEvent,
    http: &reqwest::Client,
    job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
) -> Flow {
    let mut v = view;
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        // Esc blurs the filter first; only the second Esc closes. A blurred
        KeyCode::Esc => {
            if v.filter_focus {
                v.filter_focus = false;
                app.set_status(
                    Level::Info,
                    "audition keys — t current · T line · ^T another",
                );
            } else {
                app.screen = Screen::Normal;
                return Flow::KeepRunning;
            }
            app.screen = Screen::Cast(v);
        }
        KeyCode::Char('R') if !ctrl && !alt => app.load_roster(job_tx, http),
        // `x`: this speaker keeps its voice and everyone else on it moves. The
        // row under the cursor is the one that keeps it.
        KeyCode::Char('x') if !v.filter_focus && !ctrl && !alt => {
            let list = filtered_cast_rows(&app.cast_rows(), &v.filter);
            match list.get(v.cursor) {
                Some(r) if r.unassigned() => {
                    app.set_status(Level::Warn, format!("{} has no voice to keep", r.character))
                }
                // One user already: there is nothing to break apart.
                Some(r) if !r.shared() => app.set_status(
                    Level::Info,
                    format!("“{}” already speaks “{}” alone", r.character, r.voice),
                ),
                Some(r) => {
                    let others = r.shared_with.len();
                    app.screen = Screen::Confirm(Confirm {
                        title: "Make a voice exclusive".into(),
                        danger: true,
                        body: vec![
                            format!("“{}” keeps “{}”.", r.character, r.voice),
                            format!(
                                "The other {others} speaker(s) on it take a free voice of their"
                            ),
                            "own, same gender first, and re-speak the takes it reached.".into(),
                            String::new(),
                            format!("sharing now: {}", r.shared_with.join(", ")),
                        ],
                        action: ConfirmAction::ClaimVoice {
                            character: r.character.clone(),
                            voice: r.voice.clone(),
                        },
                    });
                }
                None => app.set_status(Level::Warn, "no speaker under the cursor"),
            }
        }
        // `t`: the speaker's current voice on the shown line, from cache
        KeyCode::Char('t') if !v.filter_focus && !ctrl && !alt => {
            cast_current(app, job_tx, http, &mut v);
            app.screen = Screen::Cast(v);
        }
        // `T`: the held line, rendered — the deliberate generation behind
        KeyCode::Char('T') if !v.filter_focus && !ctrl && !alt => {
            cast_pointed(app, job_tx, http, &mut v, false);
            app.screen = Screen::Cast(v);
        }
        KeyCode::Char(':') => {
            // The command line works here too — `:current` / `:try` /
            app.command_return = Some(Screen::Cast(v));
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::Command,
                ":",
                "command — :current :try :another audition, or any word (:help)",
                "",
            ));
            app.set_status(Level::Info, "command mode — Enter runs it, Esc closes");
        }
        KeyCode::Up => {
            v.cursor = v.cursor.saturating_sub(1);
            app.screen = Screen::Cast(v);
        }
        KeyCode::Down => {
            let last = filtered_cast_rows(&app.cast_rows(), &v.filter)
                .len()
                .saturating_sub(1);
            v.cursor = (v.cursor + 1).min(last);
            app.screen = Screen::Cast(v);
        }
        KeyCode::PageUp => {
            v.cursor = v.cursor.saturating_sub(8);
            app.screen = Screen::Cast(v);
        }
        KeyCode::PageDown => {
            let last = filtered_cast_rows(&app.cast_rows(), &v.filter)
                .len()
                .saturating_sub(1);
            v.cursor = (v.cursor + 8).min(last);
            app.screen = Screen::Cast(v);
        }
        KeyCode::Home => {
            v.cursor = 0;
            app.screen = Screen::Cast(v);
        }
        KeyCode::End => {
            v.cursor = filtered_cast_rows(&app.cast_rows(), &v.filter)
                .len()
                .saturating_sub(1);
            app.screen = Screen::Cast(v);
        }
        KeyCode::Backspace => {
            // Editing the filter focuses it.
            v.filter_focus = true;
            v.filter.pop();
            v.cursor = 0;
            v.scroll = 0;
            app.screen = Screen::Cast(v);
        }
        KeyCode::Char(c) if ctrl => {
            if c == 'u' {
                v.filter.clear();
                v.cursor = 0;
                v.scroll = 0;
                v.filter_focus = true;
            } else if c == 'r' || c == 'R' {
                // `^R` focuses the filter explicitly. Case-insensitive —
                v.filter_focus = true;
                app.set_status(
                    Level::Info,
                    "filter focused — every letter types · Esc back to audition keys",
                );
            } else if (c == 't' || c == 'T') && !v.filter_focus {
                // `^T`: another line, same voice. Audition focus only.
                cast_pointed(app, job_tx, http, &mut v, true);
            }
            app.screen = Screen::Cast(v);
        }
        KeyCode::Char(c) if !alt => {
            // Any other letter focuses the filter and types; `t`/`T`
            v.filter_focus = true;
            v.filter.push(c);
            v.cursor = 0;
            v.scroll = 0;
            app.screen = Screen::Cast(v);
        }
        _ => {}
    }
    Flow::KeepRunning
}
