//! The edit: where every chapter lands, what the captions say, and the commands
//! that put them there.
//!
//! Everything is composed before FilmCraft is touched, so `--plan` prints the
//! whole edit and the run is a replay of it.

use crate::element::Elements;
use crate::filmcraft::TICKS_PER_SECOND;
use crate::look::{BarGeom, Look, React};
use crate::model::{Cue, Manifest};
use crate::portrait;
use crate::srt;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// One chapter on the assembled clock.
#[derive(Clone, Debug)]
pub struct Span {
    pub n: u32,
    pub mp3: PathBuf,
    pub cues: Vec<Cue>,
    pub start: f64,
    pub dur: f64,
    /// How loud this chapter is, per video frame, 0..1 — what its portraits
    /// react to. Empty until [`Plan::listen`] has measured it.
    pub env: Vec<f32>,
}

/// One caption's window on the assembled clock.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CueWindow {
    pub span: usize,
    pub cue: usize,
    pub start: f64,
    pub end: f64,
}

/// A cue's portrait: which of the imported art files it wears, and which
/// portrait track carries it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Badge {
    pub file: usize,
    pub lane: usize,
}

/// The book's portraits: the art to import, which cue wears what, and the
/// numbers the badge is drawn at.
#[derive(Clone, Debug, Default)]
pub struct Portraits {
    /// The art, in the order the caller must import it.
    pub files: Vec<PathBuf>,
    /// One entry per cue window, in the order [`Plan::cue_windows`] walks them.
    pub badges: Vec<Option<Badge>>,
    /// How many portrait tracks the cues need.
    pub lanes: usize,
    /// The badge's scale: the timeline thumb's own ratio.
    pub ratio: f64,
    pub at: [f64; 2],
    pub react: React,
    /// The picture the thumb is drawn from, when the template has one: an index
    /// into `files`, imported along with the portraits.
    pub thumb: Option<usize>,
    /// That picture at the thumb's diameter, as FilmCraft's `scale` takes it —
    /// the art is drawn to the template's own disc, not fitted to a box.
    pub thumb_pct: f64,
    /// Art the template names that is not on disk, for the caller to warn about.
    pub missing: Vec<PathBuf>,
}

/// One act: its title, where on the clock it begins, and its number on the
/// plate.
#[derive(Clone, Debug)]
pub struct ActSpan {
    pub act: u32,
    pub title: String,
    pub chapters: Vec<u32>,
    pub start: f64,
}

#[derive(Clone, Debug)]
pub struct Plan {
    pub name: String,
    pub spans: Vec<Span>,
    pub acts: Vec<ActSpan>,
    pub total: f64,
    /// The template the look is composed from, and the frame it is composed on.
    pub look: Look,
    pub canvas: (u32, u32),
    /// The sequence's frame rate: the envelope is measured per video frame, so
    /// it is what turns a chapter's loudness back into a time.
    pub fps: f64,
}

/// One command: the id the engine dispatches on, and its parameters.
#[derive(Clone, Debug)]
pub struct Step {
    pub id: String,
    pub params: Value,
}

/// The tracks the look is composed on. A track holds one clip at a time, so
/// everything that runs the whole book — the paper, the bar — has its own, and
/// the plate's label and title have one each because they are simultaneous.
const PAPER_TRACK: i64 = 0;
const GROOVE_TRACK: i64 = 1;
/// The bar's segments are drawn for the whole book, one per act, so each needs
/// its own track — they are the first of the look's tracks after the groove.
const SEGMENTS_FIRST: i64 = 2;

/// The layer effect of a graphic clip always sits third in its stack — `motion`,
/// `opacity`, `time_remap`, the graphic — and a shape's carries this name. The
/// thumb is a shape, and this is what its keyframes are asked for by.
const GRAPHIC_SHAPE: &str = "graphic_shape";

/// How far the badge's reaction must move between two keys before the key is
/// worth writing: a fiftieth of the squash, and far below what an eye sees on a
/// badge a few dozen pixels tall.
const BADGE_STEP: f32 = 0.02;

/// The act plate's label sits this far down the zone before the title starts,
/// as `bm-video` lays it out: the label's own line, then a tenth of it.
const LABEL_LEAD_1: f64 = 1.15;
const LABEL_LEAD_2: f64 = 0.1;

/// The plate's inset from its zone, as a fraction of the frame width.
const PLATE_PAD: f64 = 0.024;

/// `graphics.set` on the selection: the shape or text just made.
fn style(props: serde_json::Value) -> Step {
    Step { id: "graphics.set".into(), params: json!({"props": props}) }
}

/// A colour at a fraction of its own alpha, for the bar's unfilled track.
fn with_alpha(colour: &str, alpha: f64) -> String {
    let rgb = &colour[..7.min(colour.len())];
    let a = (alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("{rgb}{a:02x}")
}

pub fn ticks(seconds: f64) -> i64 {
    (seconds * TICKS_PER_SECOND).round() as i64
}

/// One instant on the sequence's frame grid. FilmCraft rounds a clip's own end
/// up to a whole frame, so a clip placed at the raw instant of the one before it
/// is refused — "V11 is not free here" — whenever a window does not land on a
/// frame, which on a real book it almost never does. Two clips that butt up
/// share the instant, so they share the frame this returns.
pub(crate) fn frame_ticks(seconds: f64, fps: f64) -> i64 {
    let per = (TICKS_PER_SECOND / fps.max(1.0)).round();
    (seconds * fps).round() as i64 * per as i64
}

/// A window as whole frames: its start and its length, in ticks.
fn frame_window(start: f64, end: f64, fps: f64) -> (i64, i64) {
    let a = frame_ticks(start, fps);
    (a, (frame_ticks(end, fps) - a).max(0))
}

/// A length in seconds a `graphics.*` command takes, back from its ticks: the
/// frame grid again, so the clip is stored exactly as long as it was asked for.
fn frame_seconds(ticks: i64) -> f64 {
    ticks as f64 / TICKS_PER_SECOND
}

impl Plan {
    /// Chapters butt up in manifest order, separated by `gap` — the same edit
    /// `bm-video` renders. The lengths here come from the cue sheets, which is
    /// what `--plan` can know without FilmCraft; [`Plan::align`] replaces them
    /// with the lengths FilmCraft decodes before anything is written.
    pub fn build(
        manifest: &Manifest,
        workspace: &Path,
        gap: f64,
        look: Look,
        canvas: (u32, u32),
    ) -> Result<Plan> {
        let mut spans: Vec<Span> = Vec::new();
        for act in &manifest.acts {
            for n in &act.chapters {
                if spans.iter().any(|s| s.n == *n) {
                    bail!("chapter {n} is listed twice in the manifest");
                }
                let (mp3, cues_path) = crate::model::find_chapter(workspace, *n)?;
                let cues = crate::model::load_cues(&cues_path)?;
                let dur = cues.span();
                if dur <= 0.0 {
                    bail!("chapter {n} has no cues — merge it first");
                }
                spans.push(Span { n: *n, mp3, cues: cues.cues, start: 0.0, dur, env: Vec::new() });
            }
        }
        if spans.is_empty() {
            bail!("the manifest lists no chapters");
        }
        let fps = look.fps();
        let mut plan = Plan {
            name: manifest.default_name(),
            spans,
            acts: Vec::new(),
            total: 0.0,
            look,
            canvas,
            fps,
        };
        plan.lay_out(gap);
        plan.acts = outline(manifest, &plan.spans)?;
        Ok(plan)
    }

    /// Re-place the edit on FilmCraft's own clock, given the ids `import`
    /// returned and the lengths it decoded.
    ///
    /// The two disagree by a second or more on a real chapter: the mp3 carries
    /// the tail the mixer wrote past its last cue. Placing the next chapter on
    /// the cue sheet's figure would overwrite that tail, so the decoded length
    /// wins — the gap is then a real gap of silence, not a cut.
    pub fn align(&mut self, items: &[i64], durations: &HashMap<i64, f64>, gap: f64) -> Result<()> {
        if items.len() != self.spans.len() {
            bail!(
                "import returned {} items for {} chapters",
                items.len(),
                self.spans.len()
            );
        }
        for (s, item) in self.spans.iter_mut().zip(items) {
            let dur = durations.get(item).copied().filter(|d| *d > 0.0).ok_or_else(|| {
                anyhow::anyhow!("FilmCraft reported no length for chapter {} (item {item})", s.n)
            })?;
            s.dur = dur;
        }
        self.lay_out(gap);
        Ok(())
    }

    /// Chapters end to end with `gap` between them, and an act at its first
    /// chapter's start. The one place the clock is computed.
    fn lay_out(&mut self, gap: f64) {
        let mut t = 0.0;
        for (i, s) in self.spans.iter_mut().enumerate() {
            if i > 0 {
                t += gap;
            }
            s.start = t;
            t += s.dur;
        }
        self.total = t;
        for a in &mut self.acts {
            a.start = a
                .chapters
                .first()
                .and_then(|n| self.spans.iter().find(|s| s.n == *n))
                .map(|s| s.start)
                .unwrap_or(0.0);
        }
    }

    /// The window an act is on screen for: its first chapter to the next act's
    /// start, which is what `bm-video` shows the plate for.
    pub fn window(&self, i: usize) -> (f64, f64) {
        let start = self.acts[i].start;
        let end = self
            .acts
            .iter()
            .map(|a| a.start)
            .filter(|s| *s > start)
            .fold(self.total, f64::min);
        (start, (end - start).max(0.0))
    }

    /// Every caption in the book, in order, with its window on the clock.
    pub fn cue_windows(&self) -> Vec<CueWindow> {
        let mut out = Vec::new();
        for (s, span) in self.spans.iter().enumerate() {
            for (c, cue) in span.cues.iter().enumerate() {
                out.push(CueWindow {
                    span: s,
                    cue: c,
                    start: span.start + cue.start,
                    end: span.start + cue.end,
                });
            }
        }
        out
    }

    /// The book's portraits: which art its speakers need, and where each cue's
    /// badge goes. A narrator gets none, an unmapped name takes the template's
    /// fallback, and art the template names but the disk lacks is reported for
    /// the caller to warn about rather than failing the build.
    ///
    /// Paths in the template are relative to `root` — the tree the template
    /// itself was read from.
    pub fn portraits(&self, root: &Path) -> Result<Portraits> {
        let mut out = Portraits {
            ratio: portrait::ratio(&self.look, root, self.canvas.1),
            badges: vec![None; self.cue_windows().len()],
            ..Portraits::default()
        };
        // The thumb's own picture is imported with the rest of the art: it is
        // the file the badge's scale is taken from, drawn rather than imitated.
        if let Some(path) = self.look.thumb_picture(root) {
            out.thumb = Some(out.files.len());
            if let Some(disc) = self.look.thumb_disc(self.canvas.1) {
                let natural = portrait::png_size(&path).map(|(w, _)| w).unwrap_or(0) as f64;
                if natural > 0.0 {
                    out.thumb_pct = (disc.dia / natural * 100.0 * 1000.0).round() / 1000.0;
                }
            }
            out.files.push(path);
        }
        let Some(sticker) = self.look.sticker() else {
            return Ok(out);
        };
        out.at = sticker.at;
        out.react = sticker.react.clone();
        // The end of each lane's last badge: two cues that overlap cannot share
        // a track, and cues of different speakers always abut or overlap, so the
        // lanes fall out of the cues themselves.
        let mut lanes: Vec<f64> = Vec::new();
        for (i, w) in self.cue_windows().iter().enumerate() {
            let cue = &self.spans[w.span].cues[w.cue];
            let Some(path) = portrait::art_for(sticker, root, &cue.speaker) else {
                continue;
            };
            let path = path.as_path();
            // The badge is sized by percentage, so only the art's existence is
            // needed here — a portrait the template names but the disk lacks is
            // left out rather than failing the build.
            if portrait::png_size(path).is_none() {
                out.missing.push(path.to_path_buf());
                continue;
            }
            let file = match out.files.iter().position(|f| f == path) {
                Some(i) => i,
                None => {
                    out.files.push(path.to_path_buf());
                    out.files.len() - 1
                }
            };
            let lane = match lanes.iter().position(|end| *end <= w.start) {
                Some(l) => l,
                None => {
                    lanes.push(0.0);
                    lanes.len() - 1
                }
            };
            lanes[lane] = w.end;
            out.badges[i] = Some(Badge { file, lane });
        }
        out.lanes = lanes.len();
        Ok(out)
    }

    /// Measure every chapter's speech, so the badges have something to react to.
    /// One ffmpeg pass per chapter, which is the slow part of a build.
    pub fn listen(&mut self, fps: f64, react: &React) -> Result<usize> {
        self.fps = fps;
        let mut measured = 0;
        for span in &mut self.spans {
            let frames = (span.dur * fps).ceil().max(1.0) as usize + 1;
            span.env = portrait::envelope(&span.mp3, fps, frames, react)?;
            measured += span.env.len();
        }
        Ok(measured)
    }

    /// How many video tracks the look is composed on: the paper, the bar's
    /// groove, a segment and a highlight per act, the plate's label and title,
    /// the thumb above them all, the portrait tracks, and the template's own
    /// sprites on top.
    pub fn video_tracks(&self, portraits: &Portraits, sprites: usize) -> u32 {
        (self.thumb_track() + 1 + portraits.lanes as i64 + sprites as i64) as u32
    }

    /// The track the template's own sprites ride, in the order it lists them:
    /// above the portrait lanes, so a sprite is never hidden by a badge.
    pub fn sprite_track(&self, lanes: usize, i: usize) -> i64 {
        self.thumb_track() + 1 + lanes as i64 + i as i64
    }

    /// The track a portrait lane occupies, by its index in the look.
    pub fn portrait_track(&self, lane: usize) -> i64 {
        self.thumb_track() + 1 + lane as i64
    }

    /// The first track carrying the playing act's raised segment — one per act,
    /// above the segments they highlight.
    fn active_first(&self) -> i64 {
        SEGMENTS_FIRST + self.acts.len() as i64
    }

    /// Where the plate's two lines go, above the bar's own tracks.
    pub fn label_track(&self) -> i64 {
        self.active_first() + self.acts.len() as i64
    }

    pub fn title_track(&self) -> i64 {
        self.label_track() + 1
    }

    /// The thumb rides above the plate, as the template stacks the layers.
    pub fn thumb_track(&self) -> i64 {
        self.title_track() + 1
    }

    pub fn mp3s(&self) -> Vec<PathBuf> {
        self.spans.iter().map(|s| s.mp3.clone()).collect()
    }

    pub fn captions(&self) -> usize {
        self.spans.iter().map(|s| s.cues.len()).sum()
    }

    /// The whole book's captions on the assembled clock, numbered in order.
    pub fn srt(&self) -> String {
        let mut entries: Vec<srt::Entry> = Vec::new();
        for s in &self.spans {
            for c in &s.cues {
                entries.push(srt::Entry {
                    start: s.start + c.start,
                    end: s.start + c.end,
                    text: c.text.clone(),
                });
            }
        }
        srt::render(&entries)
    }

    /// The commands that build the edit and its look, given the project items
    /// `import` returned — one per chapter, in manifest order. Call
    /// [`Plan::align`] first: the offsets in here are the clock it laid out.
    ///
    /// The look is FilmCraft's own: a paper ground, the act plate, the bar with
    /// the playing act raised out of it, the thumb above it and the caption
    /// style, all composed from the template `bm-video` renders from. Nothing is
    /// imported as a picture; the thumb's own movement is [`Plan::animate`], a
    /// second pass because it needs the clip id FilmCraft only reports back.
    pub fn steps(
        &self,
        items: &[i64],
        captions: &Path,
        portraits: &Portraits,
        art: &[i64],
        sprites: &Elements,
        sprite_art: &[i64],
    ) -> Result<Vec<Step>> {
        if items.len() != self.spans.len() {
            bail!(
                "import returned {} items for {} chapters",
                items.len(),
                self.spans.len()
            );
        }
        if art.len() != portraits.files.len() {
            bail!(
                "import returned {} items for {} portraits",
                art.len(),
                portraits.files.len()
            );
        }
        let mut steps: Vec<Step> = Vec::new();
        for (s, item) in self.spans.iter().zip(items) {
            steps.push(Step {
                id: "timeline.place".into(),
                params: json!({"item": item, "track": "A1", "seconds": s.start}),
            });
        }
        steps.extend(self.ground()?);
        steps.extend(self.plate()?);
        steps.extend(self.bar()?);
        steps.extend(self.thumb(portraits, art)?);
        steps.extend(sprites.place(sprite_art, self.sprite_track(portraits.lanes, 0), self.total)?);
        steps.extend(self.badges(portraits, art)?);
        steps.push(Step {
            id: "captions.import".into(),
            params: json!({"path": captions, "name": "Storycast"}),
        });
        steps.push(self.caption_style()?);
        Ok(steps)
    }

    /// The paper: one full-frame rectangle under everything.
    fn ground(&self) -> Result<Vec<Step>> {
        let (w, h) = self.canvas;
        let paper = self
            .look
            .colour("background")
            .context("template palette.background is missing or not a colour")?;
        // A shape's `position` is its centre, not its corner — the paper is
        // placed by the middle of the frame so it covers all of it.
        Ok(vec![
            Step {
                id: "graphics.newShape".into(),
                params: json!({
                    "shape": "rectangle",
                    "position": [w as f64 / 2.0, h as f64 / 2.0],
                    "size": [w, h],
                    "seconds": self.total,
                    "track": PAPER_TRACK,
                    "time": 0,
                }),
            },
            style(json!({"fill_color": paper})),
        ])
    }

    /// The act plate: the label, then the title, each in its own template type,
    /// up for as long as the act plays.
    ///
    /// The type goes in a `box`, not at a point: point text does not wrap, so a
    /// long act title would run off the frame. A box's `position` is its
    /// top-left corner. The colour arrives in the `graphics.set` that follows,
    /// which lands on the text just made — the selection.
    fn plate(&self) -> Result<Vec<Step>> {
        let (w, h) = self.canvas;
        let zone = self
            .look
            .box_of("act_title", w, h)
            .context("template has no act_title zone")?;
        let unit = self.look.unit(w);
        let lead = unit * self.look.type_.act_label * LABEL_LEAD_1
            + LABEL_LEAD_2 * unit * self.look.type_.act_label;
        let pad = w as f64 * PLATE_PAD;
        let label_ink = self.look.colour("act_label").unwrap_or_else(|| "#1c1b1a".into());
        let title_ink = self.look.colour("act_title").unwrap_or_else(|| "#1c1b1a".into());
        let mut steps: Vec<Step> = Vec::new();
        for i in 0..self.acts.len() {
            let a = &self.acts[i];
            let (start, dur) = self.window(i);
            // The acts butt up, each on the same track as the one before: they
            // have to share the frame that one ends on, or the next is refused.
            let (time, len) = frame_window(start, start + dur, self.fps);
            let mut label = json!({
                "text": self.look.label(a.act),
                "newClip": true,
                "track": self.label_track(),
                "time": time,
                "seconds": frame_seconds(len),
                "size": self.look.px("act_label", w),
                "box": [zone.w, zone.h],
                "position": [zone.x + pad, zone.y],
            });
            let mut title = json!({
                "text": a.title,
                "newClip": true,
                "track": self.title_track(),
                "time": time,
                "seconds": frame_seconds(len),
                "size": self.look.px("act_title", w),
                "box": [zone.w, (zone.h - lead).max(1.0)],
                "position": [zone.x + pad, zone.y + lead],
            });
            if let Some(font) = self.look.font("act_label") {
                label["font"] = json!(font);
            }
            if let Some(font) = self.look.font("act_title") {
                title["font"] = json!(font);
            }
            steps.push(Step { id: "graphics.newText".into(), params: label });
            steps.push(style(json!({"align": "left", "fill_color": label_ink})));
            steps.push(Step { id: "graphics.newText".into(), params: title });
            steps.push(style(json!({"align": "left", "fill_color": title_ink})));
            steps.push(Step {
                id: "markers.addChapter".into(),
                params: json!({"name": a.title, "seconds": frame_seconds(time)}),
            });
        }
        Ok(steps)
    }

    /// The bar: the track across the timeline zone, one segment per act, each as
    /// wide as the act is long, and — for the act that is playing — the segment
    /// raised out of it in the accent colour.
    fn bar(&self) -> Result<Vec<Step>> {
        let (w, h) = self.canvas;
        let geom = self.bar_geom(w, h).context("template has no timeline zone")?;
        let seg = self.look.colour("timeline_segment").unwrap_or_else(|| "#a8a294".into());
        let groove = with_alpha(&seg, 0.35);
        let on = self.look.colour("timeline_segment_active");
        let lift = (geom.track_h * self.look.active_y_scale()).round();
        // Centre-anchored, so a segment's x is its left edge plus half of it.
        let rect = |x: f64, width: f64, height: f64, track: i64, time: i64, seconds: f64| Step {
            id: "graphics.newShape".into(),
            params: json!({
                "shape": "rectangle",
                "position": [x + width / 2.0, geom.cy],
                "size": [width, height],
                "seconds": seconds,
                "track": track,
                "time": time,
            }),
        };
        // The groove runs the whole book and the segments sit over it, so they
        // cannot share a track — a track holds one clip at a time.
        let mut steps = vec![
            rect(geom.left, geom.width, geom.track_h, GROOVE_TRACK, 0, self.total),
            style(json!({"fill_color": groove})),
        ];
        for (i, (x, width)) in geom.segs.iter().enumerate() {
            steps.push(rect(*x, *width, geom.track_h, SEGMENTS_FIRST + i as i64, 0, self.total));
            steps.push(style(json!({"fill_color": seg})));
        }
        // The highlight is only up while its act plays, so its clip carries the
        // act's window and no keyframe is needed to switch it on.
        for i in 0..self.acts.len() {
            let (x, width) = geom.segs[i];
            let (start, dur) = self.window(i);
            steps.push(rect(x, width, lift, self.active_first() + i as i64, ticks(start), dur));
            match &on {
                Some(c) => steps.push(style(json!({"fill_color": c}))),
                None => steps.push(style(json!({"fill_color": seg}))),
            }
        }
        Ok(steps)
    }

    /// The thumb: the disc that marks where playback has reached. It travels
    /// the bar's whole width over the book, as `bm-video` moves it, and turns
    /// where that template has art for it, so [`Plan::animate`] gives it the
    /// keys and lets FilmCraft tween between them.
    ///
    /// The art is drawn rather than imitated when the template names one: it is
    /// the same file the badge's scale is taken from, and it is already round.
    /// A template without one gets the disc `bm-video` falls back to.
    fn thumb(&self, portraits: &Portraits, art: &[i64]) -> Result<Vec<Step>> {
        let (w, h) = self.canvas;
        let Some(disc) = self.look.thumb_disc(h) else {
            return Ok(Vec::new());
        };
        let geom = self.bar_geom(w, h).context("template has no timeline zone")?;
        if let Some(i) = portraits.thumb {
            let item = *art
                .get(i)
                .with_context(|| format!("no imported item for the thumb's art ({i})"))?;
            return Ok(vec![Step {
                id: "timeline.place".into(),
                params: json!({
                    "item": item,
                    "track": format!("V{}", self.thumb_track() + 1),
                    "time": 0,
                    "duration": ticks(self.total),
                }),
            }]);
        }
        let fill = self.look.colour("thumb").unwrap_or_else(|| "#1c1b1a".into());
        let mut props = json!({"fill_color": fill});
        if let Some(ring) = self.look.colour("thumb_ring") {
            props["stroke"] = json!(true);
            props["strokeColor"] = json!(ring);
            props["strokeWidth"] = json!(disc.ring_w);
        }
        Ok(vec![
            Step {
                id: "graphics.newShape".into(),
                params: json!({
                    "shape": "ellipse",
                    "position": [geom.left, geom.cy],
                    "size": [disc.dia, disc.dia],
                    "seconds": self.total,
                    "track": self.thumb_track(),
                    "time": 0,
                }),
            },
            style(props),
        ])
    }

    /// The badges: one portrait clip per cue that has a speaker, up for that
    /// cue's own window and no longer, on the lane the cues themselves need.
    fn badges(&self, portraits: &Portraits, art: &[i64]) -> Result<Vec<Step>> {
        let mut steps = Vec::new();
        for (i, w) in self.cue_windows().iter().enumerate() {
            let Some(b) = portraits.badges.get(i).copied().flatten() else {
                continue;
            };
            let item = *art.get(b.file).with_context(|| {
                format!("no imported item for portrait {} of {}", b.file, portraits.files.len())
            })?;
            // Cues of one lane butt up wherever two speakers follow each other,
            // so the badge sits on the frame grid the lane was packed on.
            let (time, len) = frame_window(w.start, w.end, self.fps);
            steps.push(Step {
                id: "timeline.place".into(),
                params: json!({
                    "item": item,
                    // A still placed this way is not scaled to the frame, which is
                    // what leaves the badge its own pixels to be sized from.
                    "track": format!("V{}", self.portrait_track(b.lane) + 1),
                    "time": time,
                    "duration": len,
                }),
            });
        }
        Ok(steps)
    }

    /// The commands that set the thumb moving, once FilmCraft has made it.
    ///
    /// A timed `graphics.set` replaces a property's value unless the property is
    /// already animated, so `effects.addKeyframe` comes first — and it wants the
    /// clip's id, which only the reply to `graphics.newShape` carries. That is
    /// why this is a second pass, run against the project the first one built.
    ///
    /// `replies` are what the run of `steps` answered, one per step.
    pub fn animate(
        &self,
        steps: &[Step],
        replies: &[Value],
        portraits: &Portraits,
        sprites: &Elements,
    ) -> Result<Vec<Step>> {
        let mut out = self.thumb_keys(steps, replies, portraits)?;
        out.extend(self.portrait_keys(steps, replies, portraits)?);
        out.extend(sprites.keys(
            steps,
            replies,
            self.sprite_track(portraits.lanes, 0),
            self.fps,
            self.total,
        )?);
        Ok(out)
    }

    /// The thumb's keys, from the clip id FilmCraft reported for it: it travels
    /// the bar's width over the book, and turns a whole revolution every
    /// `spin_s`. `ccw` is FilmCraft's negative angle, so a template asking for
    /// one gets an angle that falls.
    fn thumb_keys(
        &self,
        steps: &[Step],
        replies: &[Value],
        portraits: &Portraits,
    ) -> Result<Vec<Step>> {
        let (w, h) = self.canvas;
        if self.look.thumb_disc(h).is_none() {
            return Ok(Vec::new());
        }
        let geom = self.bar_geom(w, h).context("template has no timeline zone")?;
        let thumb_track = json!(format!("V{}", self.thumb_track() + 1));
        let (clip, effect) = if portraits.thumb.is_some() {
            let clip = steps
                .iter()
                .zip(replies)
                .find_map(|(s, r)| {
                    (s.id == "timeline.place" && s.params["track"] == thumb_track)
                        .then(|| r.get("result")?.get("clips")?.get(0)?.as_i64())
                        .flatten()
                })
                .context("FilmCraft reported no clip id for the thumb")?;
            (clip, "motion")
        } else {
            let clip = steps
                .iter()
                .zip(replies)
                .find_map(|(step, reply)| {
                    (step.params.get("shape").and_then(Value::as_str) == Some("ellipse"))
                        .then(|| reply.get("result")?.get("clip")?.as_i64())
                        .flatten()
                })
                .context("FilmCraft reported no clip id for the thumb")?;
            (clip, GRAPHIC_SHAPE)
        };
        let set = |x: f64, time: i64| {
            thumb_prop(effect, clip, "position", json!([x, geom.cy]), time)
        };
        let (spin, sign) = match self.look.timeline.thumb.as_ref() {
            Some(t) => (
                t.spin_s.max(0.05),
                if t.direction.eq_ignore_ascii_case("cw") { 1.0 } else { -1.0 },
            ),
            None => (1.2, -1.0),
        };
        let turns = (self.total / spin).max(1.0);
        let mut out = vec![
            Step {
                id: "effects.addKeyframe".into(),
                params: json!({
                    "clip": clip,
                    "effect": effect,
                    "param": "position",
                    "time": 0,
                }),
            },
            set(geom.left, 0),
            set(geom.left + geom.width, ticks(self.total)),
            // One revolution is a straight line in angle, so three keys carry
            // the whole book at a constant rate.
            Step {
                id: "effects.addKeyframe".into(),
                params: json!({
                    "clip": clip,
                    "effect": effect,
                    "param": "rotation",
                    "time": 0,
                }),
            },
            thumb_prop(effect, clip, "rotation", json!(0.0), 0),
            thumb_prop(effect, clip, "rotation", json!(sign * 360.0), ticks(spin)),
            thumb_prop(
                effect,
                clip,
                "rotation",
                json!(sign * 360.0 * turns),
                ticks(self.total),
            ),
        ];
        if portraits.thumb.is_some() && portraits.thumb_pct > 0.0 {
            out.push(Step {
                id: "effects.setParam".into(),
                params: json!({
                    "clip": clip,
                    "effect": "motion",
                    "param": "scale",
                    "value": portraits.thumb_pct,
                    "time": 0,
                }),
            });
        }
        Ok(out)
    }

    /// The badges' keyframes, from the clip ids FilmCraft reported for them.
    ///
    /// Each badge is sized from its own art and the template's ratio, and then
    /// squashed and stretched by the loudness of its speaker's own cue: the
    /// height by `react.scale_y`, the width down by `react.scale_x`. FilmCraft
    /// scales a clip's two axes separately once `uniform_scale` is off.
    fn portrait_keys(
        &self,
        steps: &[Step],
        replies: &[Value],
        portraits: &Portraits,
    ) -> Result<Vec<Step>> {
        if portraits.lanes == 0 {
            return Ok(Vec::new());
        }
        let (w, h) = self.canvas;
        // The badge's centre, on the whole pixel `bm-video` rounds it to.
        let at = [(w as f64 * portraits.at[0]).round(), (h as f64 * portraits.at[1]).round()];
        // The badges' own tracks, so the thumb's place — also a video track — is
        // not mistaken for one of them.
        let badge_tracks: Vec<String> = (0..portraits.lanes)
            .map(|l| format!("V{}", self.portrait_track(l) + 1))
            .collect();
        let placed: Vec<i64> = steps
            .iter()
            .zip(replies)
            .filter(|(s, _)| {
                s.id == "timeline.place"
                    && s.params["track"]
                        .as_str()
                        .map(|t| badge_tracks.iter().any(|n| n == t))
                        .unwrap_or(false)
            })
            .filter_map(|(_, r)| r.get("result")?.get("clips")?.get(0)?.as_i64())
            .collect();
        let badges: Vec<(CueWindow, Badge)> = self
            .cue_windows()
            .into_iter()
            .zip(&portraits.badges)
            .filter_map(|(win, b)| b.map(|b| (win, b)))
            .collect();
        if placed.len() != badges.len() {
            bail!(
                "FilmCraft placed {} portraits for {} cues",
                placed.len(),
                badges.len()
            );
        }
        let mut out = Vec::new();
        for ((win, _badge), clip) in badges.iter().zip(&placed) {
            let span = &self.spans[win.span];
            let cue = &span.cues[win.cue];
            let set = |param: &str, value: Value, time: i64| Step {
                id: "effects.setParam".into(),
                params: json!({
                    "clip": clip,
                    "effect": "motion",
                    "param": param,
                    "value": value,
                    "time": time,
                }),
            };
            out.push(set("uniform_scale", json!(false), 0));
            out.push(set("position", json!(at), 0));
            let keys = self.badge_keys(win, cue);
            for (k, (frame, amount)) in keys.iter().enumerate() {
                    let (width, height) = portrait::size_pct(portraits.ratio, &portraits.react, *amount);
                let time = frame_ticks(span.start + *frame as f64 / self.fps.max(1.0), self.fps);
                if k == 0 {
                    out.push(Step {
                        id: "effects.addKeyframe".into(),
                        params: json!({
                            "clip": clip,
                            "effect": "motion",
                            "param": "scale",
                            "time": time,
                        }),
                    });
                    out.push(Step {
                        id: "effects.addKeyframe".into(),
                        params: json!({
                            "clip": clip,
                            "effect": "motion",
                            "param": "scale_width",
                            "time": time,
                        }),
                    });
                }
                out.push(set("scale", json!(height), time));
                out.push(set("scale_width", json!(width), time));
            }
        }
        Ok(out)
    }

    /// The frames a cue's badge is keyed on: the cue's own window of its
    /// chapter's envelope, decimated. A cue with no measurement at all gets one
    /// key at rest, so its badge is the right size rather than its own pixels.
    fn badge_keys(&self, win: &CueWindow, cue: &Cue) -> Vec<(usize, f32)> {
        let span = &self.spans[win.span];
        let fps = self.fps.max(1.0);
        let first = (cue.start * fps).floor().max(0.0) as usize;
        let last = (((cue.end * fps).ceil() as usize) + 1).min(span.env.len());
        let slice = span.env.get(first.min(last)..last).unwrap_or(&[]);
        let keys = portrait::keys(slice, BADGE_STEP);
        if keys.is_empty() {
            return vec![(first, 0.0)];
        }
        keys.into_iter().map(|(f, a)| (first + f, a)).collect()
    }

    /// The bar's geometry on this frame's pixels, from the acts' windows.
    fn bar_geom(&self, width: u32, height: u32) -> Option<BarGeom> {
        let durs: Vec<f64> = (0..self.acts.len()).map(|i| self.window(i).1).collect();
        self.look.bar(&durs, width, height)
    }

    /// The caption track, styled from the template's subtitle block: its font,
    /// its size, its ink, and the plate the captions sit on.
    fn caption_style(&self) -> Result<Step> {
        let (w, h) = self.canvas;
        let zone = self
            .look
            .box_of("subtitle", w, h)
            .context("template has no subtitle zone")?;
        let mut props = json!({
            "size": self.look.px("subtitle", w),
            "align": "center",
            "anchor": "bottom",
            "margin": (1.0 - (zone.y + zone.h) / h as f64).clamp(0.0, 0.45),
        });
        if let Some(font) = self.look.font("subtitle") {
            props["font"] = json!(font);
        }
        if let Some(ink) = self.look.colour("subtitle") {
            props["color"] = json!(ink);
        }
        if let Some(plate) = self.look.colour("subtitle_plate") {
            props["background"] = json!(true);
            props["backgroundColor"] = json!(plate);
        }
        props["track"] = json!("C1");
        Ok(Step { id: "captions.setStyle".into(), params: props })
    }
}

/// One property of the thumb at one time, on whichever effect holds it: the
/// picture's clip carries it on `motion`, the fallback disc on its graphic.
fn thumb_prop(effect: &str, clip: i64, name: &str, value: Value, time: i64) -> Step {
    if effect == "motion" {
        Step {
            id: "effects.setParam".into(),
            params: json!({
                "clip": clip,
                "effect": "motion",
                "param": name,
                "value": value,
                "time": time,
            }),
        }
    } else {
        Step {
            id: "graphics.set".into(),
            params: json!({"clip": clip, "props": { name: value }, "time": time}),
        }
    }
}

/// The acts, resolved against the laid-out spans.
fn outline(manifest: &Manifest, spans: &[Span]) -> Result<Vec<ActSpan>> {
    let mut acts: Vec<ActSpan> = Vec::new();
    for act in &manifest.acts {
        let first = act
            .chapters
            .first()
            .ok_or_else(|| anyhow::anyhow!("act {} has no chapters", act.act))?;
        let start = spans
            .iter()
            .find(|s| s.n == *first)
            .map(|s| s.start)
            .ok_or_else(|| anyhow::anyhow!("act {} starts at chapter {first}", act.act))?;
        acts.push(ActSpan {
            act: act.act,
            title: act.title.clone(),
            chapters: act.chapters.clone(),
            start,
        });
    }
    Ok(acts)
}

/// The script `filmcraft-cli run` reads: one `{id, params}` per line.
pub fn script(steps: &[Step]) -> String {
    let mut out = String::new();
    for s in steps {
        out.push_str(&json!({"id": s.id, "params": s.params}).to_string());
        out.push('\n');
    }
    out
}

/// `hh:mm:ss` for the human-facing report.
pub fn clock(seconds: f64) -> String {
    let s = seconds.max(0.0).round() as u64;
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}
