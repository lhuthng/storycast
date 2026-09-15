//! Control API: the only way workers and operators talk to the scheduler.

use axum::{
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
        None => Json(serde_json::json!({"ok": false, "error": format!("unknown machine {}", u.addr)})),
    }
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
            let (start, count) = (req.start.unwrap_or(1), req.count.unwrap_or(1));
            // Reconcile first: enqueue alone only tops up crawl+digest, so a
            // range whose render/merge tasks went missing (reset ledger, older
            // builds) would digest and then idle with nothing offerable.
            inner.reconcile(start, count);
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
            let (start, count) = (req.start.unwrap_or(1), req.count.unwrap_or(1));
            Json(OpResult { ok: true, message: inner.op_eta(start, count) })
        }
        bm_proto::Op::Requeue => {
            let mut inner = st.lock().await;
            Json(OpResult { ok: true, message: inner.op_requeue_orphans() })
        }
        bm_proto::Op::Retry => {
            let mut inner = st.lock().await;
            // The blanket retry forgives every shelved task. When a chapter is
            // named, it narrows to that one task instead — which is what the
            // Tasks screen sends, so one bad digest never re-queues the batch.
            match (req.stage, req.chapter) {
                (Some(stage), Some(chapter)) => Json(OpResult {
                    ok: true,
                    message: inner.op_retry_task(stage, chapter, req.force.unwrap_or(false)),
                }),
                _ => Json(OpResult { ok: true, message: inner.op_retry_shelved() }),
            }
        }
        bm_proto::Op::RetryTask => {
            let (stage, chapter, force) = (req.stage, req.chapter, req.force.unwrap_or(false));
            match (stage, chapter) {
                (Some(stage), Some(chapter)) => {
                    let mut inner = st.lock().await;
                    Json(OpResult { ok: true, message: inner.op_retry_task(stage, chapter, force) })
                }
                _ => Json(OpResult {
                    ok: false,
                    message: "retry-task requires stage and chapter".into(),
                }),
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
            let fresh: Vec<String> =
                absorbs.into_iter().filter(|a| seen.insert(a.clone())).collect();
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
        return OpResult { ok: true, message: format!("reconcile: bible already clean ({n} characters)") };
    }
    if !merges.is_empty() {
        let mut inner = st.lock().await;
        return match inner.apply_reconcile(&merges) {
            Ok(msg) => OpResult {
                ok: true,
                message: format!(
                    "{msg}{}",
                    if plan.candidates.is_empty() {
                        String::new()
                    } else {
                        format!(
                            "; {} ambiguous pairs remain — press m again",
                            plan.candidates.len()
                        )
                    }
                ),
            },
            Err(e) => OpResult { ok: false, message: format!("reconcile refused: {e:#}") },
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
    OpResult {
        ok: true,
        message: format!(
            "reconcile: nothing certain to fold; ambiguous pairs (no auto-merge): {}",
            pairs.join("; ")
        ),
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
    let events: Vec<_> = inner.recent_events(100);
    Json(serde_json::json!({
        "tasks": inner.tasks.values().collect::<Vec<_>>(),
        "machines": inner.machines.values().collect::<Vec<_>>(),
        "beats": inner.beats.values().collect::<Vec<_>>(),
        "counts": inner.counts(),
        // Settings ride along so the TUI can prefill prompts with the values
        // that are actually in force instead of hardcoded guesses. No secrets
        // live here — those stay in .env.
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
        return Err("inductor is back — swap normally (this path is for inductor-down only)".into());
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
            Json(MachineStateUpdate { addr: "10.9.9.9".into(), state: MachineState::Error, note: String::new() }),
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
        assert!(r.characters.contains(&"A".to_string()), "{:?}", r.characters);
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
        assert!(!seg.join("0000_Đức Trí.wav").exists(), "stale run file must go");
        assert!(seg.join("0001_Adam.wav").exists(), "other voices keep cache");
        let cast = bm_core::cast::read_cast("vieneu", &layout.cast("vieneu"));
        assert_eq!(cast["A"], "Minh Triết");
    }
}
