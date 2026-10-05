//! The composition as data: the template's layer list, resolved.
//!
//! A sprite's place on a frame is a pure function of the frame index — nothing
//! is carried between frames — so a chunk composes the same alone as in
//! sequence, on any box.

use super::*;

use anyhow::bail;
use std::collections::BTreeMap;

use crate::template::Layer as LayerDoc;

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

/// One image placed in a zone, optionally looping.
pub struct Sprite {
    /// The asset's own bytes, so swapping the image re-renders its chunks.
    pub digest: [u8; 32],
    /// One entry per step of the period when it spins; a single frame holding
    /// still otherwise.
    pub frames: Vec<Arc<RgbaImage>>,
    /// Where it rests, before travel.
    x0: f64,
    y0: f64,
    /// How far it wanders, in pixels.
    travel: f64,
    bob: f64,
    period: f64,
}

/// Where a loop of `period` seconds sits at `frame`, in `[0, 1)`.
///
/// The whole point of a looping motion being a pure function: a frame index
/// alone decides the pose, so chunk 200 renders the same whether or not
/// chunks 0..200 ever ran.
fn phase(frame: usize, fps: u32, period: f64) -> f64 {
    let t = frame as f64 / fps.max(1) as f64;
    (t / period.max(1e-3)).rem_euclid(1.0)
}

impl Sprite {
    /// Where the sprite sits on `frame`, and which table entry to blit.
    pub fn at(&self, frame: usize, fps: u32) -> (i32, i32, usize) {
        let frac = phase(frame, fps, self.period);
        let x = self.x0 + self.travel * frac;
        let y = self.y0 + self.bob * (frac * std::f64::consts::TAU).sin();
        let n = self.frames.len();
        let idx = if n <= 1 { 0 } else { ((frac * n as f64).round() as usize) % n };
        (x.round() as i32, y.round() as i32, idx)
    }
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
    let (travel, bob) = match m.name.as_str() {
        // A loop across the zone: it re-enters where it left.
        "travel" => ((z.width - sw as f64).max(0.0), 0.0),
        "bob" => (0.0, m.amplitude.unwrap_or(0.05) * z.height),
        _ => (0.0, 0.0),
    };
    let frames = if m.name == "spin" {
        let n = (period * fps.max(1) as f64).round().max(1.0) as usize;
        if n > 1000 {
            bail!(
                "sprite spin over {period:.0}s is {n} frames of table; give it a shorter period_s"
            );
        }
        let sign = match m.direction.as_deref().unwrap_or("ccw").to_ascii_lowercase().as_str() {
            "cw" | "reverse" | "back" => 1.0,
            _ => -1.0,
        };
        (0..n)
            .map(|k| {
                let angle = sign * std::f64::consts::TAU * (k as f64) / (n as f64);
                Arc::new(raster::rotate(&art, angle))
            })
            .collect()
    } else {
        vec![Arc::new(art)]
    };
    Ok(Sprite {
        digest,
        frames,
        x0: z.left + (z.width - sw as f64) / 2.0,
        y0: z.top + (z.height - sh as f64) / 2.0,
        travel,
        bob,
        period,
    })
}

#[cfg(test)]
mod tests;
