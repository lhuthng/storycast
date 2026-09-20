//! Cast overview: filterable table, read-only.
//!
//! Nothing here assigns anything — swapping happens in the picker (`:s`), and
//! this screen deliberately offers no shortcut to it. It opens in audition
//! focus: `t` tests the speaker's current voice on the shown line (cache
//! only), `T` renders the held line, `^T` re-rolls it. Any other letter
//! focuses the filter instead; while it is focused every letter types and
//! the audition keys go quiet. `Esc` blurs back, `^R` focuses explicitly.
//! (`q` is deliberately *not* bound either way, so it filters like any
//! other letter.)
use crate::tui::{
    app::App,
    input::audition::{cast_current, cast_pointed},
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
        // Esc blurs the filter first; only the second Esc closes. A blurred
        // `q` would quit Normal, but here every letter must stay typeable.
        KeyCode::Esc => {
            if v.filter_focus {
                v.filter_focus = false;
                app.set_status(
                    Level::Info,
                    "audition keys — t current · T line · ^T another",
                );
            } else {
                app.screen = Screen::Normal;
                return false;
            }
            app.screen = Screen::Cast(v);
        }
        KeyCode::Char('R') if !ctrl && !alt => app.load_roster(job_tx, http),
        // `t`: the speaker's current voice on the shown line, from cache
        // only — never synthesis. A miss names the render key instead of
        // playing something nearby. Audition focus only.
        KeyCode::Char('t') if !v.filter_focus && !ctrl && !alt => {
            cast_current(app, job_tx, http, &mut v);
            app.screen = Screen::Cast(v);
        }
        // `T`: the held line, rendered — the deliberate generation behind
        // the cache-only `t` above.
        KeyCode::Char('T') if !v.filter_focus && !ctrl && !alt => {
            cast_pointed(app, job_tx, http, &mut v, false);
            app.screen = Screen::Cast(v);
        }
        KeyCode::Char(':') => {
            // The command line works here too — `:current` / `:try` /
            // `:another` audition from the highlighted speaker in either
            // focus, which is also how to audition while filtered.
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
                // the terminal may report either case with CONTROL held.
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
            // above already claimed theirs in audition focus.
            v.filter_focus = true;
            v.filter.push(c);
            v.cursor = 0;
            v.scroll = 0;
            app.screen = Screen::Cast(v);
        }
        _ => {}
    }
    false
}
