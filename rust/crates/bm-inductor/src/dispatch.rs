//! The inductor drives. Nothing dials the inductor.
//!
//! Every worker in this design is a server that answers questions: `GET
//! /status` for what it is doing, `POST /task` to run one offer, `GET /unit`
//! for one rendered wav, `POST /shutdown` to stop. This module is the other
//! half — the loop that asks.
//!
//! Why it is shaped this way, in one paragraph: the pull protocol needed the
//! inductor to be reachable *from* every worker, which a box on the public
//! internet cannot do to a laptop behind NAT, and which forced a local/remote
//! fork through the launcher, the offer and the artifact path. Inverting the
//! direction removes the requirement rather than working around it — the
//! inductor already has a route to every worker, because it launched them.
//!
//! ## The one ordering that matters
//!
//! Units are collected **before** the completion is applied. The render
//! completion gate checks the files on disk and fails a report whose units
//! never landed, so applying the report first would fail every remote render
//! that had not already uploaded its wavs — a self-inflicted strike, three of
//! which shelve the chapter.

use crate::api::Shared;
use bm_core::Layout;
use bm_proto::{Heartbeat, Stage, TaskOffer};
use std::collections::HashMap;
use std::time::Duration;

/// How often each worker is asked how it is. Short enough that a task is
/// picked up promptly, long enough that the polls are noise — and, unlike the
/// pull protocol's lease, this is also the liveness signal: a worker that
/// stops answering is a worker that is gone.
const POLL: Duration = Duration::from_secs(2);

/// Asking a question. Anything that has not answered in this long is not
/// going to.
const ASK: Duration = Duration::from_secs(5);

/// Moving one wav. Generous, because a segment is a few hundred kB over
/// whatever link the box is on.
const FETCH: Duration = Duration::from_secs(60);

/// Everything needed to ask one box anything.
#[derive(Clone)]
struct Peer {
    addr: String,
    port: u16,
    token: String,
}

impl Peer {
    fn base(&self) -> String {
        format!("http://{}:{}", self.addr, self.port)
    }
}

/// A worker is a long-lived task keyed by address. One per box, each polling
/// on its own clock: a worker running a twenty-minute render must not stop the
/// inductor from asking after the others.
pub async fn run(state: Shared, layout: Layout) {
    let Some(token) = bm_core::token::read(&layout.root) else {
        println!("dispatch: no cluster token — every worker would refuse the request");
        return;
    };
    let Ok(http) = reqwest::Client::builder()
        // Worker addresses are loopback, LAN, or a cloud private network. An
        // ambient `HTTP_PROXY` answering in their place is the trap this repo
        // has already paid for three times.
        .no_proxy()
        .build()
    else {
        println!("dispatch: could not build an HTTP client");
        return;
    };

    let mut running: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
    let mut idle_since: Option<u64> = None;
    println!("dispatch: the inductor drives — workers are asked, never asked to call home");

    loop {
        // Re-read the roster every tick: machines are added and removed while
        // this runs.
        for (addr, port) in targets(&state).await {
            let alive = running
                .get(&addr)
                .map(|h| !h.is_finished())
                .unwrap_or(false);
            if alive {
                continue;
            }
            let peer = Peer {
                addr: addr.clone(),
                port,
                token: token.clone(),
            };
            let (st, lay) = (state.clone(), layout.clone());
            // A `reqwest::Client` is an `Arc` inside, so this shares the
            // connection pool rather than opening a new one per worker.
            let cl = http.clone();
            running.insert(
                addr,
                tokio::spawn(async move { drive(st, lay, peer, cl).await }),
            );
        }
        running.retain(|_, h| !h.is_finished());

        if let Some(mins) = idle_minutes(&state).await {
            if !idle(&state).await {
                idle_since = None;
            } else {
                let since = *idle_since.get_or_insert_with(bm_proto::now_secs);
                let idle = bm_proto::now_secs().saturating_sub(since);
                if idle >= mins * 60 {
                    println!(
                        "dispatch: nothing to do for {} min — shutting the cluster down",
                        mins
                    );
                    stop_everything(&state, &token, &http).await;
                }
            }
        }

        tokio::time::sleep(POLL).await;
    }
}

/// Ask one worker, forever: what are you doing, and if nothing — here is work.
///
/// A worker is never told *how* to reach the inductor, and this function never
/// gives it one. Everything the worker needs arrives in the offer.
async fn drive(state: Shared, layout: Layout, peer: Peer, http: reqwest::Client) {
    let base = peer.base();
    // The task runs in its own task, **not** awaited here. Awaiting it would
    // stop the `/status` poll for the whole stage — twenty minutes for a
    // render — and a beat older than 90 s is exactly what the reaper's orphan
    // pass reads as "worker gone". That is not a hypothetical: it requeued a
    // digest the worker was 45% through, and then rejected the report as stale.
    let mut job: Option<tokio::task::JoinHandle<()>> = None;
    loop {
        let Some(beat) = ask(&http, &base, "/status", &peer.token).await else {
            // Silent box: the ledger's machine state is the inductor's own
            // opinion, and it should say so rather than keep claiming Online.
            // The exception — a box that is booting, being pushed to, or
            // provisioned with no worker started yet — lives in `note_silence`,
            // with the reason it exists.
            state.lock().await.note_silence(&peer.addr);
            tokio::time::sleep(POLL).await;
            continue;
        };
        let beat: Heartbeat = match serde_json::from_slice(&beat) {
            Ok(b) => b,
            Err(e) => {
                println!(
                    "dispatch: {} answered a status this cannot read: {e}",
                    peer.addr
                );
                tokio::time::sleep(POLL).await;
                continue;
            }
        };
        // One entry point for liveness, shared with the pull protocol's
        // register/heartbeat handlers — see `state::observe`.
        let dirty = {
            let mut inner = state.lock().await;
            inner.observe(&beat)
        };
        if dirty {
            state.lock().await.save();
        }

        // A worker mid-task is left alone. `task_id` is set for the whole
        // stage, so this is also what keeps the dispatcher from piling a
        // second offer onto a box that is already working.
        // Both guards are wanted. `task_id` is the worker's own answer and
        // the only one that is right during the window between assigning a
        // task and the worker's next beat; `job` keeps a second offer from
        // being built at all while one is outstanding.
        let running = job.as_ref().map(|h| !h.is_finished()).unwrap_or(false);
        if !running && beat.task_id.is_none() {
            // The lock is taken in its own block **on purpose**. Writing
            // `if let Some(o) = state.lock().await.offer(..)` looks identical
            // and is a deadlock: Rust keeps an `if let` scrutinee's temporaries
            // alive for the whole block, so the guard would be held across
            // `run_one` — the entire stage — and every other reader
            // (`/api/state`, the reaper, the TUI) would block behind it until
            // the render finished. A `tokio::sync::Mutex` does not warn about
            // this the way a `std::sync` one does at an `await`; it just
            // wedges.
            let next = {
                let mut inner = state.lock().await;
                inner.offer(&beat.worker_id)
            };
            if let Some(offer) = next {
                let (st, lay, cl, pr) = (state.clone(), layout.clone(), http.clone(), peer.clone());
                job = Some(tokio::spawn(async move {
                    run_one(&st, &lay, &cl, &pr, offer).await;
                }));
            }
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Hand one offer over, bring its artifacts home, then record the outcome.
async fn run_one(
    state: &Shared,
    layout: &Layout,
    http: &reqwest::Client,
    peer: &Peer,
    offer: TaskOffer,
) {
    let task_id = offer.task_id.clone();
    let stage = offer.stage;
    let chapter = offer.chapter;
    let engine = offer.engine.clone();
    let sent = http
        .post(format!("{}/task", peer.base()))
        .bearer_auth(&peer.token)
        // No deadline: the response *is* the stage's outcome, and a render
        // takes as long as it takes. Timing out here would abandon a task the
        // worker is still running, and the lease would then strike it.
        .json(&offer)
        .send()
        .await;
    let response = match sent {
        Ok(r) => r,
        Err(e) => {
            println!("dispatch: {} {task_id} — send failed: {e}", peer.addr);
            return;
        }
    };
    let status = response.status();
    if status == reqwest::StatusCode::CONFLICT {
        // Raced with the worker's own idea of busy. The offer is not lost: the
        // lease expires and the task comes back.
        return;
    }
    // Read once, then decide. Decoding straight from the response would treat
    // any non-`Complete` body — a proxy's error page, a 500 from a worker that
    // predates the matching fix in `push.rs` — as an unreadable answer and
    // drop the task on the floor, which is exactly what happened the first
    // time this ran.
    let body = match response.bytes().await {
        Ok(b) => b,
        Err(e) => {
            println!(
                "dispatch: {} {task_id} — could not read the answer: {e}",
                peer.addr
            );
            return;
        }
    };
    if !status.is_success() {
        let head: String = String::from_utf8_lossy(&body).chars().take(200).collect();
        println!(
            "dispatch: {} {task_id} — worker answered {status}: {head}",
            peer.addr
        );
        return;
    }
    let complete: bm_proto::Complete = match serde_json::from_slice(&body) {
        Ok(c) => c,
        Err(e) => {
            println!("dispatch: {} {task_id} — unreadable answer: {e}", peer.addr);
            return;
        }
    };

    // **Before** the completion is applied. The gate reads the filesystem.
    if stage == Stage::Render {
        collect_units(layout, http, peer, chapter, &engine).await;
    }

    let line = {
        let mut inner = state.lock().await;
        inner.complete(&complete)
    };
    println!("dispatch: {line}");
}

/// Ask the worker for every unit the inductor does not have.
///
/// There is no ledger, no acknowledgement and no lease here, and that is
/// deliberate: the inductor asks only for the names it already knows it is
/// missing, so a transfer that fails is simply asked for again next time and a
/// name it already holds is never requested. A local worker answers this by
/// having the files already — `missing_wavs` returns nothing and no request is
/// made — which is why the local and cloud cases need no branch between them.
async fn collect_units(
    layout: &Layout,
    http: &reqwest::Client,
    peer: &Peer,
    chapter: u32,
    engine: &str,
) {
    let Some(missing) = crate::segments::missing_wavs(layout, engine, chapter) else {
        return;
    };
    if missing.is_empty() {
        return;
    }
    let dir = layout.seg_dir(engine, chapter);
    let _ = std::fs::create_dir_all(&dir);
    // The same store the pull path's `POST /api/segment` writes through, so
    // the two directions cannot disagree about where a segment belongs.
    let store = bm_core::segments::LocalStore::new(layout.clone());
    let mut got = 0usize;
    for name in &missing {
        let url = format!(
            "{}/unit?chapter={chapter}&engine={engine}&name={name}",
            peer.base()
        );
        let Ok(resp) = http
            .get(&url)
            .bearer_auth(&peer.token)
            .timeout(FETCH)
            .send()
            .await
        else {
            continue;
        };
        if !resp.status().is_success() {
            continue;
        }
        let Ok(bytes) = resp.bytes().await else {
            continue;
        };
        // Same bounds the merge stage enforces, so a truncated or empty
        // transfer is refused here rather than discovered by the mixer.
        if bytes.len() < 1000 || bytes.len() > bm_core::assemble::MAX_SEGMENT_BYTES {
            continue;
        }
        if bm_core::segments::SegmentStore::put(&store, engine, chapter, name, &bytes).is_ok() {
            got += 1;
        }
    }
    println!(
        "dispatch: {} ch{chapter} — collected {got}/{} unit(s) that were missing here",
        peer.addr,
        missing.len()
    );
}

/// One request, with the answer as bytes, or `None` if the box did not answer.
async fn ask(http: &reqwest::Client, base: &str, path: &str, token: &str) -> Option<Vec<u8>> {
    let resp = http
        .get(format!("{base}{path}"))
        .bearer_auth(token)
        .timeout(ASK)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.bytes().await.ok().map(|b| b.to_vec())
}

/// Every machine that answers the inverted protocol.
///
/// The port is defaulted on the `Machine` rather than being a record of how a
/// box was launched, so a worker is driven because it *is* a worker — not
/// because some other file remembers its provenance. An operator who wants a
/// box left alone sets `task_port` to `null` in `machines.json`.
async fn targets(state: &Shared) -> Vec<(String, u16)> {
    let inner = state.lock().await;
    let mut out: Vec<(String, u16)> = inner
        .machines
        .values()
        .filter_map(|m| m.task_port.map(|p| (m.addr.clone(), p)))
        .collect();
    out.sort();
    out
}

/// Nothing running and nothing startable — see `Inner::idle` for why this is
/// not `busy()`.
async fn idle(state: &Shared) -> bool {
    state.lock().await.idle()
}

/// The idle timeout, or `None` when it is switched off.
async fn idle_minutes(state: &Shared) -> Option<u64> {
    let mins = state.lock().await.settings.idle_mins;
    (mins > 0).then_some(mins as u64)
}

/// Tell every worker to stop, then stop.
///
/// The inductor initiates this too: there is no heartbeat answer to hang a
/// latch on, so the command is its own request. Workers that do not answer are
/// left to their own watchdog, which fires on the same silence this loop just
/// noticed.
async fn stop_everything(state: &Shared, token: &str, http: &reqwest::Client) {
    for (addr, port) in targets(state).await {
        let url = format!("http://{addr}:{port}/shutdown");
        match http.post(&url).bearer_auth(token).timeout(ASK).send().await {
            Ok(r) if r.status().is_success() => println!("dispatch: {addr} acknowledged shutdown"),
            Ok(r) => println!("dispatch: {addr} refused shutdown ({})", r.status()),
            Err(e) => println!("dispatch: {addr} did not answer shutdown ({e})"),
        }
    }
    // The ledger is written on every mutation, so there is nothing to flush —
    // and this process holds a port the next run wants.
    std::process::exit(0);
}
