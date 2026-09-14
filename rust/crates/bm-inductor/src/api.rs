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
    match req.op {
        bm_proto::Op::Translate => {
            let mut inner = st.lock().await;
            let (start, count) = (req.start.unwrap_or(21), req.count.unwrap_or(80));
            let (crawls, digests) = inner.enqueue_translate(start, count);
            Json(OpResult {
                ok: true,
                message: format!("translate ch{start}..: {crawls} crawls + {digests} digests queued"),
            })
        }
        bm_proto::Op::CrawlSetup => {
            let (layout, template) = {
                let mut inner = st.lock().await;
                if let Some(t) = req.url_template.clone() {
                    inner.settings.url_template = t.clone();
                    let _ = inner.settings.save(&inner.layout.settings());
                }
                (inner.layout.clone(), inner.settings.url_template.clone())
            };
            Json(op_crawl_setup(&layout, &template, req.start.unwrap_or(21)).await)
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
                return Json(OpResult { ok: false, message: "swap needs character + voice".into() });
            }
            let mut inner = st.lock().await;
            match inner.op_swap_voice(&character, &voice) {
                Ok(msg) => Json(OpResult { ok: true, message: msg }),
                Err(e) => Json(OpResult { ok: false, message: format!("swap failed: {e:#}") }),
            }
        }
        bm_proto::Op::Eta => {
            let inner = st.lock().await;
            let (start, count) = (req.start.unwrap_or(21), req.count.unwrap_or(80));
            Json(OpResult { ok: true, message: inner.op_eta(start, count) })
        }
    }
}

/// Persist the URL template and probe-crawl one chapter to prove the
/// selector still produces plausible text.
async fn op_crawl_setup(
    _layout: &bm_core::Layout,
    template: &str,
    sample: u32,
) -> OpResult {
    let url = template.replace("{n}", &sample.to_string());
    let text = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .unwrap()
        .get(&url)
        .header("User-Agent", "Mozilla/5.0")
        .send()
        .await
    {
        Ok(r) => match r.text().await {
            Ok(t) => t,
            Err(e) => {
                return OpResult { ok: false, message: format!("probe crawl: read failed: {e:#}") }
            }
        },
        Err(e) => return OpResult { ok: false, message: format!("probe crawl: fetch failed: {e:#}") },
    };
    let cleaned = bm_core::crawl::clean_storya_html(&text);
    let first = cleaned.lines().next().unwrap_or("").to_string();
    let looks_right = cleaned.len() > 200
        && (first.starts_with("Chương ") || first.contains("chương"));
    OpResult {
        ok: looks_right,
        message: format!(
            "probe ch{sample}: {} chars, headline {first:?} — {}",
            cleaned.len(),
            if looks_right { "selector OK" } else { "SELECTOR SUSPECT (short or no headline)" }
        ),
    }
}
/// Read the sidecar roster, enforce the accent policy on the cast file, and
/// refill any gaps. Falls back to the offline roster when no sidecar answers.
/// Distribution to workers rides the next provision sync.
async fn op_voices(layout: &bm_core::Layout, engine: &str) -> OpResult {
    let policy = bm_core::voices::policy_for(engine);
    // Live roster when a sidecar answers, offline fallback otherwise.
    // Enrolled clones have bare labels (voice == label).
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap();
    let mut enrolled: Vec<String> = Vec::new();
    let mut live = false;
    if let Ok(r) = http.get("http://127.0.0.1:8818/voices").send().await {
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
    let mut cast = bm_core::cast::read_cast(&cast_path);
    let before = cast.len();
    // Drop assignments the policy rejects and that no enrolled clone covers.
    cast.retain(|_, v| allowed.contains(v.as_str()) || enrolled.iter().any(|e| e == v));
    let dropped = before - cast.len();
    if dropped > 0 {
        let _ = bm_core::write_json(&cast_path, &cast);
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
        if let Err(e) =
            bm_core::cast::load_cast(sp, &cast_path, &layout.bible(), &policy, true)
        {
            return OpResult {
                ok: false,
                message: format!("cast refill failed on {}: {e:#}", sp.display()),
            };
        }
    }
    let cast = bm_core::cast::read_cast(&cast_path);
    let gaps = cast.len().saturating_sub(filled_from);
    OpResult {
        ok: true,
        message: format!(
            "voices ({}, {} enrolled clones): pruned {dropped}, filled {gaps} gaps, {} speakers mapped",
            if live { "live roster" } else { "offline roster" },
            enrolled.len(),
            cast.len()
        ),
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
