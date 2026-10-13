//! Task ledger: filterable list plus the detail page. `retry_task` lives here.
use crate::tui::input::Flow;
use crate::tui::{
    app::App,
    input::{dispatch_op, op_key},
    jobs::Job,
    model::{filtered_tasks, Facet},
    screen::{Confirm, Screen, TaskDetail, TasksView, TextKind, TextPrompt},
    style::Level,
};
use bm_proto::{Op, OpRequest, Task, TaskState};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::BTreeSet;

/// The worker a row is **out** with, if any.
fn held_worker(t: &Task) -> Option<String> {
    if !matches!(t.state, TaskState::Assigned | TaskState::Running) {
        return None;
    }
    t.assigned_to.clone().or_else(|| t.racers.first().cloned())
}

/// What a facet change prints: the chip, and the size of the list it left.
fn facet_note(app: &App, filter: &str, facet: Facet, live: &BTreeSet<String>) -> String {
    let n = filtered_tasks(&app.tasks, filter, facet, live).len();
    format!("facet: {} — {n} task(s)", facet.label())
}

/// Dispatch a selective re-queue for one task.
pub(crate) fn retry_task(
    app: &mut App,
    job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
    http: &reqwest::Client,
    task: &Task,
    force: bool,
) {
    let req = OpRequest {
        op: Op::RetryTask,
        stage: Some(task.stage),
        chapter: Some(task.chapter),
        force: Some(force),
        ..Default::default()
    };
    if app.inflight.contains(&op_key(&req)) {
        app.set_status(
            Level::Warn,
            format!("{} is already being requeued", task.id()),
        );
        return;
    }
    let id = task.id();
    dispatch_op(app, job_tx, http, req);
    // The list stays open: watching the row leave Shelved *is* the confirmation.
    app.set_status(
        Level::Ok,
        if force {
            format!("{id} requeued, force — stale output cleared")
        } else {
            format!("{id} requeued — watch its state")
        },
    );
}

pub(crate) async fn key_tasks(
    app: &mut App,
    view: TasksView,
    key: KeyEvent,
    http: &reqwest::Client,
    job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
) -> Flow {
    let mut v = view;
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    // Who is still answering, read once: the `abandoned` facet and the release
    let live = app.live_worker_ids();
    let shown = filtered_tasks(&app.tasks, &v.filter, v.facet, &live);
    let last = shown.len().saturating_sub(1);
    // The highlighted task, cloned out here so the borrow of `app.tasks` ends
    let selected: Option<Task> = shown.get(v.cursor.min(last)).map(|t| (*t).clone());
    match key.code {
        // Esc and q both close. `q` is safe to bind here because no filter
        KeyCode::Esc | KeyCode::Char('q') => app.screen = Screen::Normal,
        KeyCode::Up => {
            v.cursor = v.cursor.saturating_sub(1);
            app.screen = Screen::Tasks(v);
        }
        KeyCode::Down => {
            v.cursor = (v.cursor + 1).min(last);
            app.screen = Screen::Tasks(v);
        }
        KeyCode::PageUp => {
            v.cursor = v.cursor.saturating_sub(8);
            app.screen = Screen::Tasks(v);
        }
        KeyCode::PageDown => {
            v.cursor = (v.cursor + 8).min(last);
            app.screen = Screen::Tasks(v);
        }
        KeyCode::Home => {
            v.cursor = 0;
            app.screen = Screen::Tasks(v);
        }
        KeyCode::End => {
            v.cursor = last;
            app.screen = Screen::Tasks(v);
        }
        // The facet cycle: one keypress per stage, state or "abandoned".
        KeyCode::Left | KeyCode::Right => {
            v.facet = v.facet.step(!matches!(key.code, KeyCode::Left));
            v.cursor = 0;
            v.scroll = 0;
            let note = facet_note(app, &v.filter, v.facet, &live);
            app.set_status(Level::Info, note);
            app.screen = Screen::Tasks(v);
        }
        KeyCode::Enter => match &selected {
            Some(t) => {
                let page = TaskDetail {
                    stage: t.stage,
                    chapter: t.chapter,
                    scroll: 0,
                    list: v,
                };
                app.screen = Screen::TaskDetail(page);
            }
            None => app.set_status(Level::Warn, "no task selected"),
        },
        // Ctrl-U widens: chips off, text cleared, cursor home — both halves of
        KeyCode::Char(c) if ctrl && c == 'u' => {
            v.filter.clear();
            v.facet = Facet::All;
            v.cursor = 0;
            v.scroll = 0;
            let note = facet_note(app, &v.filter, v.facet, &live);
            app.set_status(Level::Info, format!("widened — {note}"));
            app.screen = Screen::Tasks(v);
        }
        KeyCode::Char(_) if ctrl => {}
        KeyCode::Char('u') | KeyCode::Char('F') => {
            let force = matches!(key.code, KeyCode::Char('F'));
            match &selected {
                Some(t) => retry_task(app, job_tx, http, t, force),
                None => app.set_status(Level::Warn, "no task selected"),
            }
        }
        KeyCode::Char('R') => {
            // Bulk requeue, row-independent: every merge back to pending,
            dispatch_op(
                app,
                job_tx,
                http,
                OpRequest {
                    op: Op::Remerge,
                    ..Default::default()
                },
            );
            app.set_status(Level::Info, "requeueing every merge — render cache kept");
        }
        KeyCode::Char('E') => {
            // Bulk re-speak: worth one Enter, like every other destructive
            app.screen = Screen::Confirm(Confirm::rerender());
        }
        // Give the row's assignment back: the answer to a box that took a task
        KeyCode::Char('x') | KeyCode::Char('X') => match &selected {
            Some(t) if held_worker(t).is_some() => {
                let force = matches!(key.code, KeyCode::Char('X'));
                let id = t.id();
                let released = dispatch_op(
                    app,
                    job_tx,
                    http,
                    OpRequest {
                        op: Op::Release,
                        stage: Some(t.stage),
                        chapter: Some(t.chapter),
                        force: Some(force),
                        ..Default::default()
                    },
                );
                if released {
                    app.set_status(
                        Level::Info,
                        format!(
                            "releasing {id}{} — watch its state",
                            if force {
                                " (forced, its worker is still beating)"
                            } else {
                                ""
                            }
                        ),
                    );
                }
                app.screen = Screen::Tasks(v);
            }
            Some(t) => app.set_status(
                Level::Warn,
                format!("{} is not out with a worker — nothing to release", t.id()),
            ),
            None => app.set_status(Level::Warn, "no task selected"),
        },
        // Everything one box holds. The only release that asks first: it is a
        KeyCode::Char('W') => match selected.as_ref().and_then(held_worker) {
            Some(worker) => {
                let count = app
                    .tasks
                    .iter()
                    .filter(|t| {
                        matches!(t.state, TaskState::Assigned | TaskState::Running)
                            && t.is_holder(&worker)
                    })
                    .count();
                let beating = live.contains(&worker);
                app.screen = Screen::Confirm(Confirm::release_worker(worker, count, beating, v));
            }
            None => app.set_status(
                Level::Warn,
                "that row is not out with a worker — nothing to release",
            ),
        },
        // Requeue every assignment whose worker has no live beat. This is
        KeyCode::Char('A') => {
            if dispatch_op(
                app,
                job_tx,
                http,
                OpRequest {
                    op: Op::Requeue,
                    ..Default::default()
                },
            ) {
                app.set_status(
                    Level::Info,
                    "requeueing every assignment whose worker went quiet — watch the ledger",
                );
            }
            app.screen = Screen::Tasks(v);
        }
        KeyCode::Char(':') => {
            // A global command from the ledger. `u` stays a row-scoped
            app.command_return = Some(Screen::Tasks(v.clone()));
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::Command,
                ":",
                "command — u/F/x/X/W/A act on the ledger here, everything else is global",
                "",
            ));
            app.set_status(Level::Info, "command mode — Enter runs it, Esc closes");
        }
        KeyCode::Backspace => {
            v.filter.pop();
            v.cursor = 0;
            v.scroll = 0;
            app.screen = Screen::Tasks(v);
        }
        KeyCode::Char(c) if !alt => {
            v.filter.push(c);
            v.cursor = 0;
            v.scroll = 0;
            app.screen = Screen::Tasks(v);
        }
        _ => {}
    }
    Flow::KeepRunning
}

pub(crate) async fn key_task_detail(
    app: &mut App,
    view: TaskDetail,
    key: KeyEvent,
    http: &reqwest::Client,
    job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
) -> Flow {
    let mut v = view.clone();
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let task = app
        .tasks
        .iter()
        .find(|t| t.stage == v.stage && t.chapter == v.chapter)
        .cloned();
    match key.code {
        KeyCode::Esc => app.screen = Screen::Tasks(v.list),
        KeyCode::Char('q') => app.screen = Screen::Normal,
        KeyCode::Up | KeyCode::Char('k') => {
            v.scroll = v.scroll.saturating_sub(1);
            app.screen = Screen::TaskDetail(v);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            v.scroll += 1;
            app.screen = Screen::TaskDetail(v);
        }
        KeyCode::PageUp => {
            v.scroll = v.scroll.saturating_sub(8);
            app.screen = Screen::TaskDetail(v);
        }
        KeyCode::PageDown => {
            v.scroll += 8;
            app.screen = Screen::TaskDetail(v);
        }
        KeyCode::Home => {
            v.scroll = 0;
            app.screen = Screen::TaskDetail(v);
        }
        KeyCode::Char('u') | KeyCode::Char('F') if !ctrl => {
            let force = matches!(key.code, KeyCode::Char('F'));
            match &task {
                Some(t) => {
                    let t = t.clone();
                    retry_task(app, job_tx, http, &t, force);
                    // Stay on the page: the state field above updates in
                    app.screen = Screen::TaskDetail(v);
                }
                None => app.set_status(Level::Warn, "that task is no longer in the ledger"),
            }
        }
        _ => {}
    }
    Flow::KeepRunning
}
