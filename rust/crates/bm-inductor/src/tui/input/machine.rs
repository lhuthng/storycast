//! Machine detail: any dismiss key closes.
use crossterm::event::{KeyCode, KeyEvent};
use crate::tui::{app::App, screen::Screen};

pub(crate) async fn key_machine(app: &mut App, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char('i') => {
                app.screen = Screen::Normal;
            }
            _ => {}
        }
        false
}
