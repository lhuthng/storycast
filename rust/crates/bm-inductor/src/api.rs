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
    // A registering worker is alive by definition — this is what flips a
    // background-provisioned box Online with no polling involved.
    if let Some(m) = inner.machines.get_mut(&addr) {
        m.state = MachineState::Online;
    }
    inner.workers.insert(r.worker_id.clone(), addr.clone());
    inner
        .caps
        .insert(r.worker_id.clone(), r.capabilities.clone());
    // A worker announcing itself is a bind: its config survives restarts in
    // machines.json, not just in memory. The "unknown"-user placeholder
    // carries no configured values, so it stays memory-only as before.
    if inner.machines.get(&addr).map(|m| m.ssh_user.as_str()) != Some("unknown") {
        inner.persist_box(&addr, &r.hostname);
    }
    // Staged-rollout visibility: an agent without `render-segments` keeps
    // taking every other stage but never sees a render offer. Say so on the
    // machine, or the idle box looks broken.
    if !r.capabilities.iter().any(|c| c == "render-segments") {
        if let Some(m) = inner.machines.get_mut(&addr) {
            m.note = "agent predates render-segments: crawl/digest/merge only".into();
        }
    }
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
            m.state = MachineState::Online;
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

async fn add_machine(State(st): State<Shared>, Json(m): Json<Machine>) -> impl IntoResponse {
    let mut inner = st.lock().await;
    let addr = m.addr.clone();
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
}

async fn set_machine_state(
    State(st): State<Shared>,
    Json(u): Json<MachineStateUpdate>,
) -> impl IntoResponse {
    let mut inner = st.lock().await;
    match inner.machines.get_mut(&u.addr) {
        Some(m) => {
            m.state = u.state;
            if !u.note.is_empty() {
                m.note = u.note;
            }
            inner.save();
            Json(serde_json::json!({"ok": true}))
        }
        None => {
            Json(serde_json::json!({"ok": false, "error": format!("unknown machine {}", u.addr)}))
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
            let mut inner = st.lock().await;
            let (start, count) = (req.start.unwrap_or(1), req.count.unwrap_or(1));
            // Reconcile first: enqueue alone only tops up crawl+digest, so a
            // range whose render/merge tasks went missing (reset ledger, older
            // builds) would digest and then idle with nothing offerable.
            inner.reconcile(start, count);
            let (crawls, digests) = inner.enqueue_translate(start, count);
            Json(OpResult::ok(format!(
                "translate ch{start}..: {crawls} crawls + {digests} digests queued"
            )))
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
            Json(op_crawl_setup(&layout, &template, req.start.unwrap_or(1)).await)
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
            let voice = req.voice.clone().unwrap_or_default();
            Json(op_preview_voice(&voice, req.text.as_deref()).await)
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
            // The blanket retry forgives every shelved task. When a chapter is
            // named, it narrows to that one task instead — which is what the
            // Tasks screen sends, so one bad digest never re-queues the batch.
            match (req.stage, req.chapter) {
                (Some(stage), Some(chapter)) => Json(OpResult::ok(inner.op_retry_task(
                    stage,
                    chapter,
                    req.force.unwrap_or(false),
                ))),
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
        bm_proto::Op::Remix => {
            let mut inner = st.lock().await;
            match inner.op_remix(req.speed, req.effect_volume, req.music_volume) {
                Ok(msg) => Json(OpResult::ok(msg)),
                Err(e) => Json(OpResult::fail(format!("remix failed: {e:#}"))),
            }
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
        return OpResult::ok(format!("reconcile: bible already clean ({n} characters)"));
    }
    if !merges.is_empty() {
        let mut inner = st.lock().await;
        return match inner.apply_reconcile(&merges) {
            Ok(msg) => OpResult::ok(format!(
                "{msg}{}",
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

/// Persist the URL template and probe-crawl one chapter to prove the
/// selector still produces plausible text.
async fn op_crawl_setup(_layout: &bm_core::Layout, template: &str, sample: u32) -> OpResult {
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
            Err(e) => return OpResult::fail(format!("probe crawl: read failed: {e:#}")),
        },
        Err(e) => return OpResult::fail(format!("probe crawl: fetch failed: {e:#}")),
    };
    let cleaned = bm_core::crawl::clean_storya_html(&text);
    let first = cleaned.lines().next().unwrap_or("").to_string();
    let looks_right =
        cleaned.len() > 200 && (first.starts_with("Chương ") || first.contains("chương"));
    // A probe that reads but looks wrong is a *failure* with a diagnosis, not a
    // success with a caveat: `ok` drives the colour, so it must say `false`.
    let message = format!(
        "probe ch{sample}: {} chars, headline {first:?} — {}",
        cleaned.len(),
        if looks_right {
            "selector OK"
        } else {
            "SELECTOR SUSPECT (short or no headline)"
        }
    );
    if looks_right {
        OpResult::ok(message)
    } else {
        OpResult::fail(message)
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
        Err(e) => return OpResult::fail(format!("voices: {e}")),
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

/// Roster with no scheduler: a throwaway Inner over the files on disk. The
/// TUI uses this when the inductor is down (X stops it) so picking voices
/// never needs the control plane. Sidecar-dependent parts degrade exactly as
/// they do for a live inductor with a dead sidecar.
pub(crate) async fn offline_roster(layout_root: &std::path::Path) -> Roster {
    let layout = bm_core::Layout::new(layout_root);
    let settings = bm_core::config::Settings::load(&layout.settings());
    let engine = settings.engine.clone();
    let mut inner = Inner::new(layout.clone(), settings);
    inner.load_ledger();
    let characters = inner.known_characters();
    let cast = inner.cast_snapshot();
    build_roster(&layout, &engine, characters, cast).await
}

/// Swap with no scheduler: the same `op_swap_voice` against a throwaway
/// Inner, which persists cast + ledger itself. Two locks before touching
/// anything: the inductor API must be down (its scheduler owns these files
/// while it answers), and no local worker may be alive (a mid-render worker
/// keeps rendering the old cast). Remote strays are the operator's
/// responsibility — the supported flow is X (which sweeps them), then swap.
pub(crate) async fn offline_swap(
    api: &str,
    layout_root: &std::path::Path,
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
    offline_swap_apply(layout_root, character, voice)
}

/// The file mutation itself, minus the guards: throwaway Inner over disk
/// files, same `op_swap_voice` the live path runs (which persists cast +
/// ledger itself). Split out so tests can run it without a scheduler, a
/// network, or a worker-shaped hole in the room.
fn offline_swap_apply(
    layout_root: &std::path::Path,
    character: &str,
    voice: &str,
) -> Result<String, String> {
    let layout = bm_core::Layout::new(layout_root);
    let settings = bm_core::config::Settings::load(&layout.settings());
    let mut inner = Inner::new(layout, settings);
    inner.load_ledger();
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
    layout_root: &std::path::Path,
    speed: Option<f64>,
    effect_volume: Option<f64>,
    music_volume: Option<f64>,
) -> Result<String, String> {
    if super::backend::inductor_up(api).await {
        return Err(
            "inductor is back — remix normally (this path is for inductor-down only)".into(),
        );
    }
    if super::backend::local_workers_alive() {
        return Err("local workers still running — X first, then remix".into());
    }
    offline_remix_apply(layout_root, speed, effect_volume, music_volume)
}

fn offline_remix_apply(
    layout_root: &std::path::Path,
    speed: Option<f64>,
    effect_volume: Option<f64>,
    music_volume: Option<f64>,
) -> Result<String, String> {
    let layout = bm_core::Layout::new(layout_root);
    let settings = bm_core::config::Settings::load(&layout.settings());
    let mut inner = Inner::new(layout, settings);
    inner.load_ledger();
    inner
        .op_remix(speed, effect_volume, music_volume)
        .map(|m| format!("{m} [offline — inductor was down]"))
        .map_err(|e| e.to_string())
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
async fn op_preview_voice(voice: &str, text: Option<&str>) -> OpResult {
    let voice = voice.trim();
    if voice.is_empty() {
        return OpResult::fail("preview needs a voice name");
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
    // one. Without it, Tab triages on a random segment.
    let exact = text.map(str::trim).filter(|t| !t.is_empty());
    if let Some(want) = exact {
        match bm_core::assemble::pick_exact(&cands, character, want) {
            Some(pick) => return serve_segment(pick),
            None => {
                return OpResult::fail(bm_core::assemble::segment_miss(
                    layout, character, voice, true,
                ))
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
        .route("/api/segment", post(put_segment))
        .route("/api/machines", post(add_machine))
        .route("/api/machines", delete(drop_machine))
        .route("/api/machines/state", post(set_machine_state))
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
            }),
        )
        .await;
        {
            let inner = st.lock().await;
            assert!(!inner.machines.contains_key("10.9.9.9"));
        }
    }

    #[tokio::test]
    async fn offline_roster_reads_cast_and_speakers_from_disk() {
        let d = scratch();
        let layout = bm_core::Layout::new(d.path());
        std::fs::write(layout.cast("vieneu"), r#"{"A":"Đức Trí"}"#).unwrap();
        std::fs::write(
            layout.script(1),
            r#"{"roster":["A"],"segments":[{"speaker":"A","text":"x"}]}"#,
        )
        .unwrap();
        let r = offline_roster(d.path()).await;
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

        let msg = offline_swap_apply(d.path(), "A", "Minh Triết").expect("offline swap");
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
        bm_core::write_json(
            &layout.bm_state().join("ledger.json"),
            &serde_json::json!({"tasks": [
                {"chapter": 1, "stage": "merge", "state": "done",
                 "attempts": 0, "assigned_to": null, "lease_until": null,
                 "detail": "", "updated": 0},
            ]}),
        )
        .unwrap();

        let msg = offline_remix_apply(d.path(), Some(1.5), Some(0.5), Some(0.0))
            .expect("offline remix");
        assert!(msg.contains("1.5"), "{msg}");
        assert!(msg.contains("offline"), "{msg}");
        assert!(!layout.final_mp3(1).exists(), "stale mp3 must go");
        let settings = bm_core::config::Settings::load(&layout.settings());
        assert_eq!(
            (settings.speed, settings.effect_volume, settings.music_volume),
            (1.5, 0.5, 0.0)
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
}
