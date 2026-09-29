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
    // queued row is the whole answer to "why is this not running": a job that
    // waits says what it waits for.
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
    dispatch(app, job_tx, op_job(app, http, req))
}

/// Identity of one op *instance*.
///
/// Keyed by what the op acts on, not just by its name: retrying digest:3 and
/// digest:4 are two different jobs and must not suppress each other, while a
/// second press of the same key is still refused as a duplicate.
///
/// The two widest fields are in the key for the same reason. A `release` that
/// names one **worker** is not the release of one row, and two `fix-speaker`
/// ops on one chapter are two different segments — without them here the second
/// press of either is refused as a duplicate of the first, which is a lie about
/// work that was never dispatched.
///
/// `dispatch`'s direction is in the key for the same reason, and it is the one
/// where getting it wrong is worst: `:go` and `:hold` share an op and an empty
/// payload, so without the flag a `:hold` pressed while the `:go` round trip is
/// still out is refused as "dispatch is already running" — the opposite of what
/// was asked for, and silent apart from a status line nobody would read as a
/// refusal to hold.
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

pub(crate) async fn handle_key(
    app: &mut App,
    key: KeyEvent,
    http: &reqwest::Client,
    job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
) -> bool {
    // Tab is the footer's own key: "the other side of the dashboard". A screen
    // may spend it on something of its own (the sound editor's layer tabs, the
    // jobs view's own toggle), and the three modal screens swallow every press —
    // but everywhere else it means what the footer says, and it comes back to
    // the screen it was pressed on, exactly like the jobs view already did from
    // the dashboard.
    if key.code == KeyCode::Tab && tab_is_free(&app.screen) {
        let previous = Box::new(app.screen.clone());
        app.screen = Screen::Jobs {
            scroll: 0,
            previous,
        };
        return false;
    }
    let before = app.screen.clone();
    let quit = route(app, key, http, job_tx).await;
    note_layer(app, before, key.code);
    quit
}

/// Whether screens have spent `Tab` on something of their own.
///
/// The three modal screens are on the list for a different reason: their keys
/// are a closed set, and a `Tab` that swapped a confirmation for the jobs view
/// would lose the question it was about to ask.
fn tab_is_free(screen: &Screen) -> bool {
    !matches!(
        screen,
        Screen::Sound(_)
            | Screen::Jobs { .. }
            | Screen::Text(_)
            | Screen::Confirm(_)
            | Screen::Pick(_)
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
/// the sound editor's own layers: `Esc` closes them to the screen they were
/// opened from, so `:script` from Normal goes back to Normal, but the two
/// Escs *inside* the window are its own before that. Kept out of
/// [`is_layer`] on purpose: they are not drawn over another screen, and the
/// `:`-return logic would double-unwrap them.
fn is_place_with_exit(screen: &Screen) -> bool {
    matches!(screen, Screen::Script(_) | Screen::Sound(_) | Screen::Digest(_))
}

/// Remember where a layer was opened over, once the key that opened it has run.
///
/// **After the handler, not before**, because only the handler knows whether
/// this keypress opened a layer or cancelled one — and from the outside the two
/// are the same pair of screens in the same order. So the cancel keys push
/// nothing (the arm that ran has already popped), a place screen is not a layer
/// at all, and a layer that was *answered* rather than cancelled pops the entry
/// it was holding, which is what keeps the stack from drifting away from the
/// screens actually on it.
fn note_layer(app: &mut App, before: Screen, code: KeyCode) {
    let after = app.screen.clone();
    // The `:` line ran: the prompt is spent, and what the command opened sits
    // over the screen the command ran in. Without this, `:m` on the dashboard
    // would raise its confirmation over a prompt that is no longer on screen,
    // and cancelling it would bring the dead prompt back.
    if let Some(parent) = app.prompt_spent.take() {
        if is_layer(&after) || is_place_with_exit(&after) {
            // A layer sits over `parent`; a `:`-opened window with its own
            // internal Esc ladder closes to `parent` when its ladder runs
            // out. Same entry shape, same one-`back_out` exit.
            app.back.push(parent);
        } else {
            // The command left a place screen on top: the prompt is gone, and
            // so is the entry it pushed on the way in.
            app.back.pop();
        }
        return;
    }
    if matches!(
        code,
        KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N')
    ) {
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
///
/// Split from [`handle_key`] so the recursion behind a `:` command — which
/// presses the key the word names — cannot push a second layer entry for one
/// keypress. Every key runs exactly one [`note_layer`].
pub(crate) async fn route(
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
    if let Screen::Crawl { scroll } = app.screen.clone() {
        return crawl::key_crawl(app, scroll, key).await;
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
    normal::normal_key(app, key, http, job_tx).await
}
