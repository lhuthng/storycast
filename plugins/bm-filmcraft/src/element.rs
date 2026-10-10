//! The template's sprites: a picture placed somewhere, and the tracks that move
//! it.
//!
//! `bm-video` draws the same picture from the same layer, so the placement is
//! its placement: the asset is fitted into its zone without distortion and
//! centred in it, and an `x` or `y` track is a fraction of that zone. What
//! FilmCraft cannot do is put a *curve* on a key — it tweens two of them in a
//! straight line — so a track is decimated into as few keys as hold its eased
//! shape within half a pixel.

use crate::look::Look;
use crate::motion::{self, MotionTrack};
use crate::plan::{frame_ticks, ticks, Step};
use crate::portrait::png_size;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// How far a straight line may sit from the curve before the key is worth
/// writing: half a pixel of movement, half a degree of turn, half a percent of a
/// size or an opacity. All well below what an eye reads.
const POSITION_TOL: f64 = 0.5;
const SCALE_TOL: f64 = 0.5;
const ROTATE_TOL: f64 = 0.5;
const OPACITY_TOL: f64 = 0.5;

/// One picture the template places: where it rests, how big it is, and what
/// moves it.
#[derive(Clone, Debug)]
pub struct Element {
    pub name: String,
    /// Index into [`Elements::files`].
    pub file: usize,
    /// The zone's centre, where the picture rests.
    pub cx: f64,
    pub cy: f64,
    /// What an `x` or `y` track's 1.0 spans.
    pub zone_w: f64,
    pub zone_h: f64,
    /// The picture fitted into its zone, as a percentage of its own pixels.
    pub scale_pct: f64,
    pub tracks: Vec<MotionTrack>,
    pub period: f64,
}

#[derive(Clone, Debug, Default)]
pub struct Elements {
    /// The art, in the order the caller must import it.
    pub files: Vec<PathBuf>,
    pub list: Vec<Element>,
    /// Art the template names that is not on disk, for the caller to warn about.
    pub missing: Vec<PathBuf>,
}

impl Elements {
    /// Resolve every sprite the template places. A layer whose art is missing is
    /// reported rather than fatal, as a portrait is; a layer that names no asset,
    /// or a zone the template has not got, is a template bug and says so.
    pub fn build(look: &Look, root: &Path, width: u32, height: u32, total: f64) -> Result<Elements> {
        let mut out = Elements::default();
        for (i, layer) in look.sprites().enumerate() {
            let name = format!("sprite {i}");
            let rel = layer
                .asset
                .as_deref()
                .filter(|a| !a.is_empty())
                .with_context(|| format!("{name} names no asset"))?;
            let path = resolve(root, rel);
            let Some((aw, ah)) = png_size(&path) else {
                out.missing.push(path);
                continue;
            };
            let zone_name = layer.zone.as_deref().unwrap_or("illustration");
            let zone = look.box_of(zone_name, width, height).with_context(|| {
                format!("{name} is placed in zone {zone_name:?}, which the template has not got")
            })?;
            // Fitted into the zone without distortion, as `bm-video` fits it.
            let fit = (zone.w / aw.max(1) as f64).min(zone.h / ah.max(1) as f64);
            let fitted_w = aw as f64 * fit;
            let m = layer.motion.clone().unwrap_or_default();
            let period = m.period_s.unwrap_or(total).max(1e-3);
            let tracks = if m.tracks.is_empty() {
                motion::preset_tracks(&m, zone.w, fitted_w, period)?
            } else {
                m.tracks.clone()
            };
            motion::check(&tracks)?;
            let file = match out.files.iter().position(|f| *f == path) {
                Some(i) => i,
                None => {
                    out.files.push(path);
                    out.files.len() - 1
                }
            };
            out.list.push(Element {
                name,
                file,
                cx: zone.x + zone.w / 2.0,
                cy: zone.y + zone.h / 2.0,
                zone_w: zone.w,
                zone_h: zone.h,
                scale_pct: round3(fit * 100.0),
                tracks,
                period,
            });
        }
        Ok(out)
    }

    /// One clip per picture, on its own track, up for the whole book. What is
    /// not animated is set once, in [`Elements::keys`], where the clip ids are.
    pub fn place(&self, art: &[i64], first_track: i64, total: f64) -> Result<Vec<Step>> {
        let mut steps = Vec::new();
        for (i, el) in self.list.iter().enumerate() {
            let item = *art
                .get(el.file)
                .with_context(|| format!("no imported item for the {}", el.name))?;
            steps.push(Step {
                id: "timeline.place".into(),
                params: json!({
                    "item": item,
                    "track": track_name(first_track + i as i64),
                    "time": 0,
                    "duration": ticks(total),
                }),
            });
        }
        Ok(steps)
    }

    /// Where each picture sits, and how it moves, on the clips FilmCraft made
    /// for them.
    pub fn keys(
        &self,
        steps: &[Step],
        replies: &[Value],
        first_track: i64,
        fps: f64,
        total: f64,
    ) -> Result<Vec<Step>> {
        let mut out = Vec::new();
        for (i, el) in self.list.iter().enumerate() {
            let name = track_name(first_track + i as i64);
            let clip = steps
                .iter()
                .zip(replies)
                .find_map(|(s, r)| {
                    (s.id == "timeline.place" && s.params["track"] == name)
                        .then(|| r.get("result")?.get("clips")?.get(0)?.as_i64())
                        .flatten()
                })
                .with_context(|| format!("FilmCraft reported no clip id for the {}", el.name))?;
            // The tolerance is in the property's own pixels, percentages and
            // degrees, so each axis carries the unit its value is drawn in.
            let (zw, zh, size) = (el.zone_w, el.zone_h, el.scale_pct);
            let mut w = Writer { clip, el, fps: fps.max(1.0), total, out: Vec::new() };
            // Position is one value, so the two axes share their key times: a key
            // for `x` alone would hold `y` where FilmCraft last saw it.
            w.channel("motion", "position", &[("x", zw), ("y", zh)], POSITION_TOL, |w, f| {
                json!([
                    round3(w.el.cx + w.sample("x", f) * w.el.zone_w),
                    round3(w.el.cy + w.sample("y", f) * w.el.zone_h)
                ])
            });
            w.channel("motion", "scale", &[("scale", size)], SCALE_TOL, |w, f| {
                json!(round3(w.sample("scale", f) * size))
            });
            w.channel("motion", "rotation", &[("rotate", 1.0)], ROTATE_TOL, |w, f| {
                json!(round3(w.sample("rotate", f)))
            });
            w.channel("opacity", "opacity", &[("opacity", 100.0)], OPACITY_TOL, |w, f| {
                json!(round3(w.sample("opacity", f) * 100.0))
            });
            out.extend(w.out);
        }
        Ok(out)
    }
}

/// The keys one picture's motion is written as: `FilmCraft` wants an
/// `effects.addKeyframe` before a timed `effects.setParam` will stick, and both
/// want the clip's id.
struct Writer<'a> {
    clip: i64,
    el: &'a Element,
    fps: f64,
    total: f64,
    out: Vec<Step>,
}

impl Writer<'_> {
    fn sample(&self, prop: &str, frame: usize) -> f64 {
        motion::value(&self.el.tracks, prop, frame as f64 / self.fps, self.total, self.el.period)
    }

    fn animated(&self, prop: &str) -> bool {
        self.el.tracks.iter().any(|t| t.prop == prop)
    }

    /// One property on the clip: its keys, decimated to `tol` — or one value at
    /// the start when no track moves it. Each axis carries the scale its value is
    /// drawn at, so `tol` is in the units a person sees.
    fn channel(
        &mut self,
        effect: &str,
        param: &str,
        axes: &[(&str, f64)],
        tol: f64,
        value: impl Fn(&Self, usize) -> Value,
    ) {
        let live: Vec<(&str, f64)> = axes
            .iter()
            .copied()
            .filter(|(p, _)| self.animated(p))
            .collect();
        if live.is_empty() {
            self.out.push(self.set(effect, param, value(self, 0), 0));
            return;
        }
        let mut frames: Vec<usize> = live
            .iter()
            .flat_map(|(p, unit)| self.key_frames(|w, f| w.sample(p, f) * unit, tol))
            .collect();
        frames.sort_unstable();
        frames.dedup();
        for (k, f) in frames.iter().enumerate() {
            let time = frame_ticks(*f as f64 / self.fps, self.fps);
            if k == 0 {
                self.out.push(Step {
                    id: "effects.addKeyframe".into(),
                    params: json!({
                        "clip": self.clip,
                        "effect": effect,
                        "param": param,
                        "time": time,
                    }),
                });
            }
            let v = value(self, *f);
            self.out.push(self.set(effect, param, v, time));
        }
    }

    /// The frames worth a key: between two of them a straight line stays within
    /// `tol` of the curve, so FilmCraft's own tween draws the eased shape.
    fn key_frames(&self, v: impl Fn(&Self, usize) -> f64, tol: f64) -> Vec<usize> {
        // The frames a clip of this length holds, which is the length rounded
        // to the grid — and the last of them *starts* a frame before the end, so
        // the last key lands inside the clip rather than on the instant it
        // stops, where FilmCraft would clamp it.
        let frames = (self.total * self.fps).round().max(1.0) as usize;
        let mut out = vec![0usize];
        let mut last = 0usize;
        let mut i = 1usize;
        while i < frames {
            if deviates(&|f| v(self, f), last, i, tol) {
                let keep = if i - 1 > last { i - 1 } else { i };
                out.push(keep);
                last = keep;
            }
            i += 1;
        }
        if out.last() != Some(&(frames - 1)) {
            out.push(frames - 1);
        }
        out
    }

    fn set(&self, effect: &str, param: &str, value: Value, time: i64) -> Step {
        Step {
            id: "effects.setParam".into(),
            params: json!({
                "clip": self.clip,
                "effect": effect,
                "param": param,
                "value": value,
                "time": time,
            }),
        }
    }
}

/// Whether the segment `a`..`b` of a curve leaves the straight line between its
/// ends by more than `tol`.
fn deviates(v: &impl Fn(usize) -> f64, a: usize, b: usize, tol: f64) -> bool {
    let (va, vb) = (v(a), v(b));
    let span = (b - a) as f64;
    (a + 1..b).any(|j| {
        let line = va + (vb - va) * ((j - a) as f64 / span);
        (v(j) - line).abs() > tol
    })
}

/// FilmCraft names a track `V1`, `V2`, …: one-based, the plan's zero-based.
fn track_name(track: i64) -> String {
    format!("V{}", track + 1)
}

fn resolve(root: &Path, rel: &str) -> PathBuf {
    let p = Path::new(rel);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    }
}

// Geometry in thousandths: readable in the script, and a value that survives the
// round trip through JSON.
fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}
