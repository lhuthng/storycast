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

#[allow(unused_imports)]
pub(crate) use alias::{default_worker_id, worker_alias_for, ALIAS_POOL};
#[allow(unused_imports)]
pub(crate) use heartbeat::{idle_secs, idle_watchdog};
#[allow(unused_imports)]
pub(crate) use identity::{
    bundle_slots, capabilities, clear_task, heartbeat_now, hostname_simple, install_credentials,
    set_task, warn_on_pack_mismatch, WorkerIdentity,
};
#[allow(unused_imports)]
pub(crate) use offer::{announce_budget, run_offer, TaskResult};
#[allow(unused_imports)]
pub(crate) use probe::{census_refresh_kind, sidecar_processes, wants_shutdown, Load, LoadProbe};
#[allow(unused_imports)]
pub(crate) use sidecar::{
    budget_verdict, Budget, PolicyRefusal, Sidecar, SIDECAR_MAX_RENDERS, SIDECAR_MIN_LIFETIME_SECS,
    SIDECAR_RSS_FRACTION,
};
#[allow(unused_imports)]
pub(crate) use stages::{
    pending_units, render_action, run_crawl, run_digest, run_merge, run_render, MergeJob,
    RenderAction,
};
#[allow(unused_imports)]
pub(crate) use worker::{tunnel_missing_hint, worker_loop};

mod alias;
mod heartbeat;
mod identity;
mod offer;
mod probe;
mod sidecar;
mod stages;
mod worker;

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
    pub(crate) task_id: Option<String>,
    pub(crate) stage: Option<String>,
    pub(crate) chapter: Option<u32>,
    pub(crate) frac: f32,
    pub(crate) activity: String,
    /// A stage that finished but was never acknowledged, the completion
    /// hook's stash. Written by the task handler (serve mode) the moment a
    /// stage ends, cleared by the hook once the inductor accepts it through
    /// the reverse tunnel, or by the next offer (the inductor is talking
    /// again, so this outcome is stale by definition). `None` in pull mode,
    /// whose reports retry on their own channel.
    pub(crate) pending: Option<Complete>,
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
