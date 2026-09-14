//! Control API: the only way workers and operators talk to the scheduler.

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{delete, get, post},
    Router,
};
use bm_proto::{Complete, Heartbeat, Machine, OpRequest, OpResult, Register, TaskRequest};
use serde::Deserialize;
use std::sync::Arc;

use crate::state::Inner;

pub type Shared = Arc<tokio::sync::Mutex<Inner>>;

#[derive(Deserialize)]
struct TaskQuery {
    worker_id: String,
}

async fn register(State(st): State<Shared>, Json(r): Json<Register>) -> impl IntoResponse {
    let mut inner = st.lock().await;
    // Known machine by address, else the local one for loopback agents.
    let addr = if inner.machines.contains_key(&r.addr) {
        r.addr.clone()
    } else if r.addr == "127.0.0.1" || r.addr == "localhost" {
        "127.0.0.1".into()
    } else {
        let m = Machine::new(&r.addr, "unknown", 22, None, "worker");
        inner.machines.insert(r.addr.clone(), m);
        r.addr.clone()
    };
    inner.workers.insert(r.worker_id.clone(), addr);
    inner.save();
    Json(serde_json::json!({"ok": true}))
}

async fn heartbeat(State(st): State<Shared>, Json(h): Json<Heartbeat>) -> impl IntoResponse {
    let mut inner = st.lock().await;
    // Refresh the worker→machine mapping on every beat: it heals itself
    // across inductor restarts (the persisted ledger may predate it).
    inner.workers.insert(h.worker_id.clone(), h.addr.clone());
    inner.save();
    let addr = inner.workers.get(&h.worker_id).cloned();
    if let Some(addr) = addr {
        if let Some(m) = inner.machines.get_mut(&addr) {
            m.last_seen = bm_proto::now_secs();
        }
    }
    inner.beats.insert(h.worker_id.clone(), h);
    Json(serde_json::json!({"ok": true}))
}

async fn task(State(st): State<Shared>, Query(q): Query<TaskQuery>) -> impl IntoResponse {
    let mut inner = st.lock().await;
    match inner.offer(&q.worker_id) {
        Some(offer) => (StatusCode::OK, Json(serde_json::to_value(offer).unwrap())).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

async fn complete(State(st): State<Shared>, Json(c): Json<Complete>) -> impl IntoResponse {
    let mut inner = st.lock().await;
    let line = inner.complete(&c);
    println!("{line}");
    Json(serde_json::json!({"ok": true}))
}

async fn add_machine(State(st): State<Shared>, Json(m): Json<Machine>) -> impl IntoResponse {
    let mut inner = st.lock().await;
    inner.machines.insert(m.addr.clone(), m);
    inner.save();
    Json(serde_json::json!({"ok": true}))
}

#[derive(Deserialize)]
struct AddrQuery {
    addr: String,
}

async fn drop_machine(State(st): State<Shared>, Query(q): Query<AddrQuery>) -> impl IntoResponse {
    let mut inner = st.lock().await;
    inner.machines.remove(&q.addr);
    inner.save();
    Json(serde_json::json!({"ok": true}))
}

async fn op(State(st): State<Shared>, Json(req): Json<OpRequest>) -> Json<OpResult> {
    let mut inner = st.lock().await;
    match req.op {
        bm_proto::Op::Translate => {
            let (start, count) = (req.start.unwrap_or(21), req.count.unwrap_or(80));
            let (crawls, digests) = inner.enqueue_translate(start, count);
            Json(OpResult {
                ok: true,
                message: format!("translate ch{start}..: {crawls} crawls + {digests} digests queued"),
            })
        }
        other => Json(OpResult {
            ok: false,
            message: format!("op {} lands in M6", other.as_str()),
        }),
    }
}

async fn state(State(st): State<Shared>) -> impl IntoResponse {
    let inner = st.lock().await;
    Json(serde_json::json!({
        "tasks": inner.tasks.values().collect::<Vec<_>>(),
        "machines": inner.machines.values().collect::<Vec<_>>(),
        "beats": inner.beats.values().collect::<Vec<_>>(),
        "counts": inner.counts(),
    }))
}

pub fn router(st: Shared) -> Router {
    Router::new()
        .route("/api/register", post(register))
        .route("/api/heartbeat", post(heartbeat))
        .route("/api/task", get(task))
        .route("/api/complete", post(complete))
        .route("/api/machines", post(add_machine))
        .route("/api/machines", delete(drop_machine))
        .route("/api/op", post(op))
        .route("/api/state", get(state))
        // Merge reports carry base64 mp3s (~7MB); the 2MB default would 413 them.
        .layer(axum::extract::DefaultBodyLimit::disable())
        .with_state(st)
}

// Silence the unused-import warning until M6 operations need TaskRequest.
#[allow(dead_code)]
fn _task_req_is_part_of_the_protocol(_r: TaskRequest) {}
