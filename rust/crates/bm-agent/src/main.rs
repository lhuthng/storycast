//! Worker agent: pull one pipeline stage at a time, run it, report back.

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
        #[arg(long)]
        serve_tasks: Option<u16>,
    },
    /// List local audio segments as JSON: the inventory half of the
    Segments {
        /// Emit the machine-readable manifest (the only output format).
        #[arg(long, default_value_t = true)]
        json: bool,
    },
    /// Take delivery of the model artifact from a release, on the box.
    FetchArtifact {
        /// Release download URL of a `models.tar.zst`.
        url: String,
        /// The models directory itself, e.g. `~/bm-worker/engines/vieneu/models`.
        dest: PathBuf,
        /// The manifest hash the inductor read, which this must agree with.
        #[arg(long)]
        expect: Option<String>,
        /// Keep only the members under this directory, and land *it* at
        #[arg(long, value_name = "DIR")]
        strip_prefix: Option<String>,
    },
}

/// One entry of the segment inventory: which chapter, which engine, which
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

/// Exit codes [`Cmd::FetchArtifact`] means by them, named once so the
mod fetch_exit {
    pub const LANDED: i32 = 0;
    pub const CORRUPT: i32 = 20;
    pub const UNREACHABLE: i32 = 21;
}

/// Take delivery of the model artifact, and say what happened on one line.
fn fetch_artifact(url: &str, dest: &Path, expect: Option<&str>, strip_prefix: Option<&str>) -> i32 {
    use std::time::Instant;
    let started = Instant::now();
    let mut last = Instant::now();
    // With a prefix the artifact is a pack, and the models tag rule would name
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
    let r = match (strip_prefix, expect) {
        (Some(_), Some(hash)) => bm_core::artifact::fetch_pack(url, dest, hash, "", &mut say),
        (Some(dir), None) => {
            eprintln!("FETCH-CORRUPT (--strip-prefix {dir} needs --expect: a pack release is only accepted against the profile hash it is replacing)");
            return fetch_exit::CORRUPT;
        }
        // No expectation from the caller: land it, then report the tag the
        (None, Some(hash)) => bm_core::artifact::fetch(url, dest, hash, &mut say),
        (None, None) => bm_core::artifact::fetch_unpinned(url, dest, &mut say),
    };
    match r {
        Ok(landed) => {
            // The tag is the *models* bundle's identity and this box was not
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
fn main() -> Result<()> {
    let cli = Cli::parse();
    // Handled before the layout is resolved, too: the box this runs on has
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
                    sidecar.ensure(&layout, None).await?;
                    run_render(&layout, chapter, &engine, &sidecar.tts(), &shared).await?;
                    sidecar.stop();
                }
                "merge" => {
                    // A hand-driven merge has no offer to carry the take list,
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
        Cmd::FetchArtifact { .. } => unreachable!("handled before the layout is resolved"),
    }
    Ok(())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod census_probe;
