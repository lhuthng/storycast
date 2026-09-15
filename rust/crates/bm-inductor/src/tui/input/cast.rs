//! Cast overview: filterable table, Enter hands to the picker.
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use crate::tui::{
    app::App,
    jobs::Job,
    model::filtered_cast_rows,
    screen::{CastView, PickStage, Picker, Screen},
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
            KeyCode::Enter => {
                let rows = app.cast_rows();
                let list = filtered_cast_rows(&rows, &v.filter);
                match list.get(v.cursor) {
                    None => app.set_status(Level::Error, "no speaker selected"),
                    Some(row) => {
                        // Hand straight to step 2: the overview exists to make a
                        // reassignment, not only to be read.
                        let mut p = Picker::new();
                        p.character = row.character.clone();
                        p.stage = PickStage::Voice;
                        app.set_status(
                            Level::Info,
                            format!("choosing a voice for “{}”", row.character),
                        );
                        app.screen = Screen::Pick(p);
                    }
                }
            }
            KeyCode::Up => {
                v.cursor = v.cursor.saturating_sub(1);
                app.screen = Screen::Cast(v);
            }
            KeyCode::Down => {
                v.cursor += 1;
                app.screen = Screen::Cast(v);
            }
            KeyCode::PageUp => {
                v.cursor = v.cursor.saturating_sub(8);
                app.screen = Screen::Cast(v);
            }
            KeyCode::PageDown => {
                v.cursor += 8;
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
