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

/// How soon after a task **finishes** the worker is offered the next one.
///
/// A render is one take per task now, so waiting out the full poll between
/// segments would spend a fifth of the render's wall time doing nothing. This
/// is used only on the transition — while a task is running the poll stays at
/// [`POLL`], so a busy box is not polled five times a second.
const HOT: Duration = Duration::from_millis(200);

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
    let mut job: Option<(String, Stage, tokio::task::JoinHandle<()>)> = None;
    let mut was_running = false;
    // What this dispatcher has told this box about its sidecar, and what the
    // box says back. Converged inside the loop below, on the poll that
    // already exists.
    let mut sidecar_book = SidecarBook::default();
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

        // The sidecar-policy instruction is converged here, on the poll that
        // already exists, so it reaches the box in every state a box can be
        // in: down-and-back, rebooted-into-default, busy-behind-timeout,
        // or freshly edited. A beat from an older agent carries
        // `sidecar_keep: None`, which only makes the first push happen —
        // never a stuck refusal (the worker defaults to keeping).
        converge_sidecar_policy(&state, &http, &peer, &mut sidecar_book, &beat).await;

        // A worker mid-task is left alone. `task_id` is set for the whole
        // stage, so this is also what keeps the dispatcher from piling a
        // second offer onto a box that is already working.
        // Both guards are wanted. `task_id` is the worker's own answer and
        // the only one that is right during the window between assigning a
        // task and the worker's next beat; `job` keeps a second offer from
        // being built at all while one is outstanding.
        //
        // Losing digest racers are the exception: another box already won
        // the row (it reads `Done`, or `Shelved` after the last failure),
        // so this box is burning LLM tokens for a report that will be
        // dropped as stale. Abort the POST — the worker reads a dropped
        // connection as a cancel — and the next poll hands it fresh work.
        // Digest-only: a render's units land with its report, so killing it
        // mid-batch would discard audio that already exists.
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
        // the queue one take deeper, and the offer that answers it is already
        // waiting. Any other iteration is steady state.
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
/// instruction channel is **convergent, not one-shot**: a single push misses
/// every state that matters — the box down at edit time, the box that reboots
/// back to its default later, the inductor restarted since, the worker busy
/// behind the timeout, the hand-edited `machines.json`. So the dispatcher
/// re-tells a worker whenever two books disagree:
///
/// * `desired` — what the box's policy says (render on ⇒ keep the sidecar).
///   Read from the ledger each tick, so edits and hand-written files flow in.
/// * `delivered` — what this process last *successfully* told the box. Wiped
///   on any failure, so a flaky box is retried until an ack lands.
/// * `reported` — what the box last **said** it believes (the beat's
///   `sidecar_keep`). This is the one that survives the box rebooting into
///   its default: a box that never got the instruction reports `true`, and
///   the mismatch re-drives the push even though `delivered` still claims
///   otherwise.
///
/// Any disagreement pushes; agreement converges. Two reads per poll, no
/// timers, and a box that answers nothing costs two map lookups.
#[derive(Default, Clone)]
struct SidecarBook {
    desired: Option<bool>,
    delivered: Option<bool>,
    reported: Option<bool>,
    /// The thread half of the same instruction: what the box's record asks for
    /// (`None` = no override), what this process last delivered, and what the
    /// box last reported. Converged exactly like `keep`.
    desired_threads: Option<u32>,
    delivered_threads: Option<u32>,
    reported_threads: Option<u32>,
    /// Edge-triggered logging: the first mismatch (or first failure) says so,
    /// a persistent one does not — the trap the tunnel supervisor's 1339
    /// lines documented. Reset when the mismatch clears or is resolved.
    logged: bool,
    /// Failures against one desired value. The push is retried every tick
    /// while a mismatch stands, but a worker that predates the endpoint
    /// answers 404 forever — five tries and this book stops until `desired`
    /// changes, rather than one line per two seconds for ever.
    failures: u8,
}

impl SidecarBook {
    /// True when the dispatcher should (re-)tell the box: either half of the
    /// instruction has drifted from what the box believes.
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
    /// box still reports it. After a reboot the box reports the default, and
    /// the mismatch re-drives the push.
    fn keep_drifted(&self) -> bool {
        let Some(desired) = self.desired else {
            return false;
        };
        !(self.delivered == Some(desired) && self.reported != Some(!desired))
    }

    /// The thread half: delivered-and-acknowledged is enough only while the
    /// box reports the same value — a reboot resets it to "no opinion".
    fn threads_drifted(&self) -> bool {
        let Some(desired) = self.desired_threads else {
            return false;
        };
        !(self.delivered_threads == Some(desired) && self.reported_threads == Some(desired))
    }
}

/// Pushes per desired value before giving up until the value changes. Five
/// ticks ≈ ten seconds of unanswered pushes — enough for a busy handler to
/// free up several times over, and small enough that an old agent's 404
/// cannot flood the log.
const SIDECAR_PUSH_TRIES: u8 = 5;

/// Tell a worker whether it keeps a TTS sidecar, as a consequence of its
/// policy: render is the only stage that needs the model, so render-off maps
/// to keep-none. The worker answers once it has accepted the value — which
/// is why the call is retried by the poll, not given a longer timeout.
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
///
/// `None` is "no opinion": a box that has never had its stages edited gets no
/// instruction at all, and the worker's own default stands — which keeps this
/// from putting a `keep=true` on the wire for every box in the cluster on every
/// poll.
///
/// A parked box is `Some(false)`, and it is checked **first** so that parking is
/// expressible without a stored policy. That is the "give the 2.85 GB back" half
/// of parking, and it is not an optimisation to skip: a box held warm for a
/// render nobody will send it is the exact waste the operator parked it to stop.
///
/// The unpark path needs nothing of its own — it falls back to the policy, so a
/// box that had render on before is warm again and one that never did stays as
/// it was. Pure, so the two rules and their order are testable without a poll.
fn desired_sidecar_keep(m: &bm_proto::Machine) -> Option<bool> {
    if m.relaxed() {
        return Some(false);
    }
    m.task_policy
        .as_ref()
        .map(|p| p.iter().any(|t| t.stage == Stage::Render && t.enabled))
}

/// The ONNX thread count this box's sidecar should open with, or `None` for
/// "no opinion" (the sidecar's own default). Unlike `keep`, this is never a
/// consequence of the policy: it is exactly what the operator typed in the
/// TUI, so a box that has never had it edited sends nothing.
fn desired_sidecar_threads(m: &bm_proto::Machine) -> Option<u32> {
    m.tts_threads.map(u32::from)
}

/// The converge step, run once per poll inside `drive`. Updates the book
/// from the beat, pushes on drift, and logs edge-triggered.
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
        // per value, so a policy that flips back and forth gets fresh tries
        // each way.
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
    // body would otherwise carry a `keep=true` nobody asked for.
    if book.desired.is_none() && book.desired_threads.is_none() {
        return;
    }
    // `keep` with no opinion rides as `true`, the worker's own default: it is
    // only sent while the *thread* half is converging, and a no-op there.
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
            // later must be told again even though this process once
            // succeeded.
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
    if status == reqwest::StatusCode::FORBIDDEN {
        // The worker refused a render because the operator's policy turns
        // render off for this box. Not a failure: the policy is the
        // operator's decision, so the covered rows are released **strike-free**
        // (the same rule a lease expiry gets) and offered to other boxes. A
        // 200 `ok:false` here would cost the chapter one of its three strikes
        // — three policy flips would shelve a chapter for a decision the
        // operator made.
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
///
/// A box with no address is left out. It is registered from the moment it is
/// launched — that is what makes an account read able to repair it — but until
/// the account hands it a public address its key is its *instance id*, and there
/// is no `http://i-0abc…:8917/status` to ask. Polling it would be a guaranteed
/// connection failure every two seconds, which is noise in the log and a lie in
/// the workers pane.
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

#[cfg(test)]
mod tests;
