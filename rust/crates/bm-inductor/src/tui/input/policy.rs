//! Work-policy editor keys: reorder and toggle a machine's stages.
//!
//! The gesture is the operator's own: Space picks a row up, the arrows carry it
//! up or down the priority list, Space drops it. Enter toggles a stage on or
//! off. Every change is saved at once through the API, so the panel holds no
//! unsaved decision to lose.
//!
//! **Enabling a stage is also a statement about files.** What a box is given is
//! selected from this policy (`provision::sources`), and the files only move at
//! the next provision — so a box told it may merge, provisioned when it could
//! not, will be offered merge work with no clips on it. That does not fail:
//! the merge degrades a missing clip to one warning and mixes silence. So the
//! key that widens a policy says so, in the panel, at the moment it happens.
use crate::tui::{
    app::App,
    input::dispatch,
    jobs::Job,
    screen::{PolicyView, Screen},
    style::Level,
};
use crate::tui::input::Flow;
use crossterm::event::{KeyCode, KeyEvent};

pub(crate) async fn key_policy(
    app: &mut App,
    _view: PolicyView,
    key: KeyEvent,
    http: &reqwest::Client,
    job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
) -> Flow {
    let mut save = false;
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.screen = Screen::Normal;
            app.set_status(Level::Info, "closed the policy editor");
            return Flow::KeepRunning;
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
        // Computed while the view is borrowed, reported after it: `log_at` takes
        // `&mut app` and the dispatch below still needs the view it was handed.
        let mut widened: Option<String> = None;
        if let Screen::Policy(v) = &app.screen {
            // What the box's files now have to cover, against what it was
            // provisioned for. The machine in `app.machines` still holds the
            // *old* policy — the save is a round trip — so this compares the
            // edit against the truth rather than against itself.
            let old: Vec<bm_proto::Stage> = app
                .machines
                .iter()
                .find(|m| m.addr == v.addr)
                .map(|m| bm_core::provision::sources::stages_of(&m.effective_task_policy()))
                .unwrap_or_default();
            let new = bm_core::provision::sources::stages_of(&v.prefs);
            let gained: Vec<&str> = new
                .iter()
                .filter(|s| !old.contains(s))
                .map(|s| s.as_str())
                .collect();
            if !gained.is_empty() {
                widened = Some(format!(
                    "policy now covers {} — re-provision this box (p) or it is offered that work with no files for it",
                    gained.join("+")
                ));
            }
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
        if let Some(msg) = widened {
            app.log_at(Level::Warn, msg.clone());
            app.set_status(Level::Warn, msg);
        }
    }
    Flow::KeepRunning
}
