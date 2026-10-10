//! The composition as data: the template's layer list, resolved.
//!
//! A sprite's place on a frame is a pure function of the frame index — nothing
//! is carried between frames — so a chunk composes the same alone as in
//! sequence, on any box.

use super::*;

use anyhow::bail;
use std::collections::BTreeMap;

use crate::template::{Layer as LayerDoc, Motion, MotionTrack, EASE_NAMES, MOTION_NAMES};

/// Which painter draws a layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Paper,
    ActPlate,
    Bar,
    Thumb,
    Captions,
    Portraits,
    Sprite,
}

/// A layer, resolved: the painter to run, and the sprite it draws if any.
pub struct Layer {
    pub kind: Kind,
    pub sprite: Option<usize>,
}

/// How many poses a cycle may hold. Each one is a rotated, resized copy of the
/// art, so this is the ceiling on a sprite's memory, not its motion.
const MAX_POSES: usize = 1000;

/// The properties a track may animate.
const PROPS: [&str; 5] = ["x", "y", "scale", "rotate", "opacity"];

/// One image placed in a zone, animated by its template's tracks.
pub struct Sprite {
    /// The asset's own bytes, so swapping the image re-renders its chunks.
    pub digest: [u8; 32],
    /// One pose per step of the cycle when `scale` or `rotate` moves, one still
    /// otherwise: turning and resizing are the expensive part, so they are a
    /// table and only `x`, `y` and `opacity` are applied live.
    pub frames: Vec<Arc<RgbaImage>>,
    /// The centre it rests at, before the `x` and `y` tracks.
    cx: f64,
    cy: f64,
    /// What an `x` or `y` track's 1.0 is a fraction of: its own zone.
    zone_w: f64,
    zone_h: f64,
    tracks: Vec<MotionTrack>,
    period: f64,
    total: f64,
}

/// Where in a cycle of `cycle` seconds `t` sits, in `[0, 1)`.
///
/// The whole point of a looping motion being a pure function: a time alone
/// decides the pose, so chunk 200 renders the same whether or not chunks 0..200
/// ever ran.
fn phase(t: f64, cycle: f64) -> f64 {
    (t / cycle.max(1e-3)).rem_euclid(1.0)
}

impl Sprite {
    /// Where the sprite sits on `frame`, which pose to blit, and how opaque.
    pub fn at(&self, frame: usize, fps: u32) -> (i32, i32, usize, f32) {
        let t = frame as f64 / fps.max(1) as f64;
        let of = |prop: &str| self.value(prop, t);
        let n = self.frames.len();
        let idx = if n <= 1 { 0 } else { ((phase(t, self.period) * n as f64).round() as usize) % n };
        let pose = &self.frames[idx];
        let x = self.cx + of("x") * self.zone_w - pose.width() as f64 / 2.0;
        let y = self.cy + of("y") * self.zone_h - pose.height() as f64 / 2.0;
        (x.round() as i32, y.round() as i32, idx, of("opacity").clamp(0.0, 1.0) as f32)
    }

    fn value(&self, prop: &str, t: f64) -> f64 {
        value(&self.tracks, prop, t, self.total, self.period)
    }
}

/// The value a property is at, `t` seconds in: its tracks in time order, each
/// within its own window, and the last value held past the end of them all.
fn value(tracks: &[MotionTrack], prop: &str, t: f64, total: f64, period: f64) -> f64 {
    let mut held: Option<f64> = None;
    for tr in tracks.iter().filter(|tr| tr.prop == prop) {
        let dur = tr.dur_s.unwrap_or(period).max(1e-3);
        let start = if tr.align.eq_ignore_ascii_case("end") { (total - dur).max(0.0) } else { 0.0 };
        if t < start {
            return held.unwrap_or(tr.from);
        }
        let passes = if tr.repeat == 0 { f64::INFINITY } else { f64::from(tr.repeat) };
        let at = (t - start) / dur;
        if at < passes {
            let i = at.floor();
            let (a, b) = if tr.pingpong && (i as i64) % 2 == 1 { (tr.to, tr.from) } else { (tr.from, tr.to) };
            return a + (b - a) * ease(&tr.ease, at - i);
        }
        held = Some(if tr.pingpong && passes % 2.0 == 0.0 { tr.from } else { tr.to });
    }
    held.unwrap_or_else(|| dflt(prop))
}

/// What a property sits at when no track names it: an opacity or a scale of one,
/// and no offset anywhere else.
fn dflt(prop: &str) -> f64 {
    if prop == "scale" || prop == "opacity" {
        1.0
    } else {
        0.0
    }
}

/// One segment's pace, `u` in 0..1. The names are the vocabulary both renderers
/// share; an unknown one is refused when the template is loaded.
pub fn ease(name: &str, u: f64) -> f64 {
    let u = u.clamp(0.0, 1.0);
    match canonical(name).as_str() {
        "in" => u * u,
        "out" => 1.0 - (1.0 - u) * (1.0 - u),
        "in-out" => {
            if u < 0.5 {
                2.0 * u * u
            } else {
                1.0 - 2.0 * (1.0 - u) * (1.0 - u)
            }
        }
        "in-cubic" => u * u * u,
        "out-cubic" => 1.0 - (1.0 - u).powi(3),
        "in-out-cubic" => {
            if u < 0.5 {
                4.0 * u * u * u
            } else {
                1.0 - (-2.0 * u + 2.0).powi(3) / 2.0
            }
        }
        "in-sin" => 1.0 - (u * std::f64::consts::FRAC_PI_2).cos(),
        "out-sin" => (u * std::f64::consts::FRAC_PI_2).sin(),
        "in-out-sin" => (1.0 - (std::f64::consts::PI * u).cos()) / 2.0,
        _ => u,
    }
}

/// An easing name as `EASE_NAMES` spells it, so the longhand aliases resolve to
/// the same arm as the shorthand.
fn canonical(name: &str) -> String {
    let n = name.trim().to_ascii_lowercase().replace('_', "-");
    match n.as_str() {
        "in-squared" => "in-quad".into(),
        "out-squared" => "out-quad".into(),
        "in-out-squared" => "in-out-quad".into(),
        other => other.to_string(),
    }
}

/// Refuse a track the renderer cannot draw, rather than quietly holding still.
fn check_tracks(tracks: &[MotionTrack]) -> Result<()> {
    for tr in tracks {
        if !PROPS.contains(&tr.prop.as_str()) {
            bail!("template animates {:?}, which is not one of {PROPS:?}", tr.prop);
        }
        if !EASE_NAMES.contains(&canonical(&tr.ease).as_str()) {
            bail!("template ease {:?} is not one of {EASE_NAMES:?}", tr.ease);
        }
    }
    Ok(())
}

/// The presets, as the tracks they always were: one pass, or a ping-pong that
/// loops. A `bob` is a sine — an `in-out-sin` segment ping-ponged is
/// `-cos(2πt/P)`, a quarter period behind the sine it replaces, at the same
/// amplitude and period.
fn preset_tracks(m: &Motion, zone: Rect, fitted_w: f64, period: f64) -> Result<Vec<MotionTrack>> {
    let track = |prop: &str, from: f64, to: f64, dur: f64, ease: &str, pingpong: bool| MotionTrack {
        prop: prop.into(),
        from,
        to,
        dur_s: Some(dur),
        ease: ease.into(),
        repeat: 0,
        pingpong,
        align: "start".into(),
    };
    let amplitude = m.amplitude.unwrap_or(0.05);
    let sign = match m.direction.as_deref().unwrap_or("ccw").to_ascii_lowercase().as_str() {
        "cw" | "reverse" | "back" => 1.0,
        _ => -1.0,
    };
    Ok(match m.name.as_str() {
        "travel" => vec![track(
            "x",
            0.0,
            (zone.width - fitted_w).max(0.0) / zone.width.max(1.0),
            period,
            "linear",
            false,
        )],
        "bob" => vec![track("y", -amplitude, amplitude, period / 2.0, "in-out-sin", true)],
        "spin" => vec![track("rotate", 0.0, sign * 360.0, period, "linear", false)],
        "" => Vec::new(),
        other => bail!("sprite motion {other:?} is not one this build draws ({MOTION_NAMES:?})"),
    })
}

/// The poses this sprite needs, one per step of `period`. Turning and resizing
/// are baked here, so a whole cycle has to fit in one table: a `scale` or a
/// `rotate` that does not come back to itself one `period_s` later is refused,
/// and told how to repeat.
fn poses(tracks: &[MotionTrack], art: &RgbaImage, fps: u32, period: f64, total: f64) -> Result<Vec<Arc<RgbaImage>>> {
    let turns = tracks.iter().any(|t| t.prop == "scale" || t.prop == "rotate");
    if !turns {
        return Ok(vec![Arc::new(art.clone())]);
    }
    for prop in ["scale", "rotate"] {
        if (value(tracks, prop, 0.0, total, period) - value(tracks, prop, period, total, period)).abs()
            > 1e-9
        {
            bail!(
                "this sprite's {prop} does not come back to itself after period_s ({period:.3}s) — \
                 give it a period_s that matches, or repeat: 0 with a dur_s that divides it"
            );
        }
    }
    let n = (period * fps.max(1) as f64).round().max(1.0) as usize;
    if n > MAX_POSES {
        bail!("a {period:.1}s cycle at {fps} fps is {n} poses of this sprite; give it a shorter period_s");
    }
    let (w, h) = (art.width(), art.height());
    Ok((0..n)
        .map(|k| {
            let t = k as f64 / fps.max(1) as f64;
            let angle = value(tracks, "rotate", t, total, period).to_radians();
            let s = value(tracks, "scale", t, total, period).max(1e-3);
            let turned = if angle == 0.0 { art.clone() } else { raster::rotate(art, angle) };
            if (s - 1.0).abs() < 1e-9 {
                return Arc::new(turned);
            }
            let scale = |v: u32| ((v as f64 * s).round().max(1.0)) as u32;
            Arc::new(raster::resize(&turned, scale(w), scale(h)))
        })
        .collect())
}

fn kind_of(s: &str) -> Result<Kind> {
    Ok(match s {
        "paper" => Kind::Paper,
        "act_plate" => Kind::ActPlate,
        "bar" => Kind::Bar,
        "thumb" => Kind::Thumb,
        "captions" => Kind::Captions,
        "portraits" => Kind::Portraits,
        "sprite" => Kind::Sprite,
        other => bail!("template layer kind {other:?} is not one this build draws"),
    })
}

/// The largest the image can be inside the box without distorting it.
fn contain(aw: u32, ah: u32, bw: u32, bh: u32) -> (u32, u32) {
    let s = (bw as f64 / aw.max(1) as f64).min(bh as f64 / ah.max(1) as f64);
    (
        ((aw as f64 * s).round() as u32).max(1),
        ((ah as f64 * s).round() as u32).max(1),
    )
}

/// Resolve the template's layers, loading each sprite's asset once.
pub fn build(
    t: &Template,
    boxes: &BTreeMap<String, Rect>,
    root: &Path,
    fps: u32,
    total: f64,
) -> Result<(Vec<Layer>, Vec<Arc<Sprite>>)> {
    let mut layers = Vec::with_capacity(t.layers.len());
    let mut sprites: Vec<Arc<Sprite>> = Vec::new();
    for l in &t.layers {
        let kind = kind_of(&l.kind)?;
        if kind != Kind::Sprite {
            layers.push(Layer { kind, sprite: None });
            continue;
        }
        sprites.push(Arc::new(sprite(boxes, root, fps, total, l)?));
        layers.push(Layer { kind, sprite: Some(sprites.len() - 1) });
    }
    Ok((layers, sprites))
}

fn sprite(
    boxes: &BTreeMap<String, Rect>,
    root: &Path,
    fps: u32,
    total: f64,
    l: &LayerDoc,
) -> Result<Sprite> {
    let rel = l.asset.as_deref().context("a sprite layer needs an asset")?;
    let name = l.zone.as_deref().unwrap_or("illustration");
    let z = boxes
        .get(name)
        .with_context(|| format!("sprite layer names no zone: {name:?}"))?;
    let path = crate::template::resolve(root, rel);
    let bytes = std::fs::read(&path)
        .with_context(|| format!("reading sprite asset {}", path.display()))?;
    let mut d = Sha256::new();
    d.update(&bytes);
    let digest: [u8; 32] = d.finalize().into();
    let art = raster::from_png(&path)?;
    let (sw, sh) = contain(art.width(), art.height(), z.w(), z.h());
    let art = raster::resize(&art, sw, sh);

    let m = l.motion.clone().unwrap_or_default();
    let period = m.period_s.unwrap_or(total).max(1e-3);
    let tracks = if m.tracks.is_empty() {
        preset_tracks(&m, *z, sw as f64, period)?
    } else {
        m.tracks.clone()
    };
    check_tracks(&tracks)?;
    let frames = poses(&tracks, &art, fps, period, total)?;
    Ok(Sprite {
        digest,
        frames,
        cx: z.left + z.width / 2.0,
        cy: z.top + z.height / 2.0,
        zone_w: z.width,
        zone_h: z.height,
        tracks,
        period,
        total,
    })
}

#[cfg(test)]
mod tests;
