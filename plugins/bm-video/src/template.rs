//! The render template — canvas, palette, zones and type scale.
//!
//! This is the same `tools/video-template.json` the Python renderer read, so the
//! approved design stays the contract: everything here is presentation, none of
//! it is pipeline state.

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

// --------------------------------------------------------------------------- #
// colour
// --------------------------------------------------------------------------- #

/// Straight (non-premultiplied) 8-bit RGBA.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const TRANSPARENT: Rgba = Rgba { r: 0, g: 0, b: 0, a: 0 };

    /// `#rrggbb`, `rgb(r,g,b)` or `rgba(r,g,b,a)` with `a` in 0..1.
    pub fn parse(s: &str) -> Result<Rgba> {
        let s = s.trim();
        if let Some(hex) = s.strip_prefix('#') {
            if hex.len() != 6 {
                bail!("colour {s:?}: only #rrggbb is supported");
            }
            let v = u32::from_str_radix(hex, 16).with_context(|| format!("colour {s:?}"))?;
            return Ok(Rgba {
                r: ((v >> 16) & 255) as u8,
                g: ((v >> 8) & 255) as u8,
                b: (v & 255) as u8,
                a: 255,
            });
        }
        let inner = s
            .strip_prefix("rgba(")
            .or_else(|| s.strip_prefix("rgb("))
            .and_then(|r| r.strip_suffix(')'))
            .ok_or_else(|| anyhow!("colour {s:?}: expected #rrggbb or rgb()/rgba()"))?;
        let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
        if parts.len() < 3 {
            bail!("colour {s:?}: rgb() needs three channels");
        }
        let chan = |t: &str| -> Result<u8> {
            let f: f64 = t.parse().with_context(|| format!("colour {s:?}"))?;
            Ok(f.round().clamp(0.0, 255.0) as u8)
        };
        let alpha = match parts.get(3) {
            Some(t) => {
                let f: f64 = t.parse().with_context(|| format!("colour {s:?}"))?;
                (f * 255.0).round().clamp(0.0, 255.0) as u8
            }
            None => 255,
        };
        Ok(Rgba { r: chan(parts[0])?, g: chan(parts[1])?, b: chan(parts[2])?, a: alpha })
    }
}

// --------------------------------------------------------------------------- #
// layout
// --------------------------------------------------------------------------- #

/// A pixel box, as the mock's model computes it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub left: f64,
    pub top: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub fn x0(&self) -> i32 {
        self.left.round() as i32
    }
    pub fn y0(&self) -> i32 {
        self.top.round() as i32
    }
    /// Width rounded to an even number: the encoders refuse an odd one.
    pub fn w(&self) -> u32 {
        (self.width.round() as i64).max(0) as u32 & !1
    }
    pub fn h(&self) -> u32 {
        (self.height.round() as i64).max(0) as u32 & !1
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Zone {
    #[serde(default)]
    pub anchor: String,
    #[serde(default)]
    pub at: [f64; 2],
    #[serde(default)]
    pub size: [f64; 2],
    #[serde(default)]
    pub aspect: Option<f64>,
    #[serde(default)]
    pub align_top: Option<String>,
    #[serde(default)]
    pub align_bottom: Option<String>,
}

fn anchor_hv(anchor: &str) -> (f64, f64) {
    let (mut h, mut v) = (None, None);
    for t in anchor.split('-') {
        match t {
            "left" => h = Some(0.0),
            "right" => h = Some(1.0),
            "top" => v = Some(0.0),
            "bottom" => v = Some(1.0),
            _ => {}
        }
    }
    (h.unwrap_or(0.5), v.unwrap_or(0.5))
}

// --------------------------------------------------------------------------- #
// the template document
// --------------------------------------------------------------------------- #

fn d_fps() -> u32 {
    30
}
fn d_preset() -> String {
    "medium".into()
}
fn d_crf() -> u32 {
    18
}
fn d_one() -> f64 {
    1.0
}
fn d_unit() -> f64 {
    1.25
}
fn d_act_label() -> f64 {
    2.5
}
fn d_act_title() -> f64 {
    6.0
}
fn d_subtitle() -> f64 {
    2.0
}
fn d_min_s() -> f64 {
    1.0
}
fn d_max_s() -> f64 {
    7.0
}
fn d_fade() -> f64 {
    0.3
}
fn d_max_lines() -> usize {
    2
}
fn d_thickness() -> f64 {
    14.0
}
fn d_radius() -> f64 {
    13.0
}
fn d_spin() -> f64 {
    1.2
}
fn d_active_scale() -> f64 {
    1.4
}
fn d_release() -> f64 {
    0.14
}
fn d_at() -> [f64; 2] {
    [0.5, 740.0 / 1080.0]
}
fn d_hoi() -> String {
    "Hồi #".into()
}

#[derive(Clone, Debug, Deserialize)]
pub struct Canvas {
    pub size: [u32; 2],
    #[serde(default = "d_fps")]
    pub fps: u32,
}

impl Default for Canvas {
    fn default() -> Self {
        Canvas { size: [1920, 1080], fps: d_fps() }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct Encode {
    #[serde(default = "d_preset")]
    pub preset: String,
    #[serde(default = "d_crf")]
    pub crf: u32,
}

impl Default for Encode {
    fn default() -> Self {
        Encode { preset: d_preset(), crf: d_crf() }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct TypeScale {
    #[serde(default = "d_unit")]
    pub unit: f64,
    #[serde(default = "d_act_label")]
    pub act_label: f64,
    #[serde(default = "d_act_title")]
    pub act_title: f64,
    #[serde(default = "d_subtitle")]
    pub subtitle: f64,
}

impl Default for TypeScale {
    fn default() -> Self {
        TypeScale {
            unit: d_unit(),
            act_label: d_act_label(),
            act_title: d_act_title(),
            subtitle: d_subtitle(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct ActTitle {
    #[serde(default = "d_hoi")]
    pub label: String,
}

impl Default for ActTitle {
    fn default() -> Self {
        ActTitle { label: d_hoi() }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct Track {
    #[serde(default = "d_thickness")]
    pub thickness: f64,
}

impl Default for Track {
    fn default() -> Self {
        Track { thickness: d_thickness() }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct Thumb {
    #[serde(default = "d_radius")]
    pub radius: f64,
    #[serde(default = "d_one")]
    pub scale: f64,
    #[serde(default)]
    pub direction: String,
    #[serde(default)]
    pub single: Option<String>,
    #[serde(default)]
    pub sheet: Option<String>,
    #[serde(default = "d_spin")]
    pub spin_s: f64,
}

impl Default for Thumb {
    fn default() -> Self {
        Thumb {
            radius: d_radius(),
            scale: d_one(),
            direction: String::new(),
            single: None,
            sheet: None,
            spin_s: d_spin(),
        }
    }
}

/// The active segment's lift. `Default` matches the serde default on purpose:
/// the shipped template omits the whole `segment` object, so a derived `Default`
/// of 0.0 would draw the active bar segment with no height at all.
#[derive(Clone, Debug, Deserialize)]
pub struct Segment {
    #[serde(default = "d_active_scale")]
    pub active_y_scale: f64,
}

impl Default for Segment {
    fn default() -> Self {
        Segment { active_y_scale: d_active_scale() }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Timeline {
    #[serde(default)]
    pub track: Track,
    #[serde(default)]
    pub thumb: Option<Thumb>,
    #[serde(default)]
    pub segment: Segment,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SubtitleStyle {
    #[serde(default = "d_max_lines")]
    pub max_lines: usize,
}

impl Default for SubtitleStyle {
    fn default() -> Self {
        SubtitleStyle { max_lines: d_max_lines() }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct Subtitle {
    #[serde(default = "d_min_s")]
    pub min_s: f64,
    #[serde(default = "d_max_s")]
    pub max_s: f64,
    #[serde(default = "d_fade")]
    pub fade_s: f64,
    #[serde(default)]
    pub style: SubtitleStyle,
}

impl Default for Subtitle {
    fn default() -> Self {
        Subtitle {
            min_s: d_min_s(),
            max_s: d_max_s(),
            fade_s: d_fade(),
            style: SubtitleStyle::default(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct React {
    #[serde(default)]
    pub scale_x: f64,
    #[serde(default)]
    pub scale_y: f64,
    #[serde(default = "d_release")]
    pub release: f64,
    #[serde(default)]
    pub floor_pct: Option<f64>,
    #[serde(default)]
    pub ceil_pct: Option<f64>,
    #[serde(default)]
    pub floor_db: Option<f64>,
    #[serde(default)]
    pub ceil_db: Option<f64>,
}

impl Default for React {
    fn default() -> Self {
        React {
            scale_x: 0.0,
            scale_y: 0.0,
            release: d_release(),
            floor_pct: None,
            ceil_pct: None,
            floor_db: None,
            ceil_db: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct StickerCfg {
    #[serde(default)]
    pub fallback: Option<String>,
    #[serde(default)]
    pub speakers: BTreeMap<String, String>,
    #[serde(default = "d_at")]
    pub at: [f64; 2],
    #[serde(default = "d_one")]
    pub zoom: f64,
    #[serde(default)]
    pub react: React,
}

impl Default for StickerCfg {
    fn default() -> Self {
        StickerCfg {
            fallback: None,
            speakers: BTreeMap::new(),
            at: d_at(),
            zoom: d_one(),
            react: React::default(),
        }
    }
}

/// `radial-gradient(<rx> <ry> at <cx> <cy>, <colour> <pct>, …)`, the mock's.
#[derive(Clone, Debug)]
pub struct RadialGradient {
    pub cx: f64,
    pub cy: f64,
    pub rx: f64,
    pub ry: f64,
    pub stops: Vec<(f64, Rgba)>,
}

impl RadialGradient {
    pub fn parse(s: &str) -> Result<RadialGradient> {
        let body = s
            .trim()
            .strip_prefix("radial-gradient(")
            .and_then(|r| r.strip_suffix(')'))
            .ok_or_else(|| anyhow!("gradient {s:?}: expected radial-gradient(…)"))?;
        let (geometry, rest) = body
            .split_once(" at ")
            .ok_or_else(|| anyhow!("gradient {s:?}: expected 'at'"))?;
        let seat: Vec<&str> = rest.splitn(2, ',').collect();
        if seat.len() != 2 {
            bail!("gradient {s:?}: expected at least one stop");
        }
        let pct = |t: &str| -> Result<f64> {
            Ok(t.trim().trim_end_matches('%').parse::<f64>()? / 100.0)
        };
        let geo: Vec<&str> = geometry.split_whitespace().collect();
        let ctr: Vec<&str> = seat[0].split_whitespace().collect();
        if geo.len() < 2 || ctr.len() < 2 {
            bail!("gradient {s:?}: need two radii and two centre coordinates");
        }
        let mut stops = Vec::new();
        for part in seat[1].split(',') {
            let w: Vec<&str> = part.split_whitespace().collect();
            if w.len() < 2 {
                bail!("gradient {s:?}: stop {part:?} needs a colour and a position");
            }
            stops.push((pct(w[1])?, Rgba::parse(w[0])?));
        }
        Ok(RadialGradient {
            cx: pct(ctr[0])?,
            cy: pct(ctr[1])?,
            rx: pct(geo[0])?,
            ry: pct(geo[1])?,
            stops,
        })
    }
}

// --------------------------------------------------------------------------- #
// layers
// --------------------------------------------------------------------------- #

/// The layer kinds the renderer can draw. `paper` and `portraits` are the two
/// ends of the z-order; the rest stack between them in the file's order.
pub const LAYER_KINDS: &[&str] =
    &["paper", "act_plate", "bar", "thumb", "captions", "portraits", "sprite"];

/// The motion presets. Each is a pure function of the frame index, which is
/// what lets any chunk render alone, in any order, on any box.
pub const MOTION_NAMES: &[&str] = &["spin", "travel", "bob"];

/// One entry in the composition. `kind` names a painter; the rest is its data.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Layer {
    pub kind: String,
    /// Which zone places it. A painter with its own geometry ignores this.
    #[serde(default)]
    pub zone: Option<String>,
    /// A `sprite`'s image, relative to the template's root.
    #[serde(default)]
    pub asset: Option<String>,
    #[serde(default)]
    pub motion: Option<Motion>,
}

/// A looping motion. `period_s` defaults to the whole video, so omitting it
/// means one pass from start to finish.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Motion {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub period_s: Option<f64>,
    /// `cw` or `ccw`; only `spin` reads it.
    #[serde(default)]
    pub direction: Option<String>,
    /// Fraction of the zone's height, or of the rest scale for `bob`.
    #[serde(default)]
    pub amplitude: Option<f64>,
}

/// The order the composition was drawn in before it was data, so a template
/// with no `layers` keeps rendering exactly what it did.
fn default_layers() -> Vec<Layer> {
    ["paper", "act_plate", "bar", "thumb", "captions", "portraits"]
        .iter()
        .map(|k| Layer { kind: (*k).to_string(), ..Default::default() })
        .collect()
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Template {
    #[serde(default)]
    pub encode: Encode,
    #[serde(default)]
    pub canvas: Canvas,
    #[serde(default)]
    pub palette: BTreeMap<String, String>,
    #[serde(default)]
    pub zones: BTreeMap<String, Zone>,
    #[serde(default)]
    pub font_files: BTreeMap<String, String>,
    #[serde(default, rename = "type")]
    pub type_: TypeScale,
    #[serde(default, rename = "act_title")]
    pub act_title: ActTitle,
    #[serde(default)]
    pub timeline: Timeline,
    #[serde(default)]
    pub subtitle: Subtitle,
    #[serde(default)]
    pub speaker_sticker: Option<StickerCfg>,
    #[serde(default = "default_layers")]
    pub layers: Vec<Layer>,
    /// Where a relative path in the template resolves from.
    #[serde(skip)]
    pub root: PathBuf,
    #[serde(skip)]
    pub font_path: BTreeMap<String, PathBuf>,
}

impl Template {
    pub fn load(path: &Path, root: &Path) -> Result<Template> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading template {}", path.display()))?;
        let mut t: Template = serde_json::from_str(&text)
            .with_context(|| format!("parsing template {}", path.display()))?;
        t.root = root.to_path_buf();
        check_layers(&t.layers)?;
        for (role, rel) in &t.font_files {
            let p = resolve(root, rel);
            if !p.exists() {
                bail!("template font_files.{role}: no such file {}", p.display());
            }
            t.font_path.insert(role.clone(), p);
        }
        Ok(t)
    }

    pub fn fps(&self) -> u32 {
        self.canvas.fps.max(1)
    }

    /// Every image a `sprite` layer names, in layer order.
    pub fn sprite_assets(&self) -> Vec<&str> {
        self.layers
            .iter()
            .filter(|l| l.kind == "sprite")
            .filter_map(|l| l.asset.as_deref())
            .collect()
    }

    /// The pixel box model, resolved in dependency order for `align_*`.
    pub fn boxes(&self, w: u32, h: u32) -> BTreeMap<String, Rect> {
        let (cw, ch) = (self.canvas.size[0] as f64, self.canvas.size[1] as f64);
        let mut out: BTreeMap<String, Rect> = BTreeMap::new();
        for (name, z) in &self.zones {
            let (hh, vv) = anchor_hv(&z.anchor);
            let bw = z.size[0];
            let bh = match z.aspect {
                Some(a) if a != 0.0 => (bw * cw) / (a * ch),
                _ => z.size[1],
            };
            out.insert(
                name.clone(),
                Rect {
                    left: z.at[0] * w as f64 - bw * w as f64 * hh,
                    top: z.at[1] * h as f64 - bh * h as f64 * vv,
                    width: bw * w as f64,
                    height: bh * h as f64,
                },
            );
        }
        // A zone that aligns to another may be visited before its target; iterate
        // to a fixed point rather than depending on the file's key order.
        for _ in 0..=self.zones.len() {
            let mut changed = false;
            for (name, z) in &self.zones {
                let base = out[name];
                let mut top = base.top;
                if let Some(t) = z.align_top.as_deref() {
                    if let Some(r) = out.get(t) {
                        top = r.top;
                    }
                }
                if let Some(b) = z.align_bottom.as_deref() {
                    if let Some(r) = out.get(b) {
                        top = r.top + r.height - base.height;
                    }
                }
                if top != base.top {
                    out.get_mut(name).unwrap().top = top;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        out
    }

    pub fn box_of(&self, boxes: &BTreeMap<String, Rect>, name: &str) -> Result<Rect> {
        boxes
            .get(name)
            .copied()
            .ok_or_else(|| anyhow!("template has no zone {name:?}"))
    }

    pub fn colour(&self, key: &str) -> Result<Rgba> {
        let raw = self
            .palette
            .get(key)
            .ok_or_else(|| anyhow!("template palette has no {key:?}"))?;
        Rgba::parse(raw).with_context(|| format!("palette.{key}"))
    }

    pub fn gradient(&self, key: &str) -> Result<RadialGradient> {
        let raw = self
            .palette
            .get(key)
            .ok_or_else(|| anyhow!("template palette has no {key:?}"))?;
        RadialGradient::parse(raw).with_context(|| format!("palette.{key}"))
    }

    pub fn font_path(&self, role: &str) -> Result<PathBuf> {
        self.font_path
            .get(role)
            .cloned()
            .ok_or_else(|| anyhow!("template names no font for role {role:?}"))
    }
}

/// Reject a bad composition when the template is read, not mid-render.
fn check_layers(layers: &[Layer]) -> Result<()> {
    for (i, l) in layers.iter().enumerate() {
        if !LAYER_KINDS.contains(&l.kind.as_str()) {
            bail!(
                "template layers[{i}].kind {:?}: expected one of {}",
                l.kind,
                LAYER_KINDS.join(", ")
            );
        }
        if l.kind == "sprite" && l.asset.is_none() {
            bail!("template layers[{i}]: a sprite needs an asset");
        }
        if let Some(m) = &l.motion {
            if !m.name.is_empty() && !MOTION_NAMES.contains(&m.name.as_str()) {
                bail!(
                    "template layers[{i}].motion.name {:?}: expected {} (or omit it to hold still)",
                    m.name,
                    MOTION_NAMES.join(", ")
                );
            }
        }
    }
    Ok(())
}

/// A template path: absolute as written, otherwise relative to `root`.
pub fn resolve(root: &Path, rel: &str) -> PathBuf {
    let p = Path::new(rel);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    }
}

#[cfg(test)]
mod tests;
