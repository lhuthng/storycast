//! Work-policy editor keys: reorder and toggle a machine's stages.
//!
//! The gesture is the operator's own: Space picks a row up, the arrows carry it
//! up or down the priority list, Space drops it. Enter toggles a stage on or
//! off. Every change is saved at once through the API, so the panel holds no
//! unsaved decision to lose.
use crate::tui::{
    app::App,
    input::dispatch,
    jobs::Job,
    screen::{PolicyView, Screen},
    style::Level,
};
use crossterm::event::{KeyCode, KeyEvent};

pub(crate) async fn key_policy(
    app: &mut App,
    _view: PolicyView,
    key: KeyEvent,
    http: &reqwest::Client,
    job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
) -> bool {
    let mut save = false;
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.screen = Screen::Normal;
            app.set_status(Level::Info, "closed the policy editor");
            return false;
        }
        KeyCode::Enter => {
            if let Screen::Policy(v) = &mut app.screen {
                if let Some(p) = v.prefs.get_mut(v.cursor) {
                    p.enabled = !p.enabled;
                    save = true;
                }
            }
        }
        KeyCode::Char(' ') => {
            let mut msg = String::new();
            if let Screen::Policy(v) = &mut app.screen {
                v.grabbed = match v.grabbed {
                    Some(_) => None,
                    None => Some(v.cursor),
                };
                msg = match v.grabbed {
                    Some(i) => format!(
                        "grabbed {} — arrows move it, Space drops it",
                        v.prefs.get(i).map(|p| p.stage.as_str()).unwrap_or("?")
                    ),
                    None => "dropped it back in place".into(),
                };
            }
            app.set_status(Level::Info, msg);
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if let Screen::Policy(v) = &mut app.screen {
                match v.grabbed {
                    Some(g) if g > 0 => {
                        v.prefs.swap(g, g - 1);
                        v.grabbed = Some(g - 1);
                        v.cursor = g - 1;
                        save = true;
                    }
                    // Already at the top: a grab is not an error, it just
                    // cannot move further.
                    Some(_) => {}
                    None => v.cursor = v.cursor.saturating_sub(1),
                }
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Screen::Policy(v) = &mut app.screen {
                match v.grabbed {
                    Some(g) if g + 1 < v.prefs.len() => {
                        v.prefs.swap(g, g + 1);
                        v.grabbed = Some(g + 1);
                        v.cursor = g + 1;
                        save = true;
                    }
                    Some(_) => {}
                    None => v.cursor = (v.cursor + 1).min(v.prefs.len().saturating_sub(1)),
                }
            }
        }
        _ => {}
    }
    if save {
        if let Screen::Policy(v) = &app.screen {
            dispatch(
                app,
                job_tx,
                Job::SaveTaskPolicy {
                    api: app.api.clone(),
                    http: http.clone(),
                    addr: v.addr.clone(),
                    task_policy: v.prefs.clone(),
                },
            );
        }
    }
    false
}
