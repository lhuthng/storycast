//! Worker agent: pull one pipeline stage at a time, run it, report back.
//!
//! Two modes:
//! - `run`   , a single stage for one chapter, standalone (no inductor).
//! - `worker`, register with the inductor and pull tasks until stopped.
//!
//! The agent never loads a model and never writes the authoritative bible.
//! TTS goes through the sidecar over HTTP; the agent owns the sidecar's
//! lifecycle, which is what keeps sidecar RSS bounded no matter how many
//! chapters flow through: it is kept warm *across* tasks (a per-task stop would
//! reload ~2.85 GB for every offer), reaped after an idle interval, dropped
//! before a merge's ffmpeg pass, and **recycled at a task boundary once it has
//! outgrown its memory budget**, because a box that renders continuously is
//! never idle, so idleness alone is not a memory guard.

mod hook;
mod push;
mod tts;

use anyhow::{Context, Result};
use bm_core::{config::Settings, Layout};
use bm_proto::{Complete, Heartbeat, Register, TaskOffer};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tts::{Health, Tts};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const SIDECAR_PORT: u16 = 8818;

/// Sidecar startup budget: how long `/health` is polled before giving up.
/// The load is minutes on a slow box, so this is generous on purpose, and it
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
    /// inductor address at all, so it cannot dial one, the inductor does all
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
        /// in `.bm/`, see `bm_core::token`. Required in serve-only mode,
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
    /// Take delivery of the model artifact from a release, on the box.
    ///
    /// The delivery half of `tools/models.sh publish`: download beside the
    /// destination, verify every file against the manifest that travelled in
    /// the same archive, and swap the tree into place. The exit code is the
    /// contract the provisioner reads, and the split is the point:
    ///
    /// - `0` landed and verified
    /// - `20` bytes arrived and are **not** the ones asked for — stop, never
    ///   fall back to pushing over them
    /// - `21` the artifact was not there — the caller falls back to the rsync
    ///
    /// So a GitHub incident cannot stop a cluster from provisioning, and a
    /// corrupt artifact cannot be papered over by pushing it again.
    FetchArtifact {
        /// Release download URL of a `models.tar.zst`.
        url: String,
        /// The models directory itself, e.g. `~/bm-worker/engines/vieneu/models`.
        dest: PathBuf,
        /// The manifest hash the inductor read, which this must agree with.
        /// Optional: without it the bundle is accepted on its own manifest,
        /// which proves the archive is self-consistent and nothing more.
        #[arg(long)]
        expect: Option<String>,
        /// Keep only the members under this directory, and land *it* at
        /// `dest` — for a bundle that is a subtree beside its manifest.
        ///
        /// A profile pack is published as `assets/…` with its `manifest.json`
        /// beside it, because that is the path system a pack is keyed by and the
        /// hash of the live tree folds over. The weights have no such prefix,
        /// so this is the only difference between the two deliveries, and it is
        /// a flag rather than a second subcommand: the part that has to be
        /// right — verify against the manifest that travelled in the archive,
        /// then swap in two renames — stays one implementation.
        #[arg(long, value_name = "DIR")]
        strip_prefix: Option<String>,
    },
}

/// One entry of the segment inventory: which chapter, which engine, which
/// file, how big, and what it hashes to. Shape lives in `bm_core::segments`
/// so the inductor's diff can never disagree about it; the walk is one call.
fn segment_manifest(audio_dir: &std::path::Path) -> Vec<bm_core::segments::SegmentEntry> {
    bm_core::segments::manifest(audio_dir)
}

/// What the worker is doing right now, the heartbeat source of truth.
#[derive(Debug, Clone, Default)]
struct Progress {
    task_id: Option<String>,
    stage: Option<String>,
    chapter: Option<u32>,
    frac: f32,
    activity: String,
    /// A stage that finished but was never acknowledged, the completion
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

/// Exit codes [`Cmd::FetchArtifact`] means by them, named once so the
/// provisioner's `match` and this function cannot drift apart.
///
/// **Not 2 and 3**, and that is not an aesthetic choice: `clap` exits **2** on
/// a usage error, and the subcommand it does not recognise is exactly what an
/// agent predating this one answers. Reading that as "the bytes are corrupt"
/// would stop a provision that should have fallen back to the push, and say the
/// one thing that is not true. So the codes sit above every exit the toolchain
/// produces on its own, and anything unrecognised is treated as absence.
mod fetch_exit {
    pub const LANDED: i32 = 0;
    pub const CORRUPT: i32 = 20;
    pub const UNREACHABLE: i32 = 21;
}

/// Take delivery of the model artifact, and say what happened on one line.
///
/// Progress goes to stderr, throttled, because a 363 MB body on a box with a
/// 200 KB/s link is minutes of silence otherwise — and the provisioner streams
/// it, so the operator watching `:prov` sees the download instead of a stall.
/// `Content-Length` is used when the host sends one and nothing is invented
/// when it does not: a progress line that guesses its denominator is worse than
/// one that admits it has none.
fn fetch_artifact(url: &str, dest: &Path, expect: Option<&str>, strip_prefix: Option<&str>) -> i32 {
    use std::time::Instant;
    let started = Instant::now();
    let mut last = Instant::now();
    // With a prefix the artifact is a pack, and the models tag rule would name
    // it `models-v…` — a name that means nothing here and would be the one
    // wrong line in the whole feature. So it says what it is being asked for.
    let tag = match strip_prefix {
        Some(dir) => Some(dir.to_string()),
        None => expect.map(bm_core::artifact::tag_for),
    };
    let mut say = |done: u64, total: Option<u64>| {
        if last.elapsed() < std::time::Duration::from_secs(2) {
            return;
        }
        last = Instant::now();
        let mb = done as f64 / (1024.0 * 1024.0);
        let speed = mb / started.elapsed().as_secs_f64().max(0.001);
        match total {
            Some(t) => eprintln!(
                "[fetch] {} {mb:.1}/{:.1} MiB ({:.0}%) at {speed:.1} MiB/s",
                tag.as_deref().unwrap_or("artifact"),
                t as f64 / (1024.0 * 1024.0),
                done as f64 / t as f64 * 100.0
            ),
            None => eprintln!(
                "[fetch] {} {mb:.1} MiB at {speed:.1} MiB/s",
                tag.as_deref().unwrap_or("artifact")
            ),
        }
    };
    // A pack is only ever fetched with an expectation — the hash of the live
    // tree is the whole reason the release can be trusted to replace it — so an
    // unprefixed destination with no hash keeps the older, weaker contract
    // rather than inventing a second one.
    let r = match (strip_prefix, expect) {
        (Some(_), Some(hash)) => bm_core::artifact::fetch_pack(url, dest, hash, "", &mut say),
        (Some(dir), None) => {
            eprintln!("FETCH-CORRUPT (--strip-prefix {dir} needs --expect: a pack release is only accepted against the profile hash it is replacing)");
            return fetch_exit::CORRUPT;
        }
        // No expectation from the caller: land it, then report the tag the
        // bundle's own manifest names, which is the only claim available.
        (None, Some(hash)) => bm_core::artifact::fetch(url, dest, hash, &mut say),
        (None, None) => bm_core::artifact::fetch_unpinned(url, dest, &mut say),
    };
    match r {
        Ok(landed) => {
            // The tag is the *models* bundle's identity and this box was not
            // told the pack's — the provisioner is, and it names the release in
            // its own line. An empty tag therefore prints nothing rather than a
            // trailing comma and a blank.
            let tag = match landed.tag.is_empty() {
                true => String::new(),
                false => format!(", {}", landed.tag),
            };
            println!(
                "FETCH-OK ({} files, {:.0} MiB{})",
                landed.files,
                landed.bytes as f64 / (1024.0 * 1024.0),
                tag
            );
            fetch_exit::LANDED
        }
        Err(bm_core::artifact::FetchError::Unreachable(m)) => {
            eprintln!("FETCH-UNREACHABLE ({m})");
            fetch_exit::UNREACHABLE
        }
        Err(bm_core::artifact::FetchError::Corrupt(m)) => {
            eprintln!("FETCH-CORRUPT ({m})");
            fetch_exit::CORRUPT
        }
    }
}

/// The share of this box's RAM the TTS sidecar may hold before it is recycled.
/// The loaded model is ~2.85 GB (about 36% of an 8 GiB box), so this is a
/// growth budget, not a size limit. A fraction of the box rather than a fixed
/// cap: too low on a big box reloads the model for nothing, too high on a
/// small box never fires before the OOM killer does.
const SIDECAR_RSS_FRACTION: f64 = 0.5;

/// Renders one sidecar process may serve before it is recycled regardless of
/// its measured footprint. The RSS reading is a sample that can miss a slow
/// climb and reads zero on platforms without per-process memory; a count
/// cannot be unavailable. The coarse backstop under the fine one.
const SIDECAR_MAX_RENDERS: u64 = 200;

/// The shortest life a sidecar may have before the guard will recycle it
/// again. Bounds the cost of a badly-tuned budget (a baseline already over
/// the cap) to one reload per interval instead of one per task boundary.
const SIDECAR_MIN_LIFETIME_SECS: u64 = 300;

/// The guard's thresholds, resolved **once per process**.
///
/// Environment overrides instead of a `Settings` field: a provisioned worker
/// has no `settings.json` at all, and this is a property of the box (its RAM),
/// so it belongs to the box's environment. A worker logs the resolved budget
/// once at startup; unset means the shipped default.
#[derive(Debug, Clone, Copy)]
struct Budget {
    /// An absolute cap in MiB, or `None` to use [`SIDECAR_RSS_FRACTION`] of
    /// the box's RAM. Absolute when set: an operator testing a value wants
    /// that value, not that value scaled by whatever the box turns out to be.
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
    /// The compiled defaults with any per-box override applied, read from the
    /// environment once and cached: a half-written environment must not be
    /// able to change the guard's thresholds mid-run.
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

    /// The cap for a box with `total_bytes` of RAM, in MiB, `None` when the
    /// box did not report its memory and no absolute cap was given. An unknown
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

/// A render refused because the operator's policy turns render off for this
/// box. A type so the serve path can answer it 403 (strike-free release on
/// the inductor's side) while a genuine failure answers 200 `ok: false` (a
/// strike). Hand-rolled `Display`/`Error`: one two-line type wants no
/// dependency.
#[derive(Debug)]
struct PolicyRefusal(&'static str);

impl std::fmt::Display for PolicyRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for PolicyRefusal {}

struct Sidecar {
    tts_url: String,
    child: Option<tokio::process::Child>,
    /// Renders this worker has spoken through the process **currently on the
    /// port**. Reset whenever a process is started or reaped, so it counts the
    /// model's work rather than the worker's lifetime, which is the question
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
    /// The `threads` value the model on the port was last *asked* to open with,
    /// or `None` for the sidecar's own default. An edit in the TUI changes
    /// this and `ensure` recycles a server that no longer matches, so the edit
    /// lands at the next render instead of waiting for the model to be reaped
    /// for some other reason.
    applied_threads: Option<u32>,
}

/// The memory guard's decision, as a pure function of what was measured.
///
/// Split from [`Sidecar::over_budget`] so every branch is testable without a
/// 2.85 GB model in the way, and taking the [`Budget`] as an argument so a
/// test can pin a chosen threshold. Order matters:
///
/// 1. the cooldown, so a bad budget costs one reload per interval, not one
///    per boundary;
/// 2. the render count, deliberately not gated on the memory reading: a count
///    cannot be unavailable, and gating it is how the whole guard goes
///    quietly dead on a platform that reports no per-process memory;
/// 3. the resident set, the direct guard that catches growth inside a batch.
///
/// The reason is returned as text so the log names the figure that fired.
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
            applied_threads: None,
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
    /// a process goes away, a spawn, a stop, a reap, so `served` never
    /// describes a model that is no longer there.
    fn forget_work(&mut self) {
        self.served = 0;
        self.serving_since = None;
    }

    /// Why the sidecar on this port should be recycled before the next render,
    /// or `None` to leave it alone.
    ///
    /// Measurement only, the decision is [`budget_verdict`], which is pure so
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
    /// The rule that keeps the box alive: a server that exists but is still
    /// loading is *waited for*, never raced. `bm-tts` binds its port before
    /// the load, so a bound port answering 503 means "starting", and spawning
    /// here would put two models in RAM on an 8 GiB box.
    async fn ensure(&mut self, layout: &Layout, desired_threads: Option<u32>) -> Result<()> {
        if self.serving_current().await {
            if self.applied_threads == desired_threads {
                return Ok(());
            }
            // The count changed (or a value was pushed where the box had
            // none): recycle so the next render opens the model with it. An
            // adopted server is `applied_threads: None`, so a pushed value
            // always disagrees and a provision-started model is replaced too.
            // This is the one place an adopted server is dropped for a reason
            // other than memory; it is still the operator's own instruction.
            self.reap_all().await;
        }
        // Not ready, but two different things can be in the way, and they want
        // opposite treatment.
        //
        // **Starting:** a previous attempt (ours, provisioning's detached one,
        // another task's) is still loading. Wait for it, never race it; starting
        // a second model here is the OOM on an 8 GiB box.
        //
        // **Up but wrong:** `/health` answers and `/policy` does not, which is a
        // server from an older deploy. It has to go, and it is nobody's child
        // `stop` only signals our own, so it is asked over `/shutdown`. Without
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
        let resolved = desired_threads
            .map(|n| n as usize)
            .unwrap_or_else(bm_core::config::tts_threads);
        let (bin, args) = layout.sidecar_command(self.port(), resolved);
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
                // A fresh process opened with this count: record it so a later
                // `ensure` with the same value does not recycle it again.
                self.applied_threads = desired_threads;
                // A fresh process: the renders counted against the previous one
                // are not its work and must not count towards its budget.
                self.forget_work();
                return Ok(());
            }
            if child.try_wait()?.is_some() {
                // Our child exited. If an instance that is *starting* holds the
                // port, it won the bind while we were loading, wait for it
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
    /// away, the idle reaper, `ensure` finding nothing on the port, and
    /// `reap_all`, so clearing `served` here is what keeps the count attached
    /// to a *process* rather than to this worker's lifetime.
    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
        }
        self.forget_work();
    }

    /// Stop **every** sidecar on this box: the child we spawned, and a server
    /// somebody else started (provisioning's detached `nohup … &`, or the
    /// inductor's audition server) which is asked to exit over HTTP because
    /// it is not ours to signal.
    ///
    /// Both callers share one reason: the model must actually be gone before
    /// the next thing that needs the memory runs (a merge's ffmpeg pass, or
    /// the memory guard). Both wait for the port to go quiet: "asked" is not
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

    /// Recycle the model if it has outgrown its budget, **the memory guard**.
    ///
    /// The sidecar's working set grows across a long run of inferences and
    /// nothing returns it to the OS, and a busy box is never idle, so the
    /// idle reaper never fires. Safe on two counts: it runs between tasks
    /// (the caller invokes it before the first `/infer`; recycling mid-render
    /// would lose a take, and TTS is stochastic), and it is bounded by the
    /// [`SIDECAR_MIN_LIFETIME_SECS`] cooldown.
    ///
    /// The action is `reap_all`, not `stop`: the one case where the worker
    /// overrides its own "an adopted server is somebody else's to keep" rule,
    /// because a box about to be killed by its own memory cannot leave the
    /// decision to whoever started the model.
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

/// Crawl one chapter, through whichever provider the offer named.
///
/// The stage itself is now thin on purpose: *how* a chapter is obtained is a
/// script's business (or the built-in fetcher's), and the only things this
/// function owns are the ones that are the same for every provider, the
/// progress line, the artifact write, and the verdict that travels back in the
/// completion.
///
/// **On a blocking thread.** The provider uses a blocking HTTP client and runs
/// an interpreter, so it cannot live inside the async task: `spawn_blocking`
/// keeps the heartbeat alive and keeps a nested runtime out of a tokio worker.
async fn run_crawl(
    layout: &Layout,
    n: u32,
    spec: bm_proto::CrawlSpec,
    url: Option<String>,
    attempt: u32,
    shared: &Shared,
) -> Result<(u64, Option<String>, bm_proto::CrawlReport)> {
    let how = if spec.engine.is_empty() {
        "built-in".to_string()
    } else {
        format!("{} {}", spec.engine, spec.script)
    };
    set_progress(shared, 0.05, format!("crawl ch{n} ({how})"));
    let crawled = tokio::task::spawn_blocking(move || {
        bm_core::crawl::Provider::new(&spec).crawl(n, url.as_deref(), attempt)
    })
    .await
    .context("the crawl thread panicked")??;

    let report = bm_core::crawl::report_of(&crawled);
    for line in &crawled.log {
        println!("crawl ch{n}: {line}");
    }
    let text = match &crawled.outcome {
        bm_core::crawl::CrawlOutcome::Text { text, .. } => {
            bm_core::atomic_write(&layout.chapter_txt(n), text)?;
            set_progress(
                shared,
                1.0,
                format!("{how} crawled ch{n} ({} bytes)", text.len()),
            );
            Some(text.clone())
        }
        // Nothing to write: the site has no such chapter. A terminal
        // non-failure, so no strike and no artifact.
        bm_core::crawl::CrawlOutcome::Absent { reason } => {
            set_progress(shared, 1.0, format!("ch{n} is not on the site: {reason}"));
            None
        }
        bm_core::crawl::CrawlOutcome::Blocked(b) => {
            set_progress(shared, 1.0, format!("crawl ch{n} blocked: {}", b.detail));
            None
        }
    };
    Ok((1, text, report))
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
        // Analyze only, never persist: the inductor is the single writer of
        // the script and the bible. A worker-mode write would land on the
        // shared disk underneath the winner, a lost digest race's orphaned
        // thread finishing late would overwrite the applied script with its
        // own uncanonicalized version. The script travels home in the report.
        let outcome = rt.block_on(bm_core::digest::analyze_chapter(
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
        // Standalone mode keeps legacy behaviour: persist the script and
        // merge here. Worker mode returns the delta and the inductor merges
        // as the single writer.
        bm_core::digest::write_script(&layout, n, &outcome.script)?;
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
    Ok((outcome.delta, outcome.script))
}

/// What a render offer asks for. Pure, so the zero-units arm, "do nothing
/// and report success", the easiest arm to write as a fall-through, is
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
    /// The offer names no units at all, an empty chapter. Report `ok` with
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
/// Forced names are the ones the inductor's own store lacks, the surgical
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
        // The storage tier rides the offer: an `.mp3` name with a bitrate is
        // stored encoded, everything else is the sidecar's wav under whatever
        // name it was given. The encode is this box's job because the
        // sidecar speaks wav, and the name is checked rather than trusted —
        // a `.mp3` name holding wav bytes would be a lie every reader after
        // the store pays for.
        let bytes: std::borrow::Cow<[u8]> = if u.name.ends_with(".mp3") && u.mp3_kbps > 0 {
            let kbps = u.mp3_kbps;
            let src = std::env::temp_dir()
                .join(format!("bm-encode-{}-{}", std::process::id(), u.take_key))
                .with_extension("wav");
            let dst = src.with_extension("mp3");
            std::fs::write(&src, &wav)?;
            let (src_in, dst_in) = (src.clone(), dst.clone());
            tokio::task::spawn_blocking(move || {
                bm_core::assemble::encode_mp3(&src_in, &dst_in, kbps)
            })
            .await
            .map_err(|e| anyhow::anyhow!("encode task failed: {e}"))?
            .with_context(|| format!("encoding {} at {kbps}k", u.name))?;
            let _ = std::fs::remove_file(&src);
            let encoded = std::fs::read(&dst)?;
            let _ = std::fs::remove_file(&dst);
            std::borrow::Cow::Owned(encoded)
        } else {
            std::borrow::Cow::Borrowed(&wav[..])
        };
        std::fs::write(seg_dir.join(&u.name), &bytes)?;
        files.push(bm_proto::UnitFile {
            name: u.name.clone(),
            b64: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes),
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
    let policy = bm_core::cast::policy_for_bible(engine, layout);
    let installed = bm_core::pool::installed_voices(layout);
    let cast = bm_core::cast::load_cast(
        &script_path,
        &cast_path,
        &layout.bible(),
        &policy,
        installed.as_ref(),
        true,
    )?;
    let local = engine == "vieneu";
    let planned = bm_core::assemble::Planned::plan(&segments);
    let first = planned
        .speech
        .first()
        .map(|s| s.get("text").and_then(|t| t.as_str()).unwrap_or(""))
        .unwrap_or("");
    let title = bm_core::assemble::title_speech(layout, n, &cast, first);
    let units = bm_core::assemble::plan_render(&planned, &cast, &seg_dir, local, title.as_ref())?;
    // A take's file is content-addressed, and the chapter's plan is what names
    // it, so this path owns the plan the way the inductor does when it offers:
    // build it, reconcile it against the stored one — a first plan adopts an
    // existing legacy cache, a re-plan trusts the files it recorded — and write
    // it back. Rendering then covers exactly the takes the diff calls dirty, and
    // the mixer reads the names this wrote, so a hand-driven render and a
    // hand-driven merge agree without either recomputing a name.
    std::fs::create_dir_all(&seg_dir)?;
    let plan_path = layout.plan(n);
    let stored = bm_core::assemble::RenderPlan::load(&plan_path);
    // The same setting the inductor plans from, read here because this path
    // owns the plan on this box: the tier decides the extension the take
    // names carry.
    let quality =
        bm_core::assemble::TakeQuality::parse(&Settings::load(&layout.settings()).take_quality);
    let up = bm_core::assemble::reconcile(
        stored.as_ref(),
        bm_core::assemble::RenderPlan::build(n, engine, &units, quality),
        &seg_dir,
    );
    up.plan.save(&plan_path)?;
    // Unit and plan entry are the same position: `build` maps them in order.
    let todo: Vec<_> = up
        .dirty
        .iter()
        .filter_map(|i| {
            let unit = units.get(*i)?;
            let file = up.plan.takes.get(*i)?.file.clone();
            Some((unit, file))
        })
        .collect();
    let total = todo.len();
    // No accent gate: any voice the sidecar can synthesize is allowed. If the
    // engine itself rejects a voice, that failure surfaces from /infer.
    // The render audit log belongs with the rest of the state, not in
    // `output/`. `output/` holds deliverables and nothing else, a machine
    // -readable record of TTS calls sitting beside the mp3s is a stray
    // intermediate in the one directory an operator actually looks at.
    let manifest = layout.bm_state().join("render-manifest.jsonl");
    for (i, (u, dest)) in todo.iter().enumerate() {
        set_progress(
            shared,
            i as f32 / total.max(1) as f32,
            format!("render ch{n} {} ({}/{})", u.tag, i + 1, total),
        );
        let wav = tts
            .infer(&u.text, &u.voice, u.temperature, u.silence_p, engine)
            .await?;
        std::fs::write(seg_dir.join(dest), &wav)?;
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
    /// The chapter's take files in mix order. Empty means "no plan", this box
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
    // it lacks from the inductor, whose store holds every completed take
    // instead of requiring them on local disk. Offer-driven only: the
    // hand-driven path carries no take list and mixes what is here.
    if !takes.is_empty() {
        if let Some((http, inductor)) = fetch {
            let seg_dir = layout.seg_dir(engine, n);
            std::fs::create_dir_all(&seg_dir)?;
            for name in &takes {
                let dest = seg_dir.join(name);
                if dest.metadata().map(|m| m.len() > 1000).unwrap_or(false) {
                    continue;
                }
                let url = format!("{inductor}/api/segment?chapter={n}&engine={engine}&name={name}");
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
                std::fs::write(&dest, &bytes).with_context(|| format!("storing segment {name}"))?;
            }
        }
    } // Everything the merge writes goes into one per-chapter scratch directory
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
            &layout,
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
/// ~2s average, the cadence heartbeats already run at, no extra timer.
///
/// The same sample counts the TTS sidecars, because that is the quantity that
/// actually kills these boxes: one model is ~2.85 GB and an 8 GiB box cannot
/// hold two, so `> 1` is not a curiosity to log and move past, it is the OOM
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
/// heartbeat builder and back through the status endpoint, positional and
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
    /// sidecar count is available immediately, it is a census, not a delta.
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
/// `Default` sets `tasks: true`, because on Linux it lists every *task*
/// (thread) as a process in its own right, each one carrying its parent's
/// `/proc/<pid>` and therefore the parent's whole RSS. A sidecar with an
/// 8-thread ONNX pool was therefore counted as **8 processes holding eight
/// times its memory**: the Linux worker reported `8× 19.5G` for a 2.4 GiB
/// model on a box that simultaneously reported 3.8 GB used. Every task's
/// `/proc/<pid>/task/<tid>/statm` is byte-identical to the leader's, which is
/// what made the multiplication exact.
///
/// macOS enumerates no tasks, `Process::thread_kind` is `None` off
/// Linux/Android, so only the Linux workers were ever wrong, which is why
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
/// spellings a worker can run, the provisioned `~/bm-worker/bm-tts` and a
/// repo build at `rust/target/{debug,release}/bm-tts`, and deliberately not
/// on argv, which would also match an `ssh … bm-tts` wrapper on the
/// inductor's own box. `name` comes from the process's own `stat` parse, not
/// from a refresh flag, so the narrow [`census_refresh_kind`] keeps it.
///
/// One definition, two callers: the heartbeat's load sample, and the sidecar's
/// own memory guard. Two copies would let the number the panes show and the
/// number the guard acts on disagree, which is the worst version of this,
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
/// callers now, the heartbeat loop that pushes beats and the status endpoint
/// that answers for one, and four strings in the same order is exactly the
/// shape that gets transposed silently.
#[derive(Debug, Clone)]
struct WorkerIdentity {
    worker_id: String,
    addr: String,
    hostname: String,
    alias: String,
    /// This box's worker root. Carried here because a beat has to say which
    /// stages the sources bundle on disk actually covers, and that bundle is
    /// read from `sources-manifest.json` under this root.
    root: PathBuf,
}

/// The `(stage, adapter)` slots this box's sources bundle covers, from the
/// manifest the last provision left at the worker root.
///
/// Read **per beat** rather than cached at startup: a push happens under a
/// running agent, so a list read once at boot would keep withholding the work
/// the box has just been handed until somebody restarted the worker. A missing
/// or unreadable manifest is an empty list, which the inductor reads as "no
/// opinion" rather than "covers nothing".
///
/// The manifest is the box's own statement of what it holds; the inductor turns
/// it into a gate against the adapter the offer is *for*, which is why the list
/// is two-dimensional — one bundle carries every language, and a stage name
/// alone cannot say which of them this box can run.
fn bundle_slots(root: &Path) -> Vec<String> {
    std::fs::read_to_string(root.join(bm_core::provision::sources::MANIFEST_NAME))
        .ok()
        .and_then(|t| serde_json::from_str::<bm_core::provision::sources::SourcesManifest>(&t).ok())
        .map(|m| m.slots)
        .unwrap_or_default()
}

/// The heartbeat for right now. **One builder for both directions.**
///
/// The pull protocol posts this on a timer and the inverted protocol answers
/// `GET /status` with it. Two constructions would be two chances for the
/// inductor's liveness bookkeeping and its panes to disagree about what a
/// worker is doing, depending on which way the report travelled.
///
/// `sidecar_keep` is the worker's own belief about its sidecar: pull mode has
/// no instruction channel, so it is always `true` here, serve mode passes
/// what the inductor last told it, and the dispatcher's convergence reads
/// that back to detect a box that rebooted into its default.
fn heartbeat_now(
    p: &Progress,
    who: &WorkerIdentity,
    probe: &mut LoadProbe,
    sidecar_keep: bool,
    tts_threads: Option<u32>,
) -> Heartbeat {
    let (cpu_pct, mem_pct, mem_gb, sidecars, sidecar_gb) = probe.sample();
    // A completed stage must not survive the task boundary as a live-looking
    // heartbeat. `clear_task` normally removes the whole block together, but
    // the completion report and the next status poll are separate concurrent
    // operations. If a poll observes the terminal activity after the task id
    // has already been cleared, report the authoritative state instead of
    // resurrecting `digest chN done` as though the worker were still working.
    let stale_completion = p.task_id.is_none() && p.activity.contains(" done");
    let (task_id, stage, chapter, progress, activity) = if stale_completion {
        (None, None, None, 0.0, "idle".to_string())
    } else {
        (
            p.task_id.clone(),
            p.stage.as_deref().and_then(bm_proto::Stage::parse),
            p.chapter,
            p.frac,
            p.activity.clone(),
        )
    };
    Heartbeat {
        worker_id: who.worker_id.clone(),
        addr: who.addr.clone(),
        task_id,
        stage,
        chapter,
        progress,
        activity,
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
        sources_stages: bundle_slots(&who.root),
        sidecar_keep: Some(sidecar_keep),
        tts_threads,
        cores: std::thread::available_parallelism()
            .ok()
            .map(|n| n.get() as u32),
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
/// `merge` is advertised **only when both ffmpeg and sox are on PATH**. The
/// merge stage shells out to ffmpeg for the beds and to sox for every voice
/// treatment's room, so a box missing either would take merges it cannot
/// finish, three strikes and the chapter shelves. Reporting the capability
/// truthfully lets the scheduler skip merge here and give the box its other
/// stages, instead of poisoning the ledger with failures it cannot help.
fn capabilities() -> Vec<String> {
    let mut caps = vec![
        "crawl".into(),
        "digest".into(),
        "render".into(),
        "render-segments".into(),
    ];
    if bm_core::assemble::ffmpeg_available() && bm_core::assemble::sox_available() {
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
        // Pull mode has no instruction channel, so the sidecar's own default
        // (or its `BM_TTS_THREADS` environment) stands: `None`.
        let body = heartbeat_now(&p, &who, &mut probe, true, None);
        // The inductor's only command channel: a shutdown latch read on
        // every answer. Exiting here strands nothing, the inductor
        // reaps the lease (no strike) or requeues the ledger on its way
        // down, and an old inductor's `{"ok": true}` parses as "stay".
        if let Ok(resp) = http.post(&url).json(&body).send().await {
            if let Ok(bytes) = resp.bytes().await {
                if wants_shutdown(&bytes) {
                    // No sidecar is stopped here, and it is a known gap rather
                    // than an oversight: this is the *pull* protocol, whose
                    // sidecar lives in the task loop's own locals and is not
                    // reachable from this task (the inverted protocol, the one
                    // the inductor actually drives now, reaps in the
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
/// arrive by design, and counting that as idleness would kill a render
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
            // drives. Safe to wait for the lock, this branch is only reached
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
        // now stale by definition, the task it described has been re-queued
        // and re-decided, so its completion must not land late and surprise
        // the ledger. (The hook had its whole silent window to deliver it.)
        p.pending = None;
    }
}

/// Say so when the box's own profile is not the one the task came from.
///
/// The **pack** is the leg of the binding a box can disagree about silently.
/// The adapter and the engine are in every cache path it writes, so a
/// mismatch shows up in the filenames; the pack is a property of the `assets/`
/// and `prompts/` a provision left here, and a box carrying another pack's
/// registries is being handed prompts that read files this pack never wrote.
///
/// Warned, not refused — "refuse where bytes are made, warn on load" — and
/// refused on the inductor, where the workspace's binding, the adapter it
/// names and the engine those bytes would be spoken with are all in one hand.
/// Silent when either side is silent, so an older inductor and a pre-split
/// pointer are both just quiet.
fn warn_on_pack_mismatch(layout: &Layout, offer: &TaskOffer) {
    if offer.pack.is_empty() {
        return;
    }
    let Ok(binding) = bm_core::profile::read_binding(&layout.root) else {
        return;
    };
    if !binding.pack.name.is_empty() && binding.pack.name != offer.pack {
        println!(
            "warning: this box holds profile '{}' but the task came from '{}' — its assets are \
             another profile's; re-provision it",
            binding.pack.name, offer.pack
        );
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
/// names that were set, never the values, which do not belong in a log.
///
/// Two consumers read them straight out of the environment: the generation
/// backends in `bm-core::digest::llm` (which is why a provisioned box used to
/// die on `GEMINI_API_KEY missing` — provisioning never copies `.bm/`, so a
/// remote box holds no key file of its own), and the TTS sidecar, a
/// child process the worker spawns for a render and which inherits this
/// environment at `spawn()`.
///
/// **The inductor wins.** It holds the only copy the operator maintains (the
/// `L` screen, `.bm/llm.json`), so a value it sends replaces whatever this
/// box had — a stale key on one machine is precisely the failure this
/// replaces. An *empty* value is skipped rather than blanked, so an offer
/// from an inductor that has nothing configured changes nothing at all.
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
/// **No scheduling calls home.** A worker never asks for work, the offer is
/// authoritative and everything scheduling needs arrived inside it. The one
/// exception is data, not scheduling: a merge pulls the take files it lacks
/// from the inductor (see `run_merge`), exactly as a render pushes its units
/// there. `fetch` carries the client and base URL for that pull; tests pass
/// `None`.
#[allow(clippy::too_many_arguments)]
async fn run_offer(
    layout: &Layout,
    settings: &Settings,
    offer: &TaskOffer,
    shared: &Shared,
    sidecar: &mut Sidecar,
    fetch: Option<(&reqwest::Client, &str)>,
    // Whether a render on this box may ensure a sidecar at all. `false` is
    // the inductor's instruction (`POST /sidecar-policy`): the operator's
    // policy turned render off, and a render, the one stage that cannot
    // run without the ~2.85 GB model, is refused rather than served by
    // re-warming it. A snapshot taken per call, so no hidden mutable state
    // sits on `Sidecar` for an offer to read stale.
    keep_sidecar: bool,
    // The ONNX thread count the inductor pushed for this box's sidecar, or
    // `None` for the sidecar's own default. A per-call snapshot, like
    // `keep_sidecar`, so an edit made mid-task is applied at the next
    // boundary rather than under a running render.
    tts_threads: Option<u32>,
) -> Result<TaskResult> {
    use bm_proto::Stage::*;
    let n = offer.chapter;
    // **The offer's binding decides where this task's files live.** See
    // `Layout::rebind` for the divergence it closes; in one line, a worker root
    // is a flat mirror, so without this the box keys `cast-*` and
    // `segments-*` under `default` while the inductor that drives it keys them
    // under the adapter its ledger names. The offer is the authority because it
    // is the inductor's answer, taken at the moment it decided to hand this box
    // this task — a re-pointed inductor reaches a box on its next offer rather
    // than on its next provisioning.
    let bound = layout.rebind(&offer.adapter, &offer.engine);
    let layout = &bound;
    warn_on_pack_mismatch(layout, offer);
    // Credentials first: everything below, including the TTS sidecar this
    // call may spawn, reads them from the environment.
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
            // The offer's spec is authoritative: it carries the script the
            // inductor read, so a box that has not been re-provisioned still
            // runs the crawler the operator edited. An offer without one (an
            // older inductor) falls back to this box's own settings, which is
            // the pre-script behaviour.
            let spec = offer
                .crawl
                .clone()
                .unwrap_or_else(|| bm_core::crawl::spec_from_settings(layout, settings));
            let (units, text, report) = run_crawl(
                layout,
                n,
                spec,
                offer.url.clone(),
                offer.attempt.max(1),
                shared,
            )
            .await?;
            // An absent chapter is a *success* whose artifact does not exist;
            // a block is a failure whose message and class travel back so the
            // inductor can shelve it at once when retrying would prove nothing.
            let ok = text.is_some() || report.verdict == bm_proto::CrawlVerdict::Absent;
            let detail = match (&report.verdict, &text) {
                (bm_proto::CrawlVerdict::Text, Some(t)) => {
                    format!("crawled ch{n} ({} bytes)", t.len())
                }
                (bm_proto::CrawlVerdict::Absent, _) => {
                    format!("ch{n} is not on the site: {}", report.detail)
                }
                _ => format!("crawl ch{n} blocked [{}]: {}", report.class, report.detail),
            };
            Ok(TaskResult {
                ok,
                detail,
                delta: None,
                units,
                script: None,
                text,
                crawl: Some(report),
                mp3_b64: None,
                unit_files: Vec::new(),
            })
        }
        Digest => {
            let bible = offer.bible.clone().unwrap_or(json!({"characters": []}));
            // The inductor's pick wins; an empty offer (no active provider)
            // falls back to this worker's own settings, which refuses with
            // "press L" when it names nothing either.
            let analyzer = if offer.analyzer.is_empty() {
                settings.analyzer.clone()
            } else {
                offer.analyzer.clone()
            };
            // The backend name travels in `offer.analyzer`; what it runs
            // travels in `offer.analyzer_settings`. Both are needed here: this
            // box may have no `.bm/settings.json` at all (provisioning copies
            // the sources bundle, never `.bm/`, that
            // is the inductor's state), in which case `Settings::load` silently
            // returns `Settings::default()` and the compiled-in model runs
            // instead of the operator's. That is the bug that made a box
            // configured for `gemini-3.5-flash-lite` call `gemini-3.5-flash`.
            let mut digest_settings = settings.with_analyzer_settings(&offer.analyzer_settings);
            // The id rides the offer; the overlay carries model, endpoint and
            // slot. Both land here so error lines name the provider (`who`)
            // while routing reads the slot.
            digest_settings.analyzer = analyzer.clone();
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
                crawl: None,
                mp3_b64: None,
                unit_files: Vec::new(),
            })
        }
        Render => {
            // The sidecar policy first, as a **per-call snapshot** the caller
            // took before this offer ran: an operator who turned render off
            // for this box must not get a model relaunched under them by a
            // stray or stale render offer. A typed error, not a bare string
            // the serve path answers it with a 403, which the dispatcher
            // reads as "strike-free refusal, release the rows", while every
            // other render failure answers 200 `ok: false`, a strike. A
            // policy decision must never cost a chapter its three strikes
            // (three flips would shelve it); see `push::task` and
            // `dispatch::run_one`.
            if !keep_sidecar {
                return Err(anyhow::Error::new(PolicyRefusal(
                    "render skipped: this box's policy turns render off, so the TTS sidecar is not kept",
                )));
            }
            // The memory guard, and this is the only place it can safely run:
            // between tasks. A model that has outgrown its budget is recycled
            // here, before the first `/infer` of this offer, never during one.
            // See `Sidecar::recycle_if_over_budget` for what it measures and
            // why it is not the idle reaper's job.
            sidecar.recycle_if_over_budget().await;
            sidecar.ensure(layout, tts_threads).await?;
            let (units, unit_files) = match render_action(offer.render_units.as_deref()) {
                // Old inductor: plan from the local script, keep files
                // locally, upload nothing, exactly as before the migration.
                RenderAction::Legacy => (
                    run_render(layout, n, &offer.engine, &sidecar.tts(), shared).await?,
                    Vec::new(),
                ),
                // The offer names no units at all: nothing to speak.
                RenderAction::Noop => (0, Vec::new()),
                // The takes this offer carries, minus what this box already has.
                // Skipping here rather than on the inductor is the point: the
                // inductor cannot see this disk, and a partial offer is what
                // used to leave a box holding a strict subset of a chapter.
                //
                // `render_batch` takes of one chapter arrive per offer; the
                // worker already loops over a list, so batching changed nothing
                // here, only how often this arm is entered.
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
                crawl: None,
                mp3_b64: None,
                unit_files,
            })
        }
        Merge => {
            // Merge needs no TTS, and its ffmpeg pass is the other large
            // working set on the box: drop the model first, including a
            // provision-started one this worker never spawned, so the two
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
            // and removes the branch, and the inductor writes it to
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
                crawl: None,
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
    /// Crawl only: what happened, when "ok" alone cannot say it, an absent
    /// chapter and a login wall are both `ok: false`-shaped facts with entirely
    /// different consequences, and the inductor cannot tell them apart from a
    /// boolean and a sentence.
    crawl: Option<bm_proto::CrawlReport>,
    mp3_b64: Option<String>,
    unit_files: Vec<bm_proto::UnitFile>,
}

/// Say what the sidecar guard will do, once, when a worker starts.
///
/// The guard's numbers are a judgement until a box has measured them, and a
/// measurement needs a record of what was in force, otherwise a log full of
/// recycles says nothing about which budget produced them. One line, at startup,
/// in the same place the worker announces itself.
///
/// Resolved from the same [`Budget::from_env`] the guard uses, so the line
/// cannot describe a budget other than the one being applied.
fn announce_budget() {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    println!("{}", Budget::from_env().describe(sys.total_memory()));
    // The ONNX thread count is the other per-box number that decides what a
    // render costs, and the guard's line does not carry it. `0` is the
    // sidecar's own default, so say which one is in force — a box that set
    // `BM_TTS_THREADS` can then see that it took.
    match bm_core::config::tts_threads() {
        0 => println!(
            "TTS sidecar threads: the sidecar's own default (half this box's cores, capped at 8) — set BM_TTS_THREADS to use more"
        ),
        n => println!("TTS sidecar threads: {n} (BM_TTS_THREADS)"),
    }
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
        // its place, which surfaces as a 502 from nowhere and a worker that
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
        root: layout.root.clone(),
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
            // The sidecar is kept by default, the render lifecycle is written
            // against that, and the inductor's `POST /sidecar-policy` is the
            // one thing that may clear it (an operator turning render off).
            keep_sidecar: std::sync::atomic::AtomicBool::new(true),
            // No opinion until the inductor pushes one: the sidecar's own
            // default (or its `BM_TTS_THREADS`) stands.
            tts_threads: std::sync::atomic::AtomicU64::new(push::THREADS_UNSET),
            // Merge data-plane: the hook base *is* the inductor API through
            // the reverse tunnel. `no_proxy`, like every loopback client
            // here, an ambient HTTP_PROXY would answer instead of the tunnel.
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
    // guarantee: this worker never asks for work, it holds no address to
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
        // control API, the reverse tunnel's far end (bm-inductor's tunnel
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
        sources_stages: bundle_slots(&layout.root),
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
        let res = match run_offer(
            &layout,
            &settings,
            &offer,
            &shared,
            &mut sidecar,
            Some((&http, inductor.as_str())),
            true,
            None,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => TaskResult {
                ok: false,
                detail: format!("{} ch{} failed: {e:#}", offer.stage, offer.chapter),
                delta: None,
                units: 0,
                script: None,
                text: None,
                crawl: None,
                mp3_b64: None,
                unit_files: Vec::new(),
            },
        };
        // The stage is over as soon as `run_offer` returns. Do not leave its
        // terminal `... done` activity visible while the completion report is
        // being retried: from the worker's point of view it is idle, and the
        // report transport is bookkeeping, not work. The task id is also
        // cleared here so a status poll cannot mistake the old stage for a
        // live offer while `/api/complete` is in flight.
        clear_task(&shared);
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
            crawl: res.crawl,
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
/// remedy attached, which is how a real problem becomes unreadable noise.
fn tunnel_missing_hint(task_id: &str, attempt: u64) {
    if attempt.is_multiple_of(12) {
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

/// Split from `main` so the artifact fetch can run with no tokio runtime at
/// all.
///
/// Two reasons, both of them panics if ignored: `reqwest::blocking` refuses to
/// run inside an async context, and `process::exit` from inside one drops the
/// runtime on the way out. The fetch is a synchronous, self-contained
/// download, so it does not belong on that side of the boundary.
fn main() -> Result<()> {
    let cli = Cli::parse();
    // Handled before the layout is resolved, too: the box this runs on has
    // `~/bm-worker` and no book, and a fetch refused for want of a workspace
    // would fail for a reason that has nothing to do with the fetch.
    if let Cmd::FetchArtifact {
        url,
        dest,
        expect,
        strip_prefix,
    } = &cli.cmd
    {
        std::process::exit(fetch_artifact(
            url,
            dest,
            expect.as_deref(),
            strip_prefix.as_deref(),
        ));
    }
    tokio::runtime::Runtime::new()?.block_on(run(cli))
}

async fn run(cli: Cli) -> Result<()> {
    let layout = match cli.root {
        // Same resolution the inductor does: `--root` names the *repo*, and
        // the workspace pointer inside it decides where this book's settings,
        // data and output live. On a worker the root is the flat mirror, which
        // has no pointer and is therefore its own workspace.
        Some(r) => Layout::resolve(r)?,
        None => Layout::discover()?,
    };
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
                    // Standalone: no offer, so this box's own settings decide,
                    // and `--url` overrides the manifest's answer.
                    let spec = bm_core::crawl::spec_from_settings(&layout, &settings);
                    let (_, text, report) =
                        run_crawl(&layout, chapter, spec, url, 1, &shared).await?;
                    match (report.verdict, text) {
                        (bm_proto::CrawlVerdict::Text, Some(t)) => {
                            println!("crawled ch{chapter}: {} bytes", t.len())
                        }
                        (bm_proto::CrawlVerdict::Absent, _) => {
                            println!("ch{chapter} is not on the site: {}", report.detail)
                        }
                        _ => println!(
                            "crawl ch{chapter} blocked [{}]: {}",
                            report.class, report.detail
                        ),
                    }
                }
                "digest" => {
                    let bible: Value = serde_json::from_str(
                        &std::fs::read_to_string(layout.bible())
                            .unwrap_or_else(|_| "{\"characters\":[]}".into()),
                    )?;
                    run_digest(
                        &layout,
                        chapter,
                        &bible,
                        &settings,
                        &settings.analyzer,
                        &shared,
                        true,
                    )
                    .await?;
                }
                "render" => {
                    let tts_url = tts_url.unwrap_or_else(|| "http://127.0.0.1:8818".into());
                    let mut sidecar = Sidecar::new(&tts_url);
                    // The hand-driven path has no inductor instruction channel:
                    // the sidecar's own default or its `BM_TTS_THREADS` stands.
                    sidecar.ensure(&layout, None).await?;
                    run_render(&layout, chapter, &engine, &sidecar.tts(), &shared).await?;
                    sidecar.stop();
                }
                "merge" => {
                    // A hand-driven merge has no offer to carry the take list,
                    // but the names are still the plan's: a take file is
                    // content-addressed, so recomputing it from the script and
                    // the cast finds nothing. Read the chapter's own recorded
                    // plan instead — the same names an offer would have
                    // carried, in the same mix order. Only a chapter with no
                    // readable plan falls back to `assemble` naming them
                    // itself, which is the pre-content-addressed behaviour.
                    let takes = bm_core::assemble::RenderPlan::load(&layout.plan(chapter))
                        .map(|p| p.files())
                        .unwrap_or_default();
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
                            takes,
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
            // means a half-rsynced provision, refuse. Drift is adopted.
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
        // Already handled above, before a layout was resolved — this arm is
        // here for exhaustiveness, and the `unreachable!` is the compiler
        // being told so rather than left to discover it.
        Cmd::FetchArtifact { .. } => unreachable!("handled before the layout is resolved"),
    }
    Ok(())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod census_probe;
