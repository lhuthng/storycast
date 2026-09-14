//! Control API: the only way workers and operators talk to the scheduler.

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{delete, get, post},
    Router,
};
use bm_proto::{
    Complete, Heartbeat, Machine, OpRequest, OpResult, Register, Roster, TaskRequest, VoiceInfo,
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
        bm_proto::Op::PreviewVoice => {
            let (layout, voice) = {
                let inner = st.lock().await;
                (inner.layout.clone(), req.voice.clone().unwrap_or_default())
            };
            Json(op_preview_voice(&layout, &voice).await)
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
    // Strict, unlike the picker: this op *prunes* the cast, and pruning against
    // a silently-defaulted policy would delete assignments the operator meant to
    // keep. Better to refuse than to guess.
    let policy = match bm_core::voices::effective_policy(&layout.roster(), engine) {
        Ok(p) => p,
        Err(e) => {
            return OpResult {
                ok: false,
                message: format!("voices: {e}"),
            }
        }
    };
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
        if let Err(e) =
            bm_core::cast::load_cast(sp, &cast_path, &layout.bible(), &policy, true)
        {
            return OpResult {
                ok: false,
                message: format!("cast refill failed on {}: {e:#}", sp.display()),
            };
        }
    }
    let cast = bm_core::cast::read_cast(engine, &cast_path);
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
        // Settings ride along so the TUI can prefill prompts with the values
        // that are actually in force instead of hardcoded guesses. No secrets
        // live here — those stay in .env.
        "settings": inner.settings,
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
    let http = sidecar_client(Duration::from_secs(10));
    let mut source = "offline".to_string();
    let mut voices: Vec<VoiceInfo> = Vec::new();

    // The effective roster is the shipped catalogue with the operator's own
    // applied. Resolved once so the voice list, the allow-list and the header
    // line cannot disagree about what is assignable. A malformed `.bm/voices.json`
    // falls back to the catalogue but the error rides into `policy_note`, where
    // the operator will see it — a policy that fails open *silently* would
    // re-admit every voice they excluded.
    let (effective, roster_error) =
        bm_core::voices::effective_engine_lenient(&layout.roster(), engine);
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
        voices = effective.to_offline_voices(engine);
    }
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
        let style = format!("pool: {}", entry.tags.join(", "));
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
    voices.sort_by(|a, b| {
        (!a.allowed, &a.gender, &a.name).cmp(&(!b.allowed, &b.gender, &b.name))
    });
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

/// Render a short sample of one voice so it can be auditioned before it is
/// assigned. The file lands in `data/previews/` and the op reports the path, so
/// the inductor never needs to know how a client plays audio.
async fn op_preview_voice(layout: &bm_core::Layout, voice: &str) -> OpResult {
    let voice = voice.trim();
    if voice.is_empty() {
        return OpResult { ok: false, message: "preview needs a voice name".into() };
    }
    let http = match reqwest::Client::builder()
        .timeout(Duration::from_secs(180))
        .no_proxy()
        .build()
    {
        Ok(c) => c,
        Err(e) => return OpResult { ok: false, message: format!("preview {voice}: {e:#}") },
    };
    let resp = match http
        .post(format!("{SIDECAR}/preview"))
        .json(&serde_json::json!({"voice": voice}))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return OpResult {
                ok: false,
                message: format!("preview {voice}: TTS sidecar unreachable at {SIDECAR} ({e})"),
            }
        }
    };
    if !resp.status().is_success() {
        let code = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return OpResult {
            ok: false,
            message: format!(
                "preview {voice}: sidecar {code} — {}",
                bm_core::util::head_chars(body.trim(), 200)
            ),
        };
    }
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            return OpResult { ok: false, message: format!("preview {voice}: read failed ({e})") }
        }
    };
    let dir = layout.data().join("previews");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return OpResult { ok: false, message: format!("preview {voice}: {e}") };
    }
    let dest = dir.join(format!("{}.wav", file_safe(voice)));
    if let Err(e) = std::fs::write(&dest, &bytes) {
        return OpResult { ok: false, message: format!("preview {voice}: {e}") };
    }
    OpResult {
        ok: true,
        message: format!(
            "preview {voice}: {} KB -> {} · play: afplay \"{}\"",
            bytes.len() / 1024,
            dest.display(),
            dest.display()
        ),
    }
}

/// Voice names are Vietnamese and carry diacritics; a separator or control
/// character must never let one escape the preview directory.
fn file_safe(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() {
        "voice".into()
    } else {
        cleaned
    }
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
        .route("/api/roster", get(roster))
        // Merge reports carry base64 mp3s (~7MB); the 2MB default would 413 them.
        .layer(axum::extract::DefaultBodyLimit::disable())
        .with_state(st)
}

// Silence the unused-import warning until M6 operations need TaskRequest.
#[allow(dead_code)]
fn _task_req_is_part_of_the_protocol(_r: TaskRequest) {}
