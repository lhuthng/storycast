//! Keys in: the modal chain. Order is load-bearing — see the table in §5.
pub(crate) mod audition;
pub(crate) mod cast;
mod cloud;
pub(crate) mod command;
mod confirm;
mod crawl;
mod digest;
mod help;
mod jobs;
pub(crate) mod llm;
mod machine;
pub(crate) mod mouse;
mod normal;
mod picker;
mod policy;
mod run;
pub(crate) mod runconfig;
pub(crate) mod script;
pub(crate) mod sound;
pub(crate) mod submit;
mod tasks;
mod text;
mod workspace_list;
mod workspace_new;

use crate::tui::{
    app::App,
    jobs::{op_job, BackgroundJob, Job},
    screen::Screen,
    style::Level,
};
use bm_proto::{OpRequest, Stage};
use crossterm::event::{KeyCode, KeyEvent};

pub(crate) fn dispatch(
    app: &mut App,
    job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
    job: Job,
) -> bool {
    let Some(id) = app.next_job_id.checked_add(1) else {
        let pending = app.pending;
        app.apply(crate::tui::jobs::Ev::Done(job.fallback_done()));
        app.pending = pending;
        app.set_status(
            Level::Error,
            "background job IDs exhausted — restart the TUI",
        );
        return false;
    };
    let name = job.label();
    // Read before the job is moved onto the channel. Naming the resource on a
    let needs = job.resource_label();
    let queued = std::time::Instant::now();
    match job_tx.send(Job::Tracked {
        id,
        job: Box::new(job.into_bare()),
    }) {
        Ok(()) => {
            app.next_job_id = id;
            app.pending += 1;
            app.background_jobs.push(BackgroundJob {
                id,
                name,
                queued,
                started: None,
                activity: match needs {
                    Some(what) => format!("queued · needs {what}"),
                    None => "queued".into(),
                },
            });
            true
        }
        Err(err) => {
            let pending = app.pending;
            app.apply(crate::tui::jobs::Ev::Done(err.0.fallback_done()));
            app.pending = pending;
            app.set_status(Level::Error, "background worker is gone — restart the TUI");
            false
        }
    }
}

/// Fire a singleton op, refusing a duplicate while one is already running.
pub(crate) fn dispatch_op(
    app: &mut App,
    job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
    http: &reqwest::Client,
    req: OpRequest,
) -> bool {
    let key = op_key(&req);
    if app.inflight.contains(&key) {
        app.set_status(
            Level::Warn,
            format!("{} is already running", req.op.as_str()),
        );
        return false;
    }
    app.inflight.push(key);
    dispatch(app, job_tx, op_job(app, http, req))
}

/// Identity of one op *instance*.
pub(crate) fn op_key(req: &OpRequest) -> String {
    format!(
        "{}|{}|{}|{}|{}|{}|{}",
        req.op.as_str(),
        req.stage.map(Stage::as_str).unwrap_or("-"),
        req.chapter
            .map(|c| c.to_string())
            .unwrap_or_else(|| "-".into()),
        req.force.unwrap_or(false),
        req.segment
            .map(|s| s.to_string())
            .unwrap_or_else(|| "-".into()),
        req.worker.as_deref().unwrap_or("-"),
        req.go.unwrap_or(true),
    )
}

/// Percent-encode the few characters that can appear in an address query.
pub(crate) fn urlencode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            c => c.to_string().bytes().map(|b| format!("%{b:02X}")).collect(),
        })
        .collect()
}

/// What a key or a click asks the event loop to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flow {
    /// Keep running, whatever the key did. Every screen's answer unless the
    KeepRunning,
    /// Leave the loop: the operator quit.
    Quit,
}

pub(crate) async fn handle_key(
    app: &mut App,
    key: KeyEvent,
    http: &reqwest::Client,
    job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
) -> Flow {
    // Tab is the footer's own key: "the other side of the dashboard". A screen
    if key.code == KeyCode::Tab && tab_is_free(&app.screen) {
        let previous = Box::new(app.screen.clone());
        app.screen = Screen::Jobs {
            scroll: 0,
            previous,
        };
        return Flow::KeepRunning;
    }
    let before = app.screen.clone();
    let flow = route(app, key, http, job_tx).await;
    note_layer(app, before, key.code);
    flow
}

/// Whether screens have spent `Tab` on something of their own.
fn tab_is_free(screen: &Screen) -> bool {
    !matches!(
        screen,
        Screen::Sound(_)
            | Screen::Jobs { .. }
            | Screen::Text(_)
            | Screen::Confirm(_)
            | Screen::Pick(_)
            | Screen::WorkspaceList(_)
            | Screen::WorkspaceNew(_)
    )
}

/// The screens that are drawn *over* another one rather than replacing it.
fn is_layer(screen: &Screen) -> bool {
    matches!(
        screen,
        Screen::Confirm(_) | Screen::Text(_) | Screen::Pick(_)
    )
}

/// The `:`-opened windows that are a *step*, not a floor. They behave like
fn is_place_with_exit(screen: &Screen) -> bool {
    matches!(
        screen,
        Screen::Script(_) | Screen::Sound(_) | Screen::Digest(_)
    )
}

/// Remember where a layer was opened over, once the key that opened it has run.
fn note_layer(app: &mut App, before: Screen, code: KeyCode) {
    let after = app.screen.clone();
    // The `:` line ran: the prompt is spent, and what the command opened sits
    if let Some(parent) = app.prompt_spent.take() {
        if is_layer(&after) || is_place_with_exit(&after) {
            // A layer sits over `parent`; a `:`-opened window with its own
            app.back.push(parent);
        } else {
            // The command left a place screen on top: the prompt is gone, and
            app.back.pop();
        }
        return;
    }
    if matches!(code, KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N')) {
        return;
    }
    if is_layer(&after) {
        if std::mem::discriminant(&after) != std::mem::discriminant(&before) {
            app.back.push(before);
        }
    } else if is_layer(&before) {
        app.back.pop();
    }
}

/// Route one key to the screen that owns it.
pub(crate) async fn route(
    app: &mut App,
    key: KeyEvent,
    http: &reqwest::Client,
    job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
) -> Flow {
    if let Screen::Confirm(c) = app.screen.clone() {
        return confirm::key_confirm(app, c, key, http, job_tx).await;
    }
    if let Screen::Help { scroll } = app.screen.clone() {
        return help::key_help(app, scroll, key).await;
    }
    if let Screen::Crawl { scroll, expanded } = app.screen.clone() {
        return crawl::key_crawl(app, scroll, expanded, key).await;
    }
    if let Screen::Machine(_) = app.screen.clone() {
        return machine::key_machine(app, key).await;
    }
    if let Screen::Jobs { scroll, previous } = app.screen.clone() {
        return jobs::key_jobs(app, scroll, *previous, key).await;
    }
    if let Screen::Text(prompt) = app.screen.clone() {
        return text::key_text(app, prompt, key, http, job_tx).await;
    }
    if let Screen::Pick(picker) = app.screen.clone() {
        return picker::key_picker(app, picker, key, http, job_tx).await;
    }
    if let Screen::Cast(view) = app.screen.clone() {
        return cast::key_cast(app, view, key, http, job_tx).await;
    }
    if let Screen::Tasks(view) = app.screen.clone() {
        return tasks::key_tasks(app, view, key, http, job_tx).await;
    }
    if let Screen::TaskDetail(view) = app.screen.clone() {
        return tasks::key_task_detail(app, view, key, http, job_tx).await;
    }
    if let Screen::Run = app.screen.clone() {
        return run::key_run(app, key, http, job_tx).await;
    }
    if let Screen::Sound(view) = app.screen.clone() {
        return sound::key_sound(app, view, key, http, job_tx).await;
    }
    if let Screen::Cloud(view) = app.screen.clone() {
        return cloud::key_cloud(app, view, key, http, job_tx).await;
    }
    if let Screen::Policy(view) = app.screen.clone() {
        return policy::key_policy(app, view, key, http, job_tx).await;
    }
    if let Screen::Digest(view) = app.screen.clone() {
        return digest::key_digest(app, view, key, http, job_tx).await;
    }
    if let Screen::Script(view) = app.screen.clone() {
        return script::key_script(app, view, key, http, job_tx).await;
    }
    if let Screen::Llm(view) = app.screen.clone() {
        return llm::key_llm(app, view, key, http, job_tx).await;
    }
    if let Screen::WorkspaceNew(ws) = app.screen.clone() {
        return workspace_new::key_workspace_new(app, ws, key, job_tx).await;
    }
    if let Screen::WorkspaceList(ws) = app.screen.clone() {
        return workspace_list::key_workspace_list(app, ws, key, job_tx).await;
    }
    normal::normal_key(app, key, http, job_tx).await
}
