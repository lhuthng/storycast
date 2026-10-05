//! The chunked render: plan the timeline, compose chunks in parallel, concat.
//!
//! A render is split into fixed-length *chunks* of frames. Each chunk is
//! composed and encoded on its own and named by a hash of everything that
//! affects its pixels, so a finished chunk is a cache hit on the next run: the
//! job can be stopped and resumed at any time, and editing one speaker's
//! portrait invalidates only the chunks that speaker appears in.

use anyhow::{Context, Result};
use image::RgbaImage;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use crate::ffmpeg;
use crate::model::{self, Chapter, Manifest};
use crate::paint;
use crate::raster;
use crate::sticker;
use crate::template::{Rect, Rgba, StickerCfg, Template};
use crate::text::{self, Caption, Fonts};

mod layers;
use layers::Kind;

/// Everything a run needs, resolved before any work starts.
pub struct Options {
    pub root: PathBuf,
    pub workspace: PathBuf,
    pub outdir: PathBuf,
    pub name: String,
    pub manifest: Manifest,
    pub template: Template,
    /// Where the template and the manifest came from, so a distributed stage can
    /// mirror them by relative path.
    pub template_path: PathBuf,
    pub acts_path: PathBuf,
    /// Render only this slice of the chunk plan — a worker's assignment.
    pub slice: Option<(usize, usize)>,
    /// The plan owner's clock, when this run is a worker's. Probing instead
    /// would give a different answer on a different ffmpeg build.
    pub plan: Option<Plan>,
    pub template_digest: [u8; 32],
    pub chapter_gap: f64,
    pub preview: Option<f64>,
    pub no_subs: bool,
    pub chunk_secs: f64,
    pub jobs: usize,
    pub preset: String,
    pub crf: u32,
    pub rebuild: bool,
    pub dry_run: bool,
    pub keep_parts: bool,
}

/// What the caption layer draws on one frame.
#[derive(Clone, Copy)]
struct CapState {
    idx: i32,
    alpha: f32,
}

/// What the portrait layer draws on one frame.
#[derive(Clone, Copy)]
struct StickState {
    art: i32,
    alpha: f32,
    amount: f32,
}

struct StickerArt {
    image: RgbaImage,
    digest: [u8; 32],
}

/// Immutable, shared by every worker.
struct Ctx {
    t: Template,
    fonts: Arc<Fonts>,
    w: u32,
    h: u32,
    fps: u32,
    total: f64,
    preset: String,
    crf: u32,
    windows: Vec<(f64, f64)>,
    act_titles: Vec<String>,
    paper: Arc<RgbaImage>,
    plates: Vec<Arc<RgbaImage>>,
    plate_box: Rect,
    geom: paint::BarGeom,
    spin_s: f64,
    thumb: Option<(Vec<Arc<RgbaImage>>, u32)>,
    cap_tl: Vec<CapState>,
    stick_tl: Vec<StickState>,
    caps: Vec<Caption>,
    arts: Vec<Arc<StickerArt>>,
    s_box: (u32, u32),
    ratio: f64,
    s_cfg: Option<StickerCfg>,
    zx: i32,
    zy: i32,
    zw: u32,
    zh: u32,
    stick_at: (i32, i32),
    ink: Rgba,
    draw_caps: bool,
    /// The composition: what to draw, in the template's order.
    layers: Vec<layers::Layer>,
    sprites: Vec<Arc<layers::Sprite>>,
}

impl Ctx {
    fn act_at(&self, frame: usize) -> usize {
        let t = frame as f64 / self.fps as f64;
        for (i, (a, b)) in self.windows.iter().enumerate() {
            if t >= *a && t < *b {
                return i;
            }
        }
        self.windows.len().saturating_sub(1)
    }
}

/// Per-chunk tile cache. Bounded: a chunk only needs the captions and the
/// quantised portrait states its own span touches.
struct ChunkCache {
    caps: HashMap<usize, RgbaImage>,
    sticks: HashMap<(usize, u32), RgbaImage>,
    s_box: (u32, u32),
}

impl ChunkCache {
    fn new(s_box: (u32, u32)) -> ChunkCache {
        ChunkCache { caps: HashMap::new(), sticks: HashMap::new(), s_box }
    }

    fn caption(&mut self, ctx: &Ctx, idx: usize) -> &RgbaImage {
        self.caps.entry(idx).or_insert_with(|| {
            text::caption_tile(&ctx.fonts, ctx.zw, ctx.zh, &ctx.caps[idx].text, ctx.ink)
        })
    }

    fn sticker(&mut self, ctx: &Ctx, art: usize, amount: f32) -> &RgbaImage {
        // The squash is quantised, so a chapter reuses a few hundred tiles.
        let q = amount_level(amount);
        let key = (art, q);
        if !self.sticks.contains_key(&key) {
            let tile = sticker::sticker_tile(
                &ctx.arts[art].image,
                ctx.ratio,
                self.s_box,
                q as f32 / AMOUNT_LEVELS,
                ctx.s_cfg.as_ref(),
            );
            self.sticks.insert(key, tile);
        }
        self.sticks.get(&key).expect("just inserted")
    }
}

// --------------------------------------------------------------------------- #
// the frame
// --------------------------------------------------------------------------- #

fn compose_frame(ctx: &Ctx, frame: usize, cache: &mut ChunkCache, out: &mut RgbaImage) {
    let act = ctx.act_at(frame);
    let tsec = frame as f64 / ctx.fps as f64;
    for l in &ctx.layers {
        match l.kind {
            Kind::Paper => out.copy_from_slice(ctx.paper.as_raw()),
            Kind::ActPlate => {
                raster::blit(out, &ctx.plates[act], ctx.plate_box.x0(), ctx.plate_box.y0(), 1.0)
            }
            Kind::Bar => paint::draw_bar(out, &ctx.t, &ctx.geom, act),
            Kind::Thumb => {
                if let Some((frames, dia)) = &ctx.thumb {
                    let n = frames.len();
                    if n > 0 {
                        let frac = (tsec % ctx.spin_s) / ctx.spin_s;
                        let idx = ((frac * n as f64).round() as usize) % n;
                        let x = ctx.geom.left + (tsec / ctx.total) * ctx.geom.width - *dia as f64 / 2.0;
                        let y = ctx.geom.cy - *dia as f64 / 2.0;
                        raster::blit(out, &frames[idx], x.round() as i32, y.round() as i32, 1.0);
                    }
                }
            }
            Kind::Captions => {
                let cs = ctx.cap_tl[frame];
                if ctx.draw_caps && cs.idx >= 0 && cs.alpha > 0.0 {
                    let tile = cache.caption(ctx, cs.idx as usize);
                    raster::blit(out, tile, ctx.zx, ctx.zy, cs.alpha);
                }
            }
            Kind::Portraits => {
                let ss = ctx.stick_tl[frame];
                if ss.art >= 0 && ss.alpha > 0.0 {
                    let tile = cache.sticker(ctx, ss.art as usize, ss.amount);
                    raster::blit(out, tile, ctx.stick_at.0, ctx.stick_at.1, ss.alpha);
                }
            }
            Kind::Sprite => {
                if let Some(s) = l.sprite.and_then(|i| ctx.sprites.get(i)) {
                    let (x, y, idx) = s.at(frame, ctx.fps);
                    raster::blit(out, &s.frames[idx], x, y, 1.0);
                }
            }
        }
    }
}

/// Fade ramps for a caption/run span, in frames.
fn ramps(span: usize, fade: usize) -> (usize, usize, Vec<f32>, Vec<f32>) {
    let n_in = fade.min((span / 3).max(1));
    let n_out = fade.min(span.saturating_sub(n_in + 1));
    let ups = (1..=n_in).map(|k| k as f32 / (n_in + 1) as f32).collect();
    let downs = (1..=n_out).map(|k| k as f32 / (n_out + 1) as f32).collect();
    (n_in, n_out, ups, downs)
}

fn caption_timeline(caps: &[Caption], fps: u32, fade_s: f64, total_frames: usize) -> Vec<CapState> {
    let mut tl = vec![CapState { idx: -1, alpha: 0.0 }; total_frames];
    let fade = ((fade_s.clamp(0.1, 0.5)) * fps as f64).round().max(1.0) as usize;
    for (i, c) in caps.iter().enumerate() {
        let a = (c.start * fps as f64).round() as usize;
        let b = (c.end * fps as f64).round().max(a as f64 + 1.0) as usize;
        let (n_in, n_out, ups, downs) = ramps(b.saturating_sub(a), fade);
        let hold = b.saturating_sub(a).saturating_sub(n_in + n_out);
        for (k, u) in ups.iter().enumerate() {
            let f = a + k;
            if f < total_frames {
                tl[f] = CapState { idx: i as i32, alpha: *u };
            }
        }
        for k in 0..hold {
            let f = a + n_in + k;
            if f < total_frames {
                tl[f] = CapState { idx: i as i32, alpha: 1.0 };
            }
        }
        for (k, u) in downs.iter().enumerate() {
            let f = a + n_in + hold + k;
            if f < total_frames {
                tl[f] = CapState { idx: i as i32, alpha: 1.0 - *u };
            }
        }
    }
    tl
}

fn sticker_timeline(
    ctx_caps: &[Caption],
    art_of: &[i32],
    env: &[f32],
    fps: u32,
    total_frames: usize,
) -> Vec<StickState> {
    let mut tl = vec![StickState { art: -1, alpha: 0.0, amount: 0.0 }; total_frames];
    // The portrait is loud only over its own speaker's frames: the envelope is
    // read off the mixed chapter, so ungated it reacts to the narrator and the bed.
    let mut talk = vec![0.0f32; total_frames];
    for (c, art) in ctx_caps.iter().zip(art_of) {
        if *art < 0 {
            continue;
        }
        let a = ((c.start * fps as f64).round().max(0.0) as usize).min(total_frames);
        let b = ((c.end * fps as f64).round().max(0.0) as usize).clamp(a, total_frames);
        for (slot, v) in talk[a..b].iter_mut().zip(&env[a..b]) {
            *slot = slot.max(*v);
        }
    }
    // A run: consecutive captions from one speaker with no gap between them.
    let mut runs: Vec<(i32, usize, usize)> = Vec::new();
    for (c, art) in ctx_caps.iter().zip(art_of) {
        if *art < 0 {
            continue;
        }
        let a = (c.start * fps as f64).round() as usize;
        let b = (c.end * fps as f64).round().max(a as f64 + 1.0) as usize;
        match runs.last_mut() {
            Some(last) if last.0 == *art && a <= last.2 => last.2 = last.2.max(b),
            _ => runs.push((*art, a, b)),
        }
    }
    let fade = (0.3 * fps as f64).round().max(1.0) as usize;
    for (art, a, b) in runs {
        let (n_in, n_out, ups, downs) = ramps(b.saturating_sub(a), fade);
        let hold = b.saturating_sub(a).saturating_sub(n_in + n_out);
        for (k, u) in ups.iter().enumerate() {
            let f = a + k;
            if f < total_frames {
                tl[f] = StickState { art, alpha: *u, amount: talk[f] };
            }
        }
        for k in 0..hold {
            let f = a + n_in + k;
            if f < total_frames {
                tl[f] = StickState { art, alpha: 1.0, amount: talk[f] };
            }
        }
        for (k, u) in downs.iter().enumerate() {
            let f = a + n_in + hold + k;
            if f < total_frames {
                tl[f] = StickState { art, alpha: 1.0 - *u, amount: talk[f] };
            }
        }
    }
    tl
}

/// A hash of everything that decides a chunk's pixels; the file name wears it.
fn chunk_name(ctx: &Ctx, digest: &[u8; 32], f0: usize, f1: usize) -> String {
    let mut h = Sha256::new();
    h.update(digest);
    h.update(ctx.w.to_le_bytes());
    h.update(ctx.h.to_le_bytes());
    h.update(ctx.fps.to_le_bytes());
    h.update(ctx.total.to_bits().to_le_bytes());
    h.update((f0 as u64).to_le_bytes());
    h.update((f1 as u64).to_le_bytes());
    h.update(ctx.preset.as_bytes());
    h.update(ctx.crf.to_le_bytes());
    h.update(ctx.zw.to_le_bytes());
    h.update(ctx.zh.to_le_bytes());
    h.update(ctx.s_box.0.to_le_bytes());
    h.update(ctx.s_box.1.to_le_bytes());
    for (a, b) in &ctx.windows {
        h.update(a.to_bits().to_le_bytes());
        h.update(b.to_bits().to_le_bytes());
    }
    h.update((ctx.ratio.to_bits()).to_le_bytes());
    h.update(ctx.zx.to_le_bytes());
    h.update(ctx.zy.to_le_bytes());
    h.update(ctx.stick_at.0.to_le_bytes());
    h.update(ctx.stick_at.1.to_le_bytes());
    let t0 = f0 as f64 / ctx.fps as f64;
    let t1 = f1 as f64 / ctx.fps as f64;
    // Only the acts whose title plate shows in this chunk.
    for (i, (a, b)) in ctx.windows.iter().enumerate() {
        if *a < t1 && *b > t0 {
            h.update((i as u32).to_le_bytes());
            h.update(ctx.act_titles[i].as_bytes());
            h.update([0]);
        }
    }
    let mut caps_used: HashSet<usize> = HashSet::new();
    let mut arts_used: HashSet<usize> = HashSet::new();
    for f in f0..f1 {
        let c = ctx.cap_tl[f];
        h.update(c.idx.to_le_bytes());
        h.update(quant(c.alpha).to_le_bytes());
        let s = ctx.stick_tl[f];
        h.update(s.art.to_le_bytes());
        h.update(quant(s.alpha).to_le_bytes());
        // At the level the tile is built from, never finer: the envelope is
        // read off ffmpeg, and two builds of it disagree in the last digits.
        // Quantised finer than the pixels, that noise alone re-renders chunks
        // whose pixels are identical.
        h.update(amount_level(s.amount).to_le_bytes());
        if c.idx >= 0 {
            caps_used.insert(c.idx as usize);
        }
        if s.art >= 0 {
            arts_used.insert(s.art as usize);
        }
    }
    // Sorted, not iterated as a set: `HashSet` order varies per process, and a
    // key that differs between two identical runs is not a cache, it is a
    // re-render.
    let mut caps_used: Vec<usize> = caps_used.into_iter().collect();
    caps_used.sort_unstable();
    for i in caps_used {
        h.update(ctx.caps[i].text.as_bytes());
        h.update([0]);
    }
    // A speaker's portrait enters the key only where that speaker speaks, so
    // swapping one logo re-renders only their chunks.
    let mut arts: Vec<usize> = arts_used.into_iter().collect();
    arts.sort_unstable();
    for i in arts {
        h.update(ctx.arts[i].digest);
    }
    // A sprite is on screen for the whole video, so its bytes belong in every
    // chunk's key: swapping the art is a different video, not a stale one.
    for s in &ctx.sprites {
        h.update(s.digest);
    }
    let d = h.finalize();
    d.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn quant(x: f32) -> u32 {
    (x.clamp(0.0, 8.0) * 1024.0).round() as u32
}

/// How many squash levels the portrait is rendered at.
const AMOUNT_LEVELS: f32 = 16.0;

/// The level a frame's envelope renders at. The tile and the chunk key are
/// both a function of this integer, so the two cannot drift apart.
fn amount_level(amount: f32) -> u32 {
    (amount.clamp(0.0, 8.0) * AMOUNT_LEVELS).round() as u32
}

// --------------------------------------------------------------------------- #
// the plan
// --------------------------------------------------------------------------- #

/// The timeline as plain numbers, so a box renders the plan owner's clock
/// instead of deriving its own.
///
/// ffmpeg's container duration estimate differs between builds: on one chapter
/// here, two builds agreed to the decoded byte and disagreed by 38 ms. A
/// timeline that differs by a frame is a different chunk plan and different
/// chunk keys, so every chunk a box rendered would be thrown away.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Plan {
    pub chunk_frames: usize,
    pub total_frames: usize,
    pub total: f64,
    pub chapters: Vec<ChapterClock>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct ChapterClock {
    pub dur: f64,
    pub start: f64,
}

/// The frame clock a render is planned on.
///
/// Shared by the renderer and the dispatcher on purpose: a chunk index must mean
/// the same frame range on every box, and the chunk key is a hash of exactly
/// that range and its inputs, so the plan is reproducible from the inputs alone.
pub struct Timeline {
    pub chapters: Vec<Chapter>,
    pub starts: Vec<f64>,
    pub total: f64,
    pub total_frames: usize,
    pub chunk_frames: usize,
    pub n_chunks: usize,
}

impl Timeline {
    /// Hand this clock to a worker, so it renders what we planned.
    pub fn plan(&self) -> Plan {
        Plan {
            chunk_frames: self.chunk_frames,
            total_frames: self.total_frames,
            total: self.total,
            chapters: self
                .chapters
                .iter()
                .map(|c| ChapterClock { dur: c.dur, start: c.start })
                .collect(),
        }
    }
}

pub fn timeline(o: &Options) -> Result<Timeline> {
    let fps = o.template.fps();
    let mut chapters: Vec<Chapter> = Vec::new();
    for act in &o.manifest.acts {
        for n in &act.chapters {
            let (mp3, cues_path) = model::find_chapter(&o.workspace, *n)?;
            let cues = model::load_cues(&cues_path)?;
            chapters.push(Chapter { act: act.act, mp3, cues, dur: 0.0, start: 0.0 });
        }
    }
    if chapters.is_empty() {
        anyhow::bail!("the acts manifest names no chapters");
    }
    if let Some(p) = &o.plan {
        if p.chapters.len() != chapters.len() {
            anyhow::bail!(
                "the plan covers {} chapters, the manifest names {}",
                p.chapters.len(),
                chapters.len()
            );
        }
        for (ch, c) in chapters.iter_mut().zip(&p.chapters) {
            ch.dur = c.dur;
            ch.start = c.start;
        }
        let chunk_frames = p.chunk_frames.max(1);
        return Ok(Timeline {
            starts: chapters.iter().map(|c| c.start).collect(),
            total: p.total,
            total_frames: p.total_frames,
            chunk_frames,
            n_chunks: p.total_frames.div_ceil(chunk_frames),
            chapters,
        });
    }
    for ch in &mut chapters {
        ch.dur = ffmpeg::duration(&ch.mp3)?;
    }
    let mut starts = Vec::with_capacity(chapters.len());
    let mut clock = 0.0;
    for ch in &mut chapters {
        ch.start = clock;
        starts.push(clock);
        clock += ch.dur + o.chapter_gap;
    }
    let full_total = starts.last().copied().unwrap_or(0.0)
        + chapters.last().map(|c| c.dur).unwrap_or(0.0);
    let total = o.preview.map_or(full_total, |p| full_total.min(p));
    let total_frames = (total * fps as f64).round().max(1.0) as usize;
    let chunk_frames = ((o.chunk_secs * fps as f64).round() as usize).max(1);
    let n_chunks = total_frames.div_ceil(chunk_frames);
    Ok(Timeline { chapters, starts, total, total_frames, chunk_frames, n_chunks })
}

pub fn chunks_dir(o: &Options) -> PathBuf {
    o.outdir.join(format!("{}.parts", o.name)).join("chunks")
}

// --------------------------------------------------------------------------- #
// run
// --------------------------------------------------------------------------- #

pub fn run(o: &Options) -> Result<()> {
    let started = Instant::now();
    let (w, h) = (o.template.canvas.size[0], o.template.canvas.size[1]);
    let fps = o.template.fps();
    std::fs::create_dir_all(&o.outdir)
        .with_context(|| format!("creating {}", o.outdir.display()))?;

    // ---- inputs ---------------------------------------------------------- #
    let Timeline { chapters, starts, total, total_frames, chunk_frames, .. } = timeline(o)?;

    // One act's window runs from its first chapter to the next act's first.
    let mut windows: Vec<(f64, f64)> = Vec::new();
    for act in &o.manifest.acts {
        let start = chapters
            .iter()
            .find(|c| c.act == act.act)
            .map(|c| c.start)
            .unwrap_or(0.0);
        let end = chapters
            .iter()
            .filter(|c| c.act != act.act && c.start > start)
            .map(|c| c.start)
            .fold(f64::INFINITY, f64::min);
        let end = if end.is_finite() { end.min(total) } else { total };
        windows.push((start.min(total), end));
    }
    let act_durs: Vec<f64> = windows.iter().map(|(a, b)| (b - a).max(0.0)).collect();

    let boxes = o.template.boxes(w, h);
    let plate_box = o.template.box_of(&boxes, "act_title")?;
    let sub_zone = o.template.box_of(&boxes, "subtitle")?;
    let (zx, zy, zw, zh) = (sub_zone.x0(), sub_zone.y0(), sub_zone.w(), sub_zone.h());
    let fonts = Arc::new(Fonts::load(&o.template, w)?);

    let mut caps = text::build_captions(&o.template, &fonts, &chapters, &starts);
    caps.retain(|c| c.start < total - 0.0001);
    let srt_path = o.outdir.join(format!("{}.srt", o.name));
    let vtt_path = o.outdir.join(format!("{}.vtt", o.name));
    text::write_subtitles(&caps, &srt_path, &vtt_path)?;

    // ---- portraits ------------------------------------------------------- #
    let s_cfg = o.template.speaker_sticker.clone();
    let ratio = match &s_cfg {
        Some(cfg) => sticker::sticker_ratio(&o.template, &o.root) * cfg.zoom,
        None => 1.0,
    };
    let mut art_paths: Vec<PathBuf> = Vec::new();
    let mut art_of: Vec<i32> = vec![-1; caps.len()];
    if let Some(cfg) = &s_cfg {
        let mut seen: HashMap<String, usize> = HashMap::new();
        for (i, c) in caps.iter().enumerate() {
            let Some(p) = sticker::sticker_for(&o.root, &c.speaker, cfg) else {
                continue;
            };
            let key = p.to_string_lossy().into_owned();
            let idx = *seen.entry(key).or_insert_with(|| {
                art_paths.push(p.clone());
                art_paths.len() - 1
            });
            art_of[i] = idx as i32;
        }
    }
    let mut arts: Vec<Arc<StickerArt>> = Vec::new();
    for p in &art_paths {
        let bytes = std::fs::read(p).with_context(|| format!("reading portrait {}", p.display()))?;
        let mut d = Sha256::new();
        d.update(&bytes);
        let digest: [u8; 32] = d.finalize().into();
        arts.push(Arc::new(StickerArt { image: sticker::sticker_art(p)?, digest }));
    }
    let has_stickers = !arts.is_empty() && art_of.iter().any(|a| *a >= 0);
    let images: Vec<&RgbaImage> = arts.iter().map(|a| &a.image).collect();
    let s_box = match &s_cfg {
        Some(cfg) if has_stickers => sticker::sticker_box(cfg, &images, ratio),
        _ => (0, 0),
    };

    // ---- the per-frame overlay timelines --------------------------------- #
    let draw_caps = !o.no_subs && !caps.is_empty();
    let cap_tl = if draw_caps {
        caption_timeline(&caps, fps, o.template.subtitle.fade_s, total_frames)
    } else {
        vec![CapState { idx: -1, alpha: 0.0 }; total_frames]
    };
    let stick_tl = if has_stickers {
        // Only the chapters this run actually renders: the envelope is an ffmpeg
        // pass per chapter, and a worker's slice needs its own window only. The
        // values inside the window are identical either way, so a chunk's key
        // does not depend on how the work was split.
        let window = match o.slice {
            Some((from, count)) => (
                (from * chunk_frames).min(total_frames),
                ((from + count) * chunk_frames).min(total_frames),
            ),
            None => (0, total_frames),
        };
        let env = speech_envelopes(&chapters, fps, total_frames, s_cfg.as_ref(), o.jobs, window)?;
        sticker_timeline(&caps, &art_of, &env, fps, total_frames)
    } else {
        vec![StickState { art: -1, alpha: 0.0, amount: 0.0 }; total_frames]
    };

    // ---- the static pieces ------------------------------------------------ #
    let paper = Arc::new(paint::paper(&o.template, w, h));
    let mut plates = Vec::new();
    for act in &o.manifest.acts {
        plates.push(Arc::new(paint::act_title_plate(
            &o.template, &fonts, &boxes, act, &paper, w,
        )?));
    }
    let geom = paint::timeline_geometry(&o.template, &act_durs, w, h);
    let mut spin_s = 1.2;
    let thumb = match paint::thumb_art(&o.template, h, &o.root)? {
        Some((art, spin)) => {
            if let Some(th) = &o.template.timeline.thumb {
                spin_s = th.spin_s.max(0.05);
            }
            let dia = art.width();
            let frames = if spin { paint::thumb_frames(&o.template, &art, fps) } else { vec![art] };
            Some((frames.into_iter().map(Arc::new).collect(), dia))
        }
        None => None,
    };
    let (layers, sprites) = layers::build(&o.template, &boxes, &o.root, fps, total)?;
    let stick_at = match &s_cfg {
        Some(cfg) if has_stickers => (
            (w as f64 * cfg.at[0] - s_box.0 as f64 / 2.0).round() as i32,
            (o.template.canvas.size[1] as f64 * cfg.at[1] - s_box.1 as f64 / 2.0).round() as i32,
        ),
        _ => (0, 0),
    };

    let ctx = Ctx {
        t: o.template.clone(),
        fonts: fonts.clone(),
        w,
        h,
        fps,
        total,
        preset: o.preset.clone(),
        crf: o.crf,
        windows,
        act_titles: o.manifest.acts.iter().map(|a| a.title.clone()).collect(),
        paper,
        plates,
        plate_box,
        geom,
        spin_s,
        thumb,
        cap_tl,
        stick_tl,
        caps,
        arts,
        s_box,
        ratio,
        s_cfg,
        zx,
        zy,
        zw,
        zh,
        stick_at,
        ink: o.template.colour("subtitle")?,
        draw_caps,
        layers,
        sprites,
    };

    // ---- plan ------------------------------------------------------------- #
    let all: Vec<(usize, usize)> = (0..total_frames)
        .step_by(chunk_frames)
        .map(|f0| (f0, (f0 + chunk_frames).min(total_frames)))
        .collect();
    // A worker is handed an index range into this plan; the keys it computes are
    // the same ones the plan owner did, so its chunks land in the same cache.
    let (from, count) = o.slice.unwrap_or((0, all.len()));
    let from = from.min(all.len());
    let upto = (from + count).min(all.len());
    let chunks = &all[from..upto];
    let mut planned: Vec<(usize, usize, PathBuf)> = Vec::new();
    let mut cached = 0usize;
    for (f0, f1) in chunks {
        let key = chunk_name(&ctx, &o.template_digest, *f0, *f1);
        let dir = chunks_dir(o);
        let path = dir.join(format!("{f0:08}-{key}.mp4"));
        if path.exists() && !o.rebuild {
            cached += 1;
        }
        planned.push((*f0, *f1, path));
    }
    println!(
        "{}: {} acts, {} chapters, {} total, {} captions, {} chunks of {:.0} frames ({} cached)",
        o.name,
        o.manifest.acts.len(),
        chapters.len(),
        text::ts(total, false),
        ctx.caps.len(),
        planned.len(),
        chunk_frames as f64,
        cached
    );
    if o.dry_run {
        for (f0, f1, p) in &planned {
            println!("  [{f0:>8}..{f1:>8}) {}", p.file_name().unwrap_or_default().to_string_lossy());
        }
        return Ok(());
    }
    std::fs::create_dir_all(chunks_dir(o))?;

    // ---- render ----------------------------------------------------------- #
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let jobs = o.jobs.max(1).min(cores.max(1));
    let threads_per_job = (cores / jobs).max(1);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(jobs)
        .build()
        .context("building the worker pool")?;
    let t_render = Instant::now();
    let results: Vec<Result<()>> = pool.install(|| {
        planned
            .par_iter()
            .map(|(f0, f1, path)| render_chunk(&ctx, *f0, *f1, path, threads_per_job, o.rebuild))
            .collect()
    });
    let mut done = 0;
    for r in results {
        r?;
        done += 1;
    }
    let elapsed = t_render.elapsed();
    println!(
        "  chunks: {done} ready in {:.1}s ({} rendered, {} from cache, {:.1} fps across {jobs} workers)",
        elapsed.as_secs_f64(),
        planned.len() - cached,
        cached,
        (total_frames as f64) / elapsed.as_secs_f64().max(1e-6)
    );
    if o.slice.is_some() {
        // A worker's job is the cache, not the file: it renders its slice into
        // the shared chunk directory and the plan owner assembles.
        println!("  chunk cache: {}", chunks_dir(o).display());
        return Ok(());
    }

    // ---- assemble --------------------------------------------------------- #
    let parts = o.outdir.join(format!("{}.parts", o.name));
    let list = parts.join("chunks.txt");
    let mut body = String::from("ffconcat version 1.0\n");
    for (_, _, p) in &planned {
        body.push_str(&format!("file '{}'\n", p.canonicalize()?.to_string_lossy()));
    }
    std::fs::write(&list, body)?;
    let video = parts.join("video.mp4");
    let audio = parts.join("audio.m4a");
    let out_mp4 = o.outdir.join(format!("{}.mp4", o.name));
    println!("  assembling {} chunks…", planned.len());
    ffmpeg::concat(&list, &video)?;
    ffmpeg::build_audio(&chapters, o.chapter_gap, total, &audio)?;
    ffmpeg::mux(&video, &audio, &out_mp4, total)?;

    // The chunk cache is the point: it is what makes a re-run, a resume or a
    // single swapped portrait cheap, so it stays. Only the assembly pieces,
    // which are rebuilt from it in seconds, are cleaned up.
    if !o.keep_parts {
        let _ = std::fs::remove_file(&video);
        let _ = std::fs::remove_file(&audio);
        let _ = std::fs::remove_file(&list);
    }
    println!(
        "wrote {} ({:.1}s total; {} chunks cached in {})",
        out_mp4.display(),
        started.elapsed().as_secs_f64(),
        planned.len(),
        chunks_dir(o).display()
    );
    Ok(())
}

fn render_chunk(
    ctx: &Ctx,
    f0: usize,
    f1: usize,
    path: &Path,
    threads: usize,
    force: bool,
) -> Result<()> {
    if path.exists() && !force {
        return Ok(());
    }
    let tmp = path.with_extension("partial.mp4");
    let mut buf = raster::canvas(ctx.w, ctx.h);
    let mut cache = ChunkCache::new(ctx.s_box);
    let frames = (f0..f1).map(|f| {
        compose_frame(ctx, f, &mut cache, &mut buf);
        raster::to_rgb24(&buf)
    });
    let spec = ffmpeg::ChunkSpec {
        w: ctx.w,
        h: ctx.h,
        fps: ctx.fps,
        preset: &ctx.preset,
        crf: ctx.crf,
        threads,
    };
    ffmpeg::encode_chunk(frames, &spec, &tmp)?;
    std::fs::rename(&tmp, path).with_context(|| format!("publishing {}", path.display()))?;
    Ok(())
}

/// The speech envelope of every chapter, laid out on the whole-video clock.
fn speech_envelopes(
    chapters: &[Chapter],
    fps: u32,
    total_frames: usize,
    cfg: Option<&StickerCfg>,
    jobs: usize,
    window: (usize, usize),
) -> Result<Vec<f32>> {
    let cfg = cfg.cloned().unwrap_or_default();
    let pool = rayon::ThreadPoolBuilder::new().num_threads(jobs).build()?;
    let parts: Vec<Result<Vec<f32>>> = pool.install(|| {
        chapters
            .par_iter()
            .map(|ch| {
                let a = (ch.start * fps as f64).round() as usize;
                let b = a + (ch.dur * fps as f64).round() as usize;
                if b <= window.0 || a >= window.1 {
                    return Ok(Vec::new());
                }
                let frames = (ch.dur * fps as f64).round().max(1.0) as usize;
                sticker::speech_envelope(&ch.mp3, fps, frames, &cfg)
            })
            .collect()
    });
    let mut env = vec![0.0f32; total_frames];
    for (ch, per) in chapters.iter().zip(parts) {
        let per = per?;
        let a = (ch.start * fps as f64).round() as usize;
        for (k, v) in per.iter().enumerate() {
            if a + k < total_frames {
                env[a + k] = *v;
            }
        }
    }
    Ok(env)
}

#[cfg(test)]
mod tests;
