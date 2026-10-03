//! The instruction channel: what the inductor calls when it pushes work.

use crate::{
    clear_task, heartbeat_now, run_offer, set_task, LoadProbe, PolicyRefusal, Shared, Sidecar,
    TaskResult, WorkerIdentity,
};
use axum::{
    body::Body,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use bm_core::{config::Settings, Layout};
use bm_proto::{Complete, Heartbeat, TaskOffer};
use serde::Deserialize;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

/// Sentinel for [`Push::tts_threads`]: no inductor instruction, so the
pub(crate) const THREADS_UNSET: u64 = u64::MAX;

/// What the server needs to answer: who this worker is, what it is doing, and
pub(crate) struct Push {
    pub(crate) who: WorkerIdentity,
    /// The cluster token. Required on every request.
    pub(crate) token: String,
    pub(crate) shared: Shared,
    /// Sampling state for the status answer — the CPU delta needs the previous
    pub(crate) probe: Mutex<LoadProbe>,
    pub(crate) layout: Layout,
    pub(crate) settings: Settings,
    /// The sidecar, warm across tasks and bounded by its own lifecycle rules —
    pub(crate) sidecar: tokio::sync::Mutex<Sidecar>,
    /// One task at a time, which is what the pull protocol's single task slot
    pub(crate) busy: AtomicBool,
    /// Unix seconds of the last request the inductor made.
    pub(crate) last_contact: AtomicU64,
    /// Unix seconds the last task finished. The sidecar reaper stops the warm
    pub(crate) last_task_end: AtomicU64,
    /// Whether this worker should keep a TTS sidecar at all. **`true` is the
    pub(crate) keep_sidecar: AtomicBool,
    /// The ONNX thread count the inductor pushed for this box's sidecar, as
    pub(crate) tts_threads: AtomicU64,
    /// Data-plane dial-out, and only that: the reverse tunnel's worker-side
    pub(crate) fetch_http: reqwest::Client,
    pub(crate) fetch_base: String,
}

/// A task outcome, stored where the completion hook can find it.
fn stash_outcome(push: &Push, report: &Complete) {
    if let Ok(mut p) = push.shared.lock() {
        p.pending = Some(report.clone());
    }
}

impl Push {
    /// Note that the inductor just spoke to us.
    pub(crate) fn touch(&self) {
        self.last_contact
            .store(bm_proto::now_secs(), Ordering::SeqCst);
    }

    /// Whether a render on this box may start its sidecar.
    pub(crate) fn keep_sidecar(&self) -> bool {
        self.keep_sidecar.load(Ordering::SeqCst)
    }

    /// Record the inductor's instruction about the sidecar. The idle reaper
    pub(crate) fn set_keep_sidecar(&self, keep: bool) {
        self.keep_sidecar.store(keep, Ordering::SeqCst);
    }

    /// The thread count the inductor asked for, or `None` for "no opinion".
    pub(crate) fn tts_threads_desired(&self) -> Option<u32> {
        let v = self.tts_threads.load(Ordering::SeqCst);
        (v != THREADS_UNSET).then_some(v as u32)
    }

    /// Record the inductor's thread instruction. `None` clears it, restoring
    pub(crate) fn set_tts_threads(&self, threads: Option<u32>) {
        self.tts_threads.store(
            threads.map(u64::from).unwrap_or(THREADS_UNSET),
            Ordering::SeqCst,
        );
    }

    pub(crate) fn is_busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }

    /// How long the inductor has been silent.
    pub(crate) fn silent_for(&self) -> Duration {
        let last = self.last_contact.load(Ordering::SeqCst);
        Duration::from_secs(bm_proto::now_secs().saturating_sub(last))
    }
}

/// The router the inductor talks to.
pub(crate) fn router(push: Arc<Push>) -> Router {
    Router::new()
        .route("/status", get(status))
        .route("/task", post(task))
        .route("/unit", get(unit))
        .route("/sidecar-policy", post(sidecar_policy))
        .route("/shutdown", post(shutdown))
        .with_state(push)
}

/// `GET /status` — the worker's heartbeat, on request instead of on a timer.
async fn status(
    State(push): State<Arc<Push>>,
    headers: HeaderMap,
) -> Result<Json<Heartbeat>, (StatusCode, String)> {
    check(&headers, &push.token)?;
    push.touch();
    let p = push.shared.lock().map(|p| p.clone()).unwrap_or_default();
    let mut probe = push
        .probe
        .lock()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "probe poisoned".into()))?;
    // The worker's own sidecar belief travels in the beat: the dispatcher's
    let keep = push.keep_sidecar();
    // Told the same way, and read back by the dispatcher's convergence: the
    let threads = push.tts_threads_desired();
    Ok(Json(heartbeat_now(
        &p, &push.who, &mut probe, keep, threads,
    )))
}

/// `Authorization: Bearer <token>`, compared without short-circuiting.
fn check(headers: &HeaderMap, token: &str) -> Result<(), (StatusCode, String)> {
    let unauthorised = || {
        (
            StatusCode::UNAUTHORIZED,
            "missing or wrong cluster token".to_string(),
        )
    };
    let Some(value) = headers.get(axum::http::header::AUTHORIZATION) else {
        return Err(unauthorised());
    };
    let Ok(text) = value.to_str() else {
        return Err(unauthorised());
    };
    match text.strip_prefix("Bearer ") {
        Some(presented) if bm_core::token::matches(presented.trim(), token) => Ok(()),
        _ => Err(unauthorised()),
    }
}

/// `POST /task` — run one offer and answer with its `Complete`.
async fn task(
    State(push): State<Arc<Push>>,
    headers: HeaderMap,
    Json(offer): Json<TaskOffer>,
) -> Response {
    if let Err(e) = check(&headers, &push.token) {
        return e.into_response();
    }
    push.touch();
    // `swap` rather than load-then-store: two simultaneous posts must not both
    if push.busy.swap(true, Ordering::SeqCst) {
        let running = push
            .shared
            .lock()
            .map(|p| p.task_id.clone().unwrap_or_default())
            .unwrap_or_default();
        return (StatusCode::CONFLICT, format!("already running {running}")).into_response();
    }
    let _guard = BusyGuard {
        busy: &push.busy,
        shared: &push.shared,
    };
    // Tell the status answer what this worker is doing. The pull path sets
    set_task(&push.shared, &offer);

    let mut sidecar = push.sidecar.lock().await;
    // The inductor URL is deliberately empty: units stay on this box and the
    let started = std::time::Instant::now();
    // The sidecar policy as of **before** the offer: a snapshot, not a read
    let keep_sidecar = push.keep_sidecar();
    let tts_threads = push.tts_threads_desired();
    let result = run_offer(
        &push.layout,
        &push.settings,
        &offer,
        &push.shared,
        &mut sidecar,
        Some((&push.fetch_http, push.fetch_base.as_str())),
        keep_sidecar,
        tts_threads,
    )
    .await;
    // Deliberately **no** `sidecar.stop()` here: the sidecar is worker-owned
    clear_task(&push.shared);

    // A render refused because the operator turned render off is **not a
    let refused = result.as_ref().err().is_some_and(|e| {
        e.chain()
            .any(|c| c.downcast_ref::<PolicyRefusal>().is_some())
    });
    if refused {
        return (
            StatusCode::FORBIDDEN,
            "render refused: this box's policy turns render off".to_string(),
        )
            .into_response();
    }
    push.last_task_end
        .store(bm_proto::now_secs(), Ordering::SeqCst);

    match result {
        // Destructure once: the stash and the response are the same report,
        Ok(done) => {
            let TaskResult {
                ok,
                detail,
                delta,
                units,
                script,
                text,
                crawl,
                mp3_b64,
                unit_files,
            } = done;
            let report = Complete {
                worker_id: push.who.worker_id.clone(),
                task_id: offer.task_id.clone(),
                ok,
                detail,
                duration_secs: started.elapsed().as_secs_f64(),
                bible_delta: delta,
                units,
                script,
                text,
                crawl,
                mp3_b64,
                unit_files,
            };
            // Stash before answering: if the connection below dies in flight,
            stash_outcome(&push, &report);
            Json(report).into_response()
        }
        // A stage that *failed* is an answer, not a transport error — and the
        Err(e) => {
            let report = Complete {
                worker_id: push.who.worker_id.clone(),
                task_id: offer.task_id.clone(),
                ok: false,
                detail: format!("{} ch{} failed: {e:#}", offer.stage, offer.chapter),
                duration_secs: started.elapsed().as_secs_f64(),
                bible_delta: None,
                units: 0,
                script: None,
                text: None,
                // The failure path has no verdict to carry: the error text
                crawl: None,
                mp3_b64: None,
                unit_files: Vec::new(),
            };
            stash_outcome(&push, &report);
            Json(report).into_response()
        }
    }
}

/// Clears the busy flag AND the shared task slot however the handler leaves —
struct BusyGuard<'a> {
    busy: &'a AtomicBool,
    shared: &'a Shared,
}

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.busy.store(false, Ordering::SeqCst);
        clear_task(self.shared);
    }
}

#[derive(Deserialize)]
struct UnitQuery {
    chapter: u32,
    engine: String,
    name: String,
}

/// `GET /unit` — one rendered wav, from this worker's own segment directory.
async fn unit(
    State(push): State<Arc<Push>>,
    headers: HeaderMap,
    Query(q): Query<UnitQuery>,
) -> Response {
    if let Err(e) = check(&headers, &push.token) {
        return e.into_response();
    }
    push.touch();
    // The name arrives from the network, so it is checked before it is joined:
    if !safe_unit_name(&q.name) {
        return (
            StatusCode::BAD_REQUEST,
            format!("bad unit name {:?}", q.name),
        )
            .into_response();
    }
    let path = push.layout.seg_dir(&q.engine, q.chapter).join(&q.name);
    match tokio::fs::read(&path).await {
        Ok(bytes) => Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "audio/wav")
            .body(Body::from(bytes))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        // Absent is not an error: this worker does not have that unit — another
        Err(_) => (StatusCode::NOT_FOUND, format!("no {}", q.name)).into_response(),
    }
}

/// The body of `POST /sidecar-policy`.
#[derive(Deserialize)]
struct SidecarPolicyUpdate {
    keep: bool,
    /// The ONNX thread count to open the sidecar with; omitted/`null` means
    #[serde(default)]
    threads: Option<u32>,
}

/// `POST /sidecar-policy` — a policy consequence, not a policy.
async fn sidecar_policy(
    State(push): State<Arc<Push>>,
    headers: HeaderMap,
    Json(u): Json<SidecarPolicyUpdate>,
) -> Response {
    if let Err(e) = check(&headers, &push.token) {
        return e.into_response();
    }
    push.touch();
    push.set_keep_sidecar(u.keep);
    push.set_tts_threads(u.threads);
    if u.keep {
        println!("sidecar policy: keep — the next render may ensure it again");
    } else {
        println!(
            "sidecar policy: do not keep — the idle reaper stops it as soon as this worker is idle"
        );
    }
    // Answered without touching the sidecar's mutex on purpose: a render
    Json(serde_json::json!({"ok": true})).into_response()
}

/// `POST /shutdown` — exit on the inductor's say-so.
async fn shutdown(State(push): State<Arc<Push>>, headers: HeaderMap) -> Response {
    if let Err(e) = check(&headers, &push.token) {
        return e.into_response();
    }
    push.touch();
    println!("inductor asked for shutdown — exiting");
    // Take the sidecar with us: a child left behind is 2.85 GB held by a box
    push.sidecar.lock().await.reap_all().await;
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_millis(250)).await;
        std::process::exit(0);
    });
    Json(serde_json::json!({"ok": true, "exiting": true})).into_response()
}

/// How long the warm model is kept after the last task. Long enough to bridge
const SIDECAR_IDLE_SECS: u64 = 180;

/// Stop the sidecar once the worker has been idle past [`SIDECAR_IDLE_SECS`].
pub(crate) async fn sidecar_reaper(push: Arc<Push>) {
    loop {
        tokio::time::sleep(Duration::from_secs(30)).await;
        if push.is_busy() {
            continue;
        }
        let idle = bm_proto::now_secs().saturating_sub(push.last_task_end.load(Ordering::SeqCst));
        if idle < SIDECAR_IDLE_SECS {
            continue;
        }
        // The policy is checked **inside** the lock and before `is_running`,
        let mut sidecar = push.sidecar.lock().await;
        if !push.keep_sidecar() {
            if sidecar.is_running() {
                sidecar.stop();
                println!("sidecar stopped — the policy says this box keeps none");
            }
            continue;
        }
        if sidecar.is_running() {
            sidecar.stop();
            println!("sidecar stopped after {idle}s idle — its RSS returns to the OS");
            continue;
        }
        // An adopted server nobody owns: provisioning's boot-time instance,
        if !bm_core::is_local_node(&push.who.addr) {
            if let Some(why) = sidecar.over_budget() {
                println!("adopted sidecar {why} — reaping it so this box drains");
                sidecar.reap_all().await;
            }
        }
    }
}

/// A unit name is one stored take's filename and nothing else.
fn safe_unit_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() < 256
        && std::path::Path::new(name)
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| bm_core::assemble::TAKE_EXTENSIONS.contains(&e))
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
        && !name.starts_with('.')
}

#[cfg(test)]
mod tests;
