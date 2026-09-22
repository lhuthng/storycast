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

mod push;
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
#[command(
    name = "bm-agent",
    version,
    about = "Pipeline worker: run stages, report progress"
)]
struct Cli {
    /// Repo root (discovered when omitted: `rust/Cargo.toml` in a checkout,
    /// `.bm/profile` on a provisioned worker). Global, so a launcher can hand
    /// the root to the subcommand it spawns rather than let it guess from cwd.
    #[arg(long, global = true)]
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
    /// Run as a worker.
    ///
    /// With `--inductor` this dials that inductor and pulls tasks, exactly as
    /// it always has. **Without it the worker is serve-only**: it holds no
    /// inductor address at all, so it cannot dial one — the inductor does all
    /// the asking and drives this box over `--serve-tasks`. The absence of an
    /// address is the guarantee, not a flag saying "do not dial".
    Worker {
        #[arg(long)]
        inductor: Option<String>,
        #[arg(long)]
        worker_id: Option<String>,
        #[arg(long)]
        addr: Option<String>,
        #[arg(long)]
        tts_url: Option<String>,
        /// Answer the inverted protocol on this port: `GET /status`,
        /// `POST /task`, `GET /unit`, `POST /shutdown`. Needs a cluster token
        /// in `.bm/` — see `bm_core::token`. Required in serve-only mode,
        /// optional alongside `--inductor`.
        #[arg(long)]
        serve_tasks: Option<u16>,
    },
    /// List local audio segments as JSON: the inventory half of the
    /// inductor-owned-segments migration. No scheduler involved.
    Segments {
        /// Emit the machine-readable manifest (the only output format).
        #[arg(long, default_value_t = true)]
        json: bool,
    },
}

/// One entry of the segment inventory: which chapter, which engine, which
/// file, how big, and what it hashes to. Shape lives in `bm_core::segments`
/// so the inductor's diff can never disagree about it; the walk is one call.
fn segment_manifest(audio_dir: &std::path::Path) -> Vec<bm_core::segments::SegmentEntry> {
    bm_core::segments::manifest(audio_dir)
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
// Spawn paths live on `Layout` (`sidecar_binary`, `sidecar_command`) so the
// inductor's preview path resolves the same tree without a second copy to
// drift.

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
        let (bin, args) = layout.sidecar_command(self.port());
        if !bin.is_file() {
            anyhow::bail!(
                "no TTS sidecar at {} — build it (`make build`) or provision this box (`make provision BOX=…`)",
                bin.display()
            );
        }
        let mut child = tokio::process::Command::new(&bin)
            .args(&args)
            // The SONAME is `libonnxruntime.so.1`, and it sits beside the binary
            // at the worker root. Without this the spawn dies with "error while
            // loading shared libraries", which reads as a missing model.
            .env("LD_LIBRARY_PATH", layout.tts_lib_dir())
            .stdout(std::process::Stdio::null())
            // Piped, not nulled: when the child dies mid-startup its last
            // words name the cause (a missing lib says so plainly), and the
            // failure below quotes them instead of reading identically every
            // time.
            .stderr(std::process::Stdio::piped())
            .spawn()
            .with_context(|| format!("spawning TTS sidecar {}", bin.display()))?;
        let mut stderr = child.stderr.take();
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_secs(5)).await;
            if self.serving_current().await {
                self.child = Some(child);
                return Ok(());
            }
            if child.try_wait()?.is_some() {
                anyhow::bail!(
                    "TTS sidecar exited during startup{}",
                    child_stderr_tail(&mut stderr).await
                );
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

/// What a dead sidecar said as it died, or nothing when it said nothing.
/// Tail, not head: the loader error lands last.
async fn child_stderr_tail(stderr: &mut Option<tokio::process::ChildStderr>) -> String {
    let Some(s) = stderr else {
        return String::new();
    };
    let mut buf = String::new();
    let _ = tokio::io::AsyncReadExt::read_to_string(s, &mut buf).await;
    let t = buf.trim();
    if t.is_empty() {
        return String::new();
    }
    let tail: String = t
        .chars()
        .rev()
        .take(300)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!(": {tail}")
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
    set_progress(
        shared,
        1.0,
        format!("crawled ch{n} ({} chars)", cleaned.len()),
    );
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
    set_progress(
        shared,
        1.0,
        format!("digest ch{n} done ({} segments)", outcome.segments),
    );
    let script: Value = serde_json::from_str(&std::fs::read_to_string(layout.script(n))?)?;
    Ok((outcome.delta, script))
}

/// What a render offer asks for. Pure, so the zero-units arm — "do nothing
/// and report success", the easiest arm to write as a fall-through — is
/// pinned by a test instead of by inspection.
///
/// The offered list is the chapter's **whole** unit set, not the difference
/// against the inductor's store: the inductor cannot see this box's disk. What
/// this box still has to speak is therefore decided against the disk, in
/// `pending_units`, and not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RenderAction {
    /// Old inductor (no `render_units`): plan from the local script.
    Legacy,
    /// The offer names no units at all — an empty chapter. Report `ok` with
    /// `units: 0` at once.
    Noop,
    /// Consider these units; speak the ones this box does not already hold.
    Units,
}

fn render_action(render_units: Option<&[bm_proto::RenderUnitSpec]>) -> RenderAction {
    match render_units {
        None => RenderAction::Legacy,
        Some([]) => RenderAction::Noop,
        Some(_) => RenderAction::Units,
    }
}

/// The offered units whose file this box does not already hold, plus every
/// unit the inductor flagged as forced.
///
/// Forced names are the ones the inductor's own store lacks — the surgical
/// set a swap or retag just deleted. They render even when this disk holds a
/// same-named file, or a warm box keeps serving stale bytes under the new
/// text. Everything else skips on presence, as before.
fn pending_units<'a>(
    offered: &'a [bm_proto::RenderUnitSpec],
    force: &[String],
    seg_dir: &std::path::Path,
) -> Vec<&'a bm_proto::RenderUnitSpec> {
    offered
        .iter()
        .filter(|u| {
            force.iter().any(|f| f == &u.name)
                || !seg_dir
                    .join(&u.name)
                    .metadata()
                    .map(|m| m.len() > 1000)
                    .unwrap_or(false)
        })
        .collect()
}

async fn render_offered_units(
    layout: &Layout,
    n: u32,
    engine: &str,
    units: &[&bm_proto::RenderUnitSpec],
    tts: &Tts,
    shared: &Shared,
) -> Result<u64> {
    let total = units.len();
    let seg_dir = layout.seg_dir(engine, n);
    std::fs::create_dir_all(&seg_dir)?;
    for (i, u) in units.iter().enumerate() {
        set_progress(
            shared,
            i as f32 / total.max(1) as f32,
            format!("render ch{n} {} ({}/{})", u.tag, i + 1, total),
        );
        let wav = tts
            .infer(&u.text, &u.voice, u.temperature, u.silence_p, engine)
            .await?;
        std::fs::write(seg_dir.join(&u.name), &wav)?;
    }
    set_progress(
        shared,
        1.0,
        format!("render ch{n} done ({total} new calls)"),
    );
    Ok(total as u64)
}

/// Legacy path: plan from the local script (old inductor, or no units
/// offered). Keeps files locally and uploads nothing.
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
    let segments = data
        .get("segments")
        .and_then(|s| s.as_array())
        .cloned()
        .unwrap_or_default();
    let policy = bm_core::cast::policy_for_bible(engine, &layout.bible())?;
    let cast = bm_core::cast::load_cast(&script_path, &cast_path, &layout.bible(), &policy, true)?;
    let local = engine == "vieneu";
    let planned = bm_core::assemble::Planned::plan(&segments);
    let first = planned
        .speech
        .first()
        .map(|s| s.get("text").and_then(|t| t.as_str()).unwrap_or(""))
        .unwrap_or("");
    let title = bm_core::assemble::title_speech(layout, n, &cast, first);
    let units = bm_core::assemble::plan_render(&planned, &cast, &seg_dir, local, title.as_ref())?;
    let todo: Vec<_> = units
        .into_iter()
        .filter(|u| {
            !(u.dest.exists() && u.dest.metadata().map(|m| m.len() > 1000).unwrap_or(false))
        })
        .collect();
    let total = todo.len();
    std::fs::create_dir_all(&seg_dir)?;
    // No accent gate: any voice the sidecar can synthesize is allowed. If the
    // engine itself rejects a voice, that failure surfaces from /infer.
    // The render audit log belongs with the rest of the state, not in
    // `output/`. `output/` holds deliverables and nothing else — a machine
    // -readable record of TTS calls sitting beside the mp3s is a stray
    // intermediate in the one directory an operator actually looks at.
    let manifest = layout.bm_state().join("render-manifest.jsonl");
    for (i, u) in todo.iter().enumerate() {
        set_progress(
            shared,
            i as f32 / total.max(1) as f32,
            format!("render ch{n} {} ({}/{})", u.tag, i + 1, total),
        );
        let wav = tts
            .infer(&u.text, &u.voice, u.temperature, u.silence_p, engine)
            .await?;
        std::fs::write(&u.dest, &wav)?;
        bm_core::assemble::manifest_append(
            &manifest,
            &json!({"chapter": n, "tag": u.tag, "voice": u.voice,
                    "temp": u.temperature, "engine": engine}),
        )?;
    }
    set_progress(
        shared,
        1.0,
        format!("render ch{n} done ({total} new calls)"),
    );
    Ok(total as u64)
}

async fn run_merge(
    layout: &Layout,
    n: u32,
    engine: &str,
    gap_ms: u32,
    speed: f64,
    on: bm_core::ambience::LayerSwitch,
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
        on,
        scratch.clone(),
    );
    let assembled = tokio::task::spawn_blocking(move || {
        let (layout, n, engine, gap_ms, speed, on, scratch) = params;
        bm_core::assemble::assemble(
            &layout.script(n),
            &layout.cast(&engine),
            &layout.bible(),
            &layout.seg_dir(&engine, n),
            &scratch,
            n,
            gap_ms,
            on,
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

/// True when a heartbeat answer carries the shutdown command.
///
/// Box load for the heartbeat: CPU % plus RAM % and used GiB, sampled on
/// the beat. sysinfo needs two CPU refreshes to form a delta, so the first
/// beat reports `None` (the pane shows a dash) and every beat after is a
/// ~2s average — the cadence heartbeats already run at, no extra timer.
struct LoadProbe {
    sys: sysinfo::System,
    primed: bool,
}

impl LoadProbe {
    fn new() -> Self {
        let mut sys = sysinfo::System::new();
        sys.refresh_cpu_all();
        sys.refresh_memory();
        Self { sys, primed: false }
    }

    /// `(cpu_pct, mem_pct, mem_used_gib)`. `None` until the second sample:
    /// a CPU delta needs two refreshes, and reporting 0.0 would read as
    /// idle rather than unknown.
    fn sample(&mut self) -> (Option<f32>, Option<f32>, Option<f32>) {
        self.sys.refresh_cpu_all();
        self.sys.refresh_memory();
        if !self.primed {
            self.primed = true;
            return (None, None, None);
        }
        let total = self.sys.total_memory() as f64;
        let used = self.sys.used_memory() as f64;
        let mem_pct = if total > 0.0 {
            Some((100.0 * used / total) as f32)
        } else {
            None
        };
        let mem_gb = Some((used / 1_073_741_824.0) as f32);
        (Some(self.sys.global_cpu_usage()), mem_pct, mem_gb)
    }
}

/// Tolerant by design: an old inductor answers just `{"ok": true}` (no
/// `shutdown` key, so the default keeps us running), and a non-JSON answer
/// is ignored rather than acted on.
fn wants_shutdown(body: &[u8]) -> bool {
    serde_json::from_slice::<bm_proto::HeartbeatAck>(body)
        .map(|a| a.shutdown)
        .unwrap_or(false)
}

/// Who this worker is, as every report identifies it.
///
/// A struct rather than four arguments because the identity is passed to two
/// callers now — the heartbeat loop that pushes beats and the status endpoint
/// that answers for one — and four strings in the same order is exactly the
/// shape that gets transposed silently.
#[derive(Debug, Clone)]
struct WorkerIdentity {
    worker_id: String,
    addr: String,
    hostname: String,
    alias: String,
}

/// The heartbeat for right now. **One builder for both directions.**
///
/// The pull protocol posts this on a timer and the inverted protocol answers
/// `GET /status` with it. Two constructions would be two chances for the
/// inductor's liveness bookkeeping and its panes to disagree about what a
/// worker is doing, depending on which way the report travelled.
fn heartbeat_now(p: &Progress, who: &WorkerIdentity, probe: &mut LoadProbe) -> Heartbeat {
    let (cpu_pct, mem_pct, mem_gb) = probe.sample();
    Heartbeat {
        worker_id: who.worker_id.clone(),
        addr: who.addr.clone(),
        task_id: p.task_id.clone(),
        stage: p.stage.as_deref().and_then(bm_proto::Stage::parse),
        chapter: p.chapter,
        progress: p.frac,
        activity: p.activity.clone(),
        eta_secs: None,
        ts: bm_proto::now_secs(),
        hostname: who.hostname.clone(),
        alias: who.alias.clone(),
        cpu_pct,
        mem_pct,
        mem_gb,
        capabilities: capabilities(),
    }
}

/// What this worker can run, in one place.
///
/// Both `Register` and the `/status` answer carry it, and the inductor's
/// render gate reads whichever arrived. Two copies would let a worker claim
/// one thing on registration and another on its status poll, and the gate
/// would believe whichever it saw last.
///
/// `render-segments` is the migration gate: the inductor only offers render
/// tasks to a box that can produce units.
///
/// `merge` is advertised **only when ffmpeg is on PATH**. The merge stage shells
/// out to ffmpeg, so a box without it would take every merge offered and fail
/// each one — three strikes and the chapter shelves. Reporting the capability
/// truthfully lets the scheduler skip merge here and give the box its other
/// stages, instead of poisoning the ledger with failures it cannot help.
fn capabilities() -> Vec<String> {
    let mut caps = vec![
        "crawl".into(),
        "digest".into(),
        "render".into(),
        "render-segments".into(),
    ];
    if bm_core::assemble::ffmpeg_available() {
        caps.push("merge".into());
    }
    caps
}

async fn heartbeat_loop(
    http: reqwest::Client,
    inductor: String,
    who: WorkerIdentity,
    shared: Shared,
) {
    let url = format!("{inductor}/api/heartbeat");
    let mut probe = LoadProbe::new();
    loop {
        let p = shared.lock().map(|p| p.clone()).unwrap_or_default();
        let body = heartbeat_now(&p, &who, &mut probe);
        // The inductor's only command channel: a shutdown latch read on
        // every answer. Exiting here strands nothing — the inductor
        // reaps the lease (no strike) or requeues the ledger on its way
        // down, and an old inductor's `{"ok": true}` parses as "stay".
        if let Ok(resp) = http.post(&url).json(&body).send().await {
            if let Ok(bytes) = resp.bytes().await {
                if wants_shutdown(&bytes) {
                    println!("inductor asked for shutdown — exiting");
                    std::process::exit(0);
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Seconds of silence before a worker gives up on its inductor.
///
/// Derived from the same setting the inductor's own idle timer uses, so the
/// two cannot be configured into a state where the worker quits before the
/// inductor gets a chance to say goodbye.
fn idle_secs(s: &Settings) -> u64 {
    s.idle_mins.max(1) as u64 * 60
}

/// Exit when the inductor stops asking.
///
/// In serve-only mode this worker has no way to *notice* the inductor is gone:
/// no dial to fail, no report to be refused, just silence. Without this it
/// would hold its port and its TTS sidecar indefinitely.
///
/// A busy worker is exempt, and that exemption is load-bearing: the inductor
/// is blocked inside its own `POST /task` for the whole stage, so no polls
/// arrive by design — and counting that as idleness would kill a render
/// halfway through.
async fn idle_watchdog(push: std::sync::Arc<push::Push>, timeout: Duration) {
    loop {
        tokio::time::sleep(Duration::from_secs(15)).await;
        if push.is_busy() {
            push.touch();
            continue;
        }
        if push.silent_for() >= timeout {
            println!(
                "no contact from the inductor for {}s — exiting",
                timeout.as_secs()
            );
            std::process::exit(0);
        }
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

/// Install the offer's credentials into this process and return the variable
/// names that were set — never the values, which do not belong in a log.
///
/// Two consumers read them straight out of the environment: the generation
/// backends in `bm-core::digest::llm` (which is why a provisioned box used to
/// die on `GEMINI_API_KEY missing` — `.env` is personal and git-ignored, so it
/// is never one of the files provisioning copies), and the TTS sidecar, a
/// child process the worker spawns for a render and which inherits this
/// environment at `spawn()`.
///
/// **The inductor wins.** It holds the only copy the operator maintains, so a
/// value it sends replaces whatever this box had — a stale key on one machine
/// is precisely the failure this replaces. An *empty* value is skipped rather
/// than blanked, so a worker whose own `.env` is the only place a key exists
/// keeps working, and an offer from an inductor that has nothing configured
/// changes nothing at all.
///
/// `set_var` is process-global; it runs here, before the stage is dispatched
/// and before any child is spawned, which is the only point at which no other
/// thread is reading the environment.
fn install_credentials(creds: &bm_proto::Credentials) -> Vec<&'static str> {
    for (name, value) in creds.pairs() {
        std::env::set_var(name, value);
    }
    creds.names()
}

/// Run one offered task on this box, start to finish.
///
/// **No inductor URL, and no HTTP client.** A worker that never sends anything
/// to the inductor needs neither, and not having them is the enforcement: the
/// compiler will not let a future stage reintroduce a call home. Everything
/// this needs — the text, the script, the bible, the credentials, the units to
/// speak — arrived inside the offer.
async fn run_offer(
    layout: &Layout,
    settings: &Settings,
    offer: &TaskOffer,
    shared: &Shared,
    sidecar: &mut Sidecar,
) -> Result<TaskResult> {
    use bm_proto::Stage::*;
    let n = offer.chapter;
    // Credentials first: everything below — including the TTS sidecar this
    // call may spawn — reads them from the environment.
    let installed = install_credentials(&offer.credentials);
    if !installed.is_empty() {
        println!("[ok] credentials from inductor: {}", installed.join(", "));
    }
    // The offer is authoritative: materialize its artifacts first so any
    // machine can run any stage without shared storage.
    if let Some(text) = &offer.text {
        bm_core::atomic_write(&layout.chapter_txt(n), text)?;
    }
    if let Some(script) = &offer.script {
        bm_core::atomic_write(&layout.script(n), &serde_json::to_string_pretty(script)?)?;
    }
    // The cast decides the filenames this stage will look for, so the offer's
    // copy wins over anything on this box. Without it a merge on a provisioned
    // worker names every segment differently from the render and finds none.
    if let Some(cast) = &offer.cast {
        bm_core::atomic_write(
            &layout.cast(&offer.engine),
            &serde_json::to_string_pretty(cast)?,
        )?;
    }
    if let Some(bible) = &offer.bible {
        if offer.stage == bm_proto::Stage::Merge {
            bm_core::atomic_write(&layout.bible(), &serde_json::to_string_pretty(bible)?)?;
        }
    }
    match offer.stage {
        Crawl => {
            let url = offer.url.clone().unwrap_or_else(|| settings.chapter_url(n));
            let (units, text) = run_crawl(layout, n, &url, shared).await?;
            Ok(TaskResult {
                ok: true,
                detail: format!("crawled ch{n}"),
                delta: None,
                units,
                script: None,
                text: Some(text),
                mp3_b64: None,
            })
        }
        Digest => {
            let bible = offer.bible.clone().unwrap_or(json!({"characters": []}));
            // The inductor's pick wins; an empty offer (old inductor) falls
            // back to this worker's own settings, then to opencode.
            let analyzer = if offer.analyzer.is_empty() {
                let a = settings.analyzer.clone();
                if a.is_empty() {
                    "opencode".into()
                } else {
                    a
                }
            } else {
                offer.analyzer.clone()
            };
            // The backend name travels in `offer.analyzer`; what it runs
            // travels in `offer.analyzer_settings`. Both are needed here: this
            // box may have no `.bm/settings.json` at all (provisioning copies
            // `prompts/`, `python/`, `assets/` and `refs/`, never `.bm/` — that
            // is the inductor's state), in which case `Settings::load` silently
            // returns `Settings::default()` and the compiled-in model runs
            // instead of the operator's. That is the bug that made a box
            // configured for `gemini-3.5-flash-lite` call `gemini-3.5-flash`.
            let digest_settings = settings.with_analyzer_settings(&offer.analyzer_settings);
            let (delta, script) = run_digest(
                layout,
                n,
                &bible,
                &digest_settings,
                &analyzer,
                shared,
                false,
            )
            .await?;
            Ok(TaskResult {
                ok: true,
                detail: format!("digest ch{n} via {analyzer}"),
                delta: Some(delta),
                units: 1,
                script: Some(script),
                text: None,
                mp3_b64: None,
            })
        }
        Render => {
            sidecar.ensure(layout).await?;
            let units = match render_action(offer.render_units.as_deref()) {
                // Old inductor: plan from the local script, keep files
                // locally, upload nothing — exactly as before the migration.
                RenderAction::Legacy => {
                    run_render(layout, n, &offer.engine, &sidecar.tts(), shared).await?
                }
                // The offer names no units at all: nothing to speak.
                RenderAction::Noop => 0,
                // The whole chapter's units, minus what this box already has.
                // Skipping here rather than on the inductor is the point: the
                // inductor cannot see this disk, and a partial offer is what
                // used to leave a box holding a strict subset of a chapter.
                RenderAction::Units => {
                    // Proven non-empty by the match above.
                    let list = offer.render_units.as_deref().unwrap_or(&[]);
                    let todo =
                        pending_units(list, &offer.render_force, &layout.seg_dir(&offer.engine, n));
                    if todo.is_empty() {
                        0
                    } else {
                        render_offered_units(
                            layout,
                            n,
                            &offer.engine,
                            &todo,
                            &sidecar.tts(),
                            shared,
                        )
                        .await?
                    }
                }
            };
            sidecar.stop(); // per-task lifecycle: RSS returns to the OS here
            Ok(TaskResult {
                ok: true,
                detail: format!("render ch{n} ({units} calls)"),
                delta: None,
                units,
                script: None,
                text: None,
                mp3_b64: None,
            })
        }
        Merge => {
            let path = run_merge(
                layout,
                n,
                &offer.engine,
                offer.gap_ms,
                offer.speed,
                bm_core::ambience::LayerSwitch::new(
                    offer.ambience,
                    offer.music,
                    offer.effect_volume,
                    offer.music_volume,
                    offer.inject_volume,
                ),
                shared,
            )
            .await?;
            // **The product always comes home in the report.** This used to
            // depend on whether the box was the local node: a local merge
            // leaned on `publish()` having renamed the file straight into the
            // inductor's own `output/`, which is only true when the two share
            // a filesystem. Shipping it every time costs a few MB of base64
            // and removes the branch — and the inductor writes it to
            // `final_mp3` either way, idempotently for a box that already put
            // it there.
            let mp3 = Some(base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                &std::fs::read(&path).with_context(|| format!("reading merged {path}"))?,
            ));
            Ok(TaskResult {
                ok: true,
                detail: format!("merge ch{n} -> {path}"),
                delta: None,
                units: 1,
                script: None,
                text: None,
                mp3_b64: mp3,
            })
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
    inductor: Option<String>,
    worker_id: String,
    addr: String,
    tts_url: String,
    serve_tasks: Option<u16>,
) -> Result<()> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        // The inductor is loopback or LAN. An ambient `HTTP_PROXY` would
        // otherwise intercept every register/beat/task/complete and answer in
        // its place — which surfaces as a 502 from nowhere and a worker that
        // never joins. Same reasoning as `api::sidecar_client`.
        //
        // Deliberately *not* applied to `run_crawl`: a chapter URL is the one
        // thing here that is genuinely on the internet, and a proxy is exactly
        // what it should use.
        .no_proxy()
        .build()?;
    let hostname = hostname_simple();
    let alias = worker_alias_for(&layout.root);
    let who = WorkerIdentity {
        worker_id: worker_id.clone(),
        addr: addr.clone(),
        hostname: hostname.clone(),
        alias: alias.clone(),
    };
    let shared: Shared = Arc::new(Mutex::new(Progress {
        activity: "starting".to_string(),
        ..Default::default()
    }));
    // The instruction channel: this worker answers instead of only asking.
    let mut channel: Option<std::sync::Arc<push::Push>> = None;
    if let Some(port) = serve_tasks {
        // A worker with no token refuses to serve rather than serving openly:
        // "authenticated or off" is the only safe pair of states for a channel
        // that carries instructions.
        let Some(token) = bm_core::token::read(&layout.root) else {
            anyhow::bail!(
                "--serve-tasks needs a cluster token at {} — the inductor generates one and provisioning ships it; without it this worker would accept instructions from anything that can reach the port",
                layout.root.join(".bm").join(bm_core::token::FILE).display()
            );
        };
        let push = std::sync::Arc::new(push::Push {
            who: who.clone(),
            token,
            shared: shared.clone(),
            probe: Mutex::new(LoadProbe::new()),
            layout: layout.clone(),
            settings: settings.clone(),
            sidecar: tokio::sync::Mutex::new(Sidecar::new(&tts_url)),
            busy: std::sync::atomic::AtomicBool::new(false),
            last_contact: std::sync::atomic::AtomicU64::new(bm_proto::now_secs()),
        });
        let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
        println!("instruction channel on 0.0.0.0:{port} (token required)");
        let serving = push.clone();
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, push::router(serving)).await {
                eprintln!("instruction channel died: {e}");
            }
        });
        channel = Some(push);
    }

    // ── Serve-only ──────────────────────────────────────────────────────────
    // No inductor URL means nothing to dial, and that is the whole guarantee:
    // this worker cannot contact the inductor because it holds no address for
    // one — not because a flag told it not to. Every line below this point
    // that talks to an inductor is unreachable in this mode.
    let Some(inductor) = inductor else {
        let Some(push) = channel else {
            anyhow::bail!(
                "neither --inductor nor --serve-tasks: this worker would have nothing to do and no way to be given work"
            );
        };
        println!(
            "serve-only as {} — answering on port {}, dialling nothing",
            push.who.worker_id,
            serve_tasks.unwrap_or_default()
        );
        // The worker's own off switch. The inductor's timer covers the normal
        // case; this covers the inductor dying, where silence is otherwise
        // indistinguishable from "no work yet".
        idle_watchdog(push, Duration::from_secs(idle_secs(&settings) + 90)).await;
        return Ok(());
    };

    tokio::spawn(heartbeat_loop(
        http.clone(),
        inductor.clone(),
        who.clone(),
        shared.clone(),
    ));
    let reg = Register {
        worker_id: worker_id.clone(),
        addr: addr.clone(),
        hostname,
        capabilities: capabilities(),
        tts_url: Some(tts_url.clone()),
        version: VERSION.into(),
    };
    // The inductor may not be up yet (or the network may flap): retry
    // registration forever instead of dying on the first failure.
    loop {
        match http
            .post(format!("{inductor}/api/register"))
            .json(&reg)
            .send()
            .await
        {
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
    println!("registered as {worker_id} (alias {alias}), pulling tasks");
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
                delta: None,
                units: 0,
                script: None,
                text: None,
                mp3_b64: None,
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
        let mut reported = false;
        for attempt in 1..=3 {
            match http
                .post(format!("{inductor}/api/complete"))
                .json(&report)
                .send()
                .await
            {
                Ok(r) if r.status().is_success() => {
                    reported = true;
                    break;
                }
                Ok(r) => println!(
                    "[WARN] complete report refused ({}), retry {attempt}/3",
                    r.status()
                ),
                Err(e) => println!("[WARN] complete report lost, retry {attempt}/3: {e}"),
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
        // **No sweep.** A non-local worker's segment directory used to be
        // scratch, deleted here once its units had been uploaded and the
        // report accepted. That stopped being true when the merge moved onto
        // the renderer: this directory is now the merge's *input*, and the
        // inductor's copy is the one that is redundant (it exists for the
        // completion gate and for planning what is still missing). Deleting it
        // here failed every remote merge a stage later with missing segments.
        //
        // Reclaiming it is a job for a `gc` pass that knows the chapter is
        // finished, not for the worker that just produced it.
        let _ = (offer.render_units.as_deref(), reported);
        clear_task(&shared);
    }
}

fn hostname_simple() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "localhost".to_string())
}

/// Stable display names, one per worker root. The TUI used to hash the worker
/// id (`host-pid`), so every restart renamed every worker and 16 names
/// collided constantly. Now the name is drawn once, kept in `worker.alias`,
/// and reported on every heartbeat.
const ALIAS_POOL: [&str; 48] = [
    "fox",
    "owl",
    "bear",
    "wolf",
    "hare",
    "lynx",
    "otter",
    "hawk",
    "deer",
    "mole",
    "crane",
    "boar",
    "seal",
    "wren",
    "ibex",
    "newt",
    "badger",
    "stoat",
    "vole",
    "shrew",
    "weasel",
    "ferret",
    "mink",
    "marten",
    "sable",
    "pika",
    "marmot",
    "gopher",
    "chipmunk",
    "squirrel",
    "rabbit",
    "hedgehog",
    "porcupine",
    "armadillo",
    "opossum",
    "raccoon",
    "skunk",
    "coyote",
    "jackal",
    "hyena",
    "leopard",
    "cougar",
    "bobcat",
    "ocelot",
    "serval",
    "caracal",
    "genet",
    "civet",
];

/// The worker's display name: the kept one, or a fresh draw persisted for
/// next time. One worker per root is the deployment shape; two sharing a root
/// would share a name, so don't do that.
fn worker_alias_for(root: &std::path::Path) -> String {
    let path = root.join("worker.alias");
    if let Ok(saved) = std::fs::read_to_string(&path) {
        let saved = saved.trim().to_string();
        if !saved.is_empty() {
            return saved;
        }
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let mut h = nanos.wrapping_add(std::process::id() as u64);
    for b in hostname_simple().bytes() {
        h = h.wrapping_mul(31).wrapping_add(b as u64);
    }
    h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h ^= h >> 27;
    let name = ALIAS_POOL[h as usize % ALIAS_POOL.len()].to_string();
    let _ = std::fs::write(&path, &name);
    name
}

/// The default worker id: stable per root, not per process. The old
/// `{hostname}-{pid}` minted a new identity on every restart, so the
/// ledger's caps/workers/beats maps grew a row per restart and events
/// renamed every worker. The alias is drawn once and kept in
/// `worker.alias`, so `{hostname}-{alias}` survives restarts; an explicit
/// `--worker-id` still wins.
fn default_worker_id(root: &std::path::Path) -> String {
    format!("{}-{}", hostname_simple(), worker_alias_for(root))
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let layout = match cli.root {
        // Same resolution the inductor does: `--root` names the *repo*, and
        // the workspace pointer inside it decides where this book's settings,
        // data and output live. On a worker the root is the flat mirror, which
        // has no pointer and is therefore its own workspace.
        Some(r) => Layout::resolve(r)?,
        None => Layout::discover()?,
    };
    bm_core::config::load_dotenv(&layout.root.join(".env"));
    let settings = Settings::load(&layout.settings());
    match cli.cmd {
        Cmd::Run {
            stage,
            chapter,
            url,
            tts_url,
            engine,
        } => {
            let shared: Shared = Arc::new(Mutex::new(Progress::default()));
            let engine = if engine.is_empty() {
                settings.engine.clone()
            } else {
                engine
            };
            match stage.as_str() {
                "crawl" => {
                    let url = url.unwrap_or_else(|| settings.chapter_url(chapter));
                    run_crawl(&layout, chapter, &url, &shared).await?;
                }
                "digest" => {
                    let bible: Value = serde_json::from_str(
                        &std::fs::read_to_string(layout.bible())
                            .unwrap_or_else(|_| "{\"characters\":[]}".into()),
                    )?;
                    let analyzer = if settings.analyzer.is_empty() {
                        "opencode".into()
                    } else {
                        settings.analyzer.clone()
                    };
                    run_digest(
                        &layout, chapter, &bible, &settings, &analyzer, &shared, true,
                    )
                    .await?;
                }
                "render" => {
                    let tts_url = tts_url.unwrap_or_else(|| "http://127.0.0.1:8818".into());
                    let mut sidecar = Sidecar::new(&tts_url);
                    sidecar.ensure(&layout).await?;
                    run_render(&layout, chapter, &engine, &sidecar.tts(), &shared).await?;
                    sidecar.stop();
                }
                "merge" => {
                    run_merge(
                        &layout,
                        chapter,
                        &engine,
                        settings.gap_ms,
                        settings.speed,
                        bm_core::ambience::LayerSwitch::new(
                            settings.ambience,
                            settings.music,
                            settings.effect_volume,
                            settings.music_volume,
                            settings.inject_volume,
                        ),
                        &shared,
                    )
                    .await?;
                }
                other => anyhow::bail!("unknown stage {other:?} (crawl|digest|render|merge)"),
            }
        }
        Cmd::Worker {
            inductor,
            worker_id,
            addr,
            tts_url,
            serve_tasks,
        } => {
            let worker_id = worker_id.unwrap_or_else(|| default_worker_id(&layout.root));
            let addr = addr.unwrap_or_else(|| "127.0.0.1".into());
            let tts_url = tts_url.unwrap_or_else(|| "http://127.0.0.1:8818".into());
            // Same gate as the inductor: no pointer (or an empty live tree)
            // means a half-rsynced provision — refuse. Drift is adopted.
            // Provisioning writes the pointer.
            let pointer = bm_core::profile::verify(&layout.root)?;
            println!(
                "profile {} ({})",
                pointer.name,
                &pointer.hash[..12.min(pointer.hash.len())]
            );
            worker_loop(
                layout,
                settings,
                inductor,
                worker_id,
                addr,
                tts_url,
                serve_tasks,
            )
            .await?;
        }
        Cmd::Segments { .. } => {
            let manifest = segment_manifest(&layout.audio());
            println!("{}", serde_json::to_string(&manifest)?);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_probe_reports_nothing_then_sane_values() {
        // First sample primes the CPU delta (a 0.0 would read as idle,
        // not unknown); the second must be real percentages on any box.
        let mut probe = LoadProbe::new();
        assert_eq!(probe.sample(), (None, None, None));
        std::thread::sleep(std::time::Duration::from_millis(300));
        let (cpu, mem, gb) = probe.sample();
        let cpu = cpu.expect("second sample measures");
        let mem = mem.expect("memory always measures");
        let gb = gb.expect("memory always measures");
        assert!((0.0..=100.0).contains(&cpu), "cpu pct: {cpu}");
        assert!((0.0..=100.0).contains(&mem), "mem pct: {mem}");
        assert!(gb >= 0.0, "mem gib: {gb}");
    }

    #[test]
    fn segment_manifest_wire_format_round_trips() {
        // The inductor parses this JSON; the shape is the contract. Content
        // is covered in bm-core.
        let root = std::env::temp_dir().join(format!("bmseg{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("data/audio/segments-vieneu-7");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("0000_Adam.wav"), b"RIFF-fake").unwrap();

        let text = serde_json::to_string(&segment_manifest(&root.join("data/audio"))).unwrap();
        let back: Vec<bm_core::segments::SegmentEntry> = serde_json::from_str(&text).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(
            (back[0].chapter, back[0].name.as_str()),
            (7, "0000_Adam.wav")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn render_action_pins_the_empty_offer_to_noop() {
        use bm_proto::RenderUnitSpec;
        // Zero units means "report ok/0 at once" — never a fall-through into
        // rendering, and never the legacy path. Since the offer carries the
        // chapter's whole unit set, this arm now only fires for a chapter with
        // no units at all: "this box already holds everything" is decided in
        // `pending_units`, against the disk.
        assert_eq!(render_action(None), RenderAction::Legacy);
        assert_eq!(render_action(Some(&[])), RenderAction::Noop);
        let one = vec![RenderUnitSpec {
            tag: "0000".into(),
            name: "0000_Adam.wav".into(),
            speaker: "A".into(),
            voice: "Adam".into(),
            text: "hi".into(),
            temperature: 0.8,
            silence_p: 0.15,
        }];
        assert_eq!(render_action(Some(&one)), RenderAction::Units);

        // There is no sweep to assert any more: a rendered chapter's segment
        // directory is the merge's input, so nothing may delete it. See the
        // comment at the old call site for what that broke.
        assert_eq!(render_action(Some(&[])), RenderAction::Noop);
    }

    #[test]
    fn pending_units_skips_what_this_box_already_holds() {
        // Why the offer carries every unit: the inductor cannot see this disk,
        // so the skip has to happen here. A file that is present and
        // non-trivial is done — the same test `assemble` applies at merge time,
        // so the two cannot disagree about a chapter being ready.
        use bm_proto::RenderUnitSpec;
        let root = std::env::temp_dir().join(format!("bmpend{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let spec = |name: &str| RenderUnitSpec {
            tag: "0000".into(),
            name: name.into(),
            speaker: "A".into(),
            voice: "Adam".into(),
            text: "hi".into(),
            temperature: 0.8,
            silence_p: 0.15,
        };
        // Held, held but truncated, absent.
        std::fs::write(root.join("0000_Adam.wav"), vec![0u8; 2000]).unwrap();
        std::fs::write(root.join("0001_Adam.wav"), vec![0u8; 500]).unwrap();
        let offered = vec![
            spec("0000_Adam.wav"),
            spec("0001_Adam.wav"),
            spec("0002_Adam.wav"),
        ];

        let todo: Vec<&str> = pending_units(&offered, &[], &root)
            .into_iter()
            .map(|u| u.name.as_str())
            .collect();
        assert_eq!(
            todo,
            vec!["0001_Adam.wav", "0002_Adam.wav"],
            "a truncated file is not done, and order is preserved"
        );

        // Everything present → nothing to speak. This is the case the inductor
        // used to decide, and reporting it as a no-op render re-stamped the
        // merge's affinity to a box that held none of the chapter.
        std::fs::write(root.join("0001_Adam.wav"), vec![0u8; 2000]).unwrap();
        std::fs::write(root.join("0002_Adam.wav"), vec![0u8; 2000]).unwrap();
        assert!(pending_units(&offered, &[], &root).is_empty());

        // Forced names render even when held: a same-named file with stale
        // bytes (retagged text, repointed voice keeping the tag) must not
        // skip, or the box serves the old audio under the new plan.
        let todo: Vec<&str> = pending_units(&offered, &["0000_Adam.wav".to_string()], &root)
            .into_iter()
            .map(|u| u.name.as_str())
            .collect();
        assert_eq!(todo, vec!["0000_Adam.wav"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn shutdown_is_read_off_the_heartbeat_answer() {
        // New inductor, command set and clear.
        assert!(wants_shutdown(br#"{"ok":true,"shutdown":true}"#));
        assert!(!wants_shutdown(br#"{"ok":true,"shutdown":false}"#));
        // Old inductor: no `shutdown` key at all — the default keeps us
        // running, which is what makes either side upgradable on its own.
        assert!(!wants_shutdown(br#"{"ok": true}"#));
        // Garbage is ignored, never acted on.
        assert!(!wants_shutdown(b"not json"));
        assert!(!wants_shutdown(b""));
    }

    #[test]
    fn default_worker_id_is_stable_per_root_not_per_process() {
        // The naming fix: restarts must keep their identity, or the
        // ledger grows a row per restart and events rename every worker.
        let root = std::env::temp_dir().join(format!("bmid{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let first = default_worker_id(&root);
        assert!(first.starts_with(&format!("{}-", hostname_simple())));
        assert_eq!(
            default_worker_id(&root),
            first,
            "a restart keeps its id (the alias file persists it)"
        );
        assert!(root.join("worker.alias").is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn worker_alias_is_drawn_once_then_kept() {
        let root = std::env::temp_dir().join(format!("bmalias{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let first = worker_alias_for(&root);
        assert!(
            ALIAS_POOL.contains(&first.as_str()),
            "drawn from the pool: {first}"
        );
        assert_eq!(worker_alias_for(&root), first, "a restart keeps its name");
        assert!(
            root.join("worker.alias").is_file(),
            "persisted in the worker root"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_sidecar_argv_points_at_this_box_s_own_tree() {
        // Was `sidecar_python_prefers_the_managed_venv`. The venv order is gone
        // — there is one binary and one model directory now, and both hang off
        // the root, so the inductor and a worker resolve the same paths.
        let root = std::env::temp_dir().join(format!("bmtts{}", std::process::id()));
        let layout = Layout::new(&root);
        let (bin, args) = layout.sidecar_command(8818);

        assert_eq!(bin, root.join("bm-tts"));
        assert_eq!(args[0], "--models");
        assert_eq!(args[1], root.join("models").display().to_string());

        let value_of = |flag: &str| -> Option<String> {
            args.windows(2).find(|w| w[0] == flag).map(|w| w[1].clone())
        };
        // The dictionary and the voice store live inside the model directory,
        // and the codec shares it — one directory, not three.
        assert_eq!(
            value_of("--codec"),
            Some(root.join("models").display().to_string())
        );
        assert_eq!(
            value_of("--dict"),
            Some(root.join("models/sea_g2p.bin").display().to_string())
        );
        assert_eq!(
            value_of("--voices"),
            Some(root.join("models/voices.json").display().to_string())
        );
        assert_eq!(value_of("--port"), Some("8818".into()));
        // Loopback: the agent is the only caller, and the port is not
        // authenticated.
        assert_eq!(value_of("--bind"), Some("127.0.0.1".into()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn offered_credentials_replace_the_workers_own_and_empty_ones_do_not() {
        // The outage this closes: `.env` is personal and git-ignored, so it is
        // never among the files provisioning copies. A remote box therefore had
        // no key of its own and every digest died on `GEMINI_API_KEY missing`
        // however carefully the inductor was set up.
        std::env::remove_var("GEMINI_API_KEY");
        let names = install_credentials(&bm_proto::Credentials {
            gemini_api_key: "from-inductor".into(),
            openrouter_api_key: String::new(),
        });
        assert_eq!(
            names,
            vec!["GEMINI_API_KEY"],
            "the log gets names, never values"
        );
        assert_eq!(std::env::var("GEMINI_API_KEY").unwrap(), "from-inductor");

        // The inductor is the single source of truth: what it sends beats what
        // this box already held, which is the whole point of sending it.
        std::env::set_var("GEMINI_API_KEY", "stale-local");
        install_credentials(&bm_proto::Credentials {
            gemini_api_key: "from-inductor".into(),
            openrouter_api_key: String::new(),
        });
        assert_eq!(std::env::var("GEMINI_API_KEY").unwrap(), "from-inductor");

        // An unset key is skipped, never blanked: a worker whose own `.env` is
        // the only place a key exists keeps working, and an old inductor's
        // empty block changes nothing.
        install_credentials(&bm_proto::Credentials::default());
        assert_eq!(
            std::env::var("GEMINI_API_KEY").unwrap(),
            "from-inductor",
            "an absent value must not erase a present one"
        );
        std::env::remove_var("GEMINI_API_KEY");
    }

    /// A one-shot HTTP fixture on loopback: records each request body, answers
    /// with `response_body`. Returns `(base_url, bodies)`.
    ///
    /// The repo's own rule for this shape (ROADMAP §1.4: "pin the request shape
    /// against a local fixture server, no real API keys in tests") — and the
    /// only way to reach the arm under test, which is a network call by nature.
    fn fixture_server(response_body: &'static str) -> (String, Arc<Mutex<Vec<String>>>) {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback bind");
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        std::thread::spawn(move || {
            // A fixed budget rather than `incoming()`: with the fix removed no
            // request ever arrives, and an accept loop would sit here forever.
            for stream in listener.incoming().take(4) {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
                let mut len = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap_or(0);
                    }
                    if line.trim().is_empty() {
                        break;
                    }
                }
                let mut body = vec![0u8; len];
                let _ = reader.read_exact(&mut body);
                sink.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&body).into_owned());
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            }
        });
        (url, seen)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_digest_runs_on_the_inductors_analyzer_settings_not_the_boxes_own() {
        // The last mile of the analyzer-settings fix, and the only part a unit
        // test can reach. Deleting the overlay in the `Digest` arm left every
        // other test in this workspace green — the arm is a network call, so
        // nothing else looks at it.
        //
        // The `local` backend is the probe: its endpoint is configurable, so
        // this box's own settings can point at a dead port while the offer's
        // point at a fixture server. A pass-through reaches nothing at all.
        let (url, seen) = fixture_server(r#"{"message":{"content":"{}"}}"#);
        let dir = std::env::temp_dir().join(format!("bm-digest-settings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let layout = Layout::new(&dir);
        std::fs::create_dir_all(layout.chapters()).unwrap();
        std::fs::create_dir_all(dir.join("prompts")).unwrap();
        std::fs::write(
            layout.prompt(),
            "bible={bible_json}\nchapter={chapter_text}\n",
        )
        .unwrap();
        // The digest reads the music palette out of the scene map — it is the
        // vocabulary the prompt offers and the validator accepts — so a worker
        // with no `assets/` cannot be handed a chapter to digest at all.
        std::fs::create_dir_all(layout.assets()).unwrap();
        std::fs::write(
            layout.assets().join("scene-map.json"),
            r#"{"music_palette":{"quiet":{"tags":["soft"]},"none":{"tags":[]}}}"#,
        )
        .unwrap();

        // This box's own copy. On a real provisioned worker there is no
        // `.bm/settings.json` at all, so this would be `Settings::default()`
        // and the compiled-in model would run; a dead port makes the same
        // point without depending on what the default happens to be.
        let box_settings = Settings {
            ollama_url: "http://127.0.0.1:9".into(),
            local_model: "box-model".into(),
            ..Settings::default()
        };
        let offer = TaskOffer {
            task_id: "digest:1".into(),
            chapter: 1,
            stage: bm_proto::Stage::Digest,
            root: dir.display().to_string(),
            url: None,
            tts_url: None,
            engine: "vieneu".into(),
            model_order: vec![],
            analyzer: "local".into(),
            analyzer_settings: bm_proto::AnalyzerSettings {
                ollama_url: url.clone(),
                local_model: "offer-model".into(),
                ..Default::default()
            },
            credentials: bm_proto::Credentials::default(),
            bible: None,
            script: None,
            cast: None,
            text: Some("Chương 1\n\nCó một người đi qua cầu.\n".into()),
            gap_ms: 300,
            speed: 1.0,
            ambience: false,
            music: false,
            effect_volume: 1.0,
            music_volume: 1.0,
            inject_volume: 1.0,
            render_units: None,
            render_force: vec![],
            local_node: false,
        };
        let shared: Shared = Arc::new(Mutex::new(Progress::default()));
        let mut sidecar = Sidecar::new("http://127.0.0.1:8818");

        // The digest itself is *expected* to fail — the fixture answers `{}`,
        // which is not a valid digest, so the one repair attempt fails too.
        // What is under test is where the request went and what it asked for.
        let _ = run_offer(&layout, &box_settings, &offer, &shared, &mut sidecar).await;

        let bodies = seen.lock().unwrap().clone();
        assert!(
            !bodies.is_empty(),
            "the offer's endpoint was never called — the digest ran on this \
             box's own settings"
        );
        assert!(
            bodies.iter().any(|b| b.contains("offer-model")),
            "the offered model must be the one requested: {bodies:?}"
        );
        assert!(
            !bodies.iter().any(|b| b.contains("box-model")),
            "this box's own model must not be used: {bodies:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
