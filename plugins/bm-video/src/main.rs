//! `bm-video` — render a Storycast book's acts into one video.
//!
//! An optional plugin, not part of the pipeline: it reads what the pipeline
//! already publishes (the merged mp3s and their cue sidecars) and writes a
//! video beside them. Nothing in the workspace depends on it.
//!
//! The render is chunked and content-addressed, so it can also be spread over
//! other boxes, either way round:
//!
//! - `bm-video remote --host 192.168.69.37` pushes — it ships this source to
//!   the box, builds it there, and hands it a slice of the plan;
//! - `bm-video serve` plus `bm-video join` is a LAN pool, where the owner serves
//!   the plan and each box dials in, asks for batches and submits its chunks.
//!
//! Both hand a worker the same thing: a range of the plan and the plan owner's
//! clock. A worker is the same binary with `--from/--count` and `--plan`, so no
//! box needs a special mode.
//!
//! Both also check the worker's *code*: `digest` prints a fingerprint of this
//! source, and the owner and the box compare theirs before any pixels are
//! rendered. A box on different source would produce different pixels under an
//! identical chunk name — silently wrong rather than merely slow.

mod distribute;
mod ffmpeg;
mod model;
mod paint;
mod pool;
mod raster;
mod render;
mod source;
mod sticker;
mod template;
mod text;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Args, Debug, Clone)]
struct RenderArgs {
    /// Book workspace holding output/ (default: workspaces/beyond-myriads)
    #[arg(long)]
    workspace: Option<PathBuf>,
    /// acts manifest: {acts: [{act, title, chapters}]}
    #[arg(long)]
    acts: Option<PathBuf>,
    #[arg(long, default_value = "tools/video-template.json")]
    template: PathBuf,
    #[arg(long, default_value = "renders")]
    outdir: PathBuf,
    /// output stem (default: the manifest's name, else acts-NN-NN)
    #[arg(long, default_value = "")]
    name: String,
    /// seconds of silence between chapters
    #[arg(long, default_value_t = 0.8)]
    chapter_gap: f64,
    /// render only the first N seconds
    #[arg(long)]
    preview: Option<f64>,
    /// skip burning captions (the .srt/.vtt sidecars are still written)
    #[arg(long)]
    no_subs: bool,
    /// chunk length in seconds; the unit of caching, concurrency and dispatch
    #[arg(long, default_value_t = 4.0)]
    chunk_secs: f64,
    /// concurrent chunk workers (default: one per core)
    #[arg(long)]
    jobs: Option<usize>,
    /// x264 preset (default: the template's)
    #[arg(long)]
    preset: Option<String>,
    /// x264 CRF (default: the template's)
    #[arg(long)]
    crf: Option<u32>,
    /// render only this slice of the chunk plan — a plugin-worker's assignment
    #[arg(long, requires = "count")]
    from: Option<usize>,
    /// how many chunks that slice holds
    #[arg(long, requires = "from")]
    count: Option<usize>,
    /// the plan owner's timeline: render its clock instead of probing for one
    #[arg(long)]
    plan: Option<PathBuf>,
    /// ignore cached chunks and re-render everything
    #[arg(long)]
    rebuild: bool,
    /// print the chunk plan (keys included) and exit
    #[arg(long)]
    dry_run: bool,
    /// keep the assembled video/audio pieces too, not just the chunk cache
    #[arg(long)]
    keep_parts: bool,
}

#[derive(Parser, Debug)]
#[command(
    name = "bm-video",
    about = "Render Storycast acts into one video (a standalone plugin)"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    #[command(flatten)]
    render: RenderArgs,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Render across other boxes: ship this source, build it there, dispatch slices
    Remote(RemoteArgs),
    /// Serve the plan on the LAN; boxes join, ask for work and submit chunks
    Serve(ServeArgs),
    /// Join a pool: fetch the inputs, then ask for batches and submit them
    Join(JoinArgs),
    /// Print this source's fingerprint, for comparing an owner with a box
    Digest,
}

#[derive(Args, Debug)]
struct ServeArgs {
    #[command(flatten)]
    render: RenderArgs,
    /// where to listen; the LAN only, and there is no authentication
    #[arg(long, default_value = "0.0.0.0:8722")]
    bind: String,
    /// chunks per task a box is given
    #[arg(long, default_value_t = 4)]
    want: usize,
    /// stop once nothing has asked for work for this many seconds
    #[arg(long, default_value_t = 30)]
    idle: u64,
    /// collect the chunks but do not assemble the video
    #[arg(long)]
    no_assemble: bool,
}

#[derive(Args, Debug)]
struct JoinArgs {
    /// the pool: host:port
    #[arg(long)]
    server: String,
    /// where this box keeps the staged inputs and its chunk cache
    #[arg(long, default_value = "")]
    dir: String,
    /// chunks to ask for at a time
    #[arg(long, default_value_t = 4)]
    want: usize,
    /// threads this box renders with (default: one per core)
    #[arg(long, default_value_t = 0)]
    jobs: usize,
    /// also pull the plugin's source and build it here
    #[arg(long)]
    fetch_source: bool,
    /// render even if this box's code cannot be matched to the pool's
    #[arg(long)]
    allow_stale: bool,
}

#[derive(Args, Debug)]
struct RemoteArgs {
    #[command(flatten)]
    render: RenderArgs,
    /// a box to render on; `local` runs the worker here (repeatable, or comma-separated)
    #[arg(long = "host", value_delimiter = ',', required = true)]
    host: Vec<String>,
    #[arg(long)]
    user: Option<String>,
    #[arg(long, default_value_t = 22)]
    port: u16,
    #[arg(long)]
    key: Option<PathBuf>,
    /// where a box keeps the plugin source, the staged inputs and its output
    #[arg(long, default_value = "~/.bm-video")]
    remote_root: String,
    /// worker processes to run on each box at once
    #[arg(long, default_value_t = 2)]
    slots: usize,
    /// threads each of those workers may use
    #[arg(long, default_value_t = 2)]
    worker_jobs: usize,
    /// run on each box before rendering — e.g. pause its normal worker
    #[arg(long)]
    hold_cmd: Option<String>,
    /// run on each box after rendering
    #[arg(long)]
    release_cmd: Option<String>,
    /// where the stage is built locally
    #[arg(long, default_value = "/tmp/bm-video-stage")]
    stage: PathBuf,
    #[arg(long)]
    keep_stage: bool,
    /// print the plan and each box's assignment, then stop
    #[arg(long)]
    plan_only: bool,
}

fn build_options(r: &RenderArgs) -> Result<render::Options> {
    let root = std::env::current_dir().context("resolving the working directory")?;
    let abs = |p: &Path| -> PathBuf {
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            root.join(p)
        }
    };
    let acts = r
        .acts
        .clone()
        .context("--acts is required: the acts manifest {acts: [{act, title, chapters}]}")?;
    let acts_path = abs(&acts);
    let template_path = abs(&r.template);
    let template = template::Template::load(&template_path, &root)?;
    let bytes = std::fs::read(&template_path)
        .with_context(|| format!("reading template {}", template_path.display()))?;
    let template_digest: [u8; 32] = Sha256::digest(&bytes).into();
    let manifest = model::Manifest::load(&acts_path)?;
    let workspace = r
        .workspace
        .as_ref()
        .map(|w| abs(w))
        .unwrap_or_else(|| root.join("workspaces").join("beyond-myriads"));

    let numbers: Vec<u32> = manifest.acts.iter().map(|a| a.act).collect();
    let name = if !r.name.is_empty() {
        r.name.clone()
    } else if let Some(named) = manifest.name.clone() {
        named
    } else {
        format!(
            "acts-{:02}-{:02}",
            numbers.iter().min().copied().unwrap_or(1),
            numbers.iter().max().copied().unwrap_or(1)
        )
    };

    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let preset = r.preset.clone().unwrap_or_else(|| template.encode.preset.clone());
    let crf = r.crf.unwrap_or(template.encode.crf);
    let outdir = abs(&r.outdir);
    let plan = match &r.plan {
        Some(p) => {
            let path = abs(p);
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading plan {}", path.display()))?;
            Some(
                serde_json::from_str(&text)
                    .with_context(|| format!("parsing plan {}", path.display()))?,
            )
        }
        None => None,
    };
    Ok(render::Options {
        root,
        workspace,
        outdir,
        name,
        manifest,
        template,
        template_path,
        acts_path,
        slice: match (r.from, r.count) {
            (Some(f), Some(c)) => Some((f, c)),
            _ => None,
        },
        plan,
        template_digest,
        chapter_gap: r.chapter_gap,
        preview: r.preview,
        no_subs: r.no_subs,
        chunk_secs: r.chunk_secs.max(0.05),
        jobs: r.jobs.unwrap_or(cores),
        preset,
        crf,
        rebuild: r.rebuild,
        dry_run: r.dry_run,
        keep_parts: r.keep_parts,
    })
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match &cli.cmd {
        Some(Cmd::Remote(ra)) => {
            let o = build_options(&ra.render)?;
            if o.slice.is_some() {
                bail!("--from/--count are a worker's assignment; `remote` splits the plan itself");
            }
            let mut hosts: Vec<distribute::Remote> = ra
                .host
                .iter()
                .map(|h| distribute::Remote {
                    host: h.clone(),
                    user: ra.user.clone(),
                    port: ra.port,
                    key: ra.key.clone(),
                    root: ra.remote_root.clone(),
                    slots: ra.slots.max(1),
                    worker_jobs: ra.worker_jobs.max(1),
                    hold: ra.hold_cmd.clone(),
                    release: ra.release_cmd.clone(),
                })
                .collect();
            distribute::dispatch(&o, &mut hosts, &ra.stage, ra.keep_stage, ra.plan_only)
        }
        Some(Cmd::Serve(sa)) => {
            let o = build_options(&sa.render)?;
            if o.slice.is_some() {
                bail!("--from/--count are a worker's assignment; `serve` hands out the plan itself");
            }
            pool::serve(&o, &sa.bind, sa.want, sa.idle, !sa.no_assemble)
        }
        Some(Cmd::Join(ja)) => {
            if ja.want == 0 {
                bail!("--want must be at least one chunk");
            }
            pool::join(
                &ja.server,
                &home_of(&ja.dir)?,
                ja.want,
                ja.jobs,
                ja.fetch_source,
                ja.allow_stale,
            )
        }
        Some(Cmd::Digest) => {
            println!("{}", source::digest()?);
            Ok(())
        }
        None => {
            let o = build_options(&cli.render)?;
            render::run(&o)
        }
    }
}

/// A box's working directory: `--dir`, or `~/.bm-video`.
fn home_of(dir: &str) -> Result<PathBuf> {
    if !dir.is_empty() {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var("HOME").context("HOME is not set; pass --dir")?;
    Ok(PathBuf::from(home).join(".bm-video"))
}
