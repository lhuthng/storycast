//! Speaker portraits: who is speaking, how big the badge is, and the speech
//! envelope it squashes to.
//!
//! `bm-video` draws the same portrait with its own compositor, and the numbers
//! are its numbers: the badge's size off the timeline thumb's, and its reaction
//! off the chapter's own loudness, measured with ffmpeg.

use crate::look::{Look, React, SpeakerSticker};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Only these speak without a portrait.
const NARRATOR: [&str; 4] = ["narrator", "người dẫn chuyện", "người dẫn", "dẫn chuyện"];

/// The portrait for `speaker`, or `None` for the narrator and anyone unmapped
/// with no fallback. Matched case-insensitively: the script spells a name one
/// way and the template another, and a silent fallback to the anonymous
/// portrait is worse than a case difference.
pub fn art_for(sticker: &SpeakerSticker, root: &Path, speaker: &str) -> Option<PathBuf> {
    let name = speaker.trim();
    if name.is_empty() || NARRATOR.contains(&name.to_lowercase().as_str()) {
        return None;
    }
    let wanted = name.to_lowercase();
    let rel = sticker
        .speakers
        .iter()
        .find(|(k, _)| k.trim().to_lowercase() == wanted)
        .map(|(_, v)| v.clone())
        .or_else(|| sticker.fallback.clone())?;
    if rel.is_empty() {
        return None;
    }
    Some(resolve(root, &rel))
}

/// The badge's scale: the timeline thumb's own ratio — so a portrait is never
/// fitted into a box and keeps its proportions — times the template's `zoom`.
///
/// A portrait's visible art is its own pixels: the template's art is tight to
/// the figure, as `bm-video` needs it to be after its alpha crop.
pub fn ratio(look: &Look, root: &Path, height: u32) -> f64 {
    let zoom = look.sticker().map(|s| s.zoom).unwrap_or(1.0);
    let Some(thumb) = look.timeline.thumb.as_ref() else {
        return zoom;
    };
    let rel = thumb
        .single
        .as_deref()
        .filter(|s| !s.is_empty())
        .or_else(|| thumb.sheet.as_deref().filter(|s| !s.is_empty()));
    let Some(rel) = rel else {
        return zoom;
    };
    let natural = png_size(&resolve(root, rel)).map(|(w, _)| w).unwrap_or(0) as f64;
    if natural <= 0.0 {
        return zoom;
    }
    let canvas_h = look.canvas.size[1] as f64;
    let dia = thumb.radius * 2.0 * thumb.scale * height as f64 / canvas_h.max(1.0);
    dia / natural * zoom
}

/// The badge at one frame: width and height as percentages of the art's own
/// pixels, which is what FilmCraft's `scale_width` and `scale` take. Instant on
/// the syllable, and the release only ever takes the badge back down.
pub fn size_pct(ratio: f64, react: &React, amount: f32) -> (f64, f64) {
    let a = f64::from(amount.clamp(0.0, 1.0));
    (
        round3(ratio * (1.0 - react.scale_x * a) * 100.0),
        round3(ratio * (1.0 + react.scale_y * a) * 100.0),
    )
}

/// The frames worth a keyframe: every peak, and wherever the release has moved
/// the value by `step`. Between two keys FilmCraft draws a straight line and the
/// release is a decay, so this keeps the attacks exact and the decay within
/// `step` — a fortieth of the badge's own reaction.
pub fn keys(amounts: &[f32], step: f32) -> Vec<(usize, f32)> {
    let mut out: Vec<(usize, f32)> = Vec::new();
    let mut last: Option<f32> = None;
    for (i, a) in amounts.iter().enumerate() {
        let next = amounts.get(i + 1).copied().unwrap_or(f32::NEG_INFINITY);
        let before = amounts.get(i.wrapping_sub(1)).copied().unwrap_or(f32::NEG_INFINITY);
        let peak = *a > before && *a >= next;
        if peak || last.is_none_or(|l| (*a - l).abs() >= step) {
            out.push((i, *a));
            last = Some(*a);
        }
    }
    if let Some(last) = amounts.last() {
        let i = amounts.len() - 1;
        if out.last().map(|(j, _)| *j) != Some(i) {
            out.push((i, *last));
        }
    }
    out
}

/// Per video frame, how loud the chapter is, 0..1 — what the portrait reacts to.
///
/// Instant attack, exponential release: the badge jumps on the syllable and
/// settles straight after. The floor and ceiling are this chapter's own
/// percentiles unless the template pins them in dB, because loudness varies
/// chapter to chapter.
pub fn envelope(mp3: &Path, fps: f64, frames: usize, react: &React) -> Result<Vec<f32>> {
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
    Ok(amounts(&String::from_utf8_lossy(&out.stdout), fps, frames, react))
}

/// ffmpeg's astats report, binned to the video's frames and shaped to 0..1.
pub fn amounts(report: &str, fps: f64, frames: usize, react: &React) -> Vec<f32> {
    let mut times: Vec<f64> = Vec::new();
    let mut levels: Vec<f64> = Vec::new();
    for line in report.lines() {
        if let Some(rest) = line.strip_prefix("frame:") {
            if let Some(t) = rest
                .split_once("pts_time:")
                .and_then(|(_, v)| v.split_whitespace().next())
                .and_then(|t| t.parse::<f64>().ok())
            {
                times.push(t);
            }
        } else if line.starts_with("lavfi.astats") {
            levels.push(
                line.split_once('=')
                    .and_then(|(_, v)| v.trim().parse::<f64>().ok())
                    .unwrap_or(-120.0),
            );
        }
    }
    if levels.is_empty() {
        return vec![0.0; frames];
    }
    // Bin each audio frame into the video frame that covers it, keeping the
    // loudest: a syllable is ~25 ms, a video frame 33 ms.
    let mut per = vec![-120.0f64; frames];
    let mut last_bin = 0usize;
    for (t, level) in times.iter().zip(levels.iter()) {
        let bin = time_bin(*t, fps, frames);
        per[bin] = per[bin].max(*level);
        last_bin = last_bin.max(bin);
    }
    // The report can stop short of the last frame; the mix's own tail holds.
    let tail = *levels.last().unwrap_or(&-120.0);
    for v in per.iter_mut().skip(last_bin + 1) {
        *v = tail;
    }

    let mut sorted = per.clone();
    sorted.sort_by(f64::total_cmp);
    let lo = react.floor_db.unwrap_or_else(|| percentile(&sorted, react.floor_pct.unwrap_or(25.0)));
    let hi = react.ceil_db.unwrap_or_else(|| percentile(&sorted, react.ceil_pct.unwrap_or(95.0)));
    let span = (hi - lo).max(1e-6);
    let release = react.release.max(0.0);
    let decay = (-1.0f64 / (release * fps).max(1e-6)).exp();
    let mut hold = 0.0f64;
    per.into_iter()
        .map(|v| {
            hold = ((v - lo) / span).clamp(0.0, 1.0).max(hold * decay);
            hold as f32
        })
        .collect()
}

/// `np.searchsorted(edges, t, "right") - 1` on `edges[k] = k / fps`.
fn time_bin(t: f64, fps: f64, frames: usize) -> usize {
    let f = t * fps;
    let count = if f >= 0.0 { f.floor() as i64 + 1 } else { 0 };
    (count - 1).clamp(0, frames as i64 - 1) as usize
}

/// NumPy's linear-interpolated percentile over an ascending slice.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return -120.0;
    }
    let rank = (p / 100.0) * (sorted.len() - 1) as f64;
    let (lo, hi) = (rank.floor(), rank.ceil());
    let (lo_i, hi_i) = (lo as usize, (hi as usize).min(sorted.len() - 1));
    if lo_i == hi_i {
        sorted[lo_i]
    } else {
        sorted[lo_i] + (sorted[hi_i] - sorted[lo_i]) * (rank - lo)
    }
}

/// A PNG's pixel size, out of its header: the ratio is taken against the thumb's
/// own art, and reading a header beats decoding the file.
pub fn png_size(path: &Path) -> Option<(u32, u32)> {
    let head = std::fs::read(path).ok()?;
    if head.len() < 24 || &head[..8] != b"\x89PNG\r\n\x1a\n" || &head[12..16] != b"IHDR" {
        return None;
    }
    let be = |i: usize| u32::from_be_bytes([head[i], head[i + 1], head[i + 2], head[i + 3]]);
    Some((be(16), be(20)))
}

// Geometry in thousandths of a percent: readable in the script, and a value
// that survives the round trip through JSON.
fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

/// A template's path, against the tree the template came from.
fn resolve(root: &Path, rel: &str) -> PathBuf {
    let p = Path::new(rel);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    }
}
