//! `bm-filmcraft` — build a Storycast book's acts into a FilmCraft project.
//!
//! An optional plugin, not part of the pipeline: it reads what the pipeline
//! already publishes (the merged mp3s and their cue sidecars) and drives a
//! FilmCraft session through `filmcraft-cli`, so the edit lands as a `.fcproj`
//! a person can open, retime and grade — and the picture is exported by
//! FilmCraft's own encoder rather than by this plugin.
//!
//! The edit it builds is the shape the book already has: the chapters end to
//! end on A1, every line a caption on a caption track, a chapter marker where
//! each act begins, and the look the template describes — paper, act plate, the
//! bar with the playing act raised out of it, the thumb moving along it, and the
//! caption style.
//!
//! `bm-video` renders the same book with its own compositor; this plugin does
//! not replace it. What it adds is the handoff: a real project, in the editor,
//! that an agent or a person can then take further.

mod element;
mod filmcraft;
mod look;
mod model;
mod motion;
mod plan;
mod portrait;
mod srt;
#[cfg(test)]
mod tests;

use anyhow::{bail, Context, Result};
use clap::Parser;
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[command(
    name = "bm-filmcraft",
    about = "Build a Storycast book's acts into a FilmCraft project (a standalone plugin)"
)]
struct Cli {
    /// acts manifest: {acts: [{act, title, chapters}]}
    #[arg(long)]
    acts: PathBuf,
    /// book workspace holding output/ (default: workspaces/beyond-myriads)
    #[arg(long, default_value = "workspaces/beyond-myriads")]
    workspace: PathBuf,
    /// where the project is written (default: renders/<name>.fcproj)
    #[arg(long)]
    out: Option<PathBuf>,
    /// also export this video, through FilmCraft's encoder
    #[arg(long)]
    export: Option<PathBuf>,
    /// the render template the look is composed from
    #[arg(long, default_value = "tools/video-template.json")]
    template: PathBuf,
    /// the filmcraft CLI to drive (default: `filmcraft-cli` on PATH)
    #[arg(long, default_value = "filmcraft-cli")]
    filmcraft_cli: PathBuf,
    /// seconds of silence between chapters
    #[arg(long, default_value_t = 0.8)]
    chapter_gap: f64,
    /// export only the first N seconds (a timing check)
    #[arg(long)]
    preview: Option<f64>,
    /// sequence frame rate (default: the template's)
    #[arg(long)]
    fps: Option<f64>,
    /// the sequence's frame: WIDTHxHEIGHT (default: the template's canvas)
    #[arg(long)]
    canvas: Option<String>,
    /// skip burning captions into the export (the .srt is still written)
    #[arg(long)]
    no_subs: bool,
    /// overwrite an existing project
    #[arg(long)]
    force: bool,
    /// print the plan and exit, without running FilmCraft
    #[arg(long)]
    plan: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let root = std::env::current_dir().context("resolving the working directory")?;
    // The look is the template's: composure, type and colour all come from the
    // same file `bm-video` renders from, so FilmCraft builds it itself.
    let look = look::Look::load(&root.join(&cli.template))?;
    let canvas = match &cli.canvas {
        Some(text) => parse_canvas(text)?,
        None => (look.canvas.size[0], look.canvas.size[1]),
    };
    let fps = cli.fps.unwrap_or_else(|| look.fps());
    let manifest = model::Manifest::load(&cli.acts)?;
    // `join` on an absolute path yields that path, so both forms work.
    let workspace = root.join(&cli.workspace);
    let mut plan = plan::Plan::build(&manifest, &workspace, cli.chapter_gap, look, canvas)?;
    // The sequence is made at `fps`, and every clip edge the plan sends is put on
    // that frame grid, so an override has to reach the plan too.
    plan.fps = fps;
    // Who speaks, and the art each of them wears. The paths in the template are
    // relative to this tree, the one the template was read from.
    let portraits = plan.portraits(&root)?;
    for path in &portraits.missing {
        eprintln!(
            "warning: no portrait art at {} — its speaker's badge is left out",
            path.display()
        );
    }

    let out = match &cli.out {
        Some(p) => root.join(p),
        None => root.join("renders").join(format!("{}.fcproj", plan.name)),
    };
    let srt_path = out.with_extension("srt");
    let script_path = out.with_extension("script.jsonl");

    if cli.plan {
        report(&plan, &out);
        println!("  (lengths from the cue sheets — a build takes FilmCraft's own)");
        return Ok(());
    }
    if out.exists() && !cli.force {
        bail!("{} exists — pass --force to overwrite it", out.display());
    }
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    let mut fc = filmcraft::Driver::new(cli.filmcraft_cli.clone());
    fc.new_project(&out, &plan.name)?;
    // The sprite tracks are the template's, so how many there are is known
    // before the sequence is made; where they sit is only known once the
    // chapters have been laid out.
    let sprites = plan.look.sprites().count();
    fc.new_sequence(&plan.name, canvas, fps, plan.video_tracks(&portraits, sprites))?;
    let items = fc.import(&plan.mp3s())?;
    plan.align(&items, &fc.item_durations()?, cli.chapter_gap)?;
    let art = fc.import(&portraits.files)?;
    let sprites = element::Elements::build(&plan.look, &root, canvas.0, canvas.1, plan.total)?;
    for path in &sprites.missing {
        eprintln!("warning: no picture at {} — that sprite is left out", path.display());
    }
    let sprite_art = fc.import(&sprites.files)?;
    report(&plan, &out);
    if !portraits.files.is_empty() {
        println!(
            "  portraits: {} cues across {} track(s), {} speaker{}",
            portraits.badges.iter().filter(|b| b.is_some()).count(),
            portraits.lanes,
            portraits.files.len(),
            if portraits.files.len() == 1 { "" } else { "s" }
        );
    }

    std::fs::write(&srt_path, plan.srt())
        .with_context(|| format!("writing {}", srt_path.display()))?;
    let steps = plan.steps(&items, &srt_path, &portraits, &art, &sprites, &sprite_art)?;
    std::fs::write(&script_path, plan::script(&steps))
        .with_context(|| format!("writing {}", script_path.display()))?;
    let replies = fc.run_script(&script_path)?;
    println!("  built: {} steps, captions {}", replies.len(), srt_path.display());
    warn_missing_fonts(&fc, &plan.look)?;

    // The badges react to the mix, which is an ffmpeg pass per chapter — the
    // slow part of a build that has portraits in it.
    if portraits.lanes > 0 {
        println!("  speech: measuring {} chapter(s)…", plan.spans.len());
        plan.listen(fps, &portraits.react)?;
    }

    // The thumb and the badges can only move once their clips exist and have
    // told us their ids, so the animation is a second pass over the project.
    let moving = plan.animate(&steps, &replies, &portraits, &sprites)?;
    if !moving.is_empty() {
        let anim_path = out.with_extension("anim.jsonl");
        std::fs::write(&anim_path, plan::script(&moving))
            .with_context(|| format!("writing {}", anim_path.display()))?;
        println!("  moving: {} steps, {}", fc.run_script(&anim_path)?.len(), anim_path.display());
    }

    match &cli.export {
        Some(video) => {
            let video = root.join(video);
            let result = fc.export(&video, cli.preview, !cli.no_subs)?;
            println!("  exported {}", export_line(&result, &video));
        }
        None => println!("  open it: filmcraft {}", out.display()),
    }
    Ok(())
}

/// The edit, as the numbers a person can check against the audio.
fn report(plan: &plan::Plan, out: &Path) {
    println!(
        "{} — {} chapters in {} acts, {} captions, {}",
        plan.name,
        plan.spans.len(),
        plan.acts.len(),
        plan.captions(),
        plan::clock(plan.total)
    );
    for s in &plan.spans {
        println!("  ch.{:<3} {}  {}", s.n, plan::clock(s.start), plan::clock(s.start + s.dur));
    }
    for a in &plan.acts {
        println!("  act {}  {}  {}", plan::clock(a.start), a.chapters.len(), a.title);
    }
    println!("  project {}", out.display());
}

/// The template's fonts this machine does not have. FilmCraft renders in system
/// fonts, so the plate only matches the template where they are installed.
fn warn_missing_fonts(fc: &filmcraft::Driver, look: &look::Look) -> Result<()> {
    for (role, family, file) in look.missing_fonts(&fc.fonts()?) {
        eprintln!(
            "warning: template font {family} ({role}) is not installed — FilmCraft has rendered a \
             fallback in its place"
        );
        if !file.is_empty() {
            eprintln!("         install it: cp {file} ~/Library/Fonts/");
        }
    }
    Ok(())
}

/// `WIDTHxHEIGHT`, as one number pair we can put in a command's parameters.
fn parse_canvas(text: &str) -> Result<(u32, u32)> {
    let (w, h) = text
        .split_once(['x', 'X'])
        .with_context(|| format!("--canvas wants WIDTHxHEIGHT, got {text}"))?;
    let size = |s: &str| -> Result<u32> {
        s.trim()
            .parse::<u32>()
            .with_context(|| format!("--canvas: {s} is not a pixel count"))
    };
    let (w, h) = (size(w)?, size(h)?);
    if w == 0 || h == 0 {
        bail!("--canvas: both numbers must be at least one pixel");
    }
    Ok((w, h))
}

/// What `file.exportMedia` answered, in one line. With `wait` the counters sit
/// under the reply's `result`, beside the path and the job id.
fn export_line(reply: &serde_json::Value, path: &Path) -> String {
    let inner = reply.get("result").unwrap_or(reply);
    let count = |key: &str| inner.get(key).and_then(serde_json::Value::as_i64);
    match (count("bytes"), count("frames")) {
        (Some(bytes), Some(frames)) => {
            format!("{} — {frames} frames, {bytes} bytes", path.display())
        }
        _ => path.display().to_string(),
    }
}
