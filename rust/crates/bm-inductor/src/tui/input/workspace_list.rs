//! Keys for `:ws` with nothing typed: the books, chosen with arrows.
//!
//! The rows are built by [`crate::tui::screen::book_items`], next to the screen
//! they fill, so `:ws` and a test read the same tree the same way.
use crate::tui::input::Flow;
use crate::tui::{
    app::App,
    input::dispatch,
    jobs::{Job, WorkspaceReq},
    screen::{Screen, WsList},
    style::Level,
};
use crossterm::event::{KeyCode, KeyEvent};

pub(crate) async fn key_workspace_list(
    app: &mut App,
    mut ws: WsList,
    key: KeyEvent,
    job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
) -> Flow {
    ws.error = None;
    match key.code {
        KeyCode::Esc => {
            app.screen = app.back_out();
            app.set_status(Level::Info, "cancelled — the workspace was not switched");
        }
        KeyCode::Up | KeyCode::Char('k') => ws.move_cursor(false),
        KeyCode::Down | KeyCode::Char('j') => ws.move_cursor(true),
        KeyCode::Home => ws.cursor = 0,
        KeyCode::End => ws.cursor = ws.rows.len().saturating_sub(1),
        KeyCode::Enter => {
            // A name the list cannot vouch for is refused here, with the reason
            // drawn under it, rather than dispatched as a pointer write that
            // moves the operator onto a directory no command can read.
            if let Some(why) = ws.unusable.get(&ws.cursor).cloned() {
                ws.error = Some(why);
                app.screen = Screen::WorkspaceList(ws);
                return Flow::KeepRunning;
            }
            let Some(item) = ws.rows.get(ws.cursor).cloned() else {
                // An empty `workspaces/` is not a switch; say so rather than
                // closing the screen on a keypress that did nothing.
                ws.error = Some("no workspaces here — `:ws new <name>` creates one".into());
                app.screen = Screen::WorkspaceList(ws);
                return Flow::KeepRunning;
            };
            let job = Job::Workspace {
                layout: app.layout.clone(),
                api: app.api.clone(),
                req: WorkspaceReq::Use(item.value.clone()),
            };
            dispatch(app, job_tx, job);
            app.screen = app.back_out();
            app.set_status(
                Level::Ok,
                format!("submitted: switch to workspace {}", item.value),
            );
        }
        _ => {}
    }
    // A screen that handles a key says so by *not* quitting: the bool out of
    // `handle_key` is the main loop's exit, not "was this handled". Returning
    // it the other way round is what made the first version of this screen
    // close the app on every arrow.
    if matches!(app.screen, Screen::WorkspaceList(_)) {
        app.screen = Screen::WorkspaceList(ws);
    }
    Flow::KeepRunning
}
