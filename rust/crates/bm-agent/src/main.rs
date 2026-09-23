//! Worker agent: pull one pipeline stage at a time, run it, report back.
//!
//! Two modes:
//! - `run`    — a single stage for one chapter, standalone (no inductor).
//! - `worker` — register with the inductor and pull tasks until stopped.
//!
//! The agent never loads a model and never writes the authoritative bible.
//! TTS goes through the sidecar over HTTP; the agent owns the sidecar's
//! lifecycle, which is what keeps sidecar RSS bounded no matter how many
//! chapters flow through: it is kept warm *across* tasks (a per-task stop would
//! reload ~2.85 GB for every offer), reaped after an idle interval, dropped
//! before a merge's ffmpeg pass, and **recycled at a task boundary once it has
//! outgrown its memory budget** — because a box that renders continuously is
//! never idle, so idleness alone is not a memory guard.

mod hook;
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

use tts::{Health, Tts};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const SIDECAR_PORT: u16 = 8818;

/// Sidecar startup budget: how long `/health` is polled before giving up.
/// The load is minutes on a slow box, so this is generous on purpose — and it
/// is the *wait* that prevents a duplicate, not a shorter timeout.
const SIDECAR_STARTUP_TICKS: usize = 60;
const SIDECAR_STARTUP_TICK: Duration = Duration::from_secs(5);

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
    /// A stage that finished but was never acknowledged — the completion
    /// hook's stash. Written by the task handler (serve mode) the moment a
    /// stage ends, cleared by the hook once the inductor accepts it through
    /// the reverse tunnel, or by the next offer (the inductor is talking
    /// again, so this outcome is stale by definition). `None` in pull mode,
    /// whose reports retry on their own channel.
    pending: Option<Complete>,
}

type Shared = Arc<Mutex<Progress>>;

fn set_progress(shared: &Shared, frac: f32, activity: String) {
    if let Ok(mut p) = shared.lock() {
        p.frac = frac.clamp(0.0, 1.0);
        p.activity = activity;
    }
}

// ---------------------------------------------------------------------------
// TTS sidecar lifecycle: warm across tasks, recycled on a budget.
// ---------------------------------------------------------------------------
// Spawn paths live on `Layout` (`sidecar_binary`, `sidecar_command`) so the
// inductor's preview path resolves the same tree without a second copy to
// drift.

/// The share of this box's RAM the TTS sidecar may hold before it is recycled.
///
/// The model is ~2.85 GB resident the moment its weights load, so on the
/// 8 GiB boxes this project provisions that is already ~36% — this is a
/// *growth* budget, not a size limit, and it has to sit far enough above the
/// floor that a freshly loaded model is nowhere near it.
///
/// A **fraction of the box**, not a fixed number of megabytes, because the two
/// failure modes are asymmetric: a fixed cap that is too low on a big box
/// reloads the model for nothing (minutes per recycle), and one that is too
/// high on a small box never fires before the OOM killer does. The fraction
/// scales with the machine and needs no configuration to be right.
const SIDECAR_RSS_FRACTION: f64 = 0.5;

/// Renders one sidecar process may serve before it is recycled regardless of
/// its measured footprint.
///
/// A second trigger on purpose. The RSS reading is the direct guard, but it is
/// a *sample* — it can miss a slow climb inside one long batch, and on a
/// platform whose per-process memory is not reported it reads zero, which would
/// silently disable the whole guard. A count cannot be unavailable and cannot
/// be misread; it is the coarse backstop under the fine one.
const SIDECAR_MAX_RENDERS: u64 = 200;

/// The shortest life a sidecar may have before the guard will recycle it again.
///
/// Without this the guard has a perverse corner: if the model's baseline
/// footprint were already over the cap, every task boundary would recycle, and
/// a box would spend its whole time reloading ~2.85 GB instead of rendering —
/// a worse failure than the leak it set out to fix. This bounds the cost of a
/// badly-tuned budget to one reload per interval.
const SIDECAR_MIN_LIFETIME_SECS: u64 = 300;

/// The guard's thresholds, resolved **once per process**.
///
/// Constants with environment overrides rather than a `Settings` field, and the
/// reason is the trap this repo has already paid for: a provisioned worker has
/// no `settings.json` at all, so a per-workspace setting would be invisible
/// exactly on the boxes that hold the leak. This is a property of the *box* —
/// how much RAM it has — so it belongs to the box's own environment.
///
/// The overrides exist to make the numbers **measurable**, which is the only
/// honest way to set them. The fraction is a judgement (see
/// [`SIDECAR_RSS_FRACTION`]); the way to replace a judgement with a figure is to
/// run a box with a known value and read the log, not to reason harder:
///
/// ```text
/// BM_TTS_MAX_RSS_MB=2048 BM_TTS_MAX_RENDERS=50 ./bm-agent --root … worker …
/// ```
///
/// A worker logs the resolved budget once at startup, so a log from a box says
/// both what was in force and what the guard did with it. Unset is the shipped
/// default in every case — this adds a knob, it does not move one.
#[derive(Debug, Clone, Copy)]
struct Budget {
    /// An absolute cap in MiB, or `None` to use [`SIDECAR_RSS_FRACTION`] of the
    /// box's RAM. Absolute when set, because an operator testing a value wants
    /// *that* value, not that value scaled by whatever the box turns out to be.
    rss_cap_mib: Option<f64>,
    max_renders: u64,
    min_lifetime_secs: u64,
}

impl Default for Budget {
    fn default() -> Self {
        Budget {
            rss_cap_mib: None,
            max_renders: SIDECAR_MAX_RENDERS,
            min_lifetime_secs: SIDECAR_MIN_LIFETIME_SECS,
        }
    }
}

impl Budget {
    /// The compiled defaults, with any per-box override applied.
    ///
    /// Read from the environment here and cached, not consulted per check: the
    /// value belongs to the box, and a half-written environment must not be able
    /// to change the guard's thresholds mid-run.
    fn from_env() -> Budget {
        // Two typed readers rather than one generic: a closure's type is fixed
        // by its first use, and `rss_cap_mib` is an `f64` while the other two
        // are counts.
        let num_f = |key: &str| {
            std::env::var(key)
                .ok()
                .and_then(|v| v.trim().parse::<f64>().ok())
        };
        let num_u = |key: &str| {
            std::env::var(key)
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
        };
        let d = Budget::default();
        Budget {
            rss_cap_mib: num_f("BM_TTS_MAX_RSS_MB"),
            max_renders: num_u("BM_TTS_MAX_RENDERS").unwrap_or(d.max_renders),
            min_lifetime_secs: num_u("BM_TTS_MIN_LIFETIME_SECS").unwrap_or(d.min_lifetime_secs),
        }
    }

    /// The cap for a box with `total_bytes` of RAM, in MiB — `None` when the box
    /// did not report its memory and no absolute cap was given. An unknown
    /// denominator is not a budget; the count trigger still applies.
    fn cap_mib(&self, total_bytes: u64) -> Option<f64> {
        if let Some(cap) = self.rss_cap_mib {
            return Some(cap);
        }
        (total_bytes > 0).then(|| total_bytes as f64 / 1_048_576.0 * SIDECAR_RSS_FRACTION)
    }

    /// One line naming the budget in force, for the startup log.
    fn describe(&self, total_bytes: u64) -> String {
        match self.cap_mib(total_bytes) {
            Some(cap) => format!(
                "TTS sidecar budget: recycle at {cap:.0} MiB resident{}{}, or {} renders, at most once per {}s",
                if self.rss_cap_mib.is_some() { " (BM_TTS_MAX_RSS_MB)" } else { "" },
                if self.rss_cap_mib.is_some() {
                    String::new()
                } else {
                    format!(" ({:.0}% of this box)", SIDECAR_RSS_FRACTION * 100.0)
                },
                self.max_renders,
                self.min_lifetime_secs,
            ),
            None => format!(
                "TTS sidecar budget: {} renders, at most once per {}s (no RSS cap — this box reports no memory)",
                self.max_renders, self.min_lifetime_secs
            ),
        }
    }
}

struct Sidecar {
    tts_url: String,
    child: Option<tokio::process::Child>,
    /// Renders this worker has spoken through the process **currently on the
    /// port**. Reset whenever a process is started or reaped, so it counts the
    /// model's work rather than the worker's lifetime — which is the question
    /// the guard is asking.
    served: u64,
    /// When the process currently serving was first used, in Unix seconds.
    /// `None` until the first render, so the cooldown is measured from work
    /// rather than from process start (a model that loads for four minutes and
    /// then serves nothing has nothing to recycle for).
    serving_since: Option<u64>,
    /// This guard's own `sysinfo` handle. Separate from the heartbeat's
    /// (`LoadProbe`) on purpose: they run on different clocks, and sharing one
    /// would mean the guard's census refreshed the CPU delta the panes read.
    sys: sysinfo::System,
    /// The thresholds in force **on this box**, resolved once at construction
    /// from the compiled defaults plus any per-box override. See [`Budget`].
    budget: Budget,
}

/// The memory guard's decision, as a pure function of what was measured.
///
/// Split out from [`Sidecar::over_budget`] because the branches are the part
/// worth pinning: a guard whose thresholds can only be exercised by holding a
/// 2.85 GB model is a guard nobody tests, and this one is the difference
/// between a long render run finishing and an OOM killing the box. Taking the
/// [`Budget`] as an argument rather than reading the constants also means a test
/// can assert a *chosen* threshold instead of the one this box happens to have.
///
/// `rss_bytes`/`total_bytes` are the sidecar's resident set and the box's RAM;
/// `served` is how many takes the process has spoken; `serving_for` is how long
/// ago it last did, or `None` if it never has.
///
/// Order matters and is deliberate:
///
/// 1. **The cooldown.** A budget that is wrong — a model whose baseline already
///    exceeds the cap — would otherwise recycle at *every* task boundary, and
///    the box would spend its life reloading instead of rendering. This bounds
///    the cost of a bad number to one reload per interval.
/// 2. **The render count.** Not gated on the memory reading on purpose: a count
///    cannot be unavailable, so this trigger still fires on a platform that
///    reports no per-process memory. Gating it on the census is exactly how the
///    whole guard would go quietly dead.
/// 3. **The resident set.** The direct guard, and the one that catches growth
///    inside a batch that the count has not reached yet.
///
/// The reason is returned as text so the log line names the number that fired.
/// "over budget" with no figures is a guard an operator can only guess at.
fn budget_verdict(
    rss_bytes: u64,
    total_bytes: u64,
    served: u64,
    serving_for: Option<u64>,
    budget: &Budget,
) -> Option<String> {
    if matches!(serving_for, Some(life) if life < budget.min_lifetime_secs) {
        return None;
    }
    if served >= budget.max_renders {
        return Some(format!(
            "has served {served} renders since it started (budget {})",
            budget.max_renders
        ));
    }
    if let Some(cap_mib) = budget.cap_mib(total_bytes) {
        let rss_mib = rss_bytes as f64 / 1_048_576.0;
        if rss_mib >= cap_mib {
            return Some(format!(
                "holds {rss_mib:.0} MiB, at or over the {cap_mib:.0} MiB budget{}",
                if budget.rss_cap_mib.is_some() {
                    " (BM_TTS_MAX_RSS_MB)"
                } else {
                    " (half this box's RAM)"
                }
            ));
        }
    }
    None
}

impl Sidecar {
    fn new(tts_url: &str) -> Self {
        Sidecar {
            tts_url: tts_url.trim_end_matches('/').to_string(),
            child: None,
            served: 0,
            serving_since: None,
            sys: sysinfo::System::new(),
            budget: Budget::from_env(),
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

    /// Whether this worker is holding a sidecar child it started.
    fn is_running(&self) -> bool {
        self.child.is_some()
    }

    /// Count the takes a render actually spoke. The guard's coarse trigger.
    fn note_renders(&mut self, units: u64) {
        if units == 0 {
            return;
        }
        self.served = self.served.saturating_add(units);
        self.serving_since.get_or_insert_with(bm_proto::now_secs);
    }

    /// Forget the work counted against the process on the port. Called wherever
    /// a process goes away — a spawn, a stop, a reap — so `served` never
    /// describes a model that is no longer there.
    fn forget_work(&mut self) {
        self.served = 0;
        self.serving_since = None;
    }

    /// Why the sidecar on this port should be recycled before the next render,
    /// or `None` to leave it alone.
    ///
    /// Measurement only — the decision is [`budget_verdict`], which is pure so
    /// every branch of it can be tested without a model in the way.
    fn over_budget(&mut self) -> Option<String> {
        let (_, bytes) = sidecar_processes(&mut self.sys);
        let total = self.sys.total_memory();
        let serving_for = self
            .serving_since
            .map(|t| bm_proto::now_secs().saturating_sub(t));
        budget_verdict(bytes, total, self.served, serving_for, &self.budget)
    }

    /// Ensure the sidecar answers, starting it if needed. Idempotent.
    /// Verifies `/policy`, not just `/health`: a stale server from a previous
    /// deploy answers health but lacks the endpoints renders depend on.
    ///
    /// **The rule that keeps the box alive:** a server that exists but is still
    /// loading is *waited for*, never raced. `bm-tts` binds its port before the
    /// ~2.85 GB load, so a bound port answering 503 means "starting" — and
    /// spawning here would put two models in RAM on an 8 GiB box.
    async fn ensure(&mut self, layout: &Layout) -> Result<()> {
        if self.serving_current().await {
            return Ok(());
        }
        // Not ready — but two different things can be in the way, and they want
        // opposite treatment.
        //
        // **Starting:** a previous attempt (ours, provisioning's detached one,
        // another task's) is still loading. Wait for it, never race it; starting
        // a second model here is the OOM on an 8 GiB box.
        //
        // **Up but wrong:** `/health` answers and `/policy` does not, which is a
        // server from an older deploy. It has to go, and it is nobody's child —
        // `stop` only signals our own — so it is asked over `/shutdown`. Without
        // this, bind-first would turn "stale sidecar" into a five-minute wait for
        // a server that can never become capable.
        match self.tts().probe().await {
            Health::Loading => {
                if self.wait_ready().await {
                    return Ok(());
                }
                anyhow::bail!(
                    "TTS sidecar is still loading after {}s",
                    SIDECAR_STARTUP_TICKS as u64 * SIDECAR_STARTUP_TICK.as_secs()
                );
            }
            Health::Up => self.reap_all().await,
            Health::Absent => self.stop(),
        }
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
        for _ in 0..SIDECAR_STARTUP_TICKS {
            tokio::time::sleep(SIDECAR_STARTUP_TICK).await;
            if self.serving_current().await {
                self.child = Some(child);
                // A fresh process: the renders counted against the previous one
                // are not its work and must not count towards its budget.
                self.forget_work();
                return Ok(());
            }
            if child.try_wait()?.is_some() {
                // Our child exited. If an instance that is *starting* holds the
                // port — it won the bind while we were loading — wait for it
                // rather than failing the render on a race we lost by
                // microseconds. Only `Loading` qualifies: a server that answers
                // but is not the one we need will never become ready, and that
                // case is better served by this bail, which quotes the child's
                // own dying words (usually "address already in use").
                if self.tts().probe().await == Health::Loading && self.wait_ready().await {
                    return Ok(());
                }
                anyhow::bail!(
                    "TTS sidecar exited during startup{}",
                    child_stderr_tail(&mut stderr).await
                );
            }
        }
        let _ = child.kill().await;
        anyhow::bail!("TTS sidecar never answered /health")
    }

    /// Poll until a server answers ready, or the startup budget runs out.
    async fn wait_ready(&self) -> bool {
        for _ in 0..SIDECAR_STARTUP_TICKS {
            tokio::time::sleep(SIDECAR_STARTUP_TICK).await;
            if self.serving_current().await {
                return true;
            }
        }
        false
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

    /// Signal our own child, if we have one, and forget the work counted
    /// against it.
    ///
    /// `stop` is called in exactly the places where a model is meant to go
    /// away — the idle reaper, `ensure` finding nothing on the port, and
    /// `reap_all` — so clearing `served` here is what keeps the count attached
    /// to a *process* rather than to this worker's lifetime.
    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
        }
        self.forget_work();
    }

    /// Stop **every** sidecar on this box: the child we spawned, and a server
    /// somebody else started (provisioning's detached `nohup … &`, or the
    /// inductor's audition server) which is asked to exit over HTTP because it
    /// is not ours to signal.
    ///
    /// Two callers, and they share one reason: the model has to actually be
    /// gone before the next thing that needs the memory runs. A **merge**, the
    /// one stage with its own large transient working set — ffmpeg on top of a
    /// resident 2.85 GB model is the co-residency that OOMs an 8 GiB box — and
    /// the **memory guard**, which recycles a model that has outgrown its
    /// budget. Both wait for the port to go quiet, because "asked" is not
    /// "freed".
    async fn reap_all(&mut self) {
        self.stop();
        let tts = self.tts();
        if tts.probe().await == Health::Absent {
            return;
        }
        if let Err(e) = tts.shutdown().await {
            // An older sidecar has no `/shutdown`. Say so and move on: one is
            // still better than two, and the cluster sweep remains the backstop.
            eprintln!("could not ask the sidecar to exit ({e}) — merge proceeds with it resident");
            return;
        }
        // The model is dropped by the process returning from `main`; give the
        // kernel a moment to take the pages back before ffmpeg asks for its own.
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            if tts.probe().await == Health::Absent {
                return;
            }
        }
        eprintln!("sidecar still answering 5s after shutdown — merge proceeds anyway");
    }

    /// Recycle the model if it has outgrown its budget — **the memory guard**.
    ///
    /// The leak this exists for: a render is not a pure function of its input
    /// as far as memory goes. The sidecar's working set grows across a long
    /// run of inferences (allocator fragmentation, cached activations, whatever
    /// the ONNX runtime keeps), and nothing returned it to the OS. On a box
    /// that renders continuously the idle reaper never fires — it is keyed on
    /// *idleness*, and a busy box is never idle — so RSS climbed until the OOM
    /// killer or `MEM_PCT_CEILING` on the inductor's side stopped it, both of
    /// which are consequences rather than cures.
    ///
    /// Two properties make this safe, and both matter:
    ///
    /// * **It runs between tasks, never during one.** The caller invokes it at
    ///   the top of a render arm, before the first `/infer`. Recycling
    ///   mid-render would kill the request and lose the take — and TTS is
    ///   stochastic, so a lost take is not reproducible from its inputs.
    /// * **It is bounded by a cooldown** (see [`SIDECAR_MIN_LIFETIME_SECS`]), so
    ///   a budget that is wrong — a model whose baseline already exceeds the
    ///   cap — degrades into one reload per interval rather than a reload per
    ///   offer.
    ///
    /// The action is `reap_all`, not `stop`, and that is deliberate. This is
    /// the one case where the worker overrides its own "an adopted server is
    /// somebody else's to keep" rule: a box about to be killed by its own
    /// memory cannot leave the decision to whoever started the model. The cost
    /// is that a local-node audition can be cut short by a recycle — the same
    /// trade a merge already makes — and the next render starts a fresh server
    /// this worker owns, so at most one recycle per adoption is ambiguous.
    async fn recycle_if_over_budget(&mut self) {
        let Some(why) = self.over_budget() else {
            return;
        };
        println!("sidecar {why} — recycling it so the next render starts from a clean model");
        self.reap_all().await;
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
) -> Result<(u64, Vec<bm_proto::UnitFile>)> {
    let total = units.len();
    let seg_dir = layout.seg_dir(engine, n);
    std::fs::create_dir_all(&seg_dir)?;
    let mut files = Vec::with_capacity(total);
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
        files.push(bm_proto::UnitFile {
            name: u.name.clone(),
            b64: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &wav),
        });
    }
    set_progress(
        shared,
        1.0,
        format!("render ch{n} done ({total} new calls)"),
    );
    Ok((total as u64, files))
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
    let policy = bm_core::cast::policy_for_bible(engine);
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

/// The mix a merge runs with: the offer's own settings, or this box's on the
/// hand-driven path. Grouped because they travel together and because the
/// take list is the plan's, not something the mixer may recompute.
struct MergeJob {
    gap_ms: u32,
    speed: f64,
    on: bm_core::ambience::LayerSwitch,
    /// The chapter's take files in mix order. Empty means "no plan" — this box
    /// then names them itself, exactly as before takes were content-addressed.
    takes: Vec<String>,
}

async fn run_merge(
    layout: &Layout,
    n: u32,
    engine: &str,
    job: MergeJob,
    shared: &Shared,
    fetch: Option<(&reqwest::Client, &str)>,
) -> Result<String> {
    let MergeJob {
        gap_ms,
        speed,
        on,
        takes,
    } = job;
    set_progress(shared, 0.1, format!("merge ch{n}"));
    // Pieces, not chapters: a merge runs on any box, so it pulls the takes
    // it lacks from the inductor — whose store holds every completed take —
    // instead of requiring them on local disk. Offer-driven only: the
    // hand-driven path carries no take list and mixes what is here.
    if !takes.is_empty() {
        if let Some((http, inductor)) = fetch {
            let seg_dir = layout.seg_dir(engine, n);
            std::fs::create_dir_all(&seg_dir)?;
            for name in &takes {
                let dest = seg_dir.join(name);
                if dest
                    .metadata()
                    .map(|m| m.len() > 1000)
                    .unwrap_or(false)
                {
                    continue;
                }
                let url =
                    format!("{inductor}/api/segment?chapter={n}&engine={engine}&name={name}");
                let bytes = http
                    .get(&url)
                    .send()
                    .await
                    .with_context(|| format!("pulling segment {name}"))?
                    .error_for_status()
                    .with_context(|| format!("pulling segment {name}"))?
                    .bytes()
                    .await
                    .with_context(|| format!("pulling segment {name}"))?;
                if bytes.len() <= 1000 {
                    anyhow::bail!("segment {name} pulled {} bytes — not a take", bytes.len());
                }
                std::fs::write(&dest, &bytes)
                    .with_context(|| format!("storing segment {name}"))?;
            }
        }
    }    // Everything the merge writes goes into one per-chapter scratch directory
    // under `.bm/`, never into `output/`. `assemble` hands back the mp3 (or a
    // wav when ffmpeg is missing) and `publish` renames it out to `output/`,
    // after which the whole scratch directory can go.
    let scratch = layout.scratch_ch(n);
    // The inductor's plan names the files; a worker that recomputed them would
    // look for legacy names and find nothing. An empty list is an old inductor:
    // then `assemble` plans the names itself, exactly as before.
    let takes: Option<Vec<String>> = (!takes.is_empty()).then_some(takes);
    let params = (
        layout.clone(),
        n,
        engine.to_string(),
        gap_ms,
        speed,
        on,
        scratch.clone(),
        takes,
    );
    let assembled = tokio::task::spawn_blocking(move || {
        let (layout, n, engine, gap_ms, speed, on, scratch, takes) = params;
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
            takes.as_deref(),
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
///
/// The same sample counts the TTS sidecars, because that is the quantity that
/// actually kills these boxes: one model is ~2.85 GB and an 8 GiB box cannot
/// hold two, so `> 1` is not a curiosity to log and move past — it is the OOM
/// warming up, and nothing in the cluster could see it before.
struct LoadProbe {
    sys: sysinfo::System,
    primed: bool,
    /// Whether the last sample already warned about a duplicate sidecar, so a
    /// box that stays wrong is reported once through its transition instead of
    /// on every beat. sysinfo is shared with the CPU delta: one timer, one
    /// refresh, no second sampling loop.
    warned_extra_sidecar: bool,
}

/// Everything one beat reports about the box: `(cpu_pct, mem_pct,
/// mem_used_gib, sidecar_count, sidecar_rss_gib)`.
///
/// A named alias because it is a five-tuple threaded from the sampler to the
/// heartbeat builder and back through the status endpoint — positional and
/// easy to transpose, so the names belong somewhere.
type Load = (
    Option<f32>,
    Option<f32>,
    Option<f32>,
    Option<u32>,
    Option<f32>,
);

impl LoadProbe {
    fn new() -> Self {
        let mut sys = sysinfo::System::new();
        sys.refresh_cpu_all();
        sys.refresh_memory();
        Self {
            sys,
            primed: false,
            warned_extra_sidecar: false,
        }
    }

    /// One sample. `None` CPU/RAM until the second call: a CPU delta needs two
    /// refreshes, and reporting 0.0 would read as idle rather than unknown. The
    /// sidecar count is available immediately — it is a census, not a delta.
    fn sample(&mut self) -> Load {
        self.sys.refresh_cpu_all();
        self.sys.refresh_memory();
        let (count, rss_gb) = self.sidecars();
        if count > 1 && !self.warned_extra_sidecar {
            self.warned_extra_sidecar = true;
            eprintln!(
                "WARNING: {count} bm-tts processes are alive ({rss_gb:.1} GB resident) — this box is a duplicate model away from the OOM killer; the cluster sweep (`X`) clears it"
            );
        } else if count <= 1 {
            self.warned_extra_sidecar = false;
        }
        if !self.primed {
            self.primed = true;
            return (None, None, None, Some(count), Some(rss_gb));
        }
        let total = self.sys.total_memory() as f64;
        let used = self.sys.used_memory() as f64;
        let mem_pct = if total > 0.0 {
            Some((100.0 * used / total) as f32)
        } else {
            None
        };
        let mem_gb = Some((used / 1_073_741_824.0) as f32);
        (
            Some(self.sys.global_cpu_usage()),
            mem_pct,
            mem_gb,
            Some(count),
            Some(rss_gb),
        )
    }

    /// `(count, total RSS GiB)` of the sidecars on this box.
    fn sidecars(&mut self) -> (u32, f32) {
        let (count, bytes) = sidecar_processes(&mut self.sys);
        (count, bytes as f32 / 1_073_741_824.0)
    }
}

/// The refresh the census runs under: RSS, and **no tasks**.
///
/// `System::refresh_processes` ends in `.with_tasks()`, and sysinfo's own
/// `Default` sets `tasks: true` — because on Linux it lists every *task*
/// (thread) as a process in its own right, each one carrying its parent's
/// `/proc/<pid>` and therefore the parent's whole RSS. A sidecar with an
/// 8-thread ONNX pool was therefore counted as **8 processes holding eight
/// times its memory**: the Linux worker reported `8× 19.5G` for a 2.4 GiB
/// model on a box that simultaneously reported 3.8 GB used. Every task's
/// `/proc/<pid>/task/<tid>/statm` is byte-identical to the leader's, which is
/// what made the multiplication exact.
///
/// macOS enumerates no tasks — `Process::thread_kind` is `None` off
/// Linux/Android — so only the Linux workers were ever wrong, which is why
/// the pane looked sane on the inductor's own box.
///
/// Named and separate so a test can pin the one flag whose *default* is the
/// wrong answer. `nothing()` is not enough on its own: it is `Default`, and
/// `Default` is where `tasks: true` lives.
fn census_refresh_kind() -> sysinfo::ProcessRefreshKind {
    sysinfo::ProcessRefreshKind::nothing()
        .with_memory()
        .without_tasks()
}

/// `(count, total RSS bytes)` of the `bm-tts` processes on this box.
///
/// Matched on the process *name* containing `bm-tts`, which covers both
/// spellings a worker can run — the provisioned `~/bm-worker/bm-tts` and a
/// repo build at `rust/target/{debug,release}/bm-tts` — and deliberately not
/// on argv, which would also match an `ssh … bm-tts` wrapper on the
/// inductor's own box. `name` comes from the process's own `stat` parse, not
/// from a refresh flag, so the narrow [`census_refresh_kind`] keeps it.
///
/// One definition, two callers: the heartbeat's load sample, and the sidecar's
/// own memory guard. Two copies would let the number the panes show and the
/// number the guard acts on disagree — which is the worst version of this,
/// because the guard would be recycling on a reading nobody could see.
fn sidecar_processes(sys: &mut sysinfo::System) -> (u32, u64) {
    sys.refresh_processes_specifics(sysinfo::ProcessesToUpdate::All, true, census_refresh_kind());
    let (mut count, mut bytes) = (0u32, 0u64);
    for p in sys.processes().values() {
        // Belt and braces, and not redundant: the refresh above cannot yield a
        // task, but `tasks: true` is the *default*, so one future call to
        // `refresh_processes` or `everything()` restores the 8× silently. A
        // thread is not a model, whatever the refresh asked for.
        if p.thread_kind().is_some() {
            continue;
        }
        if p.name().to_string_lossy().contains("bm-tts") {
            count += 1;
            bytes = bytes.saturating_add(p.memory());
        }
    }
    (count, bytes)
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
    let (cpu_pct, mem_pct, mem_gb, sidecars, sidecar_gb) = probe.sample();
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
        sidecars,
        sidecar_gb,
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
                    // No sidecar is stopped here, and it is a known gap rather
                    // than an oversight: this is the *pull* protocol, whose
                    // sidecar lives in the task loop's own locals and is not
                    // reachable from this task (the inverted protocol — the one
                    // the inductor actually drives now — reaps in the
                    // `/shutdown` handler and in `idle_watchdog`). A child left
                    // here is cleaned up by the cluster sweep (`X`) until the
                    // pull path is retired.
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
            // Do not orphan the sidecar, ours or an adopted one: an exiting
            // worker that leaves a model behind is 2.85 GB held by a box nobody
            // drives. Safe to wait for the lock — this branch is only reached
            // when no task is running.
            push.sidecar.lock().await.reap_all().await;
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
        // A new offer is the inductor, talking. Whatever the stash held is
        // now stale by definition — the task it described has been re-queued
        // and re-decided, so its completion must not land late and surprise
        // the ledger. (The hook had its whole silent window to deliver it.)
        p.pending = None;
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
/// **No scheduling calls home.** A worker never asks for work — the offer is
/// authoritative and everything scheduling needs arrived inside it. The one
/// exception is data, not scheduling: a merge pulls the take files it lacks
/// from the inductor (see `run_merge`), exactly as a render pushes its units
/// there. `fetch` carries the client and base URL for that pull; tests pass
/// `None`.
async fn run_offer(
    layout: &Layout,
    settings: &Settings,
    offer: &TaskOffer,
    shared: &Shared,
    sidecar: &mut Sidecar,
    fetch: Option<(&reqwest::Client, &str)>,
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
                unit_files: Vec::new(),
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
                unit_files: Vec::new(),
            })
        }
        Render => {
            // The memory guard, and this is the only place it can safely run:
            // between tasks. A model that has outgrown its budget is recycled
            // here, before the first `/infer` of this offer — never during one.
            // See `Sidecar::recycle_if_over_budget` for what it measures and
            // why it is not the idle reaper's job.
            sidecar.recycle_if_over_budget().await;
            sidecar.ensure(layout).await?;
            let (units, unit_files) = match render_action(offer.render_units.as_deref()) {
                // Old inductor: plan from the local script, keep files
                // locally, upload nothing — exactly as before the migration.
                RenderAction::Legacy => {
                    (
                        run_render(layout, n, &offer.engine, &sidecar.tts(), shared).await?,
                        Vec::new(),
                    )
                }
                // The offer names no units at all: nothing to speak.
                RenderAction::Noop => (0, Vec::new()),
                // The takes this offer carries, minus what this box already has.
                // Skipping here rather than on the inductor is the point: the
                // inductor cannot see this disk, and a partial offer is what
                // used to leave a box holding a strict subset of a chapter.
                //
                // `render_batch` takes of one chapter arrive per offer; the
                // worker already loops over a list, so batching changed nothing
                // here — only how often this arm is entered.
                RenderAction::Units => {
                    // Proven non-empty by the match above.
                    let list = offer.render_units.as_deref().unwrap_or(&[]);
                    let todo =
                        pending_units(list, &offer.render_force, &layout.seg_dir(&offer.engine, n));
                    if todo.is_empty() {
                        (0, Vec::new())
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
            // Count the takes against the process that spoke them, so the guard
            // has a second trigger that cannot be misread (see
            // `SIDECAR_MAX_RENDERS`). Counted after the fact and not before:
            // a batch that failed halfway still spent the model on the takes it
            // did speak, and `units` is exactly those.
            sidecar.note_renders(units);
            // **No per-task stop.** The sidecar is warmed on purpose: keeping
            // the ~2.85 GB model resident across render tasks is the whole
            // reason a per-take schedule is affordable, and stopping here would
            // reload it for every offer. It is reaped on idle by
            // `sidecar_reaper`, recycled by the budget guard above, and stopped
            // explicitly before a merge.
            Ok(TaskResult {
                ok: true,
                detail: format!("render ch{n} ({units} calls)"),
                delta: None,
                units,
                script: None,
                text: None,
                mp3_b64: None,
                unit_files,
            })
        }
        Merge => {
            // Merge needs no TTS, and its ffmpeg pass is the other large
            // working set on the box: drop the model first — including a
            // provision-started one this worker never spawned — so the two
            // never co-reside in 8 GiB.
            sidecar.reap_all().await;
            let path = run_merge(
                layout,
                n,
                &offer.engine,
                MergeJob {
                    gap_ms: offer.gap_ms,
                    speed: offer.speed,
                    on: bm_core::ambience::LayerSwitch::new(
                        offer.ambience,
                        offer.music,
                        offer.effect_volume,
                        offer.music_volume,
                        offer.inject_volume,
                    ),
                    takes: offer.merge_takes.clone(),
                },
                shared,
                fetch,
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
                unit_files: Vec::new(),
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
    unit_files: Vec<bm_proto::UnitFile>,
}

/// Say what the sidecar guard will do, once, when a worker starts.
///
/// The guard's numbers are a judgement until a box has measured them, and a
/// measurement needs a record of what was in force — otherwise a log full of
/// recycles says nothing about which budget produced them. One line, at startup,
/// in the same place the worker announces itself.
///
/// Resolved from the same [`Budget::from_env`] the guard uses, so the line
/// cannot describe a budget other than the one being applied.
fn announce_budget() {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    println!("{}", Budget::from_env().describe(sys.total_memory()));
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
    announce_budget();
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
            last_task_end: std::sync::atomic::AtomicU64::new(bm_proto::now_secs()),
            // Merge data-plane: the hook base *is* the inductor API through
            // the reverse tunnel. `no_proxy`, like every loopback client
            // here — an ambient HTTP_PROXY would answer instead of the tunnel.
            fetch_http: reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            fetch_base: format!("http://127.0.0.1:{}", bm_proto::DEFAULT_HOOK_PORT),
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
    // No inductor URL means no scheduling calls home, and that is the whole
    // guarantee: this worker never asks for work — it holds no address to
    // ask at. The one dial-out it keeps is data, not scheduling: a merge
    // pulls the take files it lacks through the reverse tunnel's hook base
    // (which only works while the inductor holds the tunnel open), exactly
    // as a render pushes its units there.
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
        // The warm sidecar is reaped when the box goes idle, so keeping it
        // across tasks never means holding 2.85 GB indefinitely.
        tokio::spawn(push::sidecar_reaper(push.clone()));
        // The completion hook: a loopback address that **is** the inductor's
        // control API — the reverse tunnel's far end (bm-inductor's tunnel
        // supervisor). This still dials nothing on its own: the address only
        // works while the inductor holds the tunnel open, and the sender
        // stays silent until the inductor has been silent. The token is the
        // cluster's own, already held for the instruction channel.
        let hook = hook::Hook::from_base(
            &format!("http://127.0.0.1:{}", bm_proto::DEFAULT_HOOK_PORT),
            &push.token,
        );
        tokio::spawn(hook::supervise(hook, push.clone(), shared.clone()));
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
        let res = match run_offer(&layout, &settings, &offer, &shared, &mut sidecar, Some((&http, inductor.as_str()))).await {
            Ok(r) => r,
            Err(e) => TaskResult {
                ok: false,
                detail: format!("{} ch{} failed: {e:#}", offer.stage, offer.chapter),
                delta: None,
                units: 0,
                script: None,
                text: None,
                mp3_b64: None,
                unit_files: Vec::new(),
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
            unit_files: res.unit_files,
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

/// What a hook post that goes nowhere means, said once. When the worker
/// stashes a completion and the tunnel is down (inductor dead, or older than
/// the tunnel), every pass would otherwise log the same failure with no
/// remedy attached — which is how a real problem becomes unreadable noise.
fn tunnel_missing_hint(task_id: &str, attempt: u64) {
    if attempt % 12 == 0 {
        println!(
            "hook: {task_id} still unreported — no tunnel answers on 127.0.0.1:{}; \
             the inductor's lease reaper will requeue it if this tunnel never comes back",
            bm_proto::DEFAULT_HOOK_PORT
        );
    }
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
                        MergeJob {
                            gap_ms: settings.gap_ms,
                            speed: settings.speed,
                            on: bm_core::ambience::LayerSwitch::new(
                                settings.ambience,
                                settings.music,
                                settings.effect_volume,
                                settings.music_volume,
                                settings.inject_volume,
                            ),
                            // A hand-driven merge: no offer, so no plan's take
                            // list. `assemble` names them itself, as this path
                            // always has.
                            takes: Vec::new(),
                        },
                        &shared,
                        None,
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
        let (cpu, mem, gb, sidecars, sidecar_gb) = probe.sample();
        assert_eq!((cpu, mem, gb), (None, None, None), "the first beat has no delta yet");
        // A census, not a delta: the count is real on the very first beat.
        let sidecars = sidecars.expect("the count is always reported");
        assert!(sidecar_gb.expect("rss always measures") >= 0.0);
        assert!(
            sidecars == 0 || sidecar_gb.unwrap() > 0.0,
            "a live sidecar holds memory"
        );
        std::thread::sleep(std::time::Duration::from_millis(300));
        let (cpu, mem, gb, _, _) = probe.sample();
        let cpu = cpu.expect("second sample measures");
        let mem = mem.expect("memory always measures");
        let gb = gb.expect("memory always measures");
        assert!((0.0..=100.0).contains(&cpu), "cpu pct: {cpu}");
        assert!((0.0..=100.0).contains(&mem), "mem pct: {mem}");
        assert!(gb >= 0.0, "mem gib: {gb}");
    }

    #[test]
    fn the_sidecar_census_asks_for_memory_and_never_for_tasks() {
        // The 8× regression, pinned where it cannot come back quietly. On
        // Linux sysinfo lists every *task* (thread) as a process in its own
        // right, each carrying its parent's whole RSS — so asking for tasks
        // turned one 2.4 GiB sidecar with an 8-thread ONNX pool into
        // `8 bm-tts processes … 19.5 GB resident` on an 11.6 GB box, and
        // handed `budget_verdict` a phantom 7.4–41 GB against a 5664 MiB cap.
        //
        // This is a property of the refresh kind, not of any box, so it is
        // assertable on macOS where the bug cannot reproduce: `thread_kind` is
        // `None` off Linux/Android, so the census reads 1 here either way.
        // Mutate it — drop the `without_tasks()` — and this fails.
        let kind = census_refresh_kind();
        assert!(
            !kind.tasks(),
            "the census must not enumerate tasks: sysinfo's `Default` sets \
             `tasks: true`, and on Linux that is one entry per thread, each \
             reporting the whole process's RSS"
        );
        assert!(
            kind.memory(),
            "the census is a memory reading; without `with_memory()` every \
             process reports 0 bytes and the guard's primary trigger is dead"
        );
    }

    #[tokio::test]
    async fn reaping_the_sidecar_reaches_one_we_did_not_spawn() {
        // The provision-started server is nobody's child, so `stop` — a signal
        // to `self.child` — cannot reach it. That is why the merge and shutdown
        // paths ask over HTTP, and the assertion is on the *ask*: a refused one
        // is survivable and reported, a missing one is the co-residency OOM.
        use std::sync::atomic::{AtomicU32, Ordering};
        let hits = std::sync::Arc::new(AtomicU32::new(0));
        let (counter, app) = (hits.clone(), {
            let hits = hits.clone();
            axum::Router::new()
                // Answering 503 is what a server mid-load does, and it is
                // enough for `reap_all` to take the "a server exists" branch.
                .route(
                    "/health",
                    axum::routing::get(|| async { axum::http::StatusCode::SERVICE_UNAVAILABLE }),
                )
                .route(
                    "/shutdown",
                    axum::routing::post(move || {
                        let hits = hits.clone();
                        async move {
                            hits.fetch_add(1, Ordering::SeqCst);
                            axum::Json(serde_json::json!({"ok": true}))
                        }
                    }),
                )
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let mut s = Sidecar::new(&format!("http://{addr}"));
        assert!(!s.is_running(), "nothing here is our child");
        // Cut the wait short: a fake server cannot stop listening, and the
        // point is the ask, not the port going quiet (a real one returns from
        // `main`, which drops the model *and* the listener).
        let _ = tokio::time::timeout(std::time::Duration::from_millis(700), s.reap_all()).await;
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "a server this worker did not spawn must still be asked to exit"
        );
    }

    // -----------------------------------------------------------------
    // The TTS sidecar memory guard
    // -----------------------------------------------------------------

    /// A box with 8 GiB and a sidecar at `rss_mib`, long enough lived that the
    /// cooldown is not what is being tested.
    const BOX_8GIB: u64 = 8 * 1024 * 1024 * 1024;

    /// The shipped thresholds, spelled out rather than resolved.
    ///
    /// `Budget::from_env()` would make every assertion below depend on the
    /// shell's environment — the trap the cold-start test already paid for once.
    /// A test that wants a different budget says so, which is also how the
    /// overrides themselves are covered.
    fn shipped() -> Budget {
        Budget::default()
    }

    #[test]
    fn a_freshly_loaded_model_is_nowhere_near_the_budget() {
        // The floor has to be far below the cap or the guard is a reload loop:
        // the model is ~2.85 GB the moment the weights load, which on an 8 GiB
        // box is ~36%. This pins that the baseline does not trip the guard.
        assert_eq!(
            budget_verdict(2_918 * 1_048_576, BOX_8GIB, 0, Some(10_000), &shipped()),
            None,
            "2.85 GB resident is the model doing its job, not a leak"
        );
    }

    #[test]
    fn a_sidecar_that_has_grown_past_half_the_box_is_recycled() {
        // The leak this exists for: RSS climbing across a long run of
        // inferences on a box that is never idle. 4 GiB is the budget on an
        // 8 GiB box, and the reason line has to name the numbers — "over
        // budget" with no figures is a guard an operator can only guess at.
        let why = budget_verdict(4 * 1024 * 1_048_576, BOX_8GIB, 0, Some(10_000), &shipped())
            .expect("at the budget is over the budget");
        assert!(why.contains("4096 MiB"), "{why}");
        assert!(
            why.contains("half this box's RAM"),
            "and says where the number came from: {why}"
        );

        // A bigger box gets a bigger budget — the point of a fraction. The same
        // 4 GiB sidecar is fine on 32 GiB.
        assert_eq!(
            budget_verdict(
                4 * 1024 * 1_048_576,
                32 * 1024 * 1024 * 1024,
                0,
                Some(10_000),
                &shipped()
            ),
            None,
            "the budget scales with the machine, so a big box is not reloading for nothing"
        );
    }

    #[test]
    fn the_render_count_fires_without_any_memory_reading() {
        // The second trigger exists for exactly this case: a platform that
        // reports no per-process memory would otherwise disable the guard
        // silently. `total_bytes: 0` is that platform.
        let why = budget_verdict(0, 0, SIDECAR_MAX_RENDERS, Some(10_000), &shipped())
            .expect("the count cannot be unavailable");
        assert!(why.contains("200 renders"), "{why}");
        assert_eq!(
            budget_verdict(0, 0, SIDECAR_MAX_RENDERS - 1, Some(10_000), &shipped()),
            None,
            "one short of the budget is under it"
        );
    }

    #[test]
    fn the_cooldown_bounds_what_a_wrong_budget_costs() {
        // The failure this prevents is worse than the leak: a model whose
        // baseline already exceeds the cap would recycle at every task
        // boundary, and the box would spend its life reloading 2.85 GB instead
        // of rendering. With the cooldown, a badly-tuned budget costs one
        // reload per interval.
        let over = 8 * 1024 * 1_048_576;
        assert_eq!(
            budget_verdict(over, BOX_8GIB, 0, Some(0), &shipped()),
            None,
            "just recycled — the next boundary must not recycle again"
        );
        assert_eq!(
            budget_verdict(
                over,
                BOX_8GIB,
                0,
                Some(SIDECAR_MIN_LIFETIME_SECS - 1),
                &shipped()
            ),
            None,
            "one second short of the floor"
        );
        assert!(
            budget_verdict(
                over,
                BOX_8GIB,
                0,
                Some(SIDECAR_MIN_LIFETIME_SECS),
                &shipped()
            )
            .is_some(),
            "and past the floor the guard acts"
        );
        // The cooldown must not hold a genuinely stuck count back for ever
        // either: it is a floor on the *interval*, not a veto.
        assert!(budget_verdict(0, 0, SIDECAR_MAX_RENDERS, None, &shipped()).is_some());
    }

    #[test]
    fn a_per_box_override_replaces_the_fraction_and_says_so() {
        // Why the override exists: the fraction is a judgement, and the only
        // honest way to replace it is to run a box with a known value and read
        // the log. This pins that a set value is used *as given* — an operator
        // testing 2048 MiB wants 2048, not 2048 scaled by the box.
        let tuned = Budget {
            rss_cap_mib: Some(2048.0),
            ..Budget::default()
        };
        assert_eq!(tuned.cap_mib(BOX_8GIB), Some(2048.0));
        assert_eq!(
            tuned.cap_mib(32 * 1024 * 1024 * 1024),
            Some(2048.0),
            "absolute means absolute, on any box"
        );
        let why = budget_verdict(2048 * 1_048_576, BOX_8GIB, 0, Some(10_000), &tuned)
            .expect("2048 MiB is at the 2048 MiB cap");
        assert!(why.contains("BM_TTS_MAX_RSS_MB"), "names the knob: {why}");

        // The count and the cooldown move with the same override.
        let strict = Budget {
            max_renders: 5,
            min_lifetime_secs: 1,
            ..Budget::default()
        };
        assert!(budget_verdict(0, 0, 5, Some(1), &strict).is_some());
        assert_eq!(
            budget_verdict(0, 0, 4, Some(1), &strict),
            None,
            "four renders is under a budget of five"
        );

        // And the shipped default is untouched by any of that.
        assert_eq!(shipped().cap_mib(BOX_8GIB), Some(4096.0));
        assert_eq!(shipped().max_renders, SIDECAR_MAX_RENDERS);
        assert_eq!(shipped().min_lifetime_secs, SIDECAR_MIN_LIFETIME_SECS);
    }

    #[test]
    fn the_startup_line_names_the_budget_that_will_be_applied() {
        // A log full of recycles says nothing unless it also says which budget
        // produced them — that is the whole reason the line exists, and the
        // reason it is resolved from the same `from_env` the guard uses.
        let default = shipped().describe(BOX_8GIB);
        assert!(default.contains("4096 MiB"), "{default}");
        assert!(default.contains("50% of this box"), "{default}");
        assert!(default.contains("200 renders"), "{default}");
        assert!(default.contains("300s"), "{default}");

        let tuned = Budget {
            rss_cap_mib: Some(1536.0),
            ..Budget::default()
        }
        .describe(BOX_8GIB);
        assert!(tuned.contains("1536 MiB"), "{tuned}");
        assert!(tuned.contains("BM_TTS_MAX_RSS_MB"), "{tuned}");

        // A box that reports no memory has no denominator, so the line says the
        // RSS trigger is off rather than implying a cap of zero.
        let unknown = shipped().describe(0);
        assert!(unknown.contains("no RSS cap"), "{unknown}");
    }

    #[test]
    fn renders_are_counted_against_the_process_that_spoke_them() {
        // `served` describes a model, not a worker: a spawn or a reap resets it,
        // or the count would eventually trip the guard on a brand-new process.
        let mut s = Sidecar::new("http://127.0.0.1:8818");
        assert_eq!(s.served, 0);
        s.note_renders(0);
        assert_eq!(s.served, 0, "an offer that spoke nothing is not work");
        assert!(s.serving_since.is_none(), "and does not start the clock");
        s.note_renders(10);
        assert_eq!(s.served, 10);
        assert!(
            s.serving_since.is_some(),
            "the clock starts at the first render"
        );
        s.note_renders(5);
        assert_eq!(s.served, 15, "a batch adds its takes, not one per offer");

        s.forget_work();
        assert_eq!((s.served, s.serving_since), (0, None));
        s.stop();
        assert_eq!(
            s.served, 0,
            "stop is a reap: the count goes with the process"
        );
    }

    #[tokio::test]
    async fn a_sidecar_under_budget_is_left_alone() {
        // The other half of the guard: it must not recycle a healthy model.
        // Nothing is listening on this port, so the census finds nothing and
        // the verdict is `None` — the same answer a well-behaved model gets.
        let mut s = Sidecar::new("http://127.0.0.1:1");
        s.note_renders(3);
        s.serving_since = Some(0);
        assert_eq!(s.over_budget(), None);
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
            name: "t-0123456789abcdef.wav".into(),
            speaker: "A".into(),
            voice: "Adam".into(),
            text: "hi".into(),
            temperature: 0.8,
            silence_p: 0.15,
            take_key: "0123456789abcdef".into(),
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
            take_key: String::new(),
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

    #[tokio::test]
    async fn a_batched_offer_renders_every_take_it_carries() {
        // The worker half of batching, end to end. `render_offered_units` was
        // already written for a list, so the claim "no worker change was
        // needed" has to be *shown*, not asserted: a regression that spoke the
        // first take and reported one would look exactly like the single-take
        // era, and nothing else in the suite would notice.
        //
        // The sidecar is faked down to the two questions `ensure` asks
        // (`/health` and a `/policy` carrying `allowed_voices`), so this never
        // spawns a model — and `/infer` counts the calls, because the count is
        // the assertion.
        use bm_proto::RenderUnitSpec;
        use std::sync::atomic::{AtomicU32, Ordering};
        let root = std::env::temp_dir().join(format!("bmbatch{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let layout = Layout::new(&root);
        std::fs::create_dir_all(layout.data()).unwrap();

        let calls = std::sync::Arc::new(AtomicU32::new(0));
        let app = {
            let calls = calls.clone();
            axum::Router::new()
                .route("/health", axum::routing::get(|| async { "ok" }))
                .route(
                    "/policy",
                    axum::routing::get(|| async {
                        axum::Json(serde_json::json!({"allowed_voices": []}))
                    }),
                )
                .route(
                    "/infer",
                    axum::routing::post(move || {
                        let calls = calls.clone();
                        async move {
                            calls.fetch_add(1, Ordering::SeqCst);
                            // `Tts::infer` refuses a body under 1000 bytes, so
                            // a real-sized one is what "spoke it" means here.
                            vec![0u8; 4096]
                        }
                    }),
                )
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        // Ten takes of one chapter, exactly what `Settings::render_batch`
        // defaults to. Content-addressed names, as the inductor plans them.
        let units: Vec<RenderUnitSpec> = (0..10)
            .map(|i| RenderUnitSpec {
                tag: format!("000{i}"),
                name: format!("t-{i:016}.wav"),
                speaker: "A".into(),
                voice: "Adam".into(),
                text: format!("line {i}"),
                temperature: 0.8,
                silence_p: 0.15,
                take_key: format!("{i:016}"),
            })
            .collect();

        let offer = TaskOffer {
            task_id: "render:7:0".into(),
            chapter: 7,
            stage: bm_proto::Stage::Render,
            root: root.display().to_string(),
            url: None,
            tts_url: Some(format!("http://{addr}")),
            engine: "vieneu".into(),
            model_order: vec![],
            analyzer: "local".into(),
            analyzer_settings: bm_proto::AnalyzerSettings::default(),
            credentials: bm_proto::Credentials::default(),
            bible: None,
            script: None,
            cast: None,
            text: None,
            gap_ms: 300,
            speed: 1.0,
            ambience: false,
            music: false,
            effect_volume: 1.0,
            music_volume: 1.0,
            inject_volume: 1.0,
            render_units: Some(units.clone()),
            render_force: vec![],
            cast_hash: "cast".into(),
            merge_takes: vec![],
            local_node: false,
        };
        let shared: Shared = Arc::new(Mutex::new(Progress::default()));
        let mut sidecar = Sidecar::new(&format!("http://{addr}"));

        let res = run_offer(&layout, &Settings::default(), &offer, &shared, &mut sidecar, None)
            .await
            .expect("a batch renders");
        assert!(res.ok);
        assert_eq!(res.units, 10, "ten takes spoken, ten reported");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            10,
            "ten requests to the sidecar"
        );
        assert_eq!(
            sidecar.served, 10,
            "and all ten counted against the model, which is what the guard reads"
        );
        let seg = layout.seg_dir("vieneu", 7);
        for u in &units {
            let p = seg.join(&u.name);
            let len = p.metadata().map(|m| m.len()).unwrap_or(0);
            assert!(len >= 1000, "{} landed ({len} bytes)", u.name);
        }

        // The second half of the contract: a take this box already holds is
        // skipped, so a retry after a partial batch re-speaks only the gap.
        std::fs::remove_file(seg.join(&units[4].name)).unwrap();
        calls.store(0, Ordering::SeqCst);
        let res = run_offer(&layout, &Settings::default(), &offer, &shared, &mut sidecar, None)
            .await
            .expect("the retry renders");
        assert_eq!(res.units, 1, "only the missing take is re-spoken");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(seg.join(&units[4].name).is_file(), "and it landed");
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
            cast_hash: String::new(),
            merge_takes: vec![],
            local_node: false,
        };
        let shared: Shared = Arc::new(Mutex::new(Progress::default()));
        let mut sidecar = Sidecar::new("http://127.0.0.1:8818");

        // The digest itself is *expected* to fail — the fixture answers `{}`,
        // which is not a valid digest, so the one repair attempt fails too.
        // What is under test is where the request went and what it asked for.
        let _ = run_offer(&layout, &box_settings, &offer, &shared, &mut sidecar, None).await;

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

/// The guard's foundation, checked against a **real** sidecar process.
///
/// `budget_verdict`'s branches are pure and covered by the tests above, but
/// neither of them can catch the two ways this guard could be dead on arrival:
/// a process-name match that never matches, and a platform that reports zero
/// per-process memory. Either would make the whole guard silently inert, and an
/// inert guard looks exactly like a guard that was never needed — the failure
/// would surface as an OOM, months later, with nothing pointing here.
///
/// Ignored by default because it starts the real binary and loads ~0.9–2.9 GB
/// (measured: 887 MiB three seconds in, mid-load, on macOS/arm64 — the same
/// understated-by-~2.8× platform the sizing note warns about). Run it
/// deliberately, on a box you want to check:
///
/// ```text
/// cargo test -p bm-agent --bin bm-agent census_probe:: -- --ignored --nocapture
/// ```
///
/// The filter must be the **module path**, `census_probe::`. An earlier version of this
/// comment said `census_against_a_real`, which matches no test name at all — and a filter
/// that matches nothing still exits 0 with `0 passed; 0 ignored; 30 filtered out`, so a
/// check nobody ran read exactly like a check that passed.
///
/// **Run it on a Linux box.** The `count == 1` assertion below is the only
/// place the 8× task-multiplication can be *observed* rather than argued
/// about: macOS enumerates no tasks, so `sidecar_processes` reads 1 here
/// whatever the refresh kind says. That is also why the regression shipped —
/// the census was only ever exercised on the inductor's own Mac.
#[cfg(test)]
mod census_probe {
    use super::*;

    /// Kill the child however this test leaves — including on a panic. A model
    /// left behind is 2.85 GB held by a box nobody is driving.
    struct Kill(std::process::Child);
    impl Drop for Kill {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// The repo root, derived rather than hardcoded: `rust/crates/bm-agent`.
    fn repo_root() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .expect("rust/crates/bm-agent is three levels below the root")
            .to_path_buf()
    }

    #[test]
    #[ignore = "starts the real bm-tts and loads the model — run deliberately, see the doc comment"]
    fn the_census_finds_a_real_bm_tts_and_reads_its_rss() {
        let layout = Layout::new(&repo_root());
        let (bin, args) = layout.sidecar_command(SIDECAR_PORT);
        // Fail loudly rather than skipping: a check that quietly does nothing
        // when it cannot run is worse than no check.
        assert!(
            bin.is_file(),
            "no sidecar at {} — build it first (`make build`)",
            bin.display()
        );

        let child = std::process::Command::new(&bin)
            .args(&args)
            .env("LD_LIBRARY_PATH", layout.tts_lib_dir())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawning bm-tts");
        let _kill = Kill(child);

        // Sample while it loads: the process exists immediately and its RSS
        // climbs, so the first non-zero reading is the answer and there is no
        // need to wait for `/health`.
        let mut sys = sysinfo::System::new();
        let mut seen = (0u32, 0u64);
        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_secs(3));
            seen = sidecar_processes(&mut sys);
            println!(
                "census: count={} rss={:.0} MiB",
                seen.0,
                seen.1 as f64 / 1_048_576.0
            );
            if seen.0 > 0 && seen.1 > 0 {
                break;
            }
        }
        assert_eq!(
            seen.0, 1,
            "the census must find exactly one bm-tts — a name match that never matches \
             would make the whole guard inert"
        );
        assert!(
            seen.1 > 0,
            "the census must report a non-zero RSS — a zero would leave the guard's \
             primary trigger dead on this platform"
        );
    }
}
