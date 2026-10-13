//! Machine detail: any dismiss key closes.
use crate::tui::input::Flow;
use crate::tui::{app::App, screen::Screen};
use crossterm::event::{KeyCode, KeyEvent};

pub(crate) async fn key_machine(app: &mut App, key: KeyEvent) -> Flow {
    match key.code {
        KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char('i') => {
            app.screen = Screen::Normal;
        }
        _ => {}
    }
    Flow::KeepRunning
}
