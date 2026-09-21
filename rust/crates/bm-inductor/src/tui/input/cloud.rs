//! Cloud view keys: navigate, re-read, close. Nothing destructive here —
//! `:up`/`:down` stay on the command line, like every other gated action.
use crate::tui::{
    app::App,
    input::dispatch,
    jobs::Job,
    screen::{CloudView, Screen},
    style::Level,
};
use crossterm::event::{KeyCode, KeyEvent};

pub(crate) async fn key_cloud(
    app: &mut App,
    _view: CloudView,
    key: KeyEvent,
    _http: &reqwest::Client,
    job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
) -> bool {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.screen = Screen::Normal;
            app.set_status(Level::Info, "closed the cloud view");
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if let Screen::Cloud(v) = &mut app.screen {
                v.cursor = v.cursor.saturating_sub(1);
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            let last = app.cloud.len().saturating_sub(1);
            if let Screen::Cloud(v) = &mut app.screen {
                v.cursor = (v.cursor + 1).min(last);
            }
        }
        KeyCode::Char('r') => {
            dispatch(
                app,
                job_tx,
                Job::AwsPool {
                    root: app.layout.root.clone(),
                    api: app.api.clone(),
                    http: _http.clone(),
                },
            );
            app.set_status(Level::Info, "re-reading the account…");
        }
        _ => {}
    }
    false
}
