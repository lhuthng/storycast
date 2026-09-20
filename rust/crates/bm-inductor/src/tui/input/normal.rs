//! Normal mode: operator keys. Destructive actions live behind `:`.
use crate::tui::{
    app::App,
    input::{dispatch, runconfig::run_preview},
    jobs::Job,
    screen::{Confirm, ConfirmAction, Screen, TasksView, TextKind, TextPrompt},
    style::{Conn, Level},
};
use crossterm::event::{KeyCode, KeyEvent};
use std::sync::{atomic::AtomicBool, Arc};

pub(crate) async fn normal_key(
    app: &mut App,
    key: KeyEvent,
    http: &reqwest::Client,
    job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
) -> bool {
    match key.code {
        KeyCode::Char('q') => {
            if app.pending > 0 {
                app.screen = Screen::Confirm(Confirm {
                    title: "Quit with work in flight?".into(),
                    danger: true,
                    body: vec![
                        format!("{} background job(s) are still running.", app.pending),
                        "The inductor keeps working without the TUI, but you will lose".into(),
                        "the event log and any in-flight result.".into(),
                    ],
                    action: ConfirmAction::Quit,
                });
            } else {
                return true;
            }
        }
        KeyCode::Char('?') => app.screen = Screen::Help { scroll: 0 },
        KeyCode::Char(':') => {
            // Command mode: every operator action behind a prompt, so a stray
            // keypress can never provision, reconcile or stop anything.
            // Words first (`:reconcile`), single letters still work (`:m`).
            app.command_return = Some(Screen::Normal);
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::Command,
                ":",
                "command — a word (:add :prov :reconcile :quit) or a letter (:m :B :X)",
                "",
            ));
            app.set_status(Level::Info, "command mode — Enter runs it, Esc closes");
        }
        KeyCode::Char('C') => {
            app.colour = !app.colour;
            let on = if app.colour { "on" } else { "off" };
            app.set_status(Level::Info, format!("colour {on}"));
        }
        KeyCode::Char('r') => {
            app.refresh(http).await;
            app.set_status(Level::Ok, "refreshed");
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.selected = app.selected.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.selected = (app.selected + 1).min(app.machines.len().saturating_sub(1));
        }
        KeyCode::Home => app.selected = 0,
        KeyCode::End => app.selected = app.machines.len().saturating_sub(1),
        KeyCode::PageUp => {
            app.events_scroll = app.events_scroll.saturating_add(5);
        }
        KeyCode::PageDown => {
            app.events_scroll = app.events_scroll.saturating_sub(5);
        }
        KeyCode::Char('G') => {
            app.events_scroll = 0;
            app.set_status(Level::Info, "event log pinned to newest");
        }
        KeyCode::Char('i') => match app.selected_machine() {
            None => app.set_status(Level::Warn, "no machine selected"),
            Some(m) => app.screen = Screen::Machine(m.addr.clone()),
        },
        KeyCode::Char('B') => {
            // Backend up now, boxes join in background — a second press
            // while the first sequence runs would provision everything
            // twice, so it is refused instead of queued.
            if app.backend_start_outstanding {
                app.set_status(Level::Warn, "backend start already running — watch events");
            } else {
                let cfg = run_preview(app);
                let cancel = Arc::new(AtomicBool::new(false));
                app.start_cancel = Some(cancel.clone());
                app.backend_start_outstanding = true;
                dispatch(
                    app,
                    job_tx,
                    Job::StartBackend {
                        layout: app.layout.clone(),
                        api: app.api.clone(),
                        api_up: app.conn == Conn::Up,
                        start: cfg.start,
                        count: cfg.count,
                        enqueue: false,
                        machines: app.effective_machines(),
                        cancel,
                        settings_key: app.ssh_defaults().key,
                    },
                );
                app.set_status(
                    Level::Info,
                    "starting backend now — boxes join in background; watch events",
                );
            }
        }
        KeyCode::Char('R') => {
            app.screen = Screen::Run;
            if app.roster.is_none() {
                app.load_roster(job_tx, http);
            }
        }
        // The task ledger. Capital K so the lowercase `k` can stay "move up" —
        // and so it reads as the sibling of `R` (run screen) and `S` (cast).
        KeyCode::Char('K') => {
            app.screen = Screen::Tasks(TasksView::new());
        }
        // The background jobs (what the footer's "N job(s) running" actually
        // is). Sibling of `K`: capital so lowercase `j` stays "move down".
        KeyCode::Char('J') => {
            let previous = Box::new(app.screen.clone());
            app.screen = Screen::Jobs {
                scroll: 0,
                previous,
            };
        }
        // Operator commands fire from the `:` line only: a stray keypress
        // must never provision, reconcile or stop anything. This arm catches
        // every gated key before the fallthrough swallows it silently.
        KeyCode::Char(c)
            if matches!(
                c,
                'a' | 'A'
                    | 'N'
                    | 'p'
                    | 'P'
                    | 'd'
                    | 't'
                    | 'c'
                    | 'v'
                    | 's'
                    | 'S'
                    | 'e'
                    | 'u'
                    | 'm'
                    | 'B'
                    | 'X'
            ) =>
        {
            app.set_status(
                Level::Warn,
                format!("use ':{c}' — operator commands live on the command line"),
            );
        }
        _ => {}
    }
    false
}
