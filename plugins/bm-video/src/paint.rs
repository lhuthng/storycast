//! The static art: the paper ground, the act-title plate, and the timeline bar.
//!
//! These are the pieces the Python renderer rasterised once per render and the
//! frame compositor then blits. Ported from `tools/video.py` (`paper`,
//! `act_title_plate`, `timeline_geometry`, `thumb_image`); pixel parity is not a
//! goal, the layout is.

use anyhow::{Context, Result};
use image::RgbaImage;
use std::collections::BTreeMap;
use std::path::Path;

use crate::model::Act;
use crate::raster::{crop, fill_rect, from_png, resize, rotate, solid};
use crate::template::{resolve, Rect, Rgba, Template};
use crate::text::{draw_text, wrap_to_width, Fonts};

/// The bar's pixel geometry: where the track sits and one segment per act.
pub struct BarGeom {
    pub left: f64,
    pub width: f64,
    pub track_h: f64,
    pub cy: f64,
    pub segs: Vec<(f64, f64)>,
}

// --------------------------------------------------------------------------- #
// the paper ground
// --------------------------------------------------------------------------- #

/// The radial grad, then the mock's soft vignette toward the frame edges.
pub fn paper(t: &Template, w: u32, h: u32) -> RgbaImage {
    let grad = match t.gradient("background") {
        Ok(g) => g,
        Err(_) => return solid(w, h, Rgba { r: 255, g: 255, b: 255, a: 255 }),
    };
    let mut out = solid(w, h, Rgba { r: 255, g: 255, b: 255, a: 255 });
    let (wf, hf) = (w as f64, h as f64);
    let stops = &grad.stops;
    let buf: &mut [u8] = &mut out;
    for (y, row) in buf.chunks_exact_mut((w * 4) as usize).enumerate() {
        let ny = (y as f64 + 0.5) / hf;
        for (x, px) in row.chunks_exact_mut(4).enumerate() {
            let nx = (x as f64 + 0.5) / wf;
            let d = ((((nx - grad.cx) / grad.rx).powi(2)
                + ((ny - grad.cy) / grad.ry).powi(2))
            .sqrt())
            .clamp(0.0, 1.0);
            let mut rgb = [255.0f64, 255.0, 255.0];
            if stops.len() == 1 {
                let c = stops[0].1;
                rgb = [c.r as f64, c.g as f64, c.b as f64];
            }
            for pair in stops.windows(2) {
                let (p0, c0) = pair[0];
                let (p1, c1) = pair[1];
                if d >= p0 && d <= p1 {
                    let u = if p1 > p0 { (d - p0) / (p1 - p0) } else { 0.0 };
                    rgb = [
                        c0.r as f64 * (1.0 - u) + c1.r as f64 * u,
                        c0.g as f64 * (1.0 - u) + c1.g as f64 * u,
                        c0.b as f64 * (1.0 - u) + c1.b as f64 * u,
                    ];
                    break;
                }
            }
            let edge = ((((nx - 0.5) * 2.0).powi(2) + ((ny - 0.5) * 2.0).powi(2)).sqrt() / 1.414)
                .clamp(0.0, 1.0);
            let v = 1.0 - 0.05 * edge * edge;
            px[0] = (rgb[0] * v).clamp(0.0, 255.0) as u8;
            px[1] = (rgb[1] * v).clamp(0.0, 255.0) as u8;
            px[2] = (rgb[2] * v).clamp(0.0, 255.0) as u8;
            px[3] = 255;
        }
    }
    out
}

// --------------------------------------------------------------------------- #
// the act-title plate
// --------------------------------------------------------------------------- #

/// The act-title zone, rasterised opaque: a crop of the paper, the label, and
/// the wrapped title, laid out exactly as `act_title_plate` does.
pub fn act_title_plate(
    t: &Template,
    fonts: &Fonts,
    boxes: &BTreeMap<String, Rect>,
    act: &Act,
    paper: &RgbaImage,
    w: u32,
) -> Result<RgbaImage> {
    let box_ = boxes
        .get("act_title")
        .copied()
        .context("template has no act_title zone")?;
    let mut tile = crop(paper, box_.x0(), box_.y0(), box_.w(), box_.h());

    let unit = w as f64 * t.type_.unit / 100.0;
    let (label_font, label_px) = fonts.label();
    let (title_font, title_px) = fonts.title();

    let pad = (w as f64 * 0.024).round() as i32;
    let label = t.act_title.label.replace('#', &act.act.to_string());
    draw_text(
        &mut tile,
        label_font,
        label_px,
        pad,
        0,
        &label,
        t.colour("act_label").unwrap_or(Rgba::TRANSPARENT),
    );
    let label_h = unit * t.type_.act_label * 1.15;
    let ty = label_h + 0.1 * unit * t.type_.act_label;
    let lines = wrap_to_width(
        title_font,
        title_px,
        &act.title,
        (box_.w() as f64 - 2.0 * pad as f64) as f32,
        2,
    );
    let lh = unit * t.type_.act_title * 1.06;
    let title_ink = t.colour("act_title").unwrap_or(Rgba::TRANSPARENT);
    for (k, line) in lines.iter().enumerate() {
        draw_text(
            &mut tile,
            title_font,
            title_px,
            pad,
            (ty + k as f64 * lh).round() as i32,
            line,
            title_ink,
        );
    }
    Ok(tile)
}

// --------------------------------------------------------------------------- #
// the timeline bar
// --------------------------------------------------------------------------- #

pub fn timeline_geometry(t: &Template, durs: &[f64], w: u32, h: u32) -> BarGeom {
    let boxes = t.boxes(w, h);
    let tz = match boxes.get("timeline") {
        Some(r) => *r,
        None => {
            return BarGeom { left: 0.0, width: 0.0, track_h: 0.0, cy: 0.0, segs: Vec::new() }
        }
    };
    let ch = t.canvas.size[1] as f64;
    let track_h = (h as f64 * t.timeline.track.thickness / ch).round();
    let gap = (h as f64 * 0.008).round();
    let total = durs.iter().sum::<f64>();
    let total = if total == 0.0 { 1.0 } else { total };
    let usable = tz.width - gap * (durs.len() as f64 - 1.0);
    let mut x = tz.left;
    let mut segs = Vec::with_capacity(durs.len());
    for dur in durs {
        let sw = usable * dur / total;
        segs.push((x, sw));
        x += sw + gap;
    }
    BarGeom { left: tz.left, width: tz.width, track_h, cy: tz.top + tz.height / 2.0, segs }
}

/// The whole track, then one segment per act, then the active one raised.
pub fn draw_bar(dst: &mut RgbaImage, t: &Template, g: &BarGeom, active: usize) {
    let base = t.colour("timeline_segment").unwrap_or(Rgba::TRANSPARENT);
    let on = t.colour("timeline_segment_active").unwrap_or(Rgba::TRANSPARENT);
    fill_rect(
        dst,
        Rect { left: g.left, top: g.cy - g.track_h / 2.0, width: g.width, height: g.track_h },
        base,
        0.35,
    );
    for (x, sw) in &g.segs {
        fill_rect(
            dst,
            Rect { left: *x, top: g.cy - g.track_h / 2.0, width: *sw, height: g.track_h },
            base,
            1.0,
        );
    }
    if let Some((x, sw)) = g.segs.get(active) {
        let scale = t.timeline.segment.active_y_scale;
        let h = g.track_h * scale;
        fill_rect(dst, Rect { left: *x, top: g.cy - h / 2.0, width: *sw, height: h }, on, 1.0);
    }
}

// --------------------------------------------------------------------------- #
// the thumb
// --------------------------------------------------------------------------- #

/// The moving thumb: the illustration round-clipped, or the plain disc.
pub fn thumb_art(t: &Template, h: u32, root: &Path) -> Result<Option<(RgbaImage, bool)>> {
    let Some(th) = t.timeline.thumb.as_ref() else {
        return Ok(None);
    };
    let canvas_h = t.canvas.size[1] as f64;
    let dia = (h as f64 * th.radius * 2.0 * th.scale / canvas_h).round().max(1.0) as u32;
    let rel = th.single.clone().or_else(|| th.sheet.clone());
    if let Some(rel) = rel {
        let p = resolve(root, &rel);
        if !p.exists() {
            anyhow::bail!("timeline.thumb.single: no such file {}", p.display());
        }
        let art = resize(&from_png(&p)?, dia, dia);
        let mask = circle_mask(dia);
        return Ok(Some((multiply_alpha(&art, &mask), true)));
    }
    let fill = t.colour("thumb").unwrap_or(Rgba::TRANSPARENT);
    let ring = t.colour("thumb_ring").unwrap_or(Rgba::TRANSPARENT);
    Ok(Some((disc(dia, ring, fill), false)))
}

/// One frame per step of a whole revolution, so the thumb can be blitted from a
/// table instead of rotated per frame.
pub fn thumb_frames(t: &Template, art: &RgbaImage, fps: u32) -> Vec<RgbaImage> {
    let th = t.timeline.thumb.as_ref();
    let spin_s = th.map(|x| x.spin_s).unwrap_or(1.2).max(0.05);
    let sign = match th {
        Some(x) => match x.direction.to_ascii_lowercase().as_str() {
            "ccw" | "reverse" | "back" => -1.0,
            _ => 1.0,
        },
        None => 1.0,
    };
    let n = (spin_s * fps as f64).round().max(1.0) as usize;
    (0..n)
        .map(|k| {
            let angle = sign * 2.0 * std::f64::consts::PI * (k as f64) / (n as f64);
            rotate(art, angle)
        })
        .collect()
}

/// A 4× supersampled disc downsampled to `dia`, so the edge is soft.
fn circle_mask(dia: u32) -> RgbaImage {
    let s = dia.max(1) * 4;
    let mut m = RgbaImage::new(s, s);
    let c = s as f64 / 2.0;
    let buf: &mut [u8] = &mut m;
    for (y, row) in buf.chunks_exact_mut((s * 4) as usize).enumerate() {
        let dy = y as f64 + 0.5 - c;
        for (x, px) in row.chunks_exact_mut(4).enumerate() {
            let dx = x as f64 + 0.5 - c;
            let a = if dx * dx + dy * dy <= c * c { 255u8 } else { 0u8 };
            px[0] = 255;
            px[1] = 255;
            px[2] = 255;
            px[3] = a;
        }
    }
    resize(&m, dia, dia)
}

/// Multiply `art`'s alpha by `mask`'s, channel for channel.
fn multiply_alpha(art: &RgbaImage, mask: &RgbaImage) -> RgbaImage {
    let mut out = art.clone();
    let w = out.width().min(mask.width());
    let h = out.height().min(mask.height());
    for y in 0..h {
        for x in 0..w {
            let m = mask.get_pixel(x, y).0[3] as u32;
            let a = out.get_pixel(x, y).0[3] as u32;
            out.get_pixel_mut(x, y).0[3] = ((a * m) / 255) as u8;
        }
    }
    out
}

/// The plain disc: a ring with the fill inset to 68%, as `thumb_image` draws it.
fn disc(dia: u32, ring: Rgba, fill: Rgba) -> RgbaImage {
    let s = dia.max(1) * 4;
    let mut im = RgbaImage::new(s, s);
    let c = s as f64 / 2.0;
    let inner = c * 0.68;
    let buf: &mut [u8] = &mut im;
    for (y, row) in buf.chunks_exact_mut((s * 4) as usize).enumerate() {
        let dy = y as f64 + 0.5 - c;
        for (x, px) in row.chunks_exact_mut(4).enumerate() {
            let dx = x as f64 + 0.5 - c;
            let d2 = dx * dx + dy * dy;
            let (col, a) = if d2 <= inner * inner {
                (fill, 255)
            } else if d2 <= c * c {
                (ring, 255)
            } else {
                (Rgba::TRANSPARENT, 0)
            };
            px[0] = col.r;
            px[1] = col.g;
            px[2] = col.b;
            px[3] = a;
        }
    }
    resize(&im, dia, dia)
}
