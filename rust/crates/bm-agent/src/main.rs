//! Worker agent: pull one pipeline stage at a time, run it, report back.
//!
//! Two modes:
//! - `run`    — a single stage for one chapter, standalone (no inductor).
//! - `worker` — register with the inductor and pull tasks until stopped.
//!
//! The agent never loads a model and never writes the authoritative bible.
//! TTS goes through the Python sidecar over HTTP; the agent owns the sidecar's
//! lifecycle (started per render task, stopped after) so sidecar RSS stays
//! bounded no matter how many chapters flow through.

mod tts;

use anyhow::{Context, Result};
use bm_core::{config::Settings, Layout};
use bm_proto::{Complete, Heartbeat, Register, TaskOffer};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tts::Tts;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const SIDECAR_PORT: u16 = 8818;

#[derive(Parser)]
#[command(name = "bm-agent", version, about = "Pipeline worker: run stages, report progress")]
struct Cli {
    /// Repo root (discovered via prompts/analyze.txt when omitted).
    #[arg(long)]
    root: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run one stage for one chapter, standalone.
    Run {
        #[arg(long)]
        stage: String,
        #[arg(long)]
        chapter: u32,
        #[arg(long)]
        url: Option<String>,
        #[arg(long)]
        tts_url: Option<String>,
        #[arg(long, default_value = "vieneu")]
        engine: String,
    },
    /// Register with an inductor and pull tasks until stopped.
    Worker {
        #[arg(long)]
        inductor: String,
        #[arg(long)]
        worker_id: Option<String>,
        #[arg(long)]
        addr: Option<String>,
        #[arg(long)]
        tts_url: Option<String>,
    },
}

/// What the worker is doing right now — the heartbeat source of truth.
#[derive(Debug, Clone, Default)]
struct Progress {
    task_id: Option<String>,
    stage: Option<String>,
    chapter: Option<u32>,
    frac: f32,
    activity: String,
}

type Shared = Arc<Mutex<Progress>>;

fn set_progress(shared: &Shared, frac: f32, activity: String) {
    if let Ok(mut p) = shared.lock() {
        p.frac = frac.clamp(0.0, 1.0);
        p.activity = activity;
    }
}

// ---------------------------------------------------------------------------
// TTS sidecar lifecycle: started per render task, stopped after.
// ---------------------------------------------------------------------------

struct Sidecar {
    tts_url: String,
    child: Option<tokio::process::Child>,
}

impl Sidecar {
    fn new(tts_url: &str) -> Self {
        Sidecar {
            tts_url: tts_url.trim_end_matches('/').to_string(),
            child: None,
        }
    }

    fn tts(&self) -> Tts {
        Tts::new(&self.tts_url)
    }

    fn port(&self) -> u16 {
        self.tts_url
            .rsplit(':')
            .next()
            .and_then(|p| p.trim_end_matches('/').parse().ok())
            .unwrap_or(SIDECAR_PORT)
    }

    /// Ensure the sidecar answers, starting it if needed. Idempotent.
    /// Verifies `/policy`, not just `/health`: a stale server from a previous
    /// deploy answers health but lacks the endpoints renders depend on.
    async fn ensure(&mut self, layout: &Layout) -> Result<()> {
        if self.serving_current().await {
            return Ok(());
        }
        self.stop();
        // The venv lives at the repo root; the TTS modules live in python/,
        // so the server runs with cwd=python/ (`import tts_vieneu` resolves).
        let py = layout.root.join(".venv/bin/python");
        let py = if py.is_file() { py } else { PathBuf::from("python3") };
        let mut child = tokio::process::Command::new(&py)
            .arg("tts_server.py")
            .arg("--port")
            .arg(self.port().to_string())
            .current_dir(layout.python_dir())
            .env("PYTHONPATH", layout.python_dir())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .with_context(|| format!("spawning TTS sidecar via {}", py.display()))?;
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_secs(5)).await;
            if self.serving_current().await {
                self.child = Some(child);
                return Ok(());
            }
            if child.try_wait()?.is_some() {
                anyhow::bail!("TTS sidecar exited during startup");
            }
        }
        let _ = child.kill().await;
        anyhow::bail!("TTS sidecar never answered /health")
    }

    /// Health plus capability: the server must serve the policy endpoint
    /// this agent was built against.
    async fn serving_current(&self) -> bool {
        if !self.tts().health().await {
            return false;
        }
        match self.tts().policy().await {
            Ok(p) => p.get("allowed_voices").and_then(|v| v.as_array()).is_some(),
            Err(_) => false,
        }
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
        }
    }
}

// ---------------------------------------------------------------------------
// stages (blocking work runs in spawn_blocking; heartbeats stay live)
// ---------------------------------------------------------------------------

async fn run_crawl(layout: &Layout, n: u32, url: &str, shared: &Shared) -> Result<(u64, String)> {
    set_progress(shared, 0.05, format!("fetch ch{n}"));
    let text = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?
        .get(url)
        .header("User-Agent", "Mozilla/5.0")
        .send()
        .await
        .with_context(|| format!("fetching {url}"))?
        .text()
        .await?;
    let cleaned = bm_core::crawl::clean_storya_html(&text);
    if cleaned.len() < 200 {
        anyhow::bail!("ingested text suspiciously short ({} chars)", cleaned.len());
    }
    let dest = layout.chapter_txt(n);
    bm_core::atomic_write(&dest, &cleaned)?;
    set_progress(shared, 1.0, format!("crawled ch{n} ({} chars)", cleaned.len()));
    Ok((1, cleaned))
}

async fn run_digest(
    layout: &Layout,
    n: u32,
    bible: &Value,
    settings: &Settings,
    analyzer: &str,
    shared: &Shared,
    merge_local: bool,
) -> Result<(Value, Value)> {
    let layout = layout.clone();
    let layout2 = layout.clone();
    let bible = bible.clone();
    let settings = settings.clone();
    let analyzer = analyzer.to_string();
    let shared2 = shared.clone();
    let (outcome, warnings) = tokio::task::spawn_blocking(move || -> Result<_> {
        let rt = tokio::runtime::Handle::current();
        let mut last = (0.0, String::new());
        let mut cb = |f: f32, s: String| {
            last = (f, s.clone());
            set_progress(&shared2, f, s);
        };
        let outcome = rt.block_on(bm_core::digest::digest_chapter(
            &layout2, n, &bible, &settings, &analyzer, &mut cb,
        ))?;
        Ok((outcome, last))
    })
    .await??;
    let _ = warnings;
    for line in &outcome.log {
        println!("{line}");
    }
    if merge_local {
        // Standalone mode keeps legacy behaviour: merge here. Worker mode
        // returns the delta and the inductor merges as the single writer.
        let mut local: Value =
            serde_json::from_str(&std::fs::read_to_string(layout.bible())?).unwrap_or(json!({}));
        bm_core::digest::merge_bible(&mut local, &outcome.delta, &format!("{n:02}"));
        bm_core::digest::save_bible(&local, &layout.bible())?;
    }
    set_progress(shared, 1.0, format!("digest ch{n} done ({} segments)", outcome.segments));
    let script: Value = serde_json::from_str(&std::fs::read_to_string(layout.script(n))?)?;
    Ok((outcome.delta, script))
}

async fn run_render(
    layout: &Layout,
    n: u32,
    engine: &str,
    tts: &Tts,
    shared: &Shared,
) -> Result<u64> {
    let script_path = layout.script(n);
    let cast_path = layout.cast(engine);
    let seg_dir = layout.seg_dir(engine, n);
    let text = std::fs::read_to_string(&script_path)?;
    let data: Value = serde_json::from_str(&text)?;
    let segments = data.get("segments").and_then(|s| s.as_array()).cloned().unwrap_or_default();
    let policy = bm_core::cast::policy_for_bible(engine, &layout.bible())?;
    let cast = bm_core::cast::load_cast(&script_path, &cast_path, &layout.bible(), &policy, true)?;
    let local = engine == "vieneu";
    let planned = bm_core::assemble::drop_headline(&segments);
    let first = planned.first().map(|s| s.get("text").and_then(|t| t.as_str()).unwrap_or("")).unwrap_or("");
    let title = bm_core::assemble::title_speech(layout, n, &cast, first);
    let units = bm_core::assemble::plan_render(planned, &cast, &seg_dir, local, title.as_ref())?;
    let todo: Vec<_> = units
        .into_iter()
        .filter(|u| {
            !(u.dest.exists()
                && u.dest.metadata().map(|m| m.len() > 1000).unwrap_or(false))
        })
        .collect();
    let total = todo.len();
    std::fs::create_dir_all(&seg_dir)?;
    // No accent gate: any voice the sidecar can synthesize is allowed. If the
    // engine itself rejects a voice, that failure surfaces from /infer.
    let manifest = layout.output().join("render-manifest.jsonl");
    for (i, u) in todo.iter().enumerate() {
        set_progress(
            shared,
            i as f32 / total.max(1) as f32,
            format!("render ch{n} {} ({}/{})", u.tag, i + 1, total),
        );
        let wav = tts.infer(&u.text, &u.voice, u.temperature, u.silence_p, engine).await?;
        std::fs::write(&u.dest, &wav)?;
        bm_core::assemble::manifest_append(
            &manifest,
            &json!({"chapter": n, "tag": u.tag, "voice": u.voice,
                    "temp": u.temperature, "engine": engine}),
        )?;
    }
    set_progress(shared, 1.0, format!("render ch{n} done ({total} new calls)"));
    Ok(total as u64)
}

async fn run_merge(
    layout: &Layout,
    n: u32,
    engine: &str,
    gap_ms: u32,
    speed: f64,
    ambience: bool,
    shared: &Shared,
) -> Result<String> {
    set_progress(shared, 0.1, format!("merge ch{n}"));
    // Everything the merge writes goes into one per-chapter scratch directory
    // under `.bm/`, never into `output/`. `assemble` hands back the mp3 (or a
    // wav when ffmpeg is missing) and `publish` renames it out to `output/`,
    // after which the whole scratch directory can go.
    let scratch = layout.scratch_ch(n);
    let params = (
        layout.clone(),
        n,
        engine.to_string(),
        gap_ms,
        speed,
        ambience,
        scratch.clone(),
    );
    let assembled = tokio::task::spawn_blocking(move || {
        let (layout, n, engine, gap_ms, speed, ambience, scratch) = params;
        bm_core::assemble::assemble(
            &layout.script(n),
            &layout.cast(&engine),
            &layout.bible(),
            &layout.seg_dir(&engine, n),
            &scratch,
            gap_ms,
            ambience,
            speed,
            &engine,
            &layout.assets(),
        )
    })
    .await??;
    let final_path = bm_core::assemble::publish(&assembled, layout, n)?;
    // The product is out of scratch now, so the directory goes. Deliberately
    // left behind when anything above failed: a failed merge's scratch is the
    // only evidence it leaves, and `bm-inductor gc` sweeps stale ones.
    let _ = std::fs::remove_dir_all(&scratch);
    set_progress(shared, 1.0, format!("merge ch{n} done"));
    Ok(final_path.display().to_string())
}

// ---------------------------------------------------------------------------
// worker loop against the inductor API
// ---------------------------------------------------------------------------

async fn heartbeat_loop(
    http: reqwest::Client,
    inductor: String,
    worker_id: String,
    addr: String,
    hostname: String,
    shared: Shared,
) {
    let url = format!("{inductor}/api/heartbeat");
    loop {
        let p = shared.lock().map(|p| p.clone()).unwrap_or_default();
        let body = Heartbeat {
            worker_id: worker_id.clone(),
            addr: addr.clone(),
            task_id: p.task_id.clone(),
            stage: p.stage.as_deref().and_then(bm_proto::Stage::parse),
            chapter: p.chapter,
            progress: p.frac,
            activity: p.activity.clone(),
            eta_secs: None,
            ts: bm_proto::now_secs(),
            hostname: hostname.clone(),
        };
        let _ = http.post(&url).json(&body).send().await;
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn set_task(shared: &Shared, offer: &TaskOffer) {
    if let Ok(mut p) = shared.lock() {
        p.task_id = Some(offer.task_id.clone());
        p.stage = Some(offer.stage.as_str().to_string());
        p.chapter = Some(offer.chapter);
        p.frac = 0.0;
        p.activity = format!("{} ch{}", offer.stage, offer.chapter);
    }
}

fn clear_task(shared: &Shared) {
    if let Ok(mut p) = shared.lock() {
        p.task_id = None;
        p.stage = None;
        p.chapter = None;
        p.frac = 0.0;
        p.activity = "idle".to_string();
    }
}

async fn run_offer(
    layout: &Layout,
    settings: &Settings,
    offer: &TaskOffer,
    shared: &Shared,
    sidecar: &mut Sidecar,
) -> Result<TaskResult> {
    use bm_proto::Stage::*;
    let n = offer.chapter;
    // The offer is authoritative: materialize its artifacts first so any
    // machine can run any stage without shared storage.
    if let Some(text) = &offer.text {
        bm_core::atomic_write(&layout.chapter_txt(n), text)?;
    }
    if let Some(script) = &offer.script {
        bm_core::atomic_write(&layout.script(n), &serde_json::to_string_pretty(script)?)?;
    }
    match offer.stage {
        Crawl => {
            let url = offer.url.clone().unwrap_or_else(|| settings.chapter_url(n));
            let (units, text) = run_crawl(layout, n, &url, shared).await?;
            Ok(TaskResult { ok: true, detail: format!("crawled ch{n}"), delta: None, units, script: None, text: Some(text), mp3_b64: None })
        }
        Digest => {
            let bible = offer.bible.clone().unwrap_or(json!({"characters": []}));
            // The inductor's pick wins; an empty offer (old inductor) falls
            // back to this worker's own settings, then to opencode.
            let analyzer = if offer.analyzer.is_empty() {
                let a = settings.analyzer.clone();
                if a.is_empty() { "opencode".into() } else { a }
            } else {
                offer.analyzer.clone()
            };
            let (delta, script) = run_digest(layout, n, &bible, settings, &analyzer, shared, false).await?;
            Ok(TaskResult { ok: true, detail: format!("digest ch{n} via {analyzer}"), delta: Some(delta), units: 1, script: Some(script), text: None, mp3_b64: None })
        }
        Render => {
            sidecar.ensure(layout).await?;
            let units = run_render(layout, n, &offer.engine, &sidecar.tts(), shared).await?;
            sidecar.stop(); // per-task lifecycle: RSS returns to the OS here
            Ok(TaskResult { ok: true, detail: format!("render ch{n} ({units} calls)"), delta: None, units, script: None, text: None, mp3_b64: None })
        }
        Merge => {
            let path = run_merge(layout, n, &offer.engine, offer.gap_ms, offer.speed, offer.ambience, shared).await?;
            let mp3 = std::fs::read(&path).ok().map(|b| base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &b));
            Ok(TaskResult { ok: true, detail: format!("merge ch{n} -> {path}"), delta: None, units: 1, script: None, text: None, mp3_b64: mp3 })
        }
    }
}

#[derive(Debug)]
struct TaskResult {
    ok: bool,
    detail: String,
    delta: Option<Value>,
    units: u64,
    script: Option<Value>,
    text: Option<String>,
    mp3_b64: Option<String>,
}

async fn worker_loop(
    layout: Layout,
    settings: Settings,
    inductor: String,
    worker_id: String,
    addr: String,
    tts_url: String,
) -> Result<()> {
    let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).build()?;
    let hostname = hostname_simple();
    let shared: Shared = Arc::new(Mutex::new(Progress {
        activity: "starting".to_string(),
        ..Default::default()
    }));
    tokio::spawn(heartbeat_loop(
        http.clone(),
        inductor.clone(),
        worker_id.clone(),
        addr.clone(),
        hostname.clone(),
        shared.clone(),
    ));
    let reg = Register {
        worker_id: worker_id.clone(),
        addr: addr.clone(),
        hostname,
        capabilities: vec!["crawl".into(), "digest".into(), "render".into(), "merge".into()],
        tts_url: Some(tts_url.clone()),
        version: VERSION.into(),
    };
    // The inductor may not be up yet (or the network may flap): retry
    // registration forever instead of dying on the first failure.
    loop {
        match http.post(format!("{inductor}/api/register")).json(&reg).send().await {
            Ok(r) if r.status().is_success() => break,
            Ok(r) => {
                set_progress(&shared, 0.0, format!("register refused: {}", r.status()));
            }
            Err(e) => {
                set_progress(&shared, 0.0, format!("inductor unreachable, retrying: {e}"));
            }
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
    println!("registered as {worker_id}, pulling tasks");
    let mut sidecar = Sidecar::new(&tts_url);
    loop {
        let offer: Option<TaskOffer> = match http
            .get(format!("{inductor}/api/task"))
            .query(&[("worker_id", &worker_id)])
            .send()
            .await
        {
            Ok(r) if r.status() == reqwest::StatusCode::NO_CONTENT => None,
            Ok(r) => Some(r.json().await.context("parsing task offer")?),
            Err(e) => {
                set_progress(&shared, 0.0, format!("inductor unreachable: {e}"));
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
        };
        let Some(offer) = offer else {
            set_progress(&shared, 0.0, "idle".to_string());
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        };
        set_task(&shared, &offer);
        let t0 = Instant::now();
        let res = match run_offer(&layout, &settings, &offer, &shared, &mut sidecar).await {
            Ok(r) => r,
            Err(e) => TaskResult {
                ok: false,
                detail: format!("{} ch{} failed: {e:#}", offer.stage, offer.chapter),
                delta: None, units: 0, script: None, text: None, mp3_b64: None,
            },
        };
        println!("[{}] {}", if res.ok { "ok" } else { "FAIL" }, res.detail);
        // Reports must land: a lost merge report strands a finished mp3 on
        // this machine until the lease expires. Retry, then move on.
        let report = Complete {
            worker_id: worker_id.clone(),
            task_id: offer.task_id.clone(),
            ok: res.ok,
            detail: res.detail,
            duration_secs: t0.elapsed().as_secs_f64(),
            bible_delta: res.delta,
            units: res.units,
            script: res.script,
            text: res.text,
            mp3_b64: res.mp3_b64,
        };
        for attempt in 1..=3 {
            match http
                .post(format!("{inductor}/api/complete"))
                .json(&report)
                .send()
                .await
            {
                Ok(r) if r.status().is_success() => break,
                Ok(r) => println!("[WARN] complete report refused ({}), retry {attempt}/3", r.status()),
                Err(e) => println!("[WARN] complete report lost, retry {attempt}/3: {e}"),
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
        clear_task(&shared);
    }
}

fn hostname_simple() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "localhost".to_string())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let layout = match cli.root {
        Some(r) => Layout::new(r),
        None => Layout::discover()?,
    };
    bm_core::config::load_dotenv(&layout.root.join(".env"));
    let settings = Settings::load(&layout.settings());
    match cli.cmd {
        Cmd::Run { stage, chapter, url, tts_url, engine } => {
            let shared: Shared = Arc::new(Mutex::new(Progress::default()));
            let engine = if engine.is_empty() { settings.engine.clone() } else { engine };
            match stage.as_str() {
                "crawl" => {
                    let url = url.unwrap_or_else(|| settings.chapter_url(chapter));
                    run_crawl(&layout, chapter, &url, &shared).await?;
                }
                "digest" => {
                    let bible: Value = serde_json::from_str(
                        &std::fs::read_to_string(layout.bible()).unwrap_or_else(|_| "{\"characters\":[]}".into()),
                    )?;
                    let analyzer = if settings.analyzer.is_empty() { "opencode".into() } else { settings.analyzer.clone() };
                    run_digest(&layout, chapter, &bible, &settings, &analyzer, &shared, true).await?;
                }
                "render" => {
                    let tts_url = tts_url.unwrap_or_else(|| "http://127.0.0.1:8818".into());
                    let mut sidecar = Sidecar::new(&tts_url);
                    sidecar.ensure(&layout).await?;
                    run_render(&layout, chapter, &engine, &sidecar.tts(), &shared).await?;
                    sidecar.stop();
                }
                "merge" => {
                    run_merge(&layout, chapter, &engine, settings.gap_ms, settings.speed, settings.ambience, &shared).await?;
                }
                other => anyhow::bail!("unknown stage {other:?} (crawl|digest|render|merge)"),
            }
        }
        Cmd::Worker { inductor, worker_id, addr, tts_url } => {
            let worker_id = worker_id.unwrap_or_else(|| format!("{}-{}", hostname_simple(), std::process::id()));
            let addr = addr.unwrap_or_else(|| "127.0.0.1".into());
            let tts_url = tts_url.unwrap_or_else(|| "http://127.0.0.1:8818".into());
            worker_loop(layout, settings, inductor, worker_id, addr, tts_url).await?;
        }
    }
    Ok(())
}
