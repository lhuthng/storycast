//! Task ledger: filterable list plus the detail page. `retry_task` lives here.
use crate::tui::{
    app::App,
    input::{dispatch_op, op_key},
    jobs::Job,
    model::filtered_tasks,
    screen::{Screen, TaskDetail, TasksView, TextKind, TextPrompt},
    style::Level,
};
use bm_proto::{Op, OpRequest, Task};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Dispatch a selective re-queue for one task.
///
/// `force` also drops the artifact the stage would otherwise be judged complete
/// by — the difference between "offer it again" and "run it again".
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
) -> bool {
    let mut v = view;
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shown = filtered_tasks(&app.tasks, &v.filter);
    let last = shown.len().saturating_sub(1);
    // The highlighted task, cloned out here so the borrow of `app.tasks` ends
    // before any arm needs `&mut app` to dispatch. Every action below goes
    // through this value, so an index that points at the wrong row can never
    // re-queue the wrong chapter — the one mistake this screen must not make.
    let selected: Option<Task> = shown.get(v.cursor.min(last)).map(|t| (*t).clone());
    match key.code {
        // Esc and q both close. `q` is safe to bind here because no filter
        // term in the vocabulary contains it.
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
        // Ctrl-U clears the filter, so a Ctrl chord must never be read as a
        // plain `u` — that would re-queue a task while clearing the filter.
        KeyCode::Char(c) if ctrl && c == 'u' => {
            v.filter.clear();
            v.cursor = 0;
            v.scroll = 0;
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
        KeyCode::Char(':') => {
            // A global command from the ledger. `u` stays a row-scoped
            // direct key here; `:u` reaches the global retry instead.
            app.command_return = Some(Screen::Tasks(v.clone()));
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::Command,
                ":",
                "command — u/F stay row-scoped here, everything else is global",
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
    false
}

pub(crate) async fn key_task_detail(
    app: &mut App,
    view: TaskDetail,
    key: KeyEvent,
    http: &reqwest::Client,
    job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
) -> bool {
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
                    // place, which is the proof the retry landed.
                    app.screen = Screen::TaskDetail(v);
                }
                None => app.set_status(Level::Warn, "that task is no longer in the ledger"),
            }
        }
        _ => {}
    }
    false
}
