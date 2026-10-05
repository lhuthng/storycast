//! Text: the three faces, glyph rasterising, caption splitting, the sidecars.
//!
//! This box's ffmpeg has no drawtext/libass, so every glyph is drawn here. The
//! caption *shape* (sentence breaks, justification, how long a line may stay on
//! screen) is the part that makes the video readable, so it is ported from the
//! Python renderer rather than reinvented.

use ab_glyph::{point, Font, FontVec, ScaleFont};
use anyhow::{Context, Result};
use image::RgbaImage;
use std::cmp::Ordering;
use std::path::Path;

use crate::model::Chapter;
use crate::raster;
use crate::template::{Rgba, Template};

/// A dash is a pause the narrator takes, not a word: on screen it reads as one.
const DASHES: &[char] = &['\u{2014}', '\u{2013}', '\u{2015}'];
const SENTENCE_END: &[char] = &['.', '!', '?', '\u{2026}', '\u{3002}', '\u{FF01}', '\u{FF1F}', '\u{FF1B}', ';'];
/// Only these speak without naming themselves.
const NARRATOR: &[&str] = &["narrator", "người dẫn chuyện", "người dẫn", "dẫn chuyện"];

/// The three faces, each at its template size for this canvas width.
pub struct Fonts {
    label: FontVec,
    title: FontVec,
    subtitle: FontVec,
    label_px: f32,
    title_px: f32,
    subtitle_px: f32,
}

impl Fonts {
    pub fn load(t: &Template, w: u32) -> Result<Fonts> {
        let unit = w as f64 * t.type_.unit / 100.0;
        let load = |role: &str, scale: f64| -> Result<(FontVec, f32)> {
            let path = t.font_path(role)?;
            let bytes = std::fs::read(&path)
                .with_context(|| format!("reading font {}", path.display()))?;
            let font = FontVec::try_from_vec(bytes)
                .map_err(|e| anyhow::anyhow!("font {role}: {e}"))?;
            Ok((font, (unit * scale).round() as f32))
        };
        let (label, label_px) = load("act_label", t.type_.act_label)?;
        let (title, title_px) = load("act_title", t.type_.act_title)?;
        let (subtitle, subtitle_px) = load("subtitle", t.type_.subtitle)?;
        Ok(Fonts { label, title, subtitle, label_px, title_px, subtitle_px })
    }

    pub fn label(&self) -> (&FontVec, f32) {
        (&self.label, self.label_px)
    }

    pub fn title(&self) -> (&FontVec, f32) {
        (&self.title, self.title_px)
    }

    pub fn subtitle(&self) -> (&FontVec, f32) {
        (&self.subtitle, self.subtitle_px)
    }
}

/// One on-screen caption: a line (or two) of text and its frame-snapped span.
#[derive(Clone, Debug)]
pub struct Caption {
    pub start: f64,
    pub end: f64,
    pub text: String,
    pub speaker: String,
}

// --------------------------------------------------------------------------- #
// glyphs
// --------------------------------------------------------------------------- #

/// Advance width of `s` at `px`, kerning included.
pub fn measure(font: &FontVec, px: f32, s: &str) -> f32 {
    let scaled = font.as_scaled(px);
    let mut width = 0.0f32;
    let mut prev = None;
    for c in s.chars() {
        let id = scaled.glyph_id(c);
        if let Some(p) = prev {
            width += scaled.kern(p, id);
        }
        width += scaled.h_advance(id);
        prev = Some(id);
    }
    width
}

fn blend_px(dst: &mut RgbaImage, x: i32, y: i32, colour: Rgba, coverage: f32) {
    if x < 0 || y < 0 {
        return;
    }
    let (w, h) = dst.dimensions();
    if x as u32 >= w || y as u32 >= h {
        return;
    }
    let src_a = coverage.clamp(0.0, 1.0) * (colour.a as f32 / 255.0);
    if src_a <= 0.0 {
        return;
    }
    let px = dst.get_pixel_mut(x as u32, y as u32);
    let p = &mut px.0;
    let dst_a = p[3] as f32 / 255.0;
    let out_a = src_a + dst_a * (1.0 - src_a);
    if out_a <= 0.0 {
        return;
    }
    let src = [colour.r, colour.g, colour.b];
    for i in 0..3 {
        let s = src[i] as f32;
        let d = p[i] as f32;
        p[i] = ((s * src_a + d * dst_a * (1.0 - src_a)) / out_a)
            .round()
            .clamp(0.0, 255.0) as u8;
    }
    p[3] = (out_a * 255.0).round().clamp(0.0, 255.0) as u8;
}

/// One line, its **top-left** at (x, y): the baseline sits an ascent below.
pub fn draw_text(
    dst: &mut RgbaImage,
    font: &FontVec,
    px: f32,
    x: i32,
    y: i32,
    text: &str,
    colour: Rgba,
) {
    let scaled = font.as_scaled(px);
    let baseline = y as f32 + scaled.ascent();
    let mut pen = x as f32;
    let mut prev = None;
    for c in text.chars() {
        let id = scaled.glyph_id(c);
        if let Some(p) = prev {
            pen += scaled.kern(p, id);
        }
        let glyph = id.with_scale_and_position(px, point(pen, baseline));
        if let Some(outline) = font.outline_glyph(glyph) {
            let bounds = outline.px_bounds();
            outline.draw(|gx, gy, coverage| {
                let px_x = (bounds.min.x + gx as f32) as i32;
                let px_y = (bounds.min.y + gy as f32) as i32;
                blend_px(dst, px_x, px_y, colour, coverage);
            });
        }
        pen += scaled.h_advance(id);
        prev = Some(id);
    }
}

// --------------------------------------------------------------------------- #
// the caption shape
// --------------------------------------------------------------------------- #

/// The sidecar's line, as it should be read on screen.
pub fn caption_text(raw: &str) -> String {
    // A run of dashes is one "; ".
    let mut dashed = String::with_capacity(raw.len());
    let mut in_dash = false;
    for c in raw.chars() {
        if DASHES.contains(&c) {
            if !in_dash {
                dashed.push_str("; ");
                in_dash = true;
            }
        } else {
            in_dash = false;
            dashed.push(c);
        }
    }
    let chars: Vec<char> = dashed.chars().collect();
    // A dash after a full stop is just the next sentence starting.
    let mut sentences = String::with_capacity(dashed.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if SENTENCE_END.contains(&c) {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if j < chars.len() && chars[j] == ';' {
                let mut k = j + 1;
                while k < chars.len() && chars[k].is_whitespace() {
                    k += 1;
                }
                sentences.push(c);
                sentences.push(' ');
                i = k;
                continue;
            }
        }
        sentences.push(c);
        i += 1;
    }
    // Whitespace around a "; " collapses to the "; ".
    let chars: Vec<char> = sentences.chars().collect();
    let mut tight = String::with_capacity(sentences.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            let mut j = i;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if j < chars.len() && chars[j] == ';' {
                tight.push_str("; ");
                i = j + 1;
                while i < chars.len() && chars[i].is_whitespace() {
                    i += 1;
                }
                continue;
            }
            tight.extend(chars[i..j].iter().copied());
            i = j;
            continue;
        }
        tight.push(chars[i]);
        i += 1;
    }
    let trimmed = tight.trim_matches(|c: char| c == ' ' || c == ';');
    let mut out = String::with_capacity(trimmed.len());
    let mut space = false;
    for c in trimmed.chars() {
        if c.is_whitespace() {
            space = true;
        } else {
            if space && !out.is_empty() {
                out.push(' ');
            }
            space = false;
            out.push(c);
        }
    }
    out
}

/// `"` for anyone but the narrator — their sticker carries the name.
pub fn speaker_label(speaker: &str) -> String {
    let name = speaker.trim();
    if name.is_empty() || NARRATOR.iter().any(|n| *n == name.to_lowercase()) {
        String::new()
    } else {
        "\"".to_string()
    }
}

/// Greedy wrap by measured width; the last line absorbs any overflow.
pub fn wrap_to_width(
    font: &FontVec,
    px: f32,
    text: &str,
    max_width: f32,
    max_lines: usize,
) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let cand = if cur.is_empty() { word.to_string() } else { format!("{cur} {word}") };
        if !cur.is_empty() && measure(font, px, &cand) > max_width {
            lines.push(std::mem::take(&mut cur));
            cur = word.to_string();
        } else {
            cur = cand;
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if max_lines > 0 && lines.len() > max_lines {
        let head = lines[..max_lines - 1].to_vec();
        let tail = lines[max_lines - 1..].join(" ");
        let mut out = head;
        out.push(tail);
        return out;
    }
    lines
}

/// Words inside `max_lines` rows, by measured width.
fn lines_for(words: &[String], font: &FontVec, px: f32, max_width: f32) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for w in words {
        let cand = if line.is_empty() { w.clone() } else { format!("{line} {w}") };
        if line.is_empty() || measure(font, px, &cand) <= max_width {
            line = cand;
        } else {
            lines.push(std::mem::take(&mut line));
            line = w.clone();
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// Group words into sentences, so a caption break falls between them.
fn sentences(words: &[String]) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut cur: Vec<String> = Vec::new();
    for w in words {
        cur.push(w.clone());
        if w.trim_end_matches(SENTENCE_END) != w.as_str() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Wrap `words` into `greedy.len()` rows of as even a width as possible.
fn balanced_lines(words: &[String], font: &FontVec, px: f32, greedy: &[String]) -> Vec<String> {
    let rows = greedy.len();
    let n = words.len();
    if rows <= 1 || n < rows {
        return greedy.to_vec();
    }
    let gap = measure(font, px, " ");
    let mut edges = vec![0.0f32; n + 1];
    for (i, w) in words.iter().enumerate() {
        edges[i + 1] = edges[i] + measure(font, px, w);
    }
    let width = |a: usize, b: usize| edges[b] - edges[a] + gap * (b - a - 1) as f32;

    let mut best = vec![vec![f32::INFINITY; n + 1]; rows + 1];
    let mut cut = vec![vec![0usize; n + 1]; rows + 1];
    best[0][0] = 0.0;
    for r in 1..=rows {
        for b in r..=n {
            for a in r - 1..b {
                if best[r - 1][a] == f32::INFINITY {
                    continue;
                }
                let cost = best[r - 1][a] + width(a, b) * width(a, b);
                if cost < best[r][b] {
                    best[r][b] = cost;
                    cut[r][b] = a;
                }
            }
        }
    }
    if best[rows][n] == f32::INFINITY {
        return greedy.to_vec();
    }
    let mut out: Vec<Vec<String>> = Vec::new();
    let mut b = n;
    for r in (1..=rows).rev() {
        let a = cut[r][b];
        out.push(words[a..b].to_vec());
        b = a;
    }
    out.reverse();
    out.iter().map(|l| l.join(" ")).collect()
}

/// Split `dur` between captions in proportion to their text.
fn shares_for(dur: f64, weights: &[usize], min_s: f64, max_s: f64) -> Vec<f64> {
    let n = weights.len();
    let wtot = weights.iter().sum::<usize>().max(1) as f64;
    let mut shares: Vec<f64> = weights.iter().map(|w| dur * *w as f64 / wtot).collect();
    let last = n.saturating_sub(1);
    for s in shares[..last].iter_mut() {
        *s = s.clamp(min_s, max_s);
    }
    let rest = dur - shares.iter().sum::<f64>();
    if rest > 0.0 {
        shares[n - 1] += rest;
    } else if rest < 0.0 {
        let slack: Vec<usize> = shares[..last]
            .iter()
            .enumerate()
            .filter(|(_, s)| **s > min_s)
            .map(|(i, _)| i)
            .collect();
        let pool: f64 = slack.iter().map(|i| shares[*i] - min_s).sum();
        if pool > 0.0 {
            for i in &slack {
                shares[*i] -= -rest * (shares[*i] - min_s) / pool;
            }
        }
        shares[n - 1] = shares[n - 1].max(0.0);
    }
    shares
}

/// Everything a caption needs to know about the frame it must fit.
#[derive(Clone, Copy)]
struct CueStyle {
    px: f32,
    max_width: f32,
    max_lines: usize,
    min_s: f64,
    max_s: f64,
}

/// Cut one script segment into captions, each wrapped to the frame's width.
fn split_cue(
    text: &str,
    start: f64,
    end: f64,
    font: &FontVec,
    style: &CueStyle,
    label: &str,
) -> Vec<(f64, f64, String)> {
    let head: Vec<String> = label.split_whitespace().map(str::to_string).collect();

    let overflow = |w: &[String]| -> bool {
        !w.is_empty()
            && {
                let mut all = head.clone();
                all.extend_from_slice(w);
                lines_for(&all, font, style.px, style.max_width).len() > style.max_lines
            }
    };

    let split_oversized = |sent: &[String]| -> Vec<Vec<String>> {
        let mut out: Vec<Vec<String>> = Vec::new();
        let mut rest: Vec<String> = sent.to_vec();
        while overflow(&rest) {
            let mut take: Vec<String> = Vec::new();
            for w in &rest {
                let mut cand = take.clone();
                cand.push(w.clone());
                if !take.is_empty() && overflow(&cand) {
                    break;
                }
                take.push(w.clone());
            }
            if take.is_empty() {
                break;
            }
            rest.drain(..take.len());
            out.push(take);
        }
        if !rest.is_empty() {
            out.push(rest);
        }
        out
    };

    let words: Vec<String> = text.split_whitespace().map(str::to_string).collect();
    if words.is_empty() {
        return Vec::new();
    }
    let mut chunks: Vec<Vec<String>> = Vec::new();
    let mut cur: Vec<String> = Vec::new();
    for sent in sentences(&words) {
        for piece in split_oversized(&sent) {
            let mut cand = cur.clone();
            cand.extend_from_slice(&piece);
            if !cur.is_empty() && overflow(&cand) {
                chunks.push(std::mem::take(&mut cur));
                cur = piece;
            } else {
                cur = cand;
            }
        }
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    let dur = (end - start).max(0.001);
    let needed = (dur / style.max_s).ceil().max(1.0) as usize;
    if needed > chunks.len() {
        let sents = sentences(&words);
        let step = (sents.len() as f64 / needed as f64).ceil().max(1.0) as usize;
        let mut regrouped: Vec<Vec<String>> = Vec::new();
        let mut i = 0;
        while i < sents.len() {
            let hi = (i + step).min(sents.len());
            regrouped.push(sents[i..hi].iter().flatten().cloned().collect());
            i = hi;
        }
        chunks = regrouped
            .into_iter()
            .flat_map(|c| if overflow(&c) { split_oversized(&c) } else { vec![c] })
            .collect();
    }
    let weights: Vec<usize> = chunks.iter().map(|c| c.iter().map(|w| w.chars().count()).sum()).collect();
    let shares = shares_for(dur, &weights, style.min_s, style.max_s);
    let mut out = Vec::new();
    let mut t = start;
    for (i, c) in chunks.iter().enumerate() {
        let t_end = (t + shares[i]).min(end);
        let mut rows: Vec<String> = c.clone();
        if !head.is_empty() {
            let mut r: Vec<String> = head[..head.len() - 1].to_vec();
            r.push(format!("{}{}", head[head.len() - 1], c[0]));
            r.extend_from_slice(&c[1..]);
            let last = r.len() - 1;
            r[last].push('"');
            rows = r;
        }
        let greedy = lines_for(&rows, font, style.px, style.max_width);
        let balanced = balanced_lines(&rows, font, style.px, &greedy);
        out.push((t, t_end, balanced.join("\n")));
        t = t_end;
    }
    out
}

pub fn ts(seconds: f64, comma: bool) -> String {
    let s = seconds.max(0.0);
    let h = (s / 3600.0) as u64;
    let m = ((s % 3600.0) / 60.0) as u64;
    let sec = s % 60.0;
    let mut whole = sec as u64;
    let mut ms = ((sec - whole as f64) * 1000.0).round() as u64;
    if ms == 1000 {
        whole += 1;
        ms = 0;
    }
    format!("{h:02}:{m:02}:{whole:02}{}{ms:03}", if comma { ',' } else { '.' })
}

// --------------------------------------------------------------------------- #
// building the caption list
// --------------------------------------------------------------------------- #

/// Cut every chapter's cues into frame-snapped captions on the video clock.
pub fn build_captions(
    t: &Template,
    fonts: &Fonts,
    chapters: &[Chapter],
    starts: &[f64],
) -> Vec<Caption> {
    let (font, px) = fonts.subtitle();
    let style = CueStyle {
        px,
        max_width: 0.94 * t.canvas.size[0] as f32,
        max_lines: t.subtitle.style.max_lines.max(1),
        min_s: t.subtitle.min_s,
        max_s: t.subtitle.max_s,
    };
    let mut caps: Vec<Caption> = Vec::new();
    for (ch, off) in chapters.iter().zip(starts) {
        // The cue sheet is written on the planned clock and can outrun the mp3
        // it describes; a caption must not claim time the video does not have.
        let limit = off + ch.dur;
        for cue in &ch.cues {
            let text = caption_text(&cue.text);
            if text.is_empty() || off + cue.start >= limit - 0.05 {
                continue;
            }
            let a = (off + cue.start).min(limit);
            let b = (off + cue.end).min(limit);
            for (s, e, line) in split_cue(&text, a, b, font, &style, &speaker_label(&cue.speaker)) {
                caps.push(Caption {
                    start: s,
                    end: e,
                    text: line,
                    speaker: cue.speaker.clone(),
                });
            }
        }
    }
    caps.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap_or(Ordering::Equal));
    // The video can only change a caption on a frame boundary.
    let fps = t.fps() as f64;
    for c in caps.iter_mut() {
        c.start = (c.start * fps).round() / fps;
        c.end = (c.end * fps).round().max((c.start * fps).round() + 1.0) / fps;
    }
    let nexts: Vec<f64> = caps.iter().map(|c| c.start).skip(1).collect();
    for (c, next) in caps.iter_mut().zip(nexts) {
        if c.end > next {
            c.end = next;
        }
    }
    caps
}

/// The transparent tile one caption is drawn into.
pub fn caption_tile(fonts: &Fonts, w: u32, h: u32, text: &str, ink: Rgba) -> RgbaImage {
    let (font, px) = fonts.subtitle();
    let lh = px as f64 * 1.3;
    let mut im = raster::canvas(w, h);
    let lines: Vec<&str> = text.split('\n').collect();
    let top = (h as f64 - lh * lines.len() as f64) / 2.0;
    for (k, line) in lines.iter().enumerate() {
        let width = measure(font, px, line) as f64;
        let x = ((w as f64 - width) / 2.0).round() as i32;
        let y = (top + k as f64 * lh).round() as i32;
        draw_text(&mut im, font, px, x, y, line, ink);
    }
    im
}

pub fn write_subtitles(caps: &[Caption], srt: &Path, vtt: &Path) -> Result<()> {
    let mut srt_body = String::new();
    for (i, c) in caps.iter().enumerate() {
        srt_body.push_str(&format!(
            "{}\n{} --> {}\n{}\n\n",
            i + 1,
            ts(c.start, true),
            ts(c.end, true),
            c.text
        ));
    }
    std::fs::write(srt, srt_body).with_context(|| format!("writing {}", srt.display()))?;
    let mut vtt_body = String::from("WEBVTT\n\n");
    for c in caps {
        vtt_body.push_str(&format!(
            "{} --> {}\n{}\n\n",
            ts(c.start, false),
            ts(c.end, false),
            c.text
        ));
    }
    std::fs::write(vtt, vtt_body).with_context(|| format!("writing {}", vtt.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests;
