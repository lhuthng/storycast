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

async fn op(State(st): State<Shared>, Json(req): Json<OpRequest>) -> Json<OpResult> {
    match req.op {
        bm_proto::Op::Dispatch => {
            let go = req.go.unwrap_or(true);
            let line = { st.lock().await.set_dispatch(go) };
            let line = match enqueue_remainder(&st, go).await {
                Some(more) => format!("{line}; {more}"),
                None => line,
            };
            Json(OpResult::ok(line))
        }
        bm_proto::Op::Translate => {
            let (start, count) = (req.start.unwrap_or(1), req.count.unwrap_or(1));
            // The chapter index first, and **outside the lock**: building it can
            // walk a listing page, and a scheduler holding the ledger across a
            // network round trip is a stalled cluster. This is the one place a
            // `discover()` runs, once per range, on the inductor, which is
            // what keeps ten workers from each re-reading the same index.
            let (index, index_note) = {
                let inner = st.lock().await;
                if inner.settings.crawl.is_manual() {
                    (None, String::new())
                } else {
                    let (layout, settings) = (inner.layout.clone(), inner.settings.clone());
                    drop(inner);
                    match tokio::task::spawn_blocking(move || {
                        bm_core::crawl::chapter_index(&layout, &settings, start, count, false)
                    })
                    .await
                    {
                        Ok(Ok(idx)) => (Some(idx), String::new()),
                        // A broken crawler is worth knowing about *now*: the
                        // alternative is N worker tasks failing identically.
                        Ok(Err(e)) => (None, format!("; no chapter index ({e:#})")),
                        Err(_) => (
                            None,
                            "; no chapter index (the index thread panicked)".into(),
                        ),
                    }
                }
            };
            let mut inner = st.lock().await;
            // Reconcile first: enqueue alone only tops up crawl+digest, so a
            // range whose render/merge tasks went missing (reset ledger, older
            // builds) would digest and then idle with nothing offerable.
            inner.reconcile(start, count);
            // The operator just named the range, so from here this process works
            // on it: `:go` measures the remainder of *this*, not of whatever the
            // saved run config happens to say (see `set_authored_range`).
            inner.set_authored_range(start, count);
            let absent = index.as_ref().map(|i| inner.apply_index(i)).unwrap_or(0);
            let (crawls, digests) = inner.enqueue_translate(start, count);
            let absent_note = if absent > 0 {
                format!("; {absent} chapter(s) are not on the site")
            } else {
                String::new()
            };
            Json(OpResult::ok(format!(
                "translate ch{start}..: {crawls} crawls + {digests} digests queued{absent_note}{index_note}"
            )))
        }
        bm_proto::Op::Import => {
            // Reading a file and rewriting a chapter is local, blocking disk
            // work, so it runs off the runtime and without the ledger lock.
            let layout = { st.lock().await.layout.clone() };
            let (chapter, paths) = (req.chapter, req.paths.clone());
            let done = match tokio::task::spawn_blocking(move || {
                bm_core::crawl::import::import_all(&layout, chapter, &paths)
            })
            .await
            {
                Ok(Ok((done, line))) => Ok((done, line)),
                Ok(Err(e)) => Err(format!("import refused: {e:#}")),
                Err(e) => Err(format!("import failed: {e}")),
            };
            let (done, line) = match done {
                Ok(v) => v,
                Err(msg) => return Json(OpResult::fail(msg)),
            };
            let mut inner = st.lock().await;
            for got in &done {
                inner.mark_imported(got.n, got.bytes);
            }
            Json(OpResult::ok(format!("imported {line}")))
        }
        bm_proto::Op::CrawlSetup => {
            let (layout, settings) = {
                let mut inner = st.lock().await;
                if let Some(t) = req.url_template.clone() {
                    inner.settings.url_template = t.clone();
                    let _ = inner.settings.save(&inner.layout.settings());
                }
                (inner.layout.clone(), inner.settings.clone())
            };
            Json(op_crawl_setup(&layout, &settings, req.start.unwrap_or(1)).await)
        }
        bm_proto::Op::Voices => {
            let (layout, engine) = {
                let inner = st.lock().await;
                (inner.layout.clone(), inner.settings.engine.clone())
            };
            Json(op_voices(&layout, &engine).await)
        }
        bm_proto::Op::SwapVoice => {
            let (character, voice) = (
                req.character.clone().unwrap_or_default(),
                req.voice.clone().unwrap_or_default(),
            );
            if character.is_empty() || voice.is_empty() {
                return Json(OpResult::fail("swap needs character + voice"));
            }
            let mut inner = st.lock().await;
            // **Queued, not refused.** The op is unchanged for the caller and
            // what it does is not: `exclusive_request` runs the surgery at once
            // when the way is clear and parks it when it is not, where the old
            // `op_swap_voice` refused outright. See the note on the helper.
            match inner.exclusive_request(bm_proto::ExclusiveOp::SwapVoice {
                character,
                voice,
                chapters: Vec::new(),
            }) {
                Ok(msg) => Json(OpResult::ok(msg)),
                Err(e) => Json(OpResult::fail(format!("swap failed: {e:#}"))),
            }
        }
        bm_proto::Op::PreviewVoice => {
            // No lock taken: rendering a sample reads no scheduler state, and
            // holding the lock across a sidecar call would freeze the whole
            // dashboard for as long as the render takes.
            let layout = st.lock().await.layout.clone();
            let voice = req.voice.clone().unwrap_or_default();
            Json(op_preview_voice(&layout, &voice, req.text.as_deref()).await)
        }
        bm_proto::Op::Segment => {
            // Files only, no lock beyond cloning two small values: the whole
            // point is serving bytes without synthesis.
            let (layout, engine) = {
                let inner = st.lock().await;
                (inner.layout.clone(), inner.settings.engine.clone())
            };
            let character = req.character.clone().unwrap_or_default();
            let voice = req.voice.clone().unwrap_or_default();
            Json(op_segment(
                &layout,
                &engine,
                &character,
                &voice,
                req.text.as_deref(),
            ))
        }
        bm_proto::Op::Eta => {
            let inner = st.lock().await;
            let (start, count) = (req.start.unwrap_or(1), req.count.unwrap_or(1));
            Json(OpResult::ok(inner.op_eta(start, count)))
        }
        bm_proto::Op::Requeue => {
            let mut inner = st.lock().await;
            Json(OpResult::ok(inner.op_requeue_orphans()))
        }
        bm_proto::Op::Retry => {
            let mut inner = st.lock().await;
            // Three scopes, narrowing in this order. A stage + chapter is one
            // task, what the Tasks screen sends, so one bad digest never
            // re-queues the batch. A chapter alone is every shelved stage of it
            // (`:retry 24`). A stage with no chapter is refused rather than
            // widened to the whole ledger: silently doing more than was asked
            // is the failure this shape exists to avoid.
            match (req.stage, req.chapter) {
                (Some(stage), Some(chapter)) => Json(OpResult::ok(inner.op_retry_task(
                    stage,
                    chapter,
                    req.force.unwrap_or(false),
                ))),
                (None, Some(chapter)) => Json(OpResult::ok(inner.op_retry_chapter(chapter))),
                (Some(_), None) => Json(OpResult::fail(
                    "retry needs a chapter when a stage is named",
                )),
                _ => Json(OpResult::ok(inner.op_retry_shelved())),
            }
        }
        bm_proto::Op::RetryTask => {
            let (stage, chapter, force) = (req.stage, req.chapter, req.force.unwrap_or(false));
            match (stage, chapter) {
                (Some(stage), Some(chapter)) => {
                    let mut inner = st.lock().await;
                    Json(OpResult::ok(inner.op_retry_task(stage, chapter, force)))
                }
                _ => Json(OpResult::fail("retry-task requires stage and chapter")),
            }
        }
        bm_proto::Op::Release => {
            // Two scopes, exactly one per request, and the refusal keeps them
            // apart the way `retry`'s stage-without-chapter does: widening a
            // release to the whole ledger because a field was missing is the
            // kind of doing-more-than-asked this shape exists to stop. A
            // worker names every row that box holds; stage + chapter names one
            // row, which for `render` is every take of it.
            let force = req.force.unwrap_or(false);
            match (req.worker.clone(), req.stage, req.chapter) {
                (Some(worker), ..) => {
                    let mut inner = st.lock().await;
                    Json(OpResult::ok(inner.op_release_worker(&worker, force)))
                }
                (None, Some(stage), Some(chapter)) => {
                    let mut inner = st.lock().await;
                    Json(OpResult::ok(inner.op_release_task(stage, chapter, force)))
                }
                _ => Json(OpResult::fail(
                    "release needs a worker, or a stage and a chapter",
                )),
            }
        }
        bm_proto::Op::Reconcile => {
            // Plan under the lock, think outside it: the LLM call takes
            // seconds and must never block heartbeats and completions.
            let (layout, settings) = {
                let inner = st.lock().await;
                (inner.layout.clone(), inner.settings.clone())
            };
            Json(op_reconcile(&st, &layout, &settings).await)
        }
        bm_proto::Op::Retag => {
            let dry_run = req.dry_run.unwrap_or(false);
            let mut inner = st.lock().await;
            // A dry run reads scripts and writes nothing, so it must never park
            // a write — the operator asked what *would* change, and answering
            // "queued" would be a lie about work that has not been asked for.
            let outcome = match dry_run {
                true => inner.op_retag(true),
                false => inner.exclusive_request(bm_proto::ExclusiveOp::Retag {
                    chapters: Vec::new(),
                }),
            };
            match outcome {
                Ok(msg) => Json(OpResult::ok(msg)),
                Err(e) => Json(OpResult::fail(format!("retag failed: {e:#}"))),
            }
        }
        bm_proto::Op::Recast => {
            let (chapter, fixes, remove) = (req.chapter, req.fixes.clone(), req.remove.clone());
            match chapter {
                Some(chapter) => {
                    let mut inner = st.lock().await;
                    let outcome = inner.exclusive_request(bm_proto::ExclusiveOp::Recast {
                        chapter,
                        fixes,
                        remove,
                    });
                    match outcome {
                        Ok(msg) => Json(OpResult::ok(msg)),
                        Err(e) => Json(OpResult::fail(format!("recast failed: {e:#}"))),
                    }
                }
                _ => Json(OpResult::fail("recast requires a chapter")),
            }
        }
        bm_proto::Op::FixSpeaker => {
            // All three names are required, and the third is the check rather
            // than decoration: without an `expect` this is `recast` with worse
            // ergonomics, and the point of the op is that a wrong segment
            // number cannot edit the wrong line.
            let (chapter, segment, expect, speaker) = (
                req.chapter,
                req.segment,
                req.expect.clone(),
                req.speaker.clone(),
            );
            match (chapter, segment, expect, speaker) {
                (Some(chapter), Some(segment), Some(expect), Some(speaker))
                    if chapter > 0 && segment > 0 =>
                {
                    let mut inner = st.lock().await;
                    let outcome = inner.exclusive_request(bm_proto::ExclusiveOp::FixSpeaker {
                        chapter,
                        segment,
                        expect,
                        speaker,
                    });
                    match outcome {
                        Ok(msg) => Json(OpResult::ok(msg)),
                        Err(e) => Json(OpResult::fail(format!("fix-speaker failed: {e:#}"))),
                    }
                }
                _ => Json(OpResult::fail(
                    "fix-speaker requires chapter, segment, expect and speaker",
                )),
            }
        }
        bm_proto::Op::Merge => {
            // One survivor, one or more absorbed: the manual form of a
            // reconcile fold, for a pair the canon key would never match.
            // Names are validated inside (survivor in the bible, absorbed in
            // the bible or the cast, no Narrator), so a typo refuses before
            // anything is rewritten.
            let (survivor, absorbed) = (req.survivor.clone(), req.absorbed.clone());
            match survivor {
                Some(survivor) if !survivor.trim().is_empty() && !absorbed.is_empty() => {
                    let mut inner = st.lock().await;
                    let outcome = inner.exclusive_request(bm_proto::ExclusiveOp::Merge {
                        survivor,
                        absorbed,
                        chapters: Vec::new(),
                    });
                    match outcome {
                        Ok(msg) => Json(OpResult::ok(msg)),
                        Err(e) => Json(OpResult::fail(format!("merge failed: {e:#}"))),
                    }
                }
                _ => Json(OpResult::fail(
                    "merge requires a survivor and at least one absorbed name",
                )),
            }
        }
        bm_proto::Op::Remix => {
            let mut inner = st.lock().await;
            // The `None` semantics are the direct op's, unchanged. Speed, fx
            // and music stay **required** — a missing one is still an error, and
            // defaulting it to 1.0 would quietly reset a book that is mid-mix.
            // `inject` still defaults to the mix in force rather than to unity.
            // Resolved here because `ExclusiveOp::Remix` carries final numbers:
            // that is what lets a parked remix keep the values the operator
            // asked for rather than a draft they have since edited.
            for (what, v) in [
                ("speed", req.speed),
                ("fx volume", req.effect_volume),
                ("music volume", req.music_volume),
            ] {
                if v.is_none() {
                    return Json(OpResult::fail(format!("remix needs {what}")));
                }
            }
            let inject = req.inject_volume.unwrap_or(inner.settings.inject_volume);
            let outcome = inner.exclusive_request(bm_proto::ExclusiveOp::Remix {
                speed: req.speed.unwrap_or(1.0),
                effect_volume: req.effect_volume.unwrap_or(1.0),
                music_volume: req.music_volume.unwrap_or(1.0),
                inject_volume: inject,
            });
            match outcome {
                Ok(msg) => Json(OpResult::ok(msg)),
                Err(e) => Json(OpResult::fail(format!("remix failed: {e:#}"))),
            }
        }
        bm_proto::Op::SoundChanged => {
            // No `ensure_idle`: nothing here writes a voice or a cache. It
            // reads the registries and requeues merges, which is the ordinary
            // queue operation the scheduler does all day.
            let mut inner = st.lock().await;
            Json(OpResult::ok(inner.op_sound_changed()))
        }
        bm_proto::Op::Rerender => {
            let mut inner = st.lock().await;
            match inner.exclusive_request(bm_proto::ExclusiveOp::Rerender) {
                Ok(msg) => Json(OpResult::ok(msg)),
                Err(e) => Json(OpResult::fail(format!("rerender failed: {e:#}"))),
            }
        }
        bm_proto::Op::Remerge => {
            let mut inner = st.lock().await;
            match inner.exclusive_request(bm_proto::ExclusiveOp::Remerge) {
                Ok(msg) => Json(OpResult::ok(msg)),
                Err(e) => Json(OpResult::fail(format!("remerge failed: {e:#}"))),
            }
        }
        bm_proto::Op::ShutdownWorkers => {
            let mut inner = st.lock().await;
            Json(OpResult::ok(inner.op_shutdown_workers()))
        }
        bm_proto::Op::ShutdownWhenIdle => {
            let mut inner = st.lock().await;
            Json(OpResult::ok(inner.op_shutdown_when_idle()))
        }
        bm_proto::Op::Exclusive => {
            // The write the request already carries its arguments for: swap
            // fields in their usual slots, remix volumes likewise. Building
            // the arm here — from the same request — means an enqueued swap
            // is byte-identical to a direct one, so the runner needs no
            // second argument vocabulary.
            let Some(mut op) = req.exclusive else {
                return Json(OpResult::fail("exclusive requires a write to queue"));
            };
            match &mut op {
                bm_proto::ExclusiveOp::SwapVoice {
                    character, voice, ..
                } => {
                    *character = req.character.clone().unwrap_or_default();
                    *voice = req.voice.clone().unwrap_or_default();
                }
                bm_proto::ExclusiveOp::Recast {
                    chapter,
                    fixes,
                    remove,
                } => {
                    *chapter = req.chapter.unwrap_or(0);
                    *fixes = req.fixes.clone();
                    *remove = req.remove.clone();
                }
                bm_proto::ExclusiveOp::FixSpeaker {
                    chapter,
                    segment,
                    expect,
                    speaker,
                } => {
                    *chapter = req.chapter.unwrap_or(0);
                    *segment = req.segment.unwrap_or(0);
                    *expect = req.expect.clone().unwrap_or_default();
                    *speaker = req.speaker.clone().unwrap_or_default();
                }
                bm_proto::ExclusiveOp::Merge {
                    survivor, absorbed, ..
                } => {
                    *survivor = req.survivor.clone().unwrap_or_default();
                    *absorbed = req.absorbed.clone();
                }
                _ => {}
            }
            let mut inner = st.lock().await;
            match inner.exclusive_request(op) {
                Ok(msg) => Json(OpResult::ok(msg)),
                Err(e) => Json(OpResult::fail(format!("queued write refused: {e:#}"))),
            }
        }
        bm_proto::Op::ExclusiveCancel => {
            let route = req.exclusive.as_ref().map(|e| e.route().to_string());
            let mut inner = st.lock().await;
            let n = inner.exclusive_cancel(route.as_deref());
            if n == 0 {
                Json(OpResult::fail("nothing queued to drop".to_string()))
            } else {
                Json(OpResult::ok(format!(
                    "dropped {n} queued write(s) — the stages it held open take work again"
                )))
            }
        }
    }
}

/// Fold duplicates: deterministic canon-key folds over the bible AND the cast
/// (title/casing/parenthetical variants that never entered the bible) apply
/// immediately; ambiguous pairs go to the analyzer on the next press.
/// Certain folds never wait on the LLM, that call takes minutes on
/// rate-limited tiers while the TUI gives up in seconds.
async fn op_reconcile(
    st: &Shared,
    layout: &bm_core::Layout,
    settings: &bm_core::config::Settings,
) -> OpResult {
    let bible: serde_json::Value =
        bm_core::read_json(&layout.bible()).unwrap_or(serde_json::json!({"characters": []}));
    // Ambiguous aliases first: bare generics ("nữ tử", "tiền bối", "vị kia")
    // sitting in `proper_aliases` hijack every future chapter about an
    // unnamed figure (ch112 went to Lạc Lan Tuyết that way). Alias-only
    // change, serialized under the ledger lock like completions, so it races
    // nothing; idempotent, so a second press is a no-op.
    let scrubbed: Vec<String> = {
        let mut inner = st.lock().await;
        let path = inner.layout.bible();
        let mut current: serde_json::Value =
            bm_core::read_json(&path).unwrap_or(serde_json::json!({"characters": []}));
        let log = bm_core::digest::scrub_ambiguous_aliases(&mut current);
        if !log.is_empty() && bm_core::digest::save_bible(&current, &path).is_ok() {
            inner.push_event("ok", format!("reconcile scrub: {}", log.join("; ")));
            inner.save();
        }
        log
    };
    let scrub_note = if scrubbed.is_empty() {
        String::new()
    } else {
        format!("scrubbed {} ambiguous aliases; ", scrubbed.len())
    };
    let plan = bm_core::digest::reconcile_plan(&bible);
    let mut merges = plan.folds;
    {
        let cast = bm_core::cast::read_cast(&settings.engine, &layout.cast(&settings.engine));
        let keys: Vec<String> = cast.keys().cloned().collect();
        // ponytail: linear scans, merge lists are tiny
        let mut seen: std::collections::HashSet<String> =
            merges.iter().flat_map(|(_, a)| a.iter().cloned()).collect();
        for (canonical, absorbs) in bm_core::digest::cast_only_folds(&bible, &keys) {
            let fresh: Vec<String> = absorbs
                .into_iter()
                .filter(|a| seen.insert(a.clone()))
                .collect();
            if fresh.is_empty() {
                continue;
            }
            match merges.iter_mut().find(|(c, _)| c == &canonical) {
                Some((_, a)) => a.extend(fresh),
                None => merges.push((canonical, fresh)),
            }
        }
    }
    if merges.is_empty() && plan.candidates.is_empty() {
        let n = bible
            .get("characters")
            .and_then(|c| c.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        return OpResult::ok(format!(
            "{scrub_note}reconcile: bible already clean ({n} characters)"
        ));
    }
    if !merges.is_empty() {
        let mut inner = st.lock().await;
        // **Queued, not refused**, like every other surgery: the fold waits for
        // the chapters it rewrites instead of telling the operator to come back
        // when the cluster happens to be quiet. When the way is already clear
        // this runs at once and returns the fold's own message, unchanged.
        return match inner.exclusive_request(bm_proto::ExclusiveOp::Reconcile {
            merges,
            chapters: Vec::new(),
        }) {
            Ok(msg) => OpResult::ok(format!(
                "{scrub_note}{msg}{}",
                if plan.candidates.is_empty() {
                    String::new()
                } else {
                    format!(
                        "; {} ambiguous pairs remain — press m again",
                        plan.candidates.len()
                    )
                }
            )),
            Err(e) => OpResult::fail(format!("reconcile refused: {e:#}")),
        };
    }
    // No certain folds. The ambiguous pairs are listed for a human to judge
    // the analyzer hallucinates merges for mere token-sharers ("Dịch Phong"
    // into "Tịnh Vô Phong"), so it no longer auto-applies anything here.
    let pairs: Vec<String> = plan
        .candidates
        .iter()
        .map(|(a, b)| format!("{a} / {b}"))
        .collect();
    OpResult::ok(format!(
        "reconcile: nothing certain to fold; ambiguous pairs (no auto-merge): {}",
        pairs.join("; ")
    ))
}

/// Persist the URL template and prove the crawler works, through the **same
/// provider a worker will use**.
///
/// This used to fetch the chapter itself with its own client and its own copy
/// of the extraction rules, which made it a fourth fetcher and a probe of
/// something no worker would ever run: a script-mode workspace could be probed
/// "OK" while every real task failed. Now it builds the chapter index (so a
/// script's `discover()` has its say, exactly as `:translate` will ask it) and
/// runs one crawl through the configured provider, reporting the verdict the
/// provider reached.
async fn op_crawl_setup(
    layout: &bm_core::Layout,
    settings: &bm_core::config::Settings,
    sample: u32,
) -> OpResult {
    let (layout, settings) = (layout.clone(), settings.clone());
    let sample = sample.max(1);
    let probed = tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
        let index = bm_core::crawl::chapter_index(&layout, &settings, sample, 1, false)?;
        let spec = bm_core::crawl::spec_from_settings(&layout, &settings);
        let url = index.url(sample).map(str::to_string);
        let provider = bm_core::crawl::Provider::new(&spec);
        let how = if provider.is_scripted() {
            format!("{} {}", spec.engine, spec.script)
        } else {
            "built-in fetcher".to_string()
        };
        let crawled = provider.crawl(sample, url.as_deref(), 1)?;
        Ok(match &crawled.outcome {
            bm_core::crawl::CrawlOutcome::Text { text, .. } => {
                // The text **is** the verdict: it arrived, and it cleared the
                // provider's own length and size guards, which is the only
                // definition of a chapter the host has. The headline is printed
                // because the operator is the one who can say whether it is
                // their book, a "does this look like a chapter" test in Rust
                // would be a fact about one site's language, which is exactly
                // what the script owns now.
                let first = text.lines().next().unwrap_or("");
                format!(
                    "probe ch{sample} via {how}: {} bytes, headline {:?} — read it: if that is \
                     not the chapter, the selector matched the wrong thing",
                    text.len(),
                    bm_core::util::head_chars(first, 60)
                )
            }
            bm_core::crawl::CrawlOutcome::Absent { reason } => {
                format!("probe ch{sample} via {how}: the site has no such chapter ({reason})")
            }
            bm_core::crawl::CrawlOutcome::Blocked(b) => format!(
                "probe ch{sample} via {how} BLOCKED [{}]: {}",
                b.class.as_str(),
                b.detail
            ),
        })
    })
    .await;
    match probed {
        Ok(Ok(message)) => OpResult::ok(message),
        Ok(Err(e)) => OpResult::fail(format!("probe crawl failed: {e:#}")),
        Err(e) => OpResult::fail(format!("probe crawl failed: {e}")),
    }
}
/// Read the sidecar roster and refill the cast's gaps. Falls back to the
/// offline roster when no sidecar answers.
/// Distribution to workers rides the next provision sync.
async fn op_voices(layout: &bm_core::Layout, engine: &str) -> OpResult {
    // Strict, unlike the picker: this op *prunes* the cast, so it refuses
    // rather than guesses.
    let policy = bm_core::voices::effective_policy(engine);
    // Live roster when a sidecar answers, offline fallback otherwise.
    // Enrolled clones have bare labels (voice == label).
    let http = sidecar_client(Duration::from_secs(10));
    let mut enrolled: Vec<String> = Vec::new();
    let mut live = false;
    if let Ok(r) = http.get(format!("{SIDECAR}/voices")).send().await {
        if let Ok(v) = r.json::<Vec<Vec<String>>>().await {
            enrolled = v
                .into_iter()
                .filter_map(|p| match p.as_slice() {
                    [label, id] if label == id => Some(id.clone()),
                    _ => None,
                })
                .collect();
            live = true;
        }
    }
    let cast_path = layout.cast(engine);
    let cast = bm_core::cast::read_cast(engine, &cast_path);
    let filled_from = cast.len();
    // Refill gaps across every script. load_cast never overwrites an existing
    // assignment, so curated voices survive; newcomers get least-used voices.
    let scripts = layout.scripts();
    for sp in &scripts {
        let installed = bm_core::pool::installed_voices(layout);
        if let Err(e) = bm_core::cast::load_cast(
            sp,
            &cast_path,
            &layout.bible(),
            &policy,
            installed.as_ref(),
            true,
        ) {
            return OpResult::fail(format!("cast refill failed on {}: {e:#}", sp.display()));
        }
    }
    let cast = bm_core::cast::read_cast(engine, &cast_path);
    let gaps = cast.len().saturating_sub(filled_from);
    OpResult::ok(format!(
        "voices ({}, {} enrolled clones): filled {gaps} gaps, {} speakers mapped",
        if live {
            "live roster"
        } else {
            "offline roster"
        },
        enrolled.len(),
        cast.len()
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

/// Swap with no scheduler: the same `op_swap_voice` against a throwaway
/// Inner, which persists cast + ledger itself. Two locks before touching
/// anything: the inductor API must be down (its scheduler owns these files
/// while it answers), and no local worker may be alive (a mid-render worker
/// keeps rendering the old cast). Remote strays are the operator's
/// responsibility, the supported flow is X (which sweeps them), then swap.
pub(crate) async fn offline_swap(
    api: &str,
    layout: &bm_core::Layout,
    character: &str,
    voice: &str,
) -> Result<String, String> {
    if super::backend::inductor_up(api).await {
        return Err(
            "inductor is back — swap normally (this path is for inductor-down only)".into(),
        );
    }
    if super::backend::local_workers_alive() {
        return Err("local workers still running — X first, then swap".into());
    }
    offline_swap_apply(layout, character, voice)
}

/// The file mutation itself, minus the guards: throwaway Inner over disk
/// files, same `op_swap_voice` the live path runs (which persists cast +
/// ledger itself). Split out so tests can run it without a scheduler, a
/// network, or a worker-shaped hole in the room.
fn offline_swap_apply(
    layout: &bm_core::Layout,
    character: &str,
    voice: &str,
) -> Result<String, String> {
    let settings = bm_core::config::Settings::load(&layout.settings());
    let mut inner = Inner::new(layout.clone(), settings);
    inner.load_ledger();
    // The startup pass the live inductor runs before any op: an invalidation
    // is diffed against the recorded plan, so without one the swap could only
    // re-speak whole chapters. Built *before* the mutation, so it records the
    // inputs as they are now and the diff afterwards names what moved.
    inner.adopt_render_plans();
    inner
        .op_swap_voice(character, voice)
        .map(|m| format!("{m} [offline — inductor was down]"))
        .map_err(|e| e.to_string())
}

/// Remix with no scheduler: the same `op_remix` against a throwaway Inner,
/// which persists settings + ledger itself. Same guards as the swap path
/// the inductor API must be down and no local worker alive.
pub(crate) async fn offline_remix(
    api: &str,
    layout: &bm_core::Layout,
    speed: Option<f64>,
    effect_volume: Option<f64>,
    music_volume: Option<f64>,
    inject_volume: Option<f64>,
) -> Result<String, String> {
    if super::backend::inductor_up(api).await {
        return Err(
            "inductor is back — remix normally (this path is for inductor-down only)".into(),
        );
    }
    if super::backend::local_workers_alive() {
        return Err("local workers still running — X first, then remix".into());
    }
    offline_remix_apply(layout, speed, effect_volume, music_volume, inject_volume)
}

fn offline_remix_apply(
    layout: &bm_core::Layout,
    speed: Option<f64>,
    effect_volume: Option<f64>,
    music_volume: Option<f64>,
    inject_volume: Option<f64>,
) -> Result<String, String> {
    let settings = bm_core::config::Settings::load(&layout.settings());
    let mut inner = Inner::new(layout.clone(), settings);
    inner.load_ledger();
    inner
        .op_remix(speed, effect_volume, music_volume, inject_volume)
        .map(|m| format!("{m} [offline — inductor was down]"))
        .map_err(|e| e.to_string())
}

/// A sound-design write with no scheduler to notice it.
///
/// `:sound` writes the registries itself, so the write succeeds whether or not
/// the inductor is up, but the invalidation is the *scheduler's* work, and
/// without this path an edit made while the inductor was down would go
/// unnoticed until the next boot. That was survivable while adoption at boot
/// was the only mechanism; it is not survivable now that a boot can adopt an
/// unstamped merge, so the same op runs against a throwaway `Inner` here.
///
/// No `local_workers_alive` guard, unlike the swap and remix paths: those two
/// delete a voice's cached segments, which a running worker can be mid-write
/// on. This deletes published mp3s and requeues, the ordinary queue traffic.
pub(crate) async fn offline_sound_changed(
    api: &str,
    layout: &bm_core::Layout,
) -> Result<String, String> {
    if super::backend::inductor_up(api).await {
        return Err(
            "inductor is back — sound changes go through it (this path is for inductor-down only)"
                .into(),
        );
    }
    let settings = bm_core::config::Settings::load(&layout.settings());
    let mut inner = Inner::new(layout.clone(), settings);
    inner.load_ledger();
    Ok(format!(
        "{} [offline — inductor was down]",
        inner.op_sound_changed()
    ))
}

/// Build the client used for every TTS-sidecar call.
///
/// `no_proxy` is not optional: the sidecar is a LAN service on loopback, and a
/// configured `HTTP_PROXY` would otherwise intercept it, which silently
/// downgrades the roster to the offline fallback and makes previews 502.
fn sidecar_client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(timeout)
        .no_proxy()
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Everything about voices that disk alone knows: shipped catalogue, enrolled
/// clones, pool samples. No sidecar, no inductor, milliseconds, never hangs.
fn disk_voices(
    layout: &bm_core::Layout,
    engine: &str,
    effective: &bm_core::voices::EngineRoster,
) -> Vec<VoiceInfo> {
    let mut voices = effective.to_offline_voices(engine);
    for clone in bm_core::voices::enrolled_voices(&layout.voices_manifest()) {
        if !voices.iter().any(|v| v.name == clone.name) {
            voices.push(clone);
        }
    }
    // The sample pool rides the same list: a pooled sample shows its tags where
    // the style was, so the picker filter (`young`) finds it, and a sample the
    // registry names but nothing enrolled yet still shows, as vetted-at-adding
    // like any clone (the render fails loudly if it never gets enrolled). The
    // tags also ride along as `pool_tags`, which is what makes the picker able
    // to tell an auto-assignable voice from a unique one.
    for (name, entry) in bm_core::pool::load_pool(&layout.voice_pool()) {
        let style = if entry.tags.is_empty() {
            "named voice".to_string()
        } else {
            format!("pool: {}", entry.tags.join(", "))
        };
        match voices.iter_mut().find(|v| v.name == name) {
            Some(v) => {
                v.style = style;
                v.pool_tags = entry.tags.clone();
            }
            None => voices.push(VoiceInfo {
                key: String::new(),
                name,
                gender: "unknown".into(),
                accent: "unknown".into(),
                language: "vi-VN".into(),
                style,
                pool_tags: entry.tags.clone(),
                enrolled: true,
            }),
        }
    }
    // Assignable voices first, then by gender then name: a stable order means
    // the picker's cursor does not jump between refreshes.
    voices.sort_by(|a, b| (&a.gender, &a.name).cmp(&(&b.gender, &b.name)));
    voices
}

/// Roster with no scheduler and no sidecar: what the picker shows instantly.
/// A live upgrade may follow, but picking never waits for it.
pub(crate) fn local_roster(layout: &bm_core::Layout) -> Roster {
    let settings = bm_core::config::Settings::load(&layout.settings());
    let engine = settings.engine.clone();
    let mut inner = Inner::new(layout.clone(), settings);
    inner.load_ledger();
    let characters = inner.known_characters();
    let cast = inner.cast_snapshot();
    let (effective, _) = bm_core::voices::effective_engine_lenient(&engine);
    Roster {
        engine: engine.clone(),
        source: "offline".into(),
        voices: disk_voices(layout, &engine, &effective),
        cast,
        characters,
    }
}

/// Assemble the roster the picker renders: the sidecar's structured roster when
/// it answers, then its label form, then the bundled table.
///
/// Enrolled clones from `voices.json` are merged in regardless, so a clone the
/// operator added by hand never disappears just because the sidecar is down.
async fn build_roster(
    layout: &bm_core::Layout,
    engine: &str,
    characters: Vec<String>,
    cast: BTreeMap<String, String>,
) -> Roster {
    // Loopback: a serving sidecar answers in ms, a loading one 503s, a dead
    // one refuses, none of which is worth more than 2s of picker. (Was 10s
    // × 2: every :s press stared at "loading roster" for 20s+ while booting.)
    let http = sidecar_client(Duration::from_secs(2));
    let mut source = "offline".to_string();
    let mut voices: Vec<VoiceInfo> = Vec::new();

    // The effective roster is the shipped catalogue.
    let (effective, _) = bm_core::voices::effective_engine_lenient(engine);

    if let Ok(r) = http.get(format!("{SIDECAR}/roster")).send().await {
        if let Ok(v) = r.json::<Vec<VoiceInfo>>().await {
            if !v.is_empty() {
                voices = v;
                source = "live".into();
            }
        }
    }
    // A sidecar older than this build still answers /voices with SDK labels.
    if voices.is_empty() {
        if let Ok(r) = http.get(format!("{SIDECAR}/voices")).send().await {
            if let Ok(pairs) = r.json::<Vec<Vec<String>>>().await {
                let labels: Vec<(String, String)> = pairs
                    .into_iter()
                    .filter_map(|p| match p.as_slice() {
                        [label, id] => Some((label.clone(), id.clone())),
                        _ => None,
                    })
                    .collect();
                if !labels.is_empty() {
                    voices = bm_core::voices::voices_from_labels(engine, &labels);
                    source = "live (labels)".into();
                }
            }
        }
    }
    if voices.is_empty() {
        voices = disk_voices(layout, engine, &effective);
    }
    // The merges below are no-ops on the disk path (same names, same styles)
    // and complete a live answer with the local truth.
    for clone in bm_core::voices::enrolled_voices(&layout.voices_manifest()) {
        if !voices.iter().any(|v| v.name == clone.name) {
            voices.push(clone);
        }
    }
    // The sample pool rides the same list: a pooled sample shows its tags where
    // the style was, so the picker filter (`young`) finds it, and a sample the
    // registry names but nothing enrolled yet still shows, as vetted-at-adding
    // like any clone (the render fails loudly if it never gets enrolled). The
    // tags also ride along as `pool_tags`, which is what makes the picker able
    // to tell an auto-assignable voice from a unique one.
    for (name, entry) in bm_core::pool::load_pool(&layout.voice_pool()) {
        let style = if entry.tags.is_empty() {
            "named voice".to_string()
        } else {
            format!("pool: {}", entry.tags.join(", "))
        };
        match voices.iter_mut().find(|v| v.name == name) {
            Some(v) => {
                v.style = style;
                v.pool_tags = entry.tags.clone();
            }
            None => voices.push(VoiceInfo {
                key: String::new(),
                name,
                gender: "unknown".into(),
                accent: "unknown".into(),
                language: "vi-VN".into(),
                style,
                pool_tags: entry.tags.clone(),
                enrolled: true,
            }),
        }
    }
    // Assignable voices first, then by gender then name: a stable order means
    // the picker's cursor does not jump between refreshes.
    voices.sort_by(|a, b| (&a.gender, &a.name).cmp(&(&b.gender, &b.name)));
    Roster {
        engine: engine.to_string(),
        source,
        voices,
        cast,
        characters,
    }
}

/// Health plus capability, mirroring the agent's sidecar gate: the server
/// must serve the policy endpoint the agent was built against, or a stale
/// server from a previous deploy answers health but lacks `/preview`.
async fn sidecar_serving(base: &str) -> bool {
    let Ok(http) = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .no_proxy()
        .build()
    else {
        return false;
    };
    let health = http
        .get(format!("{base}/health"))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false);
    if !health {
        return false;
    }
    let Ok(resp) = http.get(format!("{base}/policy")).send().await else {
        return false;
    };
    let text = resp.text().await.unwrap_or_default();
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|p| p.get("allowed_voices").cloned())
        .and_then(|v| v.as_array().cloned())
        .is_some()
}

/// Start the local sidecar for audition duty unless one already answers.
///
/// Preview/audition is the one path that needs TTS with no render task
/// running, and since the sidecar's lifecycle went per-task, idle means
/// down. So the first audition of a quiet cluster boots the server (model
/// load takes minutes) and leaves it up: stopping it after every sample
/// would make every audition pay the load again. The binary and argv are
/// the agent's own (`Layout::sidecar_command`), so the two can never name
/// different servers.
async fn ensure_sidecar(layout: &bm_core::Layout) -> anyhow::Result<()> {
    if sidecar_serving(SIDECAR).await {
        return Ok(());
    }
    let port = SIDECAR
        .rsplit(':')
        .next()
        .and_then(|p| p.trim_end_matches('/').parse().ok())
        .unwrap_or(8818);
    let (bin, args) = layout.sidecar_command(port, bm_core::config::tts_threads());
    if !bin.is_file() {
        anyhow::bail!(
            "no TTS sidecar at {} — build it (`make build`) or provision this box",
            bin.display()
        );
    }
    // Detached by dropping the handle: this is audition duty, not a render
    // task, so no per-task owner exists to reap it. It lives until the box
    // reboots or `X` sweeps it, exactly like the provision-started one did.
    let _ = tokio::process::Command::new(&bin)
        .args(&args)
        .env("LD_LIBRARY_PATH", layout.tts_lib_dir())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_secs(5)).await;
        if sidecar_serving(SIDECAR).await {
            return Ok(());
        }
    }
    anyhow::bail!("TTS sidecar started but never answered /health")
}

/// Render one voice's speech so it can be auditioned before it is assigned.
///
/// Without `text` this is the voice *sample*: the sidecar's fixed audition line,
/// which is the only way two voices are comparable. With `text` it is a real
/// line from the book, which is what an operator actually wants to hear before
/// committing a swap.
///
/// Either way the bytes come back in `OpResult::audio_b64` and **nothing is
/// written here**. The inductor never plays anything, it is a server, and the
/// speaker is on the client's desk, so it is also the wrong machine to put a
/// file on: a path is useless to a client that does not share this filesystem,
/// and an audition that lands in `data/` accumulates one clip per voice
/// auditioned. The client owns the file, because the client owns the speaker.
async fn op_preview_voice(layout: &bm_core::Layout, voice: &str, text: Option<&str>) -> OpResult {
    let voice = voice.trim();
    if voice.is_empty() {
        return OpResult::fail("preview needs a voice name");
    }
    // Audition is the one path that needs TTS with no render task running:
    // boot the sidecar here rather than failing onto an idle box.
    if let Err(e) = ensure_sidecar(layout).await {
        return OpResult::fail(format!("preview {voice}: {e:#}"));
    }
    // Two routes into the sidecar, and the difference is the point. No text
    // means `/preview`, which speaks the sidecar's fixed audition line, the
    // only way two voice samples are comparable. Text means `/infer`, which is
    // how an operator hears a *real* line from the book instead of a sample.
    let line = text.map(str::trim).filter(|t| !t.is_empty());
    let (path, body) = match line {
        Some(t) => ("/infer", serde_json::json!({"voice": voice, "text": t})),
        None => ("/preview", serde_json::json!({"voice": voice})),
    };
    let http = match reqwest::Client::builder()
        .timeout(Duration::from_secs(180))
        .no_proxy()
        .build()
    {
        Ok(c) => c,
        Err(e) => return OpResult::fail(format!("preview {voice}: {e:#}")),
    };
    let resp = match http
        .post(format!("{SIDECAR}{path}"))
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return OpResult::fail(format!(
                "preview {voice}: TTS sidecar unreachable at {SIDECAR} ({e})"
            ))
        }
    };
    if !resp.status().is_success() {
        let code = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return OpResult::fail(format!(
            "preview {voice}: sidecar {code} — {}",
            bm_core::util::head_chars(body.trim(), 200)
        ));
    }
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => return OpResult::fail(format!("preview {voice}: read failed ({e})")),
    };
    let what = match line {
        Some(_) => "line",
        None => "sample",
    };
    audio_result(voice, what, &bytes)
}

/// Turn a rendered wav into the op's answer.
///
/// Split out from the HTTP call so the contract is testable without a sidecar:
/// bytes in, base64 out, and **nothing written**. The inductor is the wrong
/// machine to put a sample on, a path is useless to a client that does not
/// share this filesystem, and a clip that landed in `data/` would accumulate
/// one file per voice auditioned, which is exactly what the operator asked it
/// not to do.
fn audio_result(voice: &str, what: &str, bytes: &[u8]) -> OpResult {
    if bytes.is_empty() {
        // A 200 with an empty body is not audio. Passing it on would make the
        // client report a playback failure for a render that produced nothing.
        return OpResult::fail(format!("preview {voice}: the sidecar returned no audio"));
    }
    OpResult::ok(format!(
        "preview {voice} ({what}): {} KB",
        bytes.len() / 1024
    ))
    .with_audio_b64(base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        bytes,
    ))
}

/// Serve one already-rendered segment for a voice: no synthesis, just bytes
/// from this inductor's segment cache.
///
/// Only what is on local disk counts. Segments rendered on another box stay
/// there (merge affinity), and fetching them over ssh would turn a keypress
/// into a network operation with its own failure modes, the miss says so
/// instead, and names what would fix it. Discovery lives in `bm_core` so a
/// disconnected TUI can run the same lookup against its own checkout.
fn op_segment(
    layout: &bm_core::Layout,
    engine: &str,
    character: &str,
    voice: &str,
    text: Option<&str>,
) -> OpResult {
    let voice = voice.trim();
    if voice.is_empty() {
        return OpResult::fail("segment needs a voice name");
    }
    let cands = bm_core::assemble::rendered_segments(layout, engine, voice);
    if cands.is_empty() {
        return OpResult::fail(bm_core::assemble::segment_miss(
            layout, character, voice, false,
        ));
    }
    // An exact line plays that sentence or misses honestly, never a nearby
    // one. Without it, T triages on a random segment.
    let exact = text.map(str::trim).filter(|t| !t.is_empty());
    if let Some(want) = exact {
        match bm_core::assemble::pick_exact(&cands, character, want) {
            Some(pick) => return serve_segment(pick),
            None => {
                // The held line never rendered in this voice, the normal
                // state for a fresh swap, which renders chapter by chapter.
                // Fall back to one of hers that did, still zero synthesis:
                // the served sentence is held, so T compares on it rather
                // than another random pick.
                match bm_core::assemble::pick_rendered(&cands, character) {
                    Some(pick) => return serve_segment(pick),
                    None => {
                        return OpResult::fail(bm_core::assemble::segment_miss(
                            layout, character, voice, true,
                        ))
                    }
                }
            }
        }
    }
    let pick =
        bm_core::assemble::pick_rendered(&cands, character).expect("a non-empty pool always picks");
    serve_segment(pick)
}

/// Turn a picked segment into the op's answer: bytes, plus whose sentence it
/// is so the client can show and hold it.
fn serve_segment(pick: &bm_core::assemble::RenderedSegment) -> OpResult {
    let bytes = match pick.read_bytes() {
        Ok(b) => b,
        Err(e) => return OpResult::fail(format!("segment unreadable: {e}")),
    };
    let mut res = OpResult::ok(format!(
        "segment: “{}” ch{} ({} KB, rendered — nothing synthesized)",
        pick.speaker,
        pick.chapter,
        bytes.len() / 1024
    ))
    .with_audio_b64(base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        &bytes,
    ));
    if !pick.text.trim().is_empty() {
        res = res.with_line(pick.speaker.clone(), pick.text.clone());
    }
    res
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
