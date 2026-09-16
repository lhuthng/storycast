//! Cast overview: filterable table, read-only.
//!
//! Nothing here assigns anything — swapping happens in the picker (`:s`), and
//! this screen deliberately offers no shortcut to it. `t` tests the speaker's
//! current voice on the shown line, from cache only; `T` renders the held
//! line, `^T` re-rolls it. Those two letters don't type here; every other
//! letter still filters.
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use crate::tui::{
    app::App,
    input::audition::{audition, segment, shown_line},
    jobs::Job,
    model::filtered_cast_rows,
    screen::{CastView, Screen},
    style::Level,
};

pub(crate) async fn key_cast(app: &mut App, view: CastView, key: KeyEvent, http: &reqwest::Client, job_tx: &tokio::sync::mpsc::UnboundedSender<Job>) -> bool {
        let mut v = view;
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            // Esc closes; `q` is deliberately *not* bound here, so it can be
            // typed into the filter like any other letter.
            KeyCode::Esc => {
                app.screen = Screen::Normal;
            }
            KeyCode::Char('R') => app.load_roster(job_tx, http),
            // `t`: the speaker's current voice on the shown line, from cache
            // only — never synthesis. A miss names the render key instead of
            // playing something nearby.
            KeyCode::Char('t') if !ctrl && !alt => {
                let rows = app.cast_rows();
                let list = filtered_cast_rows(&rows, &v.filter);
                match list.get(v.cursor) {
                    None => app.set_status(Level::Warn, "nothing to audition"),
                    Some(row) if row.voice.is_empty() => app.set_status(
                        Level::Warn,
                        format!("“{}” has no voice assigned yet", row.character),
                    ),
                    Some(row) => match shown_line(app, &row.character, v.line.as_ref()) {
                        None => app.set_status(
                            Level::Warn,
                            "no lines in the scripts yet — nothing to test",
                        ),
                        Some(l) => {
                            v.line = Some(l.clone());
                            segment(app, job_tx, http, &row.character, &row.voice, &l.text);
                        }
                    },
                }
                app.screen = Screen::Cast(v);
            }
            // `T`: the held line, rendered — the deliberate generation behind
            // the cache-only `t` above.
            KeyCode::Char('T') if !ctrl && !alt => {
                let rows = app.cast_rows();
                let list = filtered_cast_rows(&rows, &v.filter);
                match list.get(v.cursor) {
                    None => app.set_status(Level::Warn, "nothing to audition"),
                    Some(row) if row.voice.is_empty() => app.set_status(
                        Level::Warn,
                        format!("“{}” has no voice assigned yet", row.character),
                    ),
                    Some(row) => {
                        let (character, voice) = (row.character.clone(), row.voice.clone());
                        v.line = audition(
                            app, job_tx, http, &character, &voice,
                            v.line.as_ref(), false,
                        );
                    }
                }
                app.screen = Screen::Cast(v);
            }
            KeyCode::Up => {
                v.cursor = v.cursor.saturating_sub(1);
                app.screen = Screen::Cast(v);
            }
            KeyCode::Down => {
                let last = filtered_cast_rows(&app.cast_rows(), &v.filter).len().saturating_sub(1);
                v.cursor = (v.cursor + 1).min(last);
                app.screen = Screen::Cast(v);
            }
            KeyCode::PageUp => {
                v.cursor = v.cursor.saturating_sub(8);
                app.screen = Screen::Cast(v);
            }
            KeyCode::PageDown => {
                let last = filtered_cast_rows(&app.cast_rows(), &v.filter).len().saturating_sub(1);
                v.cursor = (v.cursor + 8).min(last);
                app.screen = Screen::Cast(v);
            }
            KeyCode::Home => {
                v.cursor = 0;
                app.screen = Screen::Cast(v);
            }
            KeyCode::End => {
                v.cursor = filtered_cast_rows(&app.cast_rows(), &v.filter).len().saturating_sub(1);
                app.screen = Screen::Cast(v);
            }
            KeyCode::Backspace => {
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
                } else if c == 't' || c == 'T' {
                    // `^T`: another line, same voice. Case-insensitive — the
                    // terminal may report either case with CONTROL held.
                    let rows = app.cast_rows();
                    let list = filtered_cast_rows(&rows, &v.filter);
                    match list.get(v.cursor) {
                        None => app.set_status(Level::Warn, "nothing to audition"),
                        Some(row) if row.voice.is_empty() => app.set_status(
                            Level::Warn,
                            format!("“{}” has no voice assigned yet", row.character),
                        ),
                        Some(row) => {
                            let (character, voice) =
                                (row.character.clone(), row.voice.clone());
                            v.line = audition(
                                app, job_tx, http, &character, &voice,
                                v.line.as_ref(), true,
                            );
                        }
                    }
                }
                app.screen = Screen::Cast(v);
            }
            KeyCode::Char(c) if !alt => {
                v.filter.push(c);
                v.cursor = 0;
                v.scroll = 0;
                app.screen = Screen::Cast(v);
            }
            _ => {}
        }
        false
}
