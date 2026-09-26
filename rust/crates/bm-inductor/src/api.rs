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
    // ledger cannot drift apart — which is what happened when the bookkeeping
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
        // A registration carries no sidecar belief; the next beat does.
        sidecar_keep: None,
    };
    inner.observe(&beat);
    inner.save();
    Json(serde_json::json!({"ok": true}))
}

async fn heartbeat(State(st): State<Shared>, Json(h): Json<Heartbeat>) -> impl IntoResponse {
    let mut inner = st.lock().await;
    // Whether the report arrived by post or by the dispatcher's poll, it lands
    // in the same place — see `state::observe`.
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
    // ledger says so — on every channel, including the hook's, which has no
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
/// skipped, the completion still applies — a reject must not strand a whole
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
/// The name is validated against the expected set for (chapter, engine) — a
/// worker may not write an arbitrary path into the store — and the body
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
/// Same expected-set validation as the upload path — a worker may read only
/// files the plan names — and the same size floor, so a half-written file is
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
/// created — creation stays with register and the add-machine flow.
#[derive(Deserialize)]
struct MachineStateUpdate {
    addr: String,
    state: MachineState,
    #[serde(default)]
    note: String,
    /// A whole new work policy for this machine, when the request carries one.
    /// Absent means "leave the policy alone" — the provisioning transitions
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
                // on it is the box's one stable identity — relink matches by
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
/// because a policy edit is a scheduling decision, not a phase transition —
/// it must not drag the machine's state or note along with it.
///
/// **The sidecar instruction is deliberately *not* sent from here.** A one-shot
/// push misses every state that matters: the box down at edit time, the box
/// that reboots later and comes back with the default, the inductor restarted
/// since, the worker busy behind its 5 s timeout, the hand-edited
/// `machines.json`. The dispatcher owns convergence instead — it polls every
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
/// Writes **intent only** — one bool in `machines.json`. Everything that follows
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

async fn op(State(st): State<Shared>, Json(req): Json<OpRequest>) -> Json<OpResult> {
    match req.op {
        bm_proto::Op::Translate => {
            let (start, count) = (req.start.unwrap_or(1), req.count.unwrap_or(1));
            // The chapter index first, and **outside the lock**: building it can
            // walk a listing page, and a scheduler holding the ledger across a
            // network round trip is a stalled cluster. This is the one place a
            // `discover()` runs — once per range, on the inductor — which is
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
            match inner.op_swap_voice(&character, &voice) {
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
            // task — what the Tasks screen sends, so one bad digest never
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
            match inner.op_retag(dry_run) {
                Ok(msg) => Json(OpResult::ok(msg)),
                Err(e) => Json(OpResult::fail(format!("retag failed: {e:#}"))),
            }
        }
        bm_proto::Op::Recast => {
            let (chapter, fixes, remove) = (req.chapter, req.fixes.clone(), req.remove.clone());
            match chapter {
                Some(chapter) => {
                    let mut inner = st.lock().await;
                    match inner.op_recast(chapter, &fixes, &remove) {
                        Ok(msg) => Json(OpResult::ok(msg)),
                        Err(e) => Json(OpResult::fail(format!("recast failed: {e:#}"))),
                    }
                }
                _ => Json(OpResult::fail("recast requires a chapter")),
            }
        }
        bm_proto::Op::Remix => {
            let mut inner = st.lock().await;
            match inner.op_remix(
                req.speed,
                req.effect_volume,
                req.music_volume,
                req.inject_volume,
            ) {
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
            match inner.op_rerender_all() {
                Ok(msg) => Json(OpResult::ok(msg)),
                Err(e) => Json(OpResult::fail(format!("rerender failed: {e:#}"))),
            }
        }
        bm_proto::Op::Remerge => {
            let mut inner = st.lock().await;
            match inner.op_remerge_all() {
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
    }
}

/// Fold duplicates: deterministic canon-key folds over the bible AND the cast
/// (title/casing/parenthetical variants that never entered the bible) apply
/// immediately; ambiguous pairs go to the analyzer on the next press.
/// Certain folds never wait on the LLM — that call takes minutes on
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
        return match inner.apply_reconcile(&merges) {
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
    // No certain folds. The ambiguous pairs are listed for a human to judge —
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

/// Persist the URL template and prove the crawler works — through the **same
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
                // their book — a "does this look like a chapter" test in Rust
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
/// Read the sidecar roster, enforce the accent policy on the cast file, and
/// refill any gaps. Falls back to the offline roster when no sidecar answers.
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
    let allowed: std::collections::HashSet<&str> =
        policy.allowed.iter().map(|s| s.as_str()).collect();
    let cast_path = layout.cast(engine);
    let mut cast = bm_core::cast::read_cast(engine, &cast_path);
    let before = cast.len();
    // Drop assignments the policy rejects and that no enrolled clone covers.
    cast.retain(|_, v| allowed.contains(v.as_str()) || enrolled.iter().any(|e| e == v));
    let dropped = before - cast.len();
    if dropped > 0 {
        let _ = bm_core::cast::write_cast(engine, &cast_path, &cast);
    }
    let filled_from = cast.len();
    // Refill gaps across every script. load_cast never overwrites an existing
    // assignment, so curated voices survive; newcomers get least-used voices.
    let mut scripts: Vec<std::path::PathBuf> = std::fs::read_dir(layout.data())
        .map(|rd| {
            rd.filter_map(|e| e.ok().map(|x| x.path()))
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n.starts_with("script-") && n.ends_with(".json"))
                        .unwrap_or(false)
                })
                .collect()
        })
        .unwrap_or_default();
    scripts.sort();
    for sp in &scripts {
        if let Err(e) = bm_core::cast::load_cast(sp, &cast_path, &layout.bible(), &policy, true) {
            return OpResult::fail(format!("cast refill failed on {}: {e:#}", sp.display()));
        }
    }
    let cast = bm_core::cast::read_cast(engine, &cast_path);
    let gaps = cast.len().saturating_sub(filled_from);
    OpResult::ok(format!(
        "voices ({}, {} enrolled clones): pruned {dropped}, filled {gaps} gaps, {} speakers mapped",
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
        // Per-worker per-stage completions plus per-stage task averages —
        // the Stats pane's matrix and its TUI-side ETA.
        "stats": inner.stats.summary(),
        // Settings ride along so the TUI can prefill prompts with the values
        // that are actually in force instead of hardcoded guesses. API keys
        // stay in .env; the SSH key is a path (config, in machines.json and
        // settings.json), not a secret.
        "settings": inner.settings,
        // Scheduler events (task done/fail, retry, orphan reap, …) surfaced in
        // the TUI's event pane. The TUI deduplicates by event id.
        "events": events,
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
/// responsibility — the supported flow is X (which sweeps them), then swap.
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
/// which persists settings + ledger itself. Same guards as the swap path —
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
/// the inductor is up — but the invalidation is the *scheduler's* work, and
/// without this path an edit made while the inductor was down would go
/// unnoticed until the next boot. That was survivable while adoption at boot
/// was the only mechanism; it is not survivable now that a boot can adopt an
/// unstamped merge, so the same op runs against a throwaway `Inner` here.
///
/// No `local_workers_alive` guard, unlike the swap and remix paths: those two
/// delete a voice's cached segments, which a running worker can be mid-write
/// on. This deletes published mp3s and requeues — the ordinary queue traffic.
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
/// configured `HTTP_PROXY` would otherwise intercept it — which silently
/// downgrades the roster to the offline fallback and makes previews 502.
fn sidecar_client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(timeout)
        .no_proxy()
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Everything about voices that disk alone knows: shipped catalogue, enrolled
/// clones, pool samples. No sidecar, no inductor — milliseconds, never hangs.
fn disk_voices(
    layout: &bm_core::Layout,
    engine: &str,
    effective: &bm_core::voices::EngineRoster,
) -> Vec<VoiceInfo> {
    let mut voices = effective.to_offline_voices(engine);
    for clone in bm_core::voices::enrolled_voices(&layout.root.join("voices.json")) {
        if !voices.iter().any(|v| v.name == clone.name) {
            voices.push(clone);
        }
    }
    // The sample pool rides the same list: a pooled sample shows its tags where
    // the style was, so the picker filter (`young`) finds it — and a sample the
    // registry names but nothing enrolled yet still shows, as vetted-at-adding
    // like any clone (the render fails loudly if it never gets enrolled).
    for (name, entry) in bm_core::pool::load_pool(&layout.root.join("voice-pool.json")) {
        let style = if entry.tags.is_empty() {
            "named voice".to_string()
        } else {
            format!("pool: {}", entry.tags.join(", "))
        };
        match voices.iter_mut().find(|v| v.name == name) {
            Some(v) => v.style = style,
            None => voices.push(VoiceInfo {
                key: String::new(),
                name,
                gender: "unknown".into(),
                accent: "unknown".into(),
                language: "vi-VN".into(),
                style,
                enrolled: true,
                allowed: true,
            }),
        }
    }
    // Assignable voices first, then by gender then name: a stable order means
    // the picker's cursor does not jump between refreshes.
    voices.sort_by(|a, b| (!a.allowed, &a.gender, &a.name).cmp(&(!b.allowed, &b.gender, &b.name)));
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
    let (effective, roster_error) = bm_core::voices::effective_engine_lenient(&engine);
    Roster {
        engine: engine.clone(),
        source: "offline".into(),
        voices: disk_voices(layout, &engine, &effective),
        cast,
        characters,
        policy_note: match roster_error {
            Some(e) => format!("roster error — {e}"),
            None => bm_core::voices::policy_note(&effective),
        },
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
    // one refuses — none of which is worth more than 2s of picker. (Was 10s
    // × 2: every :s press stared at "loading roster" for 20s+ while booting.)
    let http = sidecar_client(Duration::from_secs(2));
    let mut source = "offline".to_string();
    let mut voices: Vec<VoiceInfo> = Vec::new();

    // The effective roster is the shipped catalogue. Resolved once so the
    // voice list, the allow-list and the header line cannot disagree about
    // what is assignable.
    let (effective, roster_error) = bm_core::voices::effective_engine_lenient(engine);
    let policy = effective.to_policy(engine);

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
                    voices = bm_core::voices::voices_from_labels(engine, &labels, &policy.allowed);
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
    for clone in bm_core::voices::enrolled_voices(&layout.root.join("voices.json")) {
        if !voices.iter().any(|v| v.name == clone.name) {
            voices.push(clone);
        }
    }
    // The sample pool rides the same list: a pooled sample shows its tags where
    // the style was, so the picker filter (`young`) finds it — and a sample the
    // registry names but nothing enrolled yet still shows, as vetted-at-adding
    // like any clone (the render fails loudly if it never gets enrolled).
    for (name, entry) in bm_core::pool::load_pool(&layout.root.join("voice-pool.json")) {
        let style = if entry.tags.is_empty() {
            "named voice".to_string()
        } else {
            format!("pool: {}", entry.tags.join(", "))
        };
        match voices.iter_mut().find(|v| v.name == name) {
            Some(v) => v.style = style,
            None => voices.push(VoiceInfo {
                key: String::new(),
                name,
                gender: "unknown".into(),
                accent: "unknown".into(),
                language: "vi-VN".into(),
                style,
                enrolled: true,
                allowed: true,
            }),
        }
    }
    // Assignable voices first, then by gender then name: a stable order means
    // the picker's cursor does not jump between refreshes.
    voices.sort_by(|a, b| (!a.allowed, &a.gender, &a.name).cmp(&(!b.allowed, &b.gender, &b.name)));
    Roster {
        engine: engine.to_string(),
        source,
        voices,
        cast,
        characters,
        policy_note: match roster_error {
            Some(e) => format!("roster error — {e}"),
            None => bm_core::voices::policy_note(&effective),
        },
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
/// running — and since the sidecar's lifecycle went per-task, idle means
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
    let (bin, args) = layout.sidecar_command(port);
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
/// written here**. The inductor never plays anything — it is a server, and the
/// speaker is on the client's desk — so it is also the wrong machine to put a
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
    // means `/preview`, which speaks the sidecar's fixed audition line — the
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
/// machine to put a sample on — a path is useless to a client that does not
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
/// into a network operation with its own failure modes — the miss says so
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
    // An exact line plays that sentence or misses honestly — never a nearby
    // one. Without it, T triages on a random segment.
    let exact = text.map(str::trim).filter(|t| !t.is_empty());
    if let Some(want) = exact {
        match bm_core::assemble::pick_exact(&cands, character, want) {
            Some(pick) => return serve_segment(pick),
            None => {
                // The held line never rendered in this voice — the normal
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
mod tests {
    use super::*;

    fn scratch() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let layout = bm_core::Layout::new(d.path());
        std::fs::create_dir_all(layout.data()).unwrap();
        std::fs::create_dir_all(layout.output()).unwrap();
        std::fs::write(layout.bible(), r#"{"characters":[]}"#).unwrap();
        d
    }

    /// A stub sidecar: 200 on `/health`, and on `/policy` whatever body the
    /// test hands it. Returns the base URL.
    async fn stub_sidecar(policy_body: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 4096];
                let Ok(n) = s.read(&mut buf).await else {
                    continue;
                };
                let req: String = String::from_utf8_lossy(&buf[..n]).into_owned();
                let path = req
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/");
                let body: String = if path.starts_with("/policy") {
                    policy_body.into()
                } else {
                    r#"{"ok":true}"#.into()
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = s.write_all(resp.as_bytes()).await;
            }
        });
        base
    }

    #[tokio::test]
    async fn sidecar_check_demands_health_plus_policy() {
        // Healthy server with the policy endpoint: serving.
        let up = stub_sidecar(r#"{"allowed_voices":[]}"#).await;
        assert!(sidecar_serving(&up).await);
        // Healthy but stale (a server from before `/policy` existed): not
        // serving — the agent would refuse it too, so preview must not use it.
        let stale = stub_sidecar(r#"{"ok":true}"#).await;
        assert!(!sidecar_serving(&stale).await);
        // Nothing there at all: not serving (and fast — no 5-minute wait).
        assert!(!sidecar_serving("http://127.0.0.1:9").await);
    }

    /// A full policy list, as the policy panel sends it: every stage, with
    /// render set to `enabled` and the rest untouched. Used by the
    /// dispatcher's convergence test in `dispatch.rs`.
    #[allow(dead_code)]
    fn render_policy(enabled: bool) -> Vec<bm_proto::TaskPref> {
        bm_proto::Stage::DEFAULT_PRIORITY
            .iter()
            .map(|s| bm_proto::TaskPref {
                stage: *s,
                enabled: enabled || *s != bm_proto::Stage::Render,
            })
            .collect()
    }

    /// The policy edit persists — config, not runtime — and sends nothing
    /// itself: delivery is the dispatcher's convergence job, which a one-shot
    /// push misses in every state that matters (box down at edit time, box
    /// rebooting into its default, inductor restarted, worker busy behind the
    /// timeout, hand-edited machines.json).
    #[tokio::test]
    async fn a_policy_edit_persists_and_leaves_delivery_to_the_dispatcher() {
        let d = scratch();
        let layout = bm_core::Layout::new(d.path());
        let st: Shared = std::sync::Arc::new(tokio::sync::Mutex::new(crate::state::Inner::new(
            layout,
            bm_core::config::Settings::default(),
        )));
        {
            let mut inner = st.lock().await;
            let m = Machine::new("192.168.2.2", "thang", 22, None, "worker");
            inner.machines.insert("192.168.2.2".into(), m);
        }
        let resp = set_task_policy(
            State(st.clone()),
            Json(TaskPolicyUpdate {
                addr: "192.168.2.2".into(),
                task_policy: render_policy(false),
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let inner = st.lock().await;
        let policy = inner.machines["192.168.2.2"].task_policy.clone().unwrap();
        assert!(
            !policy
                .iter()
                .find(|p| p.stage == bm_proto::Stage::Render)
                .unwrap()
                .enabled
        );
    }

    /// A plannable chapter: one run by A, so `0000_Adam.wav` is the whole
    /// expected set.
    fn one_run_layout() -> (tempfile::TempDir, bm_core::Layout) {
        let d = scratch();
        let layout = bm_core::Layout::new(d.path());
        std::fs::write(
            layout.script(1),
            r#"{"segments":[{"speaker":"A","text":"a full sentence for synthesis here"}]}"#,
        )
        .unwrap();
        std::fs::write(layout.cast("vieneu"), r#"{"A":"Adam"}"#).unwrap();
        (d, layout)
    }

    async fn seg_put(
        st: &Shared,
        chapter: u32,
        engine: &str,
        name: &str,
        body: Vec<u8>,
    ) -> (StatusCode, serde_json::Value) {
        use axum::response::IntoResponse;
        let resp = put_segment(
            State(st.clone()),
            Query(SegmentQuery {
                chapter,
                engine: engine.into(),
                name: name.into(),
            }),
            Bytes::from(body),
        )
        .await
        .into_response();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 64 << 10)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    fn segment_state(layout: &bm_core::Layout) -> Shared {
        std::sync::Arc::new(tokio::sync::Mutex::new(crate::state::Inner::new(
            layout.clone(),
            bm_core::config::Settings::default(),
        )))
    }

    #[tokio::test]
    async fn put_segment_stores_an_expected_file() {
        let (_d, layout) = one_run_layout();
        let st = segment_state(&layout);
        let wav = vec![7u8; 2000];
        let (status, v) = seg_put(&st, 1, "vieneu", "0000_Adam.wav", wav.clone()).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(v["bytes"], 2000);
        assert_eq!(
            std::fs::read(layout.seg_dir("vieneu", 1).join("0000_Adam.wav")).unwrap(),
            wav
        );
    }

    #[tokio::test]
    async fn put_segment_rejects_unknown_names_and_bad_sizes() {
        let (_d, layout) = one_run_layout();
        let st = segment_state(&layout);
        // Outside the expected set: a worker may not write arbitrary paths.
        let (status, v) = seg_put(&st, 1, "vieneu", "../../../evil.wav", vec![7u8; 2000]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
        let (status, v) = seg_put(&st, 1, "vieneu", "nope.wav", vec![7u8; 2000]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
        // Below the completeness threshold and above the unit cap: the merger
        // would ignore both, so the store refuses them instead.
        let (status, _) = seg_put(&st, 1, "vieneu", "0000_Adam.wav", vec![7u8; 900]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let over = bm_core::assemble::MAX_SEGMENT_BYTES + 1;
        let (status, v) = seg_put(&st, 1, "vieneu", "0000_Adam.wav", vec![7u8; over]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
        // Wrong engine for this run.
        let (status, _) = seg_put(&st, 1, "gemini", "0000_Adam.wav", vec![7u8; 2000]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            !layout
                .seg_dir("vieneu", 1)
                .join("../../../evil.wav")
                .exists()
                && std::fs::read_dir(layout.seg_dir("vieneu", 1))
                    .map(|rd| rd.count())
                    .unwrap_or(0)
                    == 0,
            "rejections store nothing"
        );
    }

    #[tokio::test]
    async fn register_and_heartbeat_flip_a_machine_online() {
        let d = scratch();
        let layout = bm_core::Layout::new(d.path());
        let st: Shared = std::sync::Arc::new(tokio::sync::Mutex::new(crate::state::Inner::new(
            layout,
            bm_core::config::Settings::default(),
        )));
        register(
            State(st.clone()),
            Json(Register {
                worker_id: "w1".into(),
                addr: "192.168.2.2".into(),
                hostname: "box".into(),
                capabilities: vec![],
                tts_url: None,
                version: "0.2.0".into(),
            }),
        )
        .await;
        {
            let inner = st.lock().await;
            assert_eq!(inner.machines["192.168.2.2"].state, MachineState::Online);
        }
        heartbeat(
            State(st.clone()),
            Json(Heartbeat {
                worker_id: "w1".into(),
                addr: "192.168.2.2".into(),
                task_id: None,
                stage: None,
                chapter: None,
                progress: 0.0,
                activity: "idle".into(),
                eta_secs: None,
                ts: bm_proto::now_secs(),
                hostname: "box".into(),
                alias: String::new(),
                cpu_pct: None,
                mem_pct: None,
                mem_gb: None,
                sidecars: None,
                sidecar_gb: None,
                capabilities: vec![],
                sidecar_keep: None,
            }),
        )
        .await;
        {
            let inner = st.lock().await;
            let m = &inner.machines["192.168.2.2"];
            assert_eq!(m.state, MachineState::Online);
            assert!(m.last_seen > 0);
        }
    }

    #[tokio::test]
    async fn register_carries_the_registry_handle_to_the_panes() {
        // The hawk hunt, server side: provision logs the registry handle
        // while beats carry the OS hostname — the panes can only agree if
        // register keeps the handle on the machine.
        let d = scratch();
        let layout = bm_core::Layout::new(d.path());
        bm_core::provision::save_box(
            &layout.machines(),
            &bm_core::provision::LinkedBox {
                name: "hawk".into(),
                addr: "192.168.2.2".into(),
                user: "thang".into(),
                port: 22,
                key: None,
                role: "worker".into(),
                task_policy: None,
                accepting_work: true,
            },
        )
        .unwrap();
        let st: Shared = std::sync::Arc::new(tokio::sync::Mutex::new(crate::state::Inner::new(
            layout,
            bm_core::config::Settings::default(),
        )));
        register(
            State(st.clone()),
            Json(Register {
                worker_id: "thang-marmot".into(),
                addr: "192.168.2.2".into(),
                hostname: "thang".into(),
                capabilities: vec!["render-segments".into()],
                tts_url: None,
                version: "0.2.3".into(),
            }),
        )
        .await;
        {
            let inner = st.lock().await;
            assert_eq!(
                inner.machines["192.168.2.2"].name, "hawk",
                "panes must say what provision said"
            );
        }
    }

    #[tokio::test]
    async fn a_beating_worker_clears_a_stale_would_not_start_note() {
        // Provision's verdict outlives its launch: the worker did start
        // (via :B, by hand) but the pane kept saying it would not. The
        // first beat with a pulse refutes exactly that wording — and a
        // live note is left alone.
        let d = scratch();
        let layout = bm_core::Layout::new(d.path());
        let st: Shared = std::sync::Arc::new(tokio::sync::Mutex::new(crate::state::Inner::new(
            layout,
            bm_core::config::Settings::default(),
        )));
        {
            let mut inner = st.lock().await;
            let mut m = Machine::new("192.168.2.2", "thang", 22, None, "worker");
            m.note = "provisioned but the worker would not start — :prov to retry".into();
            inner.machines.insert("192.168.2.2".into(), m);
            inner
                .workers
                .insert("thang-marmot".into(), "192.168.2.2".into());
        }
        let beat = || Heartbeat {
            worker_id: "thang-marmot".into(),
            addr: "192.168.2.2".into(),
            task_id: None,
            stage: None,
            chapter: None,
            progress: 0.0,
            activity: "idle".into(),
            eta_secs: None,
            ts: bm_proto::now_secs(),
            hostname: "thang".into(),
            alias: "marmot".into(),
            cpu_pct: None,
            mem_pct: None,
            mem_gb: None,
            sidecars: None,
            sidecar_gb: None,
            capabilities: vec![],
            sidecar_keep: None,
        };
        heartbeat(State(st.clone()), Json(beat())).await;
        {
            let inner = st.lock().await;
            let note = &inner.machines["192.168.2.2"].note;
            assert!(
                !note.contains("would not start"),
                "a live worker refutes it: {note}"
            );
        }
        {
            let mut inner = st.lock().await;
            inner.machines.get_mut("192.168.2.2").unwrap().note =
                "ready — Online on its first beat".into();
        }
        heartbeat(State(st.clone()), Json(beat())).await;
        {
            let inner = st.lock().await;
            assert_eq!(
                inner.machines["192.168.2.2"].note,
                "ready — Online on its first beat"
            );
        }
    }

    /// Parking writes intent, and intent has to outlive the process.
    ///
    /// Two things are worth pinning: it lands in `machines.json` (not the
    /// ledger, which is cleared on a re-provision) and it does **not** disturb
    /// the state — a park is not a phase change, so a box that is `Online` when
    /// it is parked must still be `Online` afterwards. Getting that wrong is how
    /// a parked box would get stamped `Offline` for going quiet, which is the
    /// one thing the operator did not ask for.
    #[tokio::test]
    async fn the_accepting_route_parks_a_box_and_the_park_outlives_a_restart() {
        let d = scratch();
        let layout = bm_core::Layout::new(d.path());
        let machines_path = layout.machines();
        let st: Shared = std::sync::Arc::new(tokio::sync::Mutex::new(crate::state::Inner::new(
            layout,
            bm_core::config::Settings::default(),
        )));
        {
            let mut inner = st.lock().await;
            let mut m = Machine::new("192.168.2.2", "thang", 22, None, "worker");
            m.set_state(MachineState::Online);
            inner.machines.insert("192.168.2.2".into(), m);
        }
        let body = |accepting: bool| {
            Json(AcceptingUpdate {
                addr: "192.168.2.2".into(),
                accepting_work: accepting,
            })
        };
        set_accepting_work(State(st.clone()), body(false)).await;
        {
            let inner = st.lock().await;
            let m = &inner.machines["192.168.2.2"];
            assert!(m.relaxed());
            assert_eq!(
                m.state,
                MachineState::Online,
                "a park is intent, not a phase — the box is still alive"
            );
        }
        // Config, not runtime: read straight off the file the next process loads.
        let boxes = bm_core::provision::load_boxes(&machines_path);
        assert_eq!(boxes.len(), 1);
        assert!(
            !boxes[0].accepting_work,
            "the park must survive a restart, so it lives beside the policy"
        );
        // Idempotent, not a toggle: the same request twice leaves it parked.
        set_accepting_work(State(st.clone()), body(false)).await;
        assert!(st.lock().await.machines["192.168.2.2"].relaxed());
        // And waking is the same call with the other value.
        set_accepting_work(State(st.clone()), body(true)).await;
        assert!(!st.lock().await.machines["192.168.2.2"].relaxed());
        assert!(bm_core::provision::load_boxes(&machines_path)[0].accepting_work);
        // Unknown addresses are refused, never created — as every machine route is.
        let reply = set_accepting_work(
            State(st.clone()),
            Json(AcceptingUpdate {
                addr: "10.9.9.9".into(),
                accepting_work: false,
            }),
        )
        .await
        .into_response();
        assert_eq!(reply.status(), axum::http::StatusCode::OK);
        assert!(!st.lock().await.machines.contains_key("10.9.9.9"));
    }

    #[tokio::test]
    async fn machine_state_route_updates_known_boxes_only() {
        let d = scratch();
        let layout = bm_core::Layout::new(d.path());
        let st: Shared = std::sync::Arc::new(tokio::sync::Mutex::new(crate::state::Inner::new(
            layout,
            bm_core::config::Settings::default(),
        )));
        {
            let mut inner = st.lock().await;
            inner.machines.insert(
                "192.168.2.2".into(),
                Machine::new("192.168.2.2", "thang", 22, None, "worker"),
            );
        }
        set_machine_state(
            State(st.clone()),
            Json(MachineStateUpdate {
                addr: "192.168.2.2".into(),
                state: MachineState::Provisioning,
                note: "pushing sources".into(),
                task_policy: None,
            }),
        )
        .await;
        {
            let inner = st.lock().await;
            let m = &inner.machines["192.168.2.2"];
            assert_eq!(m.state, MachineState::Provisioning);
            assert_eq!(m.note, "pushing sources");
        }
        // Unknown addresses are refused, never created.
        set_machine_state(
            State(st.clone()),
            Json(MachineStateUpdate {
                addr: "10.9.9.9".into(),
                state: MachineState::Error,
                note: String::new(),
                task_policy: None,
            }),
        )
        .await;
        {
            let inner = st.lock().await;
            assert!(!inner.machines.contains_key("10.9.9.9"));
        }
    }

    #[test]
    fn local_roster_reads_cast_and_speakers_from_disk() {
        let d = scratch();
        let layout = bm_core::Layout::new(d.path());
        std::fs::write(layout.cast("vieneu"), r#"{"A":"Đức Trí"}"#).unwrap();
        std::fs::write(
            layout.script(1),
            r#"{"roster":["A"],"segments":[{"speaker":"A","text":"x"}]}"#,
        )
        .unwrap();
        let r = local_roster(&layout);
        assert_eq!(r.source, "offline");
        assert_eq!(r.cast.get("A").map(|s| s.as_str()), Some("Đức Trí"));
        assert!(
            r.characters.contains(&"A".to_string()),
            "{:?}",
            r.characters
        );
        assert!(!r.voices.is_empty(), "catalogue fallback lists voices");
    }

    #[test]
    fn offline_swap_applies_the_same_invalidation_as_live() {
        let d = scratch();
        let layout = bm_core::Layout::new(d.path());
        std::fs::write(
            layout.script(1),
            r#"{"segments":[{"speaker":"A","text":"x"},{"speaker":"B","text":"z"}]}"#,
        )
        .unwrap();
        std::fs::write(layout.cast("vieneu"), r#"{"A":"Đức Trí","B":"Adam"}"#).unwrap();
        let seg = layout.seg_dir("vieneu", 1);
        std::fs::create_dir_all(&seg).unwrap();
        std::fs::write(seg.join("0000_Đức Trí.wav"), vec![0u8; 2000]).unwrap();
        std::fs::write(seg.join("0001_Adam.wav"), vec![0u8; 2000]).unwrap();

        let msg = offline_swap_apply(&layout, "A", "Minh Triết").expect("offline swap");
        assert!(msg.contains("Đức Trí -> Minh Triết"), "{msg}");
        assert!(msg.contains("offline"), "{msg}");
        assert!(
            !seg.join("0000_Đức Trí.wav").exists(),
            "stale run file must go"
        );
        assert!(
            seg.join("0001_Adam.wav").exists(),
            "other voices keep cache"
        );
        let cast = bm_core::cast::read_cast("vieneu", &layout.cast("vieneu"));
        assert_eq!(cast["A"], "Minh Triết");
    }

    #[tokio::test]
    async fn offline_remix_applies_the_same_invalidation_as_live() {
        let d = scratch();
        let layout = bm_core::Layout::new(d.path());
        std::fs::create_dir_all(layout.output()).unwrap();
        std::fs::write(layout.final_mp3(1), b"old mix").unwrap();
        // A published merge implies a script existed: the design fingerprint is
        // computed from it, so without one the chapter has no mix to invalidate
        // and the requeue would be a no-op for a reason that has nothing to do
        // with the remix.
        std::fs::write(
            layout.script(1),
            r#"{"segments":[{"speaker":"A","text":"Chương 1"}]}"#,
        )
        .unwrap();
        bm_core::write_json(
            &layout.ledger(),
            &serde_json::json!({"tasks": [
                {"chapter": 1, "stage": "merge", "state": "done",
                 "attempts": 0, "assigned_to": null, "lease_until": null,
                 "detail": "", "updated": 0},
            ]}),
        )
        .unwrap();

        let msg = offline_remix_apply(&layout, Some(1.5), Some(0.5), Some(0.0), Some(0.25))
            .expect("offline remix");
        assert!(msg.contains("1.5"), "{msg}");
        assert!(msg.contains("offline"), "{msg}");
        assert!(!layout.final_mp3(1).exists(), "stale mp3 must go");
        let settings = bm_core::config::Settings::load(&layout.settings());
        assert_eq!(
            (
                settings.speed,
                settings.effect_volume,
                settings.music_volume,
                settings.inject_volume
            ),
            (1.5, 0.5, 0.0, 0.25)
        );
        offline_remix_apply(
            &bm_core::Layout::new(d.path()),
            Some(1.0),
            Some(1.0),
            Some(1.0),
            None,
        )
        .unwrap();
        assert_eq!(
            bm_core::config::Settings::load(&layout.settings()).inject_volume,
            0.25
        );
        let ledger: serde_json::Value =
            bm_core::read_json(&layout.bm_state().join("ledger.json")).unwrap();
        assert_eq!(ledger["tasks"][0]["state"], "pending");
    }

    /// Every entry under `root`, so "the op wrote nothing" can be asserted on
    /// the tree rather than on one path somebody remembered to check.
    fn tree(root: &std::path::Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in rd.filter_map(|e| e.ok()) {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p.clone());
                }
                out.push(p.strip_prefix(root).unwrap().display().to_string());
            }
        }
        out.sort();
        out
    }

    #[test]
    fn a_rendered_sample_comes_back_as_bytes_and_leaves_no_file() {
        // The complaint this answers: auditioning wrote a clip per voice into
        // `data/previews/`, so a session of A/B-ing left a directory of wavs
        // nobody asked for. The audio now rides the wire and the *client* puts
        // it next to the speaker.
        let d = scratch();
        let layout = bm_core::Layout::new(d.path());
        let before = tree(d.path());

        // A few bytes that are not valid UTF-8, to catch a lossy round trip.
        let wav: Vec<u8> = (0u8..=255).collect();
        let res = audio_result("Đức Trí", "sample", &wav);
        assert!(res.ok, "{}", res.message);
        assert!(
            res.message.contains("sample"),
            "say which half: {}",
            res.message
        );
        assert!(
            !res.message.contains('/'),
            "there is no path to report any more: {}",
            res.message
        );

        let b64 = res.audio_b64.expect("the audio rides along");
        let back =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64.as_bytes())
                .expect("valid base64");
        assert_eq!(back, wav, "the bytes survive the wire byte for byte");

        assert_eq!(
            tree(d.path()),
            before,
            "the op wrote nothing under the root"
        );
        assert!(!layout.data().join("previews").exists(), "no audition dump");
    }

    #[test]
    fn an_empty_render_is_a_failure_not_an_empty_sample() {
        // A 200 with no body would otherwise be handed to the client as audio it
        // cannot play, and the client would blame the speaker.
        let res = audio_result("Adam", "line", b"");
        assert!(!res.ok, "{}", res.message);
        assert!(res.audio_b64.is_none(), "an empty body is not audio");
        assert!(res.message.contains("no audio"), "{}", res.message);
        assert!(
            res.message.contains("Adam"),
            "name the voice: {}",
            res.message
        );
    }

    /// The ledger as `id -> state`, sorted so a diff reads as a ledger diff.
    async fn ledger(st: &Shared) -> Vec<(String, &'static str)> {
        let inner = st.lock().await;
        let mut out: Vec<(String, &'static str)> = inner
            .tasks
            .iter()
            .map(|(id, t)| (id.clone(), t.state.as_str()))
            .collect();
        out.sort();
        out
    }

    async fn shelve(st: &Shared, chapter: u32, stage: bm_proto::Stage) {
        let mut inner = st.lock().await;
        let mut t = bm_proto::Task::new(chapter, stage);
        t.state = bm_proto::TaskState::Shelved;
        t.attempts = 3;
        inner.tasks.insert(t.id(), t);
    }

    /// `Op::Retry` carries three scopes on one request shape, so the *dispatch*
    /// is what decides how much a single call touches. The TUI parses the
    /// argument and the state layer does the work; this is the seam between
    /// them, and it is where a stage with no chapter must refuse rather than
    /// widen to every chapter of that stage.
    #[tokio::test]
    async fn retry_dispatch_narrows_by_scope_and_refuses_a_bare_stage() {
        let (_d, layout) = one_run_layout();
        let st = segment_state(&layout);
        shelve(&st, 24, bm_proto::Stage::Render).await;
        shelve(&st, 24, bm_proto::Stage::Digest).await;
        shelve(&st, 25, bm_proto::Stage::Digest).await;

        let call = |stage, chapter, force| {
            op(
                State(st.clone()),
                Json(OpRequest {
                    op: bm_proto::Op::Retry,
                    stage,
                    chapter,
                    force,
                    ..Default::default()
                }),
            )
        };

        // A stage on its own: refused, and the refusal is inert. Widening it
        // would silently requeue every chapter of that stage.
        let res = call(Some(bm_proto::Stage::Render), None, None).await;
        assert!(!res.0.ok, "{}", res.0.message);
        assert!(
            res.0.message.contains("needs a chapter"),
            "say what is missing: {}",
            res.0.message
        );
        assert_eq!(
            ledger(&st).await,
            [
                ("digest:24".to_string(), "shelved"),
                ("digest:25".to_string(), "shelved"),
                ("render:24".to_string(), "shelved"),
            ],
            "a refused scope must not move a single task"
        );

        // A chapter alone: every shelved stage of it, and nothing else.
        let res = call(None, Some(24), None).await;
        assert!(res.0.ok, "{}", res.0.message);
        assert_eq!(
            ledger(&st).await,
            [
                ("digest:24".to_string(), "pending"),
                ("digest:25".to_string(), "shelved"),
                ("render:24".to_string(), "pending"),
            ],
            "ch25 is untouched"
        );

        // Stage + chapter: exactly one task — what the Tasks screen sends.
        // Both of ch24's stages are shelved again so that "one task" and "every
        // shelved stage of the chapter" cannot produce the same ledger: a
        // chapter-wide dispatch would take `digest:24` too.
        shelve(&st, 24, bm_proto::Stage::Render).await;
        shelve(&st, 24, bm_proto::Stage::Digest).await;
        let res = call(Some(bm_proto::Stage::Render), Some(24), None).await;
        assert!(res.0.ok, "{}", res.0.message);
        assert_eq!(
            ledger(&st).await,
            [
                ("digest:24".to_string(), "shelved"),
                ("digest:25".to_string(), "shelved"),
                ("render:24".to_string(), "pending"),
            ],
            "the named task moves and its sibling stage does not"
        );

        // Neither: the blanket retry, which is what a bare `:retry` means.
        let res = call(None, None, None).await;
        assert!(res.0.ok, "{}", res.0.message);
        assert_eq!(
            ledger(&st).await,
            [
                ("digest:24".to_string(), "pending"),
                ("digest:25".to_string(), "pending"),
                ("render:24".to_string(), "pending"),
            ],
            "the blanket scope reaches the other chapter"
        );

        // `force` has to survive the wire, or the Tasks screen's `F` is a plain
        // requeue and the stale artifact it was meant to clear stays in place.
        // The message is the observable: only a forced retry says so.
        let res = call(Some(bm_proto::Stage::Render), Some(24), Some(true)).await;
        assert!(res.0.ok, "{}", res.0.message);
        assert!(
            res.0.message.contains("forced re-run"),
            "force must reach the state layer: {}",
            res.0.message
        );
    }
}

#[cfg(test)]
mod segment_tests {
    use super::*;

    #[test]
    fn segment_serves_a_rendered_wav_and_its_sentence() {
        let dir = tempfile::tempdir().unwrap();
        let layout = bm_core::Layout::new(dir.path());
        std::fs::create_dir_all(layout.data()).unwrap();
        // Two speakers; the wav covers segment 1 in Adam's voice.
        std::fs::write(
            layout.script(1),
            serde_json::json!({"segments": [
                {"speaker": "Narrator", "text": "Mở đầu."},
                {"speaker": "Kiên", "text": "Kiên lên tiếng."},
            ]})
            .to_string(),
        )
        .unwrap();
        let seg = layout.seg_dir("vieneu", 1);
        std::fs::create_dir_all(&seg).unwrap();
        std::fs::write(seg.join("0001_Adam.wav"), b"RIFF-fake").unwrap();

        let res = op_segment(&layout, "vieneu", "Kiên", "Adam", None);
        assert!(res.ok, "{}", res.message);
        assert_eq!(res.line_speaker.as_deref(), Some("Kiên"));
        assert_eq!(res.line_text.as_deref(), Some("Kiên lên tiếng."));
        assert!(res.audio_b64.is_some(), "bytes, not a path");
        assert!(
            res.message.contains("rendered"),
            "say what it was: {}",
            res.message
        );

        // The held line never rendered in Adam's voice, but one of Kiên's
        // did (the fresh-swap state: rendered chapter by chapter). Play
        // hers, still zero synthesis — and hold it, so T compares on the
        // same sentence instead of another random pick.
        let fallback = op_segment(
            &layout,
            "vieneu",
            "Kiên",
            "Adam",
            Some("a line from chapter 99"),
        );
        assert!(fallback.ok, "{}", fallback.message);
        assert_eq!(fallback.line_text.as_deref(), Some("Kiên lên tiếng."));
        assert_eq!(fallback.line_speaker.as_deref(), Some("Kiên"));
        assert!(fallback.audio_b64.is_some(), "bytes, not synthesis");

        // A voice with nothing rendered fails honestly — never synthesizes.
        let miss = op_segment(&layout, "vieneu", "Vũ", "Nobody", None);
        assert!(!miss.ok);
        assert!(
            miss.message.contains("elsewhere or not yet"),
            "{}",
            miss.message
        );
        assert!(miss.audio_b64.is_none());
    }

    #[test]
    fn segment_matches_keys_names_and_folds() {
        // Wavs carry whatever the cast held at render time — often a
        // lowercase key (`adam`) while the operator asks the display name
        // (`Adam`), or an ASCII slug (`pham-tuyen`) for `Phạm Tuyên`.
        let dir = tempfile::tempdir().unwrap();
        let layout = bm_core::Layout::new(dir.path());
        std::fs::create_dir_all(layout.data()).unwrap();
        std::fs::write(
            layout.script(3),
            serde_json::json!({"segments": [
                {"speaker": "Vũ", "text": "Vũ nói."},
                {"speaker": "Kiên", "text": "Kiên đáp."},
            ]})
            .to_string(),
        )
        .unwrap();
        let seg = layout.seg_dir("vieneu", 3);
        std::fs::create_dir_all(&seg).unwrap();
        std::fs::write(seg.join("0000_adam.wav"), b"RIFF-a").unwrap();
        std::fs::write(seg.join("0001_pham-tuyen.wav"), b"RIFF-p").unwrap();

        let res = op_segment(&layout, "vieneu", "Kiên", "Adam", None);
        assert!(res.ok, "{}", res.message);
        assert_eq!(res.line_speaker.as_deref(), Some("Vũ"));

        let res = op_segment(&layout, "vieneu", "Nobody", "Phạm Tuyên", None);
        assert!(res.ok, "{}", res.message);
        assert_eq!(res.line_text.as_deref(), Some("Kiên đáp."));
    }

    #[test]
    fn segment_miss_names_where_the_renders_are() {
        let dir = tempfile::tempdir().unwrap();
        let layout = bm_core::Layout::new(dir.path());
        std::fs::create_dir_all(layout.data()).unwrap();
        std::fs::write(
            layout.script(4),
            serde_json::json!({"segments": [{"speaker": "Kiên", "text": "Kiên đáp."}]}).to_string(),
        )
        .unwrap();
        std::fs::create_dir_all(layout.seg_dir("vieneu", 4)).unwrap();

        // Lines here, no wavs: those chapters rendered on another box.
        let miss = op_segment(&layout, "vieneu", "Kiên", "Nobody", None);
        assert!(!miss.ok);
        assert!(miss.message.contains("another box"), "{}", miss.message);
        // No lines either: the chapters themselves live elsewhere.
        let miss = op_segment(&layout, "vieneu", "Ghost", "Nobody", None);
        assert!(!miss.ok);
        assert!(
            miss.message.contains("elsewhere or not yet"),
            "{}",
            miss.message
        );
    }

    #[test]
    fn segment_prefers_the_characters_own_lines() {
        let dir = tempfile::tempdir().unwrap();
        let layout = bm_core::Layout::new(dir.path());
        std::fs::create_dir_all(layout.data()).unwrap();
        std::fs::write(
            layout.script(2),
            serde_json::json!({"segments": [
                {"speaker": "Vũ", "text": "Vũ nói."},
                {"speaker": "Kiên", "text": "Kiên đáp."},
            ]})
            .to_string(),
        )
        .unwrap();
        let seg = layout.seg_dir("vieneu", 2);
        std::fs::create_dir_all(&seg).unwrap();
        // Filenames carry whatever the cast held at render time — a key here.
        std::fs::write(seg.join("0000_adam.wav"), b"RIFF-0").unwrap();
        std::fs::write(seg.join("0001_adam.wav"), b"RIFF-1").unwrap();

        // key_for_name("vieneu", "Adam") may or may not know this fixture
        // voice; either way the raw string still matches.
        let res = op_segment(&layout, "vieneu", "Kiên", "adam", None);
        assert!(res.ok, "{}", res.message);
        assert_eq!(
            res.line_speaker.as_deref(),
            Some("Kiên"),
            "own lines win over Vũ's"
        );
        assert_eq!(res.line_text.as_deref(), Some("Kiên đáp."));
    }

    #[test]
    fn a_shipped_take_is_stored_and_a_bad_one_is_skipped() {
        use base64::Engine as _;
        let dir = tempfile::tempdir().unwrap();
        let layout = bm_core::Layout::new(dir.path());
        let plan = bm_core::assemble::RenderPlan {
            chapter: 1,
            engine: "vieneu".into(),
            plan_version: bm_core::assemble::PLAN_VERSION,
            generated: 0,
            cast_hash: String::new(),
            takes: vec![bm_core::assemble::Take {
                pos: 0,
                tag: "title".into(),
                speaker: "Narrator".into(),
                voice: "Narrator".into(),
                voice_key: String::new(),
                text: "x".into(),
                temperature: 0.7,
                silence_p: 0.1,
                take_key: "k".into(),
                file: "t-k.wav".into(),
                legacy: None,
                adopted: false,
            }],
        };
        plan.save(&layout.plan(1)).unwrap();

        let enc = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
        let wav = vec![7u8; 1500];
        store_shipments(
            &layout,
            "vieneu",
            "render:1:0",
            &[
                bm_proto::UnitFile {
                    name: "t-k.wav".into(),
                    b64: enc(&wav),
                },
                // Not in the plan: a worker may not name its own path.
                bm_proto::UnitFile {
                    name: "evil.wav".into(),
                    b64: enc(&wav),
                },
                // Half-write floor: 10 bytes is not a take.
                bm_proto::UnitFile {
                    name: "t-k.wav".into(),
                    b64: enc(&[1u8; 10]),
                },
                // Undecodable payload: skipped, not fatal.
                bm_proto::UnitFile {
                    name: "t-k.wav".into(),
                    b64: "!!!".into(),
                },
            ],
        );

        let stored = layout.seg_dir("vieneu", 1).join("t-k.wav");
        assert_eq!(std::fs::read(&stored).unwrap(), wav, "the take is home");
        assert!(
            !layout.seg_dir("vieneu", 1).join("evil.wav").exists(),
            "unexpected names never land"
        );
        // task_ids that name no chapter store nothing and do not panic.
        store_shipments(
            &layout,
            "vieneu",
            "render",
            &[bm_proto::UnitFile {
                name: "t-k.wav".into(),
                b64: enc(&wav),
            }],
        );
    }
}
