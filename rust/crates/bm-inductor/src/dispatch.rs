//! The inductor drives. Nothing dials the inductor.

use crate::api::Shared;
use bm_core::Layout;
use bm_proto::{Heartbeat, Stage, TaskOffer};
use std::collections::HashMap;
use std::time::Duration;

/// How often each worker is asked how it is. Short enough that a task is
const POLL: Duration = Duration::from_secs(2);

/// How soon after a task **finishes** the worker is offered the next one.
const HOT: Duration = Duration::from_millis(200);

/// Asking a question. Anything that has not answered in this long is not
const ASK: Duration = Duration::from_secs(5);

/// Moving one wav. Generous, because a segment is a few hundred kB over
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
pub async fn run(state: Shared, layout: Layout) {
    let Some(token) = bm_core::token::read(&layout.root) else {
        println!("dispatch: no cluster token — every worker would refuse the request");
        return;
    };
    let Ok(http) = reqwest::Client::builder()
        // Worker addresses are loopback, LAN, or a cloud private network. An
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
async fn drive(state: Shared, layout: Layout, peer: Peer, http: reqwest::Client) {
    let base = peer.base();
    // The task runs in its own task, **not** awaited here. Awaiting it would
    let mut job: Option<(String, Stage, tokio::task::JoinHandle<()>)> = None;
    let mut was_running = false;
    // What this dispatcher has told this box about its sidecar, and what the
    let mut sidecar_book = SidecarBook::default();
    loop {
        let Some(beat) = ask(&http, &base, "/status", &peer.token).await else {
            // Silent box: the ledger's machine state is the inductor's own
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
        let dirty = {
            let mut inner = state.lock().await;
            inner.observe(&beat)
        };
        if dirty {
            state.lock().await.save();
        }

        // The sidecar-policy instruction is converged here, on the poll that
        converge_sidecar_policy(&state, &http, &peer, &mut sidecar_book, &beat).await;

        // A worker mid-task is left alone. `task_id` is set for the whole
        let running = job
            .as_ref()
            .map(|(_, _, h)| !h.is_finished())
            .unwrap_or(false);
        if running {
            if let Some((tid, stage, h)) = job.as_ref() {
                if *stage == Stage::Digest {
                    let settled = {
                        state
                            .lock()
                            .await
                            .tasks
                            .get(tid)
                            .map(|t| t.state.is_terminal())
                            .unwrap_or(false)
                    };
                    if settled {
                        println!(
                            "dispatch: {} {tid} — race lost, stopping this box",
                            peer.addr
                        );
                        h.abort();
                        job = None;
                    }
                }
            }
        }
        let running = job
            .as_ref()
            .map(|(_, _, h)| !h.is_finished())
            .unwrap_or(false);
        if !running && beat.task_id.is_none() {
            // The lock is taken in its own block **on purpose**. Writing
            let next = {
                let mut inner = state.lock().await;
                inner.offer(&beat.worker_id)
            };
            if let Some(offer) = next {
                let (st, lay, cl, pr) = (state.clone(), layout.clone(), http.clone(), peer.clone());
                let tid = offer.task_id.clone();
                let stage = offer.stage;
                job = Some((
                    tid,
                    stage,
                    tokio::spawn(async move {
                        run_one(&st, &lay, &cl, &pr, offer).await;
                    }),
                ));
            }
        }
        // Poll hot exactly once after a task completes: the work just drained
        let running_now = job
            .as_ref()
            .map(|(_, _, h)| !h.is_finished())
            .unwrap_or(false);
        let just_finished = was_running && !running_now;
        was_running = running_now;
        tokio::time::sleep(if just_finished { HOT } else { POLL }).await;
    }
}

/// Everything the dispatcher remembers about what it has told one box. The
#[derive(Default, Clone)]
struct SidecarBook {
    desired: Option<bool>,
    delivered: Option<bool>,
    reported: Option<bool>,
    /// The thread half of the same instruction: what the box's record asks for
    desired_threads: Option<u32>,
    delivered_threads: Option<u32>,
    reported_threads: Option<u32>,
    /// Edge-triggered logging: the first mismatch (or first failure) says so,
    logged: bool,
    /// Failures against one desired value. The push is retried every tick
    failures: u8,
}

impl SidecarBook {
    /// True when the dispatcher should (re-)tell the box: either half of the
    fn drifted(&self) -> bool {
        // No opinion at all: the default needs no instruction.
        if self.desired.is_none() && self.desired_threads.is_none() {
            return false;
        }
        // A refusal past the retry budget holds until the value changes.
        if self.failures >= SIDECAR_PUSH_TRIES {
            return false;
        }
        self.keep_drifted() || self.threads_drifted()
    }

    /// Delivered-and-acknowledged for this value is enough **only** while the
    fn keep_drifted(&self) -> bool {
        let Some(desired) = self.desired else {
            return false;
        };
        !(self.delivered == Some(desired) && self.reported != Some(!desired))
    }

    /// The thread half: delivered-and-acknowledged is enough only while the
    fn threads_drifted(&self) -> bool {
        let Some(desired) = self.desired_threads else {
            return false;
        };
        !(self.delivered_threads == Some(desired) && self.reported_threads == Some(desired))
    }
}

/// Pushes per desired value before giving up until the value changes. Five
const SIDECAR_PUSH_TRIES: u8 = 5;

/// Tell a worker whether it keeps a TTS sidecar, as a consequence of its
async fn tell_sidecar_policy(
    http: &reqwest::Client,
    peer: &Peer,
    keep: bool,
    threads: Option<u32>,
) -> Result<(), String> {
    let url = format!("{}/sidecar-policy", peer.base());
    let resp = http
        .post(&url)
        .bearer_auth(&peer.token)
        .json(&serde_json::json!({"keep": keep, "threads": threads}))
        .send()
        .await
        .map_err(|e| format!("no answer ({e:#})"))?;
    match resp.status() {
        s if s.is_success() => Ok(()),
        reqwest::StatusCode::NOT_FOUND => Err("worker predates the sidecar-policy endpoint".into()),
        s => Err(format!("worker answered {s}")),
    }
}

/// Should this box be holding its TTS sidecar warm?
fn desired_sidecar_keep(m: &bm_proto::Machine) -> Option<bool> {
    if m.relaxed() {
        return Some(false);
    }
    m.task_policy
        .as_ref()
        .map(|p| p.iter().any(|t| t.stage == Stage::Render && t.enabled))
}

/// The ONNX thread count this box's sidecar should open with, or `None` for
fn desired_sidecar_threads(m: &bm_proto::Machine) -> Option<u32> {
    m.tts_threads.map(u32::from)
}

/// The converge step, run once per poll inside `drive`. Updates the book
async fn converge_sidecar_policy(
    state: &Shared,
    http: &reqwest::Client,
    peer: &Peer,
    book: &mut SidecarBook,
    beat: &Heartbeat,
) {
    // Desired, from the ledger; reported, from the beat.
    let (desired, desired_threads) = {
        let inner = state.lock().await;
        match inner.machines.get(&peer.addr) {
            Some(m) => (desired_sidecar_keep(m), desired_sidecar_threads(m)),
            None => (None, None),
        }
    };
    book.reported = beat.sidecar_keep;
    book.reported_threads = beat.tts_threads;
    if book.desired != desired || book.desired_threads != desired_threads {
        // A new value forgets the old value's failure count: the budget is
        book.failures = 0;
        book.logged = false;
    }
    book.desired = desired;
    book.desired_threads = desired_threads;
    if !book.drifted() {
        if book.logged {
            book.logged = false;
            println!("dispatch: {} sidecar policy converged", peer.addr);
        }
        return;
    }
    // Nothing stored on either half: the defaults need no instruction, and the
    if book.desired.is_none() && book.desired_threads.is_none() {
        return;
    }
    // `keep` with no opinion rides as `true`, the worker's own default: it is
    let keep = book.desired.unwrap_or(true);
    let threads = book.desired_threads;
    match tell_sidecar_policy(http, peer, keep, threads).await {
        Ok(()) => {
            if book.delivered != Some(keep) || book.delivered_threads != threads || book.logged {
                println!(
                    "dispatch: {} told to {} its TTS sidecar{}{}",
                    peer.addr,
                    if keep { "keep" } else { "drop" },
                    if book.desired.is_some() {
                        format!(" (policy: render {})", if keep { "on" } else { "off" })
                    } else {
                        String::new()
                    },
                    match threads {
                        Some(n) => format!(", threads {n}"),
                        None => String::new(),
                    }
                );
            }
            book.delivered = Some(keep);
            book.delivered_threads = threads;
            book.failures = 0;
            book.logged = false;
        }
        Err(e) => {
            book.failures += 1;
            // Delivered is **wiped**, not left stale: a box that answers
            book.delivered = None;
            book.delivered_threads = None;
            if !book.logged {
                book.logged = true;
                println!(
                    "dispatch: {} sidecar instruction not delivered yet — {e}{}",
                    peer.addr,
                    if book.failures >= SIDECAR_PUSH_TRIES {
                        "; giving up until its policy changes (worker may predate the endpoint)"
                    } else {
                        ""
                    }
                );
            }
        }
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
        return;
    }
    if status == reqwest::StatusCode::FORBIDDEN {
        // The worker refused a render because the operator's policy turns
        let line = {
            let mut inner = state.lock().await;
            inner.release_render_rows(&task_id, "worker's policy turns render off")
        };
        println!(
            "dispatch: {} {task_id} — refused (render off by policy); {line}",
            peer.addr
        );
        return;
    }
    // Read once, then decide. Decoding straight from the response would treat
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
async fn targets(state: &Shared) -> Vec<(String, u16)> {
    let inner = state.lock().await;
    let mut out: Vec<(String, u16)> = inner
        .machines
        .values()
        .filter(|m| m.state.dialable())
        .filter_map(|m| m.task_port.map(|p| (m.addr.clone(), p)))
        .collect();
    out.sort();
    out
}

/// Nothing running and nothing startable — see `Inner::idle` for why this is
async fn idle(state: &Shared) -> bool {
    state.lock().await.idle()
}

/// The idle timeout, or `None` when it is switched off.
async fn idle_minutes(state: &Shared) -> Option<u64> {
    let mins = state.lock().await.settings.idle_mins;
    (mins > 0).then_some(mins as u64)
}

/// Tell every worker to stop, then stop.
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
    std::process::exit(0);
}

#[cfg(test)]
mod tests;
