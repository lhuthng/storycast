//! Live cluster dashboard: machines, workers, tasks, events.
//!
//! The TUI owns no state beyond the screen. It reads `/api/state` and
//! `/api/roster`, and every operator key pushes a command onto a channel that a
//! background task executes — so provisioning, roster fetches and voice
//! previews never freeze the interface.
//!
//! Two rules hold throughout:
//!
//! * **No silent defaults.** Every prompt is prefilled with the value actually
//!   in force, and every argument is echoed before it is submitted.
//! * **No blank panes.** Each pane renders an explicit empty, loading or error
//!   state that says what to do next.

pub(crate) mod app;
pub(crate) mod audio;
pub(crate) mod audition;
pub(crate) mod clipboard;
pub(crate) mod crawl;
pub(crate) mod draw;
pub(crate) mod input;
pub(crate) mod jobs;
pub(crate) mod layout;
pub(crate) mod model;
pub(crate) mod screen;
pub(crate) mod sound;
pub(crate) mod style;
#[cfg(test)]
mod tests;

/// Event scrollback depth. Older lines fall off the top.
pub(crate) const EVENT_CAP: usize = 500;
/// `/api/state` poll period, in 200 ms ticks.
pub(crate) const REFRESH_TICKS: u64 = 4;

use crate::tui::{
    app::App,
    draw::draw,
    input::{dispatch, dispatch_op, handle_key, mouse::handle_mouse},
    jobs::DoneKind,
    jobs::{fetch_state, run_jobs, Ev, Job},
    model::reported_alias,
    style::{seen_label, worker_alias, Conn, Level},
};
use bm_core::Layout;
use bm_proto::{Heartbeat, Machine, Op, OpRequest, Task, TaskState};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::time::Duration;

pub async fn run(api: &str, layout: Layout) -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    // Enable capture through the initialized backend. Some terminals flush
    // the alternate-screen transition separately from the mouse mode, so
    // combining both commands before constructing the backend can leave the
    // app in raw mode without receiving mouse events.
    execute!(terminal.backend_mut(), EnableMouseCapture)?;
    let result = run_loop(api, layout, &mut terminal).await;
    // Always restore the terminal, even when the loop returned an error —
    // otherwise a crash leaves the operator in raw mode with no cursor.
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    result
}

async fn run_loop(
    api: &str,
    layout: Layout,
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
) -> anyhow::Result<()> {
    let mut app = App::new(api);
    app.layout = layout;
    // Read once here, not per frame: the footer shows it, and the footer is
    // redrawn on every keystroke.
    app.profile = bm_core::profile::read_pointer(&app.layout.root).ok();
    app.http = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        // The inductor is a LAN service — loopback for a solo run, a private
        // address for a cluster. A configured `HTTP_PROXY` would otherwise
        // intercept every poll and answer with its own body, which surfaces as
        // "bad state payload" and a dashboard that never connects. Same
        // reasoning as `api::sidecar_client`.
        .no_proxy()
        .build()?;
    let http = app.http.clone();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Ev>();
    let (job_tx, job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    tokio::spawn(run_jobs(job_rx, tx.clone()));

    // State poller: `/api/state` is fetched off the UI task and delivered over
    // the same channel as everything else. Polling inline used to freeze the
    // whole interface for as long as the request took — up to the client's 15s
    // timeout on a stalled network — with no repaint and no key handling in
    // between. Now a slow inductor just means the events pane goes quiet.
    let poll_http = http.clone();
    let poll_api = app.api.clone();
    let poll_tx = tx.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_millis(REFRESH_TICKS * 200));
        loop {
            ticker.tick().await;
            // The first tick fires immediately, so the dashboard fills without
            // waiting for a full period.
            if poll_tx
                .send(Ev::State(fetch_state(&poll_http, &poll_api).await))
                .is_err()
            {
                return; // the UI is gone
            }
        }
    });

    // One blocking fetch before the first draw, so the opening frame shows the
    // cluster rather than an empty shell. Failures are already tolerated.
    app.refresh(&http).await;
    loop {
        terminal.draw(|f| draw(f, &mut app))?;
        while let Ok(ev) = rx.try_recv() {
            // A finished add-sample refreshes a showing roster, so the new
            // voice is in the picker without a manual R. Read before `apply`
            // moves the event.
            let reload_roster = matches!(ev, Ev::Done(DoneKind::ReloadRoster));
            // A workspace switch or profile load moved the tree under us:
            // re-resolve before the next frame, or the panes keep showing
            // the book we just left.
            let relayout = matches!(ev, Ev::Done(DoneKind::Relayout));
            app.apply(ev);
            if reload_roster && app.roster.is_some() {
                app.load_roster(&job_tx, &http);
            }
            if relayout {
                app.relayout(&job_tx, &http);
            }
        }
        if event::poll(Duration::from_millis(200))? {
            match event::read()? {
                Event::Mouse(mouse) => {
                    if handle_mouse(&mut app, mouse, &http, &job_tx).await {
                        break;
                    }
                }
                // Terminals that report key release would otherwise fire every
                // binding twice.
                Event::Key(key)
                    if key.kind == KeyEventKind::Press
                        && handle_key(&mut app, key, &http, &job_tx).await =>
                {
                    break;
                }
                _ => {}
            }
        }
        // The key handler cannot reach the terminal, so `m` leaves its intent on
        // the App and the loop — the one place holding the terminal — carries
        // it out. Cleared unconditionally, including on the failing path, so a
        // refused toggle cannot be retried for ever.
        if std::mem::take(&mut app.mouse_toggle) {
            let r = if app.mouse_capture {
                execute!(terminal.backend_mut(), EnableMouseCapture)
            } else {
                execute!(terminal.backend_mut(), DisableMouseCapture)
            };
            // A terminal that refuses the mode change is not a reason to kill a
            // running dashboard; the status line already said what was asked.
            if let Err(e) = r {
                app.set_status(Level::Warn, format!("mouse mode unchanged: {e}"));
            }
        }
        app.tick += 1;
        // A `B` start left boxes to catch up. One job each, deliberately: the
        // start job ends the moment the inductor answers, and every box that
        // still needs work gets its own row and its own box resource, so they
        // provision at the same time instead of queueing behind one job that
        // holds the cluster for the whole catch-up.
        if let Some((machines, cancel)) = app.pending_catchup.take() {
            let settings_key = app.ssh_defaults().key;
            // Read once, before the loop: `dispatch` needs `&mut app`, so the
            // pieces of the job cannot be borrowed out of it in the call.
            let (layout, api) = (app.layout.clone(), app.api.clone());
            for machine in machines {
                if dispatch(
                    &mut app,
                    &job_tx,
                    Job::Provision {
                        layout: layout.clone(),
                        api: api.clone(),
                        machine,
                        force: false,
                        settings_key: settings_key.clone(),
                        cancel: Some(cancel.clone()),
                    },
                ) {
                    // `dispatch` sets `next_job_id` to the id it just handed
                    // out, which is the only way to name the job afterwards.
                    app.catchup_jobs.push(app.next_job_id);
                }
            }
            // The start sequence is not over: those boxes are still joining.
            // Holding the flag until the last one finishes is what keeps a
            // second `B` from queueing a duplicate push at every box.
            if !app.catchup_jobs.is_empty() {
                app.backend_start_outstanding = true;
            }
        }
        // A `B` start asked for work: fire it once the poller reports the
        // inductor is up, and only then.
        if app.conn == Conn::Up {
            if let Some((start, count)) = app.pending_enqueue.take() {
                dispatch_op(
                    &mut app,
                    &job_tx,
                    &http,
                    OpRequest {
                        op: Op::Translate,
                        start: Some(start),
                        count: Some(count),
                        ..Default::default()
                    },
                );
            }
        }
    }
    Ok(())
}

/// One plain-text snapshot of the cluster, then exit.
///
/// The TUI needs an alternate screen, colour and a keyboard, which rules it out
/// for screen readers, `watch`, CI and shell pipelines. This is the accessible
/// and scriptable view of exactly the same data.
pub async fn snapshot(api: &str) -> anyhow::Result<()> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        // Same as the dashboard's client above: the inductor is on loopback or
        // the LAN, and an ambient `HTTP_PROXY` would answer in its place.
        .no_proxy()
        .build()?;
    let base = api.trim_end_matches('/');
    let v: serde_json::Value = http
        .get(format!("{base}/api/state"))
        .send()
        .await?
        .json()
        .await?;

    let mut machines: Vec<Machine> =
        serde_json::from_value(v.get("machines").cloned().unwrap_or_default()).unwrap_or_default();
    let mut beats: Vec<Heartbeat> =
        serde_json::from_value(v.get("beats").cloned().unwrap_or_default()).unwrap_or_default();
    let mut tasks: Vec<Task> =
        serde_json::from_value(v.get("tasks").cloned().unwrap_or_default()).unwrap_or_default();
    machines.sort_by(|a, b| a.addr.cmp(&b.addr));
    beats.sort_by(|a, b| a.worker_id.cmp(&b.worker_id));
    tasks.sort_by_key(|t| (t.chapter, t.stage));

    println!("cluster @ {base}");
    println!("\nmachines ({})", machines.len());
    if machines.is_empty() {
        println!("  none — add one with: bm-inductor provision --addr <ip>");
    }
    for m in &machines {
        let now = bm_proto::now_secs();
        let workers = beats
            .iter()
            .filter(|b| b.addr == m.addr && now.saturating_sub(b.ts) < 90)
            .count();
        println!(
            "  {:<15} {:<13} workers={:<3} tts={:<22} seen={}",
            m.addr,
            m.state.as_str(),
            workers,
            m.tts_url.clone().unwrap_or_else(|| "-".into()),
            seen_label(m)
        );
        if !m.note.trim().is_empty() {
            println!("      note: {}", m.note.replace('\n', " "));
        }
    }

    println!("\nworkers ({})", beats.len());
    if beats.is_empty() {
        println!("  none — start one with: bm-agent worker --inductor <this host>");
    }
    for b in &beats {
        let name = reported_alias(&beats, &b.worker_id)
            .unwrap_or_else(|| worker_alias(&b.worker_id).0)
            .to_string();
        println!(
            "  {:<14} {:<8} ch{:<4} {:>3}%  {:<28} eta={}",
            name,
            b.stage.map(|s| s.as_str()).unwrap_or("-"),
            b.chapter
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".into()),
            (b.progress.clamp(0.0, 1.0) * 100.0).round() as u32,
            b.activity,
            b.eta_secs
                .map(bm_core::eta::human)
                .unwrap_or_else(|| "-".into())
        );
    }

    println!("\ntasks");
    match v.get("counts").and_then(|c| c.as_object()) {
        None => println!("  no task data"),
        Some(obj) if obj.is_empty() => println!("  none queued"),
        Some(obj) => {
            let mut stages: Vec<&String> = obj.keys().collect();
            stages.sort();
            for st in stages {
                let c = &obj[st.as_str()];
                let get = |k: &str| c.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
                let total: u64 = c
                    .as_object()
                    .map(|m| m.values().filter_map(|x| x.as_u64()).sum())
                    .unwrap_or(0);
                println!(
                    "  {st:<8} {}/{} done  {} open  {} failed  {} shelved",
                    get("done"),
                    total,
                    total
                        .saturating_sub(get("done"))
                        .saturating_sub(get("shelved")),
                    get("failed"),
                    get("shelved")
                );
            }
        }
    }
    let shelved: Vec<String> = {
        let mut s: Vec<String> = tasks
            .iter()
            .filter(|t| t.state == TaskState::Shelved)
            .map(|t| format!("{}:{}", t.stage, t.chapter))
            .collect();
        s.sort();
        s.dedup();
        s
    };
    if !shelved.is_empty() {
        println!("  shelved: {}", shelved.join(" "));
    }
    Ok(())
}
