//! Low-level pixel helpers: decode, resize, composite and rotate.
//!
//! Everything here is straight (non-premultiplied) 8-bit RGBA, which is what
//! `image` hands back and what the PNG tiles want. The compositing is the one
//! place the alpha maths lives, so no caller has to get it right twice.

use anyhow::{Context, Result};
use image::imageops::FilterType;
use image::{ImageBuffer, Rgba as Pixel, RgbaImage};
use std::path::Path;

use crate::template::{Rect, Rgba};

/// A fully transparent canvas.
pub fn canvas(w: u32, h: u32) -> RgbaImage {
    ImageBuffer::from_pixel(w.max(1), h.max(1), Pixel([0, 0, 0, 0]))
}

/// A canvas filled with `c`, alpha included.
pub fn solid(w: u32, h: u32, c: Rgba) -> RgbaImage {
    ImageBuffer::from_pixel(w.max(1), h.max(1), Pixel([c.r, c.g, c.b, c.a]))
}

pub fn from_png(path: &Path) -> Result<RgbaImage> {
    let im = image::open(path).with_context(|| format!("decoding {}", path.display()))?;
    Ok(im.to_rgba8())
}

/// Crop; anything outside `im` comes back transparent.
pub fn crop(im: &RgbaImage, x: i32, y: i32, w: u32, h: u32) -> RgbaImage {
    let mut out = canvas(w, h);
    if w == 0 || h == 0 {
        return out;
    }
    let (iw, ih) = (im.width() as i32, im.height() as i32);
    for oy in 0..h as i32 {
        let sy = y + oy;
        if sy < 0 || sy >= ih {
            continue;
        }
        for ox in 0..w as i32 {
            let sx = x + ox;
            if sx < 0 || sx >= iw {
                continue;
            }
            let p = *im.get_pixel(sx as u32, sy as u32);
            out.put_pixel(ox as u32, oy as u32, p);
        }
    }
    out
}

pub fn resize(im: &RgbaImage, w: u32, h: u32) -> RgbaImage {
    image::imageops::resize(im, w.max(1), h.max(1), FilterType::Lanczos3)
}

/// The box `(x, y, w, h)` of pixels with alpha > 0, or `None` if all transparent.
pub fn alpha_bbox(im: &RgbaImage) -> Option<(u32, u32, u32, u32)> {
    let (mut x0, mut y0) = (u32::MAX, u32::MAX);
    let (mut x1, mut y1) = (0u32, 0u32);
    let mut any = false;
    for (x, y, p) in im.enumerate_pixels() {
        if p.0[3] > 0 {
            any = true;
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
    }
    any.then(|| (x0, y0, x1 - x0 + 1, y1 - y0 + 1))
}

/// Straight-alpha source-over of one pixel, `sa` already scaled to 0..1.
#[inline]
fn over_px(p: &mut Pixel<u8>, srgb: [u8; 3], sa: f32) {
    if sa <= 0.0 {
        return;
    }
    let d = p.0;
    let da = d[3] as f32 / 255.0;
    let out_a = sa + da * (1.0 - sa);
    if out_a <= 0.0 {
        p.0 = [0, 0, 0, 0];
        return;
    }
    let mix = |si: usize, di: usize| -> u8 {
        let v = (srgb[si] as f32 * sa + d[di] as f32 * da * (1.0 - sa)) / out_a;
        v.round().clamp(0.0, 255.0) as u8
    };
    p.0 = [
        mix(0, 0),
        mix(1, 1),
        mix(2, 2),
        (out_a * 255.0).round().clamp(0.0, 255.0) as u8,
    ];
}

pub fn blit(dst: &mut RgbaImage, src: &RgbaImage, x: i32, y: i32, alpha: f32) {
    let mul = alpha.clamp(0.0, 1.0);
    if mul <= 0.0 {
        return;
    }
    let (dw, dh) = (dst.width() as i64, dst.height() as i64);
    for (sx, sy, s) in src.enumerate_pixels() {
        let (dx, dy) = (x as i64 + sx as i64, y as i64 + sy as i64);
        if dx < 0 || dy < 0 || dx >= dw || dy >= dh {
            continue;
        }
        let sa = s.0[3] as f32 / 255.0 * mul;
        if sa <= 0.0 {
            continue;
        }
        let p = dst.get_pixel_mut(dx as u32, dy as u32);
        over_px(p, [s.0[0], s.0[1], s.0[2]], sa);
    }
}

pub fn fill_rect(dst: &mut RgbaImage, rect: Rect, colour: Rgba, alpha: f32) {
    let sa = colour.a as f32 / 255.0 * alpha.clamp(0.0, 1.0);
    if sa <= 0.0 {
        return;
    }
    let (w, h) = (dst.width(), dst.height());
    let x0 = rect.x0().max(0) as u32;
    let y0 = rect.y0().max(0) as u32;
    if x0 >= w || y0 >= h {
        return;
    }
    let cw = rect.w().min(w - x0);
    let ch = rect.h().min(h - y0);
    if cw == 0 || ch == 0 {
        return;
    }
    for y in y0..y0 + ch {
        for x in x0..x0 + cw {
            over_px(dst.get_pixel_mut(x, y), [colour.r, colour.g, colour.b], sa);
        }
    }
}

/// Rotate about the centre, bilinear on premultiplied values (so the edges do
/// not halo), transparent outside the source.
pub fn rotate(im: &RgbaImage, radians: f64) -> RgbaImage {
    let (w, h) = (im.width(), im.height());
    let mut out = canvas(w, h);
    if w < 2 || h < 2 {
        return out;
    }
    let (cx, cy) = ((w as f64 - 1.0) / 2.0, (h as f64 - 1.0) / 2.0);
    let (sin, cos) = radians.sin_cos();
    for (px, py, dst) in out.enumerate_pixels_mut() {
        let dx = px as f64 - cx;
        let dy = py as f64 - cy;
        let sx = cx + dx * cos + dy * sin;
        let sy = cy - dx * sin + dy * cos;
        if !(0.0..=(w as f64 - 1.0)).contains(&sx) || !(0.0..=(h as f64 - 1.0)).contains(&sy) {
            continue;
        }
        let x0 = sx.floor() as u32;
        let y0 = sy.floor() as u32;
        let x1 = (x0 + 1).min(w - 1);
        let y1 = (y0 + 1).min(h - 1);
        let fx = (sx - x0 as f64) as f32;
        let fy = (sy - y0 as f64) as f32;
        let quad = [
            (im.get_pixel(x0, y0), (1.0 - fx) * (1.0 - fy)),
            (im.get_pixel(x1, y0), fx * (1.0 - fy)),
            (im.get_pixel(x0, y1), (1.0 - fx) * fy),
            (im.get_pixel(x1, y1), fx * fy),
        ];
        let mut acc = [0.0f32; 4];
        for (p, wt) in quad {
            let a = p.0[3] as f32 / 255.0;
            acc[0] += p.0[0] as f32 * a * wt;
            acc[1] += p.0[1] as f32 * a * wt;
            acc[2] += p.0[2] as f32 * a * wt;
            acc[3] += a * wt;
        }
        if acc[3] > 1e-6 {
            dst.0 = [
                (acc[0] / acc[3]).round().clamp(0.0, 255.0) as u8,
                (acc[1] / acc[3]).round().clamp(0.0, 255.0) as u8,
                (acc[2] / acc[3]).round().clamp(0.0, 255.0) as u8,
                (acc[3] * 255.0).round().clamp(0.0, 255.0) as u8,
            ];
        }
    }
    out
}

/// The frame as the rawvideo pipe wants it: rgb24, row-major, top-down.
pub fn to_rgb24(im: &RgbaImage) -> Vec<u8> {
    let mut out = vec![0u8; im.width() as usize * im.height() as usize * 3];
    for (i, p) in im.pixels().enumerate() {
        out[i * 3] = p.0[0];
        out[i * 3 + 1] = p.0[1];
        out[i * 3 + 2] = p.0[2];
    }
    out
}
