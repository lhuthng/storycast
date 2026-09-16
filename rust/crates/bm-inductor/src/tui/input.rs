//! Keys in: the modal chain. Order is load-bearing — see the table in §5.
pub(crate) mod audition;
pub(crate) mod cast;
pub(crate) mod command;
mod confirm;
mod help;
mod machine;
mod normal;
mod picker;
mod run;
pub(crate) mod runconfig;
pub(crate) mod submit;
mod tasks;
mod text;

use crate::tui::{
    app::App,
    jobs::{op_job, Job},
    screen::Screen,
    style::Level,
};
use bm_proto::{OpRequest, Stage};
use crossterm::event::KeyEvent;

pub(crate) fn dispatch(app: &mut App, job_tx: &tokio::sync::mpsc::UnboundedSender<Job>, job: Job) {
    if job_tx.send(job).is_ok() {
        app.pending += 1;
    } else {
        app.set_status(Level::Error, "background worker is gone — restart the TUI");
    }
}

/// Fire a singleton op, refusing a duplicate while one is already running.
///
/// Returns whether it was actually dispatched. Callers that set an in-flight
/// marker of their own **must** branch on this: a refused dispatch sends no
/// `Done` event, so a marker set regardless is never cleared, and the screen
/// stays wedged behind a job that does not exist.
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
    dispatch(app, job_tx, op_job(app, http, req));
    true
}

/// Identity of one op *instance*.
///
/// Keyed by what the op acts on, not just by its name: retrying digest:3 and
/// digest:4 are two different jobs and must not suppress each other, while a
/// second press of the same key is still refused as a duplicate.
pub(crate) fn op_key(req: &OpRequest) -> String {
    format!(
        "{}|{}|{}|{}",
        req.op.as_str(),
        req.stage.map(Stage::as_str).unwrap_or("-"),
        req.chapter
            .map(|c| c.to_string())
            .unwrap_or_else(|| "-".into()),
        req.force.unwrap_or(false),
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

pub(crate) async fn handle_key(
    app: &mut App,
    key: KeyEvent,
    http: &reqwest::Client,
    job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
) -> bool {
    if let Screen::Confirm(c) = app.screen.clone() {
        return confirm::key_confirm(app, c, key, http, job_tx).await;
    }
    if let Screen::Help { scroll } = app.screen.clone() {
        return help::key_help(app, scroll, key).await;
    }
    if let Screen::Machine(_) = app.screen.clone() {
        return machine::key_machine(app, key).await;
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
    normal::normal_key(app, key, http, job_tx).await
}
