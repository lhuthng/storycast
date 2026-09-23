//! The jobs overlay: what the background lanes are actually doing, scroll only.
use crate::tui::{app::App, screen::Screen};
use crossterm::event::{KeyCode, KeyEvent};

pub(crate) async fn key_jobs(
    app: &mut App,
    scroll: usize,
    previous: Screen,
    key: KeyEvent,
) -> bool {
    match key.code {
        // Tab closes what Tab opened — the same toggle shape the sound
        // editor's layer tabs use, so the key behaves the same everywhere.
        KeyCode::Esc | KeyCode::Tab | KeyCode::Char('q') | KeyCode::Char('J') | KeyCode::Enter => {
            app.screen = previous;
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.screen = Screen::Jobs {
                scroll: scroll + 1,
                previous: Box::new(previous),
            };
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.screen = Screen::Jobs {
                scroll: scroll.saturating_sub(1),
                previous: Box::new(previous),
            };
        }
        KeyCode::PageDown => {
            app.screen = Screen::Jobs {
                scroll: scroll + 8,
                previous: Box::new(previous),
            };
        }
        KeyCode::PageUp => {
            app.screen = Screen::Jobs {
                scroll: scroll.saturating_sub(8),
                previous: Box::new(previous),
            };
        }
        _ => {}
    }
    false
}
