//! Speaker portraits and the speech envelope.
//!
//! A speaker's portrait is the name: the narrator gets none, an unmapped name
//! falls back to the anonymous one, and the portrait is scaled by the timeline
//! thumb's own ratio rather than fitted to a box, so it keeps its proportions.
//! The envelope is what the portrait squashes to — read off the mixed chapter
//! and gated by the caller to the speaker's own cues.

use anyhow::{bail, Context, Result};
use image::RgbaImage;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::raster;
use crate::template::{resolve, StickerCfg, Template};

/// Only these speak without a portrait.
const NARRATOR: [&str; 4] = ["narrator", "người dẫn chuyện", "người dẫn", "dẫn chuyện"];

fn is_narrator(name: &str) -> bool {
    NARRATOR.contains(&name.to_lowercase().as_str())
}

/// The portrait for `speaker`, or `None` for the narrator and anyone unmapped
/// with no fallback. Matched case-insensitively: the script spells a name one
/// way and the config key another, and a silent fallback to the anonymous
/// portrait is worse than a case difference.
pub fn sticker_for(root: &Path, speaker: &str, cfg: &StickerCfg) -> Option<PathBuf> {
    let name = speaker.trim();
    if name.is_empty() || is_narrator(name) {
        return None;
    }
    let wanted = name.to_lowercase();
    let rel = cfg
        .speakers
        .iter()
        .find(|(k, _)| k.trim().to_lowercase() == wanted)
        .map(|(_, v)| v.clone())
        .or_else(|| cfg.fallback.clone())?;
    if rel.is_empty() {
        return None;
    }
    Some(resolve(root, &rel))
}

/// The portrait trimmed to its own pixels: the transparent margin is not part
/// of the character, and sizing by it would leave a gap beside the text.
pub fn sticker_art(path: &Path) -> Result<RgbaImage> {
    let art = raster::from_png(path)?;
    match raster::alpha_bbox(&art) {
        Some((x, y, w, h)) => Ok(raster::crop(&art, x as i32, y as i32, w, h)),
        None => Ok(art),
    }
}

/// How far the timeline thumb shrinks its own image — the sticker's scale too.
///
/// The thumb resizes `timeline.thumb.single` to `radius * scale * 2`; the
/// sticker takes the same ratio of its own width, so a portrait is never fitted
/// into a fixed box and keeps its proportions.
pub fn sticker_ratio(t: &Template, root: &Path) -> f64 {
    let Some(th) = t.timeline.thumb.as_ref() else {
        return 1.0;
    };
    let rel = th
        .single
        .as_deref()
        .filter(|s| !s.is_empty())
        .or_else(|| th.sheet.as_deref().filter(|s| !s.is_empty()));
    let Some(rel) = rel else {
        return 1.0;
    };
    let src = resolve(root, rel);
    let natural = image::image_dimensions(&src).map(|(w, _)| w).unwrap_or(1).max(1) as f64;
    let h = t.canvas.size[1] as f64;
    let dia = h * th.radius * 2.0 * th.scale / h;
    dia / natural
}

/// One box for every portrait — the widest and tallest at `ratio`, plus the
/// vertical slack squash-and-stretch needs, so the badge is the same size
/// whichever speaker is talking.
pub fn sticker_box(cfg: &StickerCfg, arts: &[&RgbaImage], ratio: f64) -> (u32, u32) {
    let mut widest = 1.0f64;
    let mut tallest = 1.0f64;
    for im in arts {
        widest = widest.max(im.width() as f64 * ratio);
        tallest = tallest.max(im.height() as f64 * ratio);
    }
    let pad = 2.0;
    let w = widest.round() as i64 + (pad * 2.0) as i64;
    let h = (tallest * (1.0 + cfg.react.scale_y)).round() as i64 + (pad * 2.0) as i64;
    (w.max(1) as u32, h.max(1) as u32)
}

/// Per video frame, how loud the mix is, 0..1 — what the sticker reacts to.
///
/// Instant attack, exponential release: the portrait jumps on the syllable and
/// settles straight after, which is what squash-and-stretch does on a hit. The
/// window is this chapter's own percentiles unless the config pins it, because
/// loudness varies chapter to chapter.
pub fn speech_envelope(mp3: &Path, fps: u32, frames: usize, cfg: &StickerCfg) -> Result<Vec<f32>> {
    if frames == 0 {
        return Ok(Vec::new());
    }
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(mp3)
        .args([
            "-af",
            "astats=metadata=1:reset=1,ametadata=print:key=lavfi.astats.Overall.RMS_level:file=-",
            "-f",
            "null",
            "-",
        ])
        .stdin(Stdio::null())
        .output()
        .context("spawning ffmpeg for the speech envelope")?;
    if !out.status.success() {
        bail!("ffmpeg could not measure {} for the speech envelope", mp3.display());
    }

    let text = String::from_utf8_lossy(&out.stdout);
    let mut times: Vec<f64> = Vec::new();
    let mut levels: Vec<f64> = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("frame:") {
            if let Some(pts) = rest
                .split_once("pts_time:")
                .and_then(|(_, v)| v.split_whitespace().next())
                .and_then(|t| t.parse::<f64>().ok())
            {
                times.push(pts);
            }
        } else if line.starts_with("lavfi.astats") {
            let level = line
                .split_once('=')
                .and_then(|(_, v)| v.trim().parse::<f64>().ok())
                .unwrap_or(-120.0);
            levels.push(level);
        }
    }
    if levels.is_empty() {
        return Ok(vec![0.0; frames]);
    }

    // Bin each audio frame into the video frame that covers it, keeping the
    // loudest: a syllable is ~25 ms, a video frame 33 ms.
    let mut per = vec![-120.0f64; frames];
    let mut max_bin = 0usize;
    for (t, level) in times.iter().zip(levels.iter()) {
        let bin = time_bin(*t, fps, frames);
        if *level > per[bin] {
            per[bin] = *level;
        }
        max_bin = max_bin.max(bin);
    }
    let last = *levels.last().unwrap_or(&-120.0);
    for v in per.iter_mut().skip(max_bin + 1) {
        *v = last;
    }

    let mut sorted = per.clone();
    sorted.sort_by(f64::total_cmp);
    let lo = cfg
        .react
        .floor_db
        .unwrap_or_else(|| percentile(&sorted, cfg.react.floor_pct.unwrap_or(25.0)));
    let hi = cfg
        .react
        .ceil_db
        .unwrap_or_else(|| percentile(&sorted, cfg.react.ceil_pct.unwrap_or(95.0)));
    let span = (hi - lo).max(1e-6);
    let release = cfg.react.release.max(0.0);
    let decay = (-1.0f64 / (release * fps as f64).max(1e-6)).exp();
    let mut hold = 0.0f64;
    let mut out_amp = Vec::with_capacity(frames);
    for v in &per {
        hold = ((v - lo) / span).clamp(0.0, 1.0).max(hold * decay);
        out_amp.push(hold as f32);
    }
    Ok(out_amp)
}

/// One frame of the portrait, squashed about its centre by `amount` (0..1).
pub fn sticker_tile(
    art: &RgbaImage,
    ratio: f64,
    box_: (u32, u32),
    amount: f32,
    cfg: Option<&StickerCfg>,
) -> RgbaImage {
    let (scale_x, scale_y) = match cfg {
        Some(c) => (c.react.scale_x, c.react.scale_y),
        None => (0.0, 0.0),
    };
    let a = amount.max(0.0) as f64;
    let w = ((art.width() as f64 * ratio * (1.0 - scale_x * a)).round() as i64).max(1) as u32;
    let h = ((art.height() as f64 * ratio * (1.0 + scale_y * a)).round() as i64).max(1) as u32;
    let scaled = raster::resize(art, w, h);
    let mut out = raster::canvas(box_.0, box_.1);
    let x = (box_.0 as i64 - w as i64).div_euclid(2) as i32;
    let y = (box_.1 as i64 - h as i64).div_euclid(2) as i32;
    raster::blit(&mut out, &scaled, x, y, 1.0);
    out
}

/// `np.searchsorted(edges, t, "right") - 1` on `edges[k] = k / fps`.
fn time_bin(t: f64, fps: u32, frames: usize) -> usize {
    let f = t * fps as f64;
    let count = if f >= 0.0 { f.floor() as i64 + 1 } else { 0 };
    (count - 1).clamp(0, frames as i64 - 1) as usize
}

/// NumPy's linear-interpolated percentile over an ascending slice.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return -120.0;
    }
    let rank = (p / 100.0) * (sorted.len() - 1) as f64;
    let lo = rank.floor();
    let hi = rank.ceil();
    let lo_i = lo as usize;
    let hi_i = (hi as usize).min(sorted.len() - 1);
    if lo_i == hi_i {
        sorted[lo_i]
    } else {
        sorted[lo_i] + (sorted[hi_i] - sorted[lo_i]) * (rank - lo)
    }
}
