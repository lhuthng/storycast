//! Cast overview: filterable table, read-only.
//!
//! Nothing here assigns anything — swapping happens in the picker (`:s`), and
//! this screen deliberately offers no shortcut to it. Auditioning lives on
//! the `:` command line (`:current` / `:try` / `:another`), so every letter
//! types into the filter.
use crate::tui::{
    app::App,
    jobs::Job,
    model::filtered_cast_rows,
    screen::{CastView, Screen, TextKind, TextPrompt},
    style::Level,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub(crate) async fn key_cast(
    app: &mut App,
    view: CastView,
    key: KeyEvent,
    http: &reqwest::Client,
    job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
) -> bool {
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
        KeyCode::Char(':') => {
            // The command line works here too — `:current` / `:try` /
            // `:another` audition from the highlighted speaker, now that
            // every letter types into the filter.
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
