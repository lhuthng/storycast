//! The crawl view: scroll and dismiss, like help. Nothing here edits a setting —
//! the view answers "what is in force", and a settings file is still a file.
use crate::tui::{app::App, screen::Screen};
use crossterm::event::{KeyCode, KeyEvent};

pub(crate) async fn key_crawl(app: &mut App, scroll: usize, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('c') | KeyCode::Enter => {
            app.screen = Screen::Normal;
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.screen = Screen::Crawl { scroll: scroll + 1 };
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.screen = Screen::Crawl {
                scroll: scroll.saturating_sub(1),
            };
        }
        KeyCode::PageDown => app.screen = Screen::Crawl { scroll: scroll + 8 },
        KeyCode::PageUp => {
            app.screen = Screen::Crawl {
                scroll: scroll.saturating_sub(8),
            }
        }
        KeyCode::Home => app.screen = Screen::Crawl { scroll: 0 },
        KeyCode::End => {
            app.screen = Screen::Crawl {
                scroll: usize::MAX / 2,
            }
        }
        _ => {}
    }
    false
}
