//! Help: scroll only, any dismiss key closes.
use crate::tui::{app::App, screen::Screen};
use crossterm::event::{KeyCode, KeyEvent};

pub(crate) async fn key_help(app: &mut App, scroll: usize, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') | KeyCode::Enter => {
            app.screen = Screen::Normal;
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.screen = Screen::Help { scroll: scroll + 1 };
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.screen = Screen::Help {
                scroll: scroll.saturating_sub(1),
            };
        }
        KeyCode::PageDown => app.screen = Screen::Help { scroll: scroll + 8 },
        KeyCode::PageUp => {
            app.screen = Screen::Help {
                scroll: scroll.saturating_sub(8),
            }
        }
        _ => {}
    }
    false
}
