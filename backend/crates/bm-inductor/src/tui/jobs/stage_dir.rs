//! One stage folder pointed elsewhere: hold, let the work in flight land,
//! move, save, resume.

use super::*;
use bm_core::stage_dir::StageDir;
use bm_proto::{Op, OpRequest, OpResult};

/// How long a move waits for the tasks already out before giving up.
const WAIT_CAP: Duration = Duration::from_secs(600);

pub(crate) async fn job_move_stage_dir(
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    layout: bm_core::Layout,
    api: String,
    http: reqwest::Client,
    kind: StageDir,
    raw: String,
) {
    let label = kind.label();
    // Re-plan here, not at the prompt: the job may have queued behind others,
    // and the folder it leaves is whatever the settings say when it runs.
    let change = match layout.plan_stage_dir(kind, &raw) {
        Ok(c) => c,
        Err(e) => return finish(&tx, Level::Error, format!("{label} folder: {e:#}")),
    };
    let from = change.from.display().to_string();
    let to = change.to.display().to_string();

    // Hold first: everything below assumes nothing new starts.
    let resume = match hold(&tx, &http, &api, label).await {
        Some(resume) => resume,
        None => return,
    };

    if let Err(e) = wait_still(&tx, &http, &api, label).await {
        return resume_and_finish(&tx, &http, &api, resume, Level::Warn, e).await;
    }

    // A cross-volume copy is minutes of filesystem work: never on the draw loop.
    let moving = {
        let layout = layout.clone();
        let change = change.clone();
        tokio::task::spawn_blocking(move || layout.move_stage_dir(&change)).await
    };
    let moved = match moving {
        Ok(Ok(moved)) => moved,
        Ok(Err(e)) => {
            return resume_and_finish(
                &tx,
                &http,
                &api,
                resume,
                Level::Error,
                format!("{label} folder: {e:#} - nothing moved"),
            )
            .await
        }
        Err(e) => {
            return resume_and_finish(
                &tx,
                &http,
                &api,
                resume,
                Level::Error,
                format!("{label} folder: the move crashed: {e} - check {from} and {to}"),
            )
            .await
        }
    };
    if let Err(e) = layout.apply_stage_dir(&change) {
        return resume_and_finish(
            &tx,
            &http,
            &api,
            resume,
            Level::Error,
            format!(
                "{label} folder: {} item(s) are in {to}, but the setting did not save: \
                 {e:#} - point it there by hand",
                moved.entries
            ),
        )
        .await;
    }
    let line = if moved.entries == 0 {
        format!("{label} folder: nothing was there to move - the setting now points at {to}")
    } else {
        format!(
            "{label} folder: moved {} item(s) from {from} to {to}",
            moved.entries
        )
    };
    resume_and_finish(&tx, &http, &api, resume, Level::Ok, line).await;
}

/// Stop distribution before anything touches the disk. `Some(resume)` says
/// whether it was distributing, and only then is it ours to give back;
/// `None` means the hold could not be taken and nothing should move.
async fn hold(
    tx: &tokio::sync::mpsc::UnboundedSender<Ev>,
    http: &reqwest::Client,
    api: &str,
    label: &str,
) -> Option<bool> {
    match fetch_state(http, api).await {
        Ok(snap) => {
            if snap.held() {
                return Some(false);
            }
            match post_op(
                http,
                api,
                OpRequest {
                    op: Op::Dispatch,
                    go: Some(false),
                    ..Default::default()
                },
            )
            .await
            {
                Ok(msg) => {
                    send(tx, Level::Info, format!("{label} folder: {msg}"));
                    Some(true)
                }
                Err(e) => {
                    send(
                        tx,
                        Level::Error,
                        format!("{label} folder: could not hold the cluster: {e} - nothing moved"),
                    );
                    let _ = tx.send(Ev::Done(DoneKind::Other));
                    None
                }
            }
        }
        Err(e) => {
            // No inductor, no offers: there is nothing to hold and nothing
            // that can be writing while the folders move.
            send(
                tx,
                Level::Warn,
                format!("{label} folder: {e} - nothing is running, moving anyway"),
            );
            Some(false)
        }
    }
}

/// Wait for the tasks already out to land: a render writes takes, a merge
/// reads them and writes the pair, and moving under either breaks it.
async fn wait_still(
    tx: &tokio::sync::mpsc::UnboundedSender<Ev>,
    http: &reqwest::Client,
    api: &str,
    label: &str,
) -> Result<(), String> {
    let started = Instant::now();
    let mut reported = usize::MAX;
    loop {
        let busy = match fetch_state(http, api).await {
            Ok(snap) => snap.writing(),
            // The inductor went away while we waited: nothing can be writing.
            Err(_) => return Ok(()),
        };
        if busy == 0 {
            return Ok(());
        }
        if started.elapsed() > WAIT_CAP {
            return Err(format!(
                "{label} folder: {busy} task(s) still out after {} min - nothing moved; \
                 try again once they land",
                WAIT_CAP.as_secs() / 60
            ));
        }
        if busy != reported {
            send(
                tx,
                Level::Info,
                format!("{label} folder: holding - waiting for {busy} task(s) to finish"),
            );
            reported = busy;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// Give distribution back when it was ours to stop, then report the verdict.
async fn resume_and_finish(
    tx: &tokio::sync::mpsc::UnboundedSender<Ev>,
    http: &reqwest::Client,
    api: &str,
    resume: bool,
    level: Level,
    line: String,
) {
    send(tx, level, line);
    if resume {
        let go = OpRequest {
            op: Op::Dispatch,
            go: Some(true),
            ..Default::default()
        };
        match post_op(http, api, go).await {
            Ok(msg) => send(tx, Level::Info, msg),
            Err(e) => send(
                tx,
                Level::Warn,
                format!("the folders are set, but distribution is still held: {e} - :go"),
            ),
        }
    }
    let _ = tx.send(Ev::Done(DoneKind::Other));
}

fn finish(tx: &tokio::sync::mpsc::UnboundedSender<Ev>, level: Level, line: String) {
    send(tx, level, line);
    let _ = tx.send(Ev::Done(DoneKind::Other));
}

async fn post_op(http: &reqwest::Client, api: &str, req: OpRequest) -> Result<String, String> {
    let url = format!("{}/api/op", api.trim_end_matches('/'));
    let res = http
        .post(&url)
        .json(&req)
        .send()
        .await
        .map_err(|e| unreachable_verdict(api, e.is_connect(), &e.to_string()))?
        .json::<OpResult>()
        .await
        .map_err(|e| format!("bad op result: {e}"))?;
    if res.ok {
        Ok(res.message)
    } else {
        Err(res.message)
    }
}
