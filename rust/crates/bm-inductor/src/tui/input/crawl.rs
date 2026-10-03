//! The crawl view: scroll, expand, dismiss. Nothing here edits a setting — the
use crate::tui::input::Flow;
use crate::tui::{app::App, screen::Screen};
use crossterm::event::{KeyCode, KeyEvent};

pub(crate) async fn key_crawl(app: &mut App, scroll: usize, expanded: bool, key: KeyEvent) -> Flow {
    let at = |scroll: usize, expanded: bool| Screen::Crawl { scroll, expanded };
    match key.code {
        // Enter shows more rather than closing. Three keys already close the
        KeyCode::Enter => app.screen = at(0, !expanded),
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('c') => app.screen = Screen::Normal,
        KeyCode::Down | KeyCode::Char('j') => app.screen = at(scroll + 1, expanded),
        KeyCode::Up | KeyCode::Char('k') => app.screen = at(scroll.saturating_sub(1), expanded),
        KeyCode::PageDown => app.screen = at(scroll + 8, expanded),
        KeyCode::PageUp => app.screen = at(scroll.saturating_sub(8), expanded),
        KeyCode::Home => app.screen = at(0, expanded),
        // The end of the *long* screen is where an operator who expanded it to
        KeyCode::End => app.screen = at(usize::MAX / 2, expanded),
        _ => {}
    }
    Flow::KeepRunning
}
