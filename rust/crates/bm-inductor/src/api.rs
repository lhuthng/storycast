//! Control API: the only way workers and operators talk to the scheduler.

use axum::{
    body::Bytes,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{delete, get, post},
    Router,
};
use bm_proto::{
    Complete, Heartbeat, Machine, MachineState, OpRequest, OpResult, Register, Roster, TaskRequest,
    VoiceInfo,
};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use crate::state::Inner;

/// The TTS sidecar the inductor talks to. LAN-only and unauthenticated, same
/// as every other sidecar call in this repo.
const SIDECAR: &str = "http://127.0.0.1:8818";

pub type Shared = Arc<tokio::sync::Mutex<Inner>>;

#[derive(Deserialize)]
struct TaskQuery {
    worker_id: String,
}

async fn register(State(st): State<Shared>, Json(r): Json<Register>) -> impl IntoResponse {
    let mut inner = st.lock().await;
    // A registration is a liveness report with nothing to report yet. It goes
    // through the same `observe` a beat does, so the two routes into the
    // ledger cannot drift apart, which is what happened when the bookkeeping
    // lived in two handlers.
    let beat = Heartbeat {
        worker_id: r.worker_id,
        addr: r.addr,
        task_id: None,
        stage: None,
        chapter: None,
        progress: 0.0,
        activity: String::new(),
        eta_secs: None,
        ts: bm_proto::now_secs(),
        hostname: r.hostname,
        alias: String::new(),
        cpu_pct: None,
        mem_pct: None,
        mem_gb: None,
        // A registration has not measured the box yet; the next beat does.
        sidecars: None,
        sidecar_gb: None,
        capabilities: r.capabilities,
        sources_stages: r.sources_stages,
        // A registration carries no sidecar belief; the next beat does.
        sidecar_keep: None,
        tts_threads: None,
        cores: None,
    };
    inner.observe(&beat);
    inner.save();
    Json(serde_json::json!({"ok": true}))
}

async fn heartbeat(State(st): State<Shared>, Json(h): Json<Heartbeat>) -> impl IntoResponse {
    let mut inner = st.lock().await;
    // Whether the report arrived by post or by the dispatcher's poll, it lands
    // in the same place, see `state::observe`.
    inner.observe(&h);
    inner.save();
    Json(
        serde_json::to_value(&bm_proto::HeartbeatAck {
            ok: true,
            shutdown: inner.shutdown_requested,
        })
        .unwrap_or(serde_json::json!({"ok": true})),
    )
}

async fn task(State(st): State<Shared>, Query(q): Query<TaskQuery>) -> impl IntoResponse {
    let mut inner = st.lock().await;
    match inner.offer(&q.worker_id) {
        Some(offer) => (StatusCode::OK, Json(serde_json::to_value(offer).unwrap())).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

async fn complete(State(st): State<Shared>, Json(c): Json<Complete>) -> impl IntoResponse {
    // Shipments first: a Done row's file must already be home when the
    // ledger says so, on every channel, including the hook's, which has no
    // collection round trip.
    if !c.unit_files.is_empty() {
        let (layout, engine) = {
            let inner = st.lock().await;
            (inner.layout.clone(), inner.settings.engine.clone())
        };
        store_shipments(&layout, &engine, &c.task_id, &c.unit_files);
    }
    let mut inner = st.lock().await;
    let line = inner.complete(&c);
    println!("{line}");
    Json(serde_json::json!({"ok": true}))
}

/// Store the takes a completion report ships, before the row turns Done.
///
/// Same bounds and expected-set check as the upload path; a bad file is
/// skipped, the completion still applies, a reject must not strand a whole
/// finished batch over one corrupt name.
fn store_shipments(
    layout: &bm_core::Layout,
    engine: &str,
    task_id: &str,
    files: &[bm_proto::UnitFile],
) {
    let Some(chapter) = task_id
        .split(':')
        .nth(1)
        .and_then(|p| p.parse::<u32>().ok())
    else {
        return;
    };
    let Some(expected) = crate::segments::expected_names(layout, engine, chapter) else {
        return;
    };
    let store = bm_core::segments::LocalStore::new(layout.clone());
    for u in files {
        let Ok(bytes) = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &u.b64)
        else {
            continue;
        };
        if !(1000..=bm_core::assemble::MAX_SEGMENT_BYTES).contains(&bytes.len()) {
            continue;
        }
        if !expected.contains(&u.name) {
            continue;
        }
        let _ = bm_core::segments::SegmentStore::put(&store, engine, chapter, &u.name, &bytes);
    }
}

#[derive(Deserialize)]
struct SegmentQuery {
    chapter: u32,
    engine: String,
    name: String,
}

/// One rendered unit, uploaded by a non-local worker. The 200 below is the
/// discard contract: a file is deleted on the worker only after the inductor
/// returns 200 for that exact file.
///
/// The name is validated against the expected set for (chapter, engine), a
/// worker may not write an arbitrary path into the store, and the body
/// against the same size bounds the merger enforces (non-trivial, ≤ 8 MB).
/// Rejects rather than storing a file the merger would ignore.
async fn put_segment(
    State(st): State<Shared>,
    Query(q): Query<SegmentQuery>,
    body: Bytes,
) -> impl IntoResponse {
    let bad = |msg: String| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"ok": false, "error": msg})),
        )
    };
    if !(1000..=bm_core::assemble::MAX_SEGMENT_BYTES).contains(&body.len()) {
        return bad(format!(
            "segment {} is {} bytes, want 1001..={}",
            q.name,
            body.len(),
            bm_core::assemble::MAX_SEGMENT_BYTES
        ));
    }
    let (layout, engine) = {
        let inner = st.lock().await;
        (inner.layout.clone(), inner.settings.engine.clone())
    };
    if q.engine != engine {
        return bad(format!(
            "engine {:?} is not this run's {engine:?}",
            q.engine
        ));
    }
    let Some(expected) = crate::segments::expected_names(&layout, &engine, q.chapter) else {
        return bad(format!("chapter {} cannot be planned here", q.chapter));
    };
    if !expected.contains(&q.name) {
        return bad(format!(
            "{} is not an expected file for chapter {}",
            q.name, q.chapter
        ));
    }
    let store = bm_core::segments::LocalStore::new(layout);
    match bm_core::segments::SegmentStore::put(&store, &engine, q.chapter, &q.name, &body) {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({"ok": true, "bytes": body.len()})),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(
                serde_json::json!({"ok": false, "error": format!("storing {} failed: {e:#}", q.name)}),
            ),
        ),
    }
}

/// One rendered unit, pulled by a merge worker that does not hold it.
///
/// Same expected-set validation as the upload path, a worker may read only
/// files the plan names, and the same size floor, so a half-written file is
/// a 404 rather than a corrupt mix. This is what lets a merge run on any box:
/// the inductor's store holds every completed take (`collect_units` pulls
/// each unit home before its completion is applied), so a worker fetches what
/// it lacks and mixes from a complete set, wherever it runs.
async fn get_segment(State(st): State<Shared>, Query(q): Query<SegmentQuery>) -> impl IntoResponse {
    let fail = |code: StatusCode, msg: String| {
        (code, Json(serde_json::json!({"ok": false, "error": msg}))).into_response()
    };
    let (layout, engine) = {
        let inner = st.lock().await;
        (inner.layout.clone(), inner.settings.engine.clone())
    };
    if q.engine != engine {
        return fail(
            StatusCode::BAD_REQUEST,
            format!("engine {:?} is not this run's {engine:?}", q.engine),
        );
    }
    let Some(expected) = crate::segments::expected_names(&layout, &engine, q.chapter) else {
        return fail(
            StatusCode::NOT_FOUND,
            format!("chapter {} cannot be planned here", q.chapter),
        );
    };
    if !expected.contains(&q.name) {
        return fail(
            StatusCode::NOT_FOUND,
            format!(
                "{} is not an expected file for chapter {}",
                q.name, q.chapter
            ),
        );
    }
    match std::fs::read(layout.seg_dir(&engine, q.chapter).join(&q.name)) {
        Ok(bytes) if bytes.len() > 1000 => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "audio/wav")],
            bytes,
        )
            .into_response(),
        _ => fail(
            StatusCode::NOT_FOUND,
            format!("{} is not on this box yet", q.name),
        ),
    }
}

async fn add_machine(State(st): State<Shared>, Json(m): Json<Machine>) -> impl IntoResponse {
    let mut inner = st.lock().await;
    let addr = m.addr.clone();
    let mut m = m;
    // A bind without a stored handle shows the address, like register.
    if m.name.is_empty() {
        m.name = inner.box_name(&addr, &addr);
    }
    inner.machines.insert(addr.clone(), m);
    // Operator bind: config goes to machines.json, runtime stays in the ledger.
    inner.persist_box(&addr, &addr);
    inner.save();
    Json(serde_json::json!({"ok": true}))
}

#[derive(Deserialize)]
struct AddrQuery {
    addr: String,
}

/// TUI-driven machine phase transitions (provisioning / error / note) while a
/// box catches up in the background. Register/heartbeat own Online; this owns
/// everything before the first beat. Unknown addresses are refused, not
/// created, creation stays with register and the add-machine flow.
#[derive(Deserialize)]
struct MachineStateUpdate {
    addr: String,
    state: MachineState,
    #[serde(default)]
    note: String,
    /// A whole new work policy for this machine, when the request carries one.
    /// Absent means "leave the policy alone", the provisioning transitions
    /// send only state and note. Always a full four-entry list (the policy
    /// panel sends every stage), so `Some` is a replacement, never a merge.
    #[serde(default)]
    task_policy: Option<Vec<bm_proto::TaskPref>>,
}

async fn set_machine_state(
    State(st): State<Shared>,
    Json(u): Json<MachineStateUpdate>,
) -> impl IntoResponse {
    let mut inner = st.lock().await;
    match inner.machines.get_mut(&u.addr) {
        Some(m) => {
            // Through `set_state` so the transition is stamped: the pane can
            // then say how long a box has been provisioning, and the boot
            // deadline can tell a fresh `Initializing` from a stuck one.
            m.set_state(u.state);
            if !u.note.is_empty() {
                // State flows rewrite the note freely, but an EC2 instance id
                // on it is the box's one stable identity, relink matches by
                // it, so a note rewrite may never erase it.
                m.note = bm_core::provision::preserve_ec2_id(&m.note, &u.note);
            }
            if let Some(p) = &u.task_policy {
                m.task_policy = Some(p.clone());
            }
            let addr = u.addr.clone();
            inner.persist_box(&addr, &addr);
            inner.save();
            Json(serde_json::json!({"ok": true}))
        }
        None => {
            Json(serde_json::json!({"ok": false, "error": format!("unknown machine {}", u.addr)}))
        }
    }
}

/// Replace one machine's work policy. Separate from `set_machine_state`
/// because a policy edit is a scheduling decision, not a phase transition
/// it must not drag the machine's state or note along with it.
///
/// **The sidecar instruction is deliberately *not* sent from here.** A one-shot
/// push misses every state that matters: the box down at edit time, the box
/// that reboots later and comes back with the default, the inductor restarted
/// since, the worker busy behind its 5 s timeout, the hand-edited
/// `machines.json`. The dispatcher owns convergence instead, it polls every
/// box every 2 s and re-tells a worker whenever what it last delivered differs
/// from the box's policy (see `dispatch::drive`). One mechanism, reachable
/// from every state, retried for free by the poll that already exists.
#[derive(Deserialize)]
struct TaskPolicyUpdate {
    addr: String,
    task_policy: Vec<bm_proto::TaskPref>,
}

/// Park a box, or wake it up.
///
/// Writes **intent only**, one bool in `machines.json`. Everything that follows
/// from it is already converged by machinery that exists: `offer` withholds work
/// because of it (so an in-flight task finishes and nothing new is handed out),
/// and `dispatch::drive` drops the box's sidecar because of it (so `SIDECAR_IDLE`
/// later the 2.85 GB is back). Nothing is pushed from here, for exactly the
/// reason spelled out above `TaskPolicyUpdate`: a one-shot command misses the box
/// that is down, the inductor that restarts, and the worker busy behind its
/// timeout.
///
/// Idempotent on purpose. The dashboard toggles, so a double-press or a retry
/// after a failed `POST` must land on a known value rather than flip twice.
#[derive(Deserialize)]
struct AcceptingUpdate {
    addr: String,
    accepting_work: bool,
}

async fn set_accepting_work(
    State(st): State<Shared>,
    Json(u): Json<AcceptingUpdate>,
) -> impl IntoResponse {
    let mut inner = st.lock().await;
    match inner.machines.get_mut(&u.addr) {
        Some(m) => {
            m.accepting_work = u.accepting_work;
            let addr = u.addr.clone();
            inner.persist_box(&addr, &addr);
            inner.save();
            Json(serde_json::json!({"ok": true, "accepting_work": u.accepting_work}))
        }
        None => {
            Json(serde_json::json!({"ok": false, "error": format!("unknown machine {}", u.addr)}))
        }
    }
}

/// The per-box ONNX thread count the TUI's `:threads` edits. `None` clears the
/// override and restores the sidecar's own default (half the cores, capped at
/// 8). Config, like `task_policy`: written to `machines.json`, and pushed to the
/// worker by the dispatcher's convergent sidecar-policy channel, so a box that
/// is down at edit time still converges when it comes back.
#[derive(Deserialize)]
struct TtsThreadsUpdate {
    addr: String,
    #[serde(default)]
    threads: Option<u16>,
}

async fn set_tts_threads(
    State(st): State<Shared>,
    Json(u): Json<TtsThreadsUpdate>,
) -> impl IntoResponse {
    let mut inner = st.lock().await;
    match inner.machines.get_mut(&u.addr) {
        Some(m) => {
            m.tts_threads = u.threads;
            let addr = u.addr.clone();
            // Config, not runtime: it belongs in machines.json beside the
            // box's login, so it survives the ledger being cleared.
            inner.persist_box(&addr, &addr);
            inner.save();
            Json(serde_json::json!({"ok": true, "tts_threads": u.threads}))
        }
        None => {
            Json(serde_json::json!({"ok": false, "error": format!("unknown machine {}", u.addr)}))
        }
    }
}

async fn set_task_policy(
    State(st): State<Shared>,
    Json(u): Json<TaskPolicyUpdate>,
) -> impl IntoResponse {
    let mut inner = st.lock().await;
    match inner.machines.get_mut(&u.addr) {
        Some(m) => {
            m.task_policy = Some(u.task_policy.clone());
            let addr = u.addr.clone();
            // Config, not runtime: the policy belongs in machines.json beside
            // the box's login, so it survives the ledger being cleared.
            inner.persist_box(&addr, &addr);
            inner.save();
            Json(serde_json::json!({"ok": true}))
        }
        None => {
            Json(serde_json::json!({"ok": false, "error": format!("unknown machine {}", u.addr)}))
        }
    }
}

/// Reconcile EC2-launched boxes with the address they carry now. Reads the
/// account off the async runtime, then applies the drift to the registry; the
/// same routine runs once at startup. Returns the repair lines so a caller can
/// surface them in its own log.
async fn relink(State(st): State<Shared>) -> impl IntoResponse {
    let root = { st.lock().await.layout.root.clone() };
    let pool = tokio::task::spawn_blocking(move || crate::aws_ops::pool(&root)).await;
    match pool {
        Ok(Ok((_cfg, instances))) => {
            let mut inner = st.lock().await;
            let lines = inner.relink_drifted(&instances);
            for l in &lines {
                inner.push_event("info", l.clone());
            }
            Json(serde_json::json!({"ok": true, "lines": lines}))
        }
        Ok(Err(e)) => Json(serde_json::json!({"ok": false, "error": format!("{e:#}")})),
        Err(e) => {
            Json(serde_json::json!({"ok": false, "error": format!("relink task failed: {e}")}))
        }
    }
}

async fn drop_machine(State(st): State<Shared>, Query(q): Query<AddrQuery>) -> impl IntoResponse {
    let mut inner = st.lock().await;
    inner.machines.remove(&q.addr);
    // Config and runtime both go: a config-only box would otherwise rejoin
    // as Unknown on the next load.
    let _ = bm_core::provision::remove_box(&inner.layout.machines(), &q.addr);
    inner.save();
    Json(serde_json::json!({"ok": true}))
}

/// `dispatch`'s enqueue: the **remainder** of the authored range, and nothing
/// else.
///
/// Deliberately index-free. `translate` fetches the chapter index because a
/// range it has never seen has to know which chapters are not on the site; the
/// remainder of a range that is already in the ledger has been through that once
/// already, and re-walking a listing page would make `:go` — the control an
/// operator reaches for when they want the cluster moving *now* — a control that
/// sometimes waits on a network round trip.
///
/// A ledger with no rows at all is the one case it refuses to guess at: there is
/// no range set up, and enqueueing one blind is how a fresh workspace starts
/// crawling chapters that may not exist. It says what to do instead.
///
/// The two callers are the two spellings of the same control — `Op::Dispatch`
/// and `serve --go` — so both come up doing exactly the same two things. A flag
/// that flipped the hold and stopped there would put crawl rows in front of a
/// manual-mode fleet, which is the failure `enqueue_translate` exists to avoid.
pub(crate) async fn enqueue_remainder(st: &Shared, go: bool) -> Option<String> {
    if !go {
        return None;
    }
    let mut inner = st.lock().await;
    let (from, to, _) = inner.remaining()?;
    if inner.tasks.is_empty() {
        return Some(
            "nothing is queued yet — `:translate <start> <count>` sets the range up first"
                .to_string(),
        );
    }
    let count = to - from + 1;
    inner.reconcile(from, count);
    let (crawls, digests) = inner.enqueue_translate(from, count);
    Some(format!(
        "{crawls} crawls + {digests} digests queued for ch{from}..{to}"
    ))
}

async fn state(State(st): State<Shared>) -> impl IntoResponse {
    let inner = st.lock().await;
    let events: Vec<_> = inner.recent_events(100);
    Json(serde_json::json!({
        "tasks": inner.tasks.values().collect::<Vec<_>>(),
        "machines": inner.machines.values().collect::<Vec<_>>(),
        "beats": inner.beats.values().collect::<Vec<_>>(),
        "counts": inner.counts(),
        // Per-worker per-stage completions plus per-stage task averages
        // the Stats pane's matrix and its TUI-side ETA.
        "stats": inner.stats.summary(),
        // Settings ride along so the TUI can prefill prompts with the values
        // that are actually in force instead of hardcoded guesses. Provider
        // keys stay in `.bm/llm.json` (never on this wire); the SSH key is a
        // path (config, in machines.json and settings.json), not a secret.
        "settings": inner.settings,
        // Scheduler events (task done/fail, retry, orphan reap, …) surfaced in
        // the TUI's event pane. The TUI deduplicates by event id.
        "events": events,
        // The exclusive-write queue, when an operator has parked one: the
        // ledger's blocked-by readout and the TUI's queue line both read
        // this. Empty almost always, so it costs one empty array.
        "exclusive": inner.exclusive_snapshot(),
        // Whether anything is being handed out at all, and where the authored
        // range stands. The footer says so while held — "held · ch4..100 ·
        // 3 done, 97 to go — :go" — because neither is visible from the task
        // table alone, where a held cluster and a finished one look identical.
        "dispatch": {
            "held": inner.dispatch_held,
            "span": inner.remaining_line(),
            "remaining": inner.remaining(),
        },
    }))
}

/// The voice picker's whole data model in one call: roster + cast + speakers.
///
/// Deliberately a separate endpoint from `/api/state`: it is only needed when
/// the operator opens the picker, and it may take a sidecar round trip.
async fn roster(State(st): State<Shared>) -> Json<Roster> {
    let (layout, engine, characters, cast) = {
        let inner = st.lock().await;
        (
            inner.layout.clone(),
            inner.settings.engine.clone(),
            inner.known_characters(),
            inner.cast_snapshot(),
        )
    };
    Json(build_roster(&layout, &engine, characters, cast).await)
}

pub fn router(st: Shared) -> Router {
    Router::new()
        .route("/api/register", post(register))
        .route("/api/heartbeat", post(heartbeat))
        .route("/api/task", get(task))
        .route("/api/complete", post(complete))
        .route("/api/segment", post(put_segment).get(get_segment))
        .route("/api/machines", post(add_machine))
        .route("/api/machines", delete(drop_machine))
        .route("/api/machines/state", post(set_machine_state))
        .route("/api/machines/policy", post(set_task_policy))
        .route("/api/machines/accepting", post(set_accepting_work))
        .route("/api/machines/tts-threads", post(set_tts_threads))
        .route("/api/relink", post(relink))
        .route("/api/op", post(op))
        .route("/api/state", get(state))
        .route("/api/roster", get(roster))
        // Merge reports carry base64 mp3s (~7MB); the 2MB default would 413 them.
        .layer(axum::extract::DefaultBodyLimit::disable())
        .with_state(st)
}

// Silence the unused-import warning until M6 operations need TaskRequest.
#[allow(dead_code)]
fn _task_req_is_part_of_the_protocol(_r: TaskRequest) {}

#[cfg(test)]
mod segment_tests;
#[cfg(test)]
mod tests;

pub(crate) mod offline;
#[allow(unused_imports)]
pub(crate) use offline::{offline_remix, offline_sound_changed, offline_swap};
#[allow(unused_imports)]
pub(crate) use roster::{build_roster, local_roster};
#[allow(unused_imports)]
pub(crate) use sidecar::{ensure_sidecar, op_preview_voice, op_segment};
pub(crate) mod ops;
pub(crate) use ops::op;
pub(crate) mod roster;
pub(crate) mod sidecar;
