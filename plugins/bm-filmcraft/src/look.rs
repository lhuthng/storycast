//! The render template: canvas, palette, zones and type.
//!
//! The same file `bm-video` renders from (`tools/video-template.json`). Reading
//! it here is what lets FilmCraft compose the look itself — the paper, the act
//! plate, the bar, the caption style — instead of importing a picture that was
//! rendered somewhere else.

use crate::motion::Motion;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

// --- the document --- //

#[derive(Clone, Debug, Deserialize)]
pub struct Canvas {
    #[serde(default = "d_size")]
    pub size: [u32; 2],
    #[serde(default = "d_fps")]
    pub fps: f64,
}

impl Default for Canvas {
    fn default() -> Self {
        Canvas { size: d_size(), fps: d_fps() }
    }
}

fn d_size() -> [u32; 2] {
    [1920, 1080]
}

fn d_fps() -> f64 {
    30.0
}

/// A named box, as a fraction of the frame. `align_top` / `align_bottom` name
/// another zone's edge for this one to take.
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

/// The type scale: one `unit`, and each role's multiple of it.
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

#[derive(Clone, Debug, Deserialize)]
pub struct Bar {
    #[serde(default = "d_thickness")]
    pub thickness: f64,
}

impl Default for Bar {
    fn default() -> Self {
        Bar { thickness: d_thickness() }
    }
}

fn d_thickness() -> f64 {
    14.0
}

/// The act that is playing: its segment, raised out of the bar.
#[derive(Clone, Debug, Deserialize)]
pub struct Active {
    #[serde(default = "d_active_scale")]
    pub active_y_scale: f64,
}

impl Default for Active {
    fn default() -> Self {
        Active { active_y_scale: d_active_scale() }
    }
}

fn d_active_scale() -> f64 {
    1.4
}

/// The moving thumb: a disc, sized from the frame's height like the bar's
/// thickness. Its ring is the outer 32% of the radius, as `bm-video` draws it.
#[derive(Clone, Debug, Deserialize)]
pub struct Thumb {
    #[serde(default = "d_radius")]
    pub radius: f64,
    #[serde(default = "d_one")]
    pub scale: f64,
    /// The art the thumb is drawn from — also what the portraits are sized
    /// against, so their badge is the same scale as the thumb on the bar.
    #[serde(default)]
    pub single: Option<String>,
    #[serde(default)]
    pub sheet: Option<String>,
    /// One whole turn takes this long. The art is round, so FilmCraft can turn
    /// it live instead of stepping a pre-rendered revolution.
    #[serde(default = "d_spin")]
    pub spin_s: f64,
    /// `ccw` (the template's) turns anticlockwise on FilmCraft's positive angle.
    #[serde(default = "d_ccw")]
    pub direction: String,
}

fn d_spin() -> f64 {
    1.2
}

fn d_ccw() -> String {
    "ccw".into()
}

fn d_radius() -> f64 {
    13.0
}

fn d_one() -> f64 {
    1.0
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Timeline {
    #[serde(default)]
    pub track: Bar,
    #[serde(default)]
    pub segment: Active,
    #[serde(default)]
    pub thumb: Option<Thumb>,
}

/// What a portrait does while its speaker talks: the badge squashes and
/// stretches about `at`, instantly on the syllable and settling after.
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

fn d_release() -> f64 {
    0.14
}

/// The speaker portraits: whose name maps to which picture, where the badge
/// sits, and how it reacts. `speakers` is matched case-insensitively; a name
/// that is the narrator's, or is not in the map, takes the fallback.
#[derive(Clone, Debug, Deserialize)]
pub struct SpeakerSticker {
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

impl Default for SpeakerSticker {
    fn default() -> Self {
        SpeakerSticker {
            fallback: None,
            speakers: BTreeMap::new(),
            at: d_at(),
            zoom: 1.0,
            react: React::default(),
        }
    }
}

fn d_at() -> [f64; 2] {
    [0.5, 0.5]
}

/// The act plate's own text: the label a title sits under.
#[derive(Clone, Debug, Deserialize)]
pub struct Plate {
    #[serde(default = "d_plate")]
    pub label: String,
}

impl Default for Plate {
    fn default() -> Self {
        Plate { label: d_plate() }
    }
}

fn d_plate() -> String {
    "Hồi #".into()
}

/// One entry in the template's composition. The look's own painters are
/// composed here by hand; a `sprite` — any picture, placed in a zone and
/// animated by its `motion` — is one FilmCraft places and keys itself.
#[derive(Clone, Debug, Deserialize)]
pub struct Layer {
    pub kind: String,
    /// Which zone places it. Defaults to `illustration`, as `bm-video` does.
    #[serde(default)]
    pub zone: Option<String>,
    /// A sprite's image, relative to the tree the template came from.
    #[serde(default)]
    pub asset: Option<String>,
    #[serde(default)]
    pub motion: Option<Motion>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Look {
    #[serde(default)]
    pub canvas: Canvas,
    #[serde(default)]
    pub zones: BTreeMap<String, Zone>,
    #[serde(default)]
    pub palette: BTreeMap<String, String>,
    #[serde(default)]
    pub fonts: BTreeMap<String, String>,
    #[serde(default)]
    pub font_files: BTreeMap<String, String>,
    #[serde(default, rename = "type")]
    pub type_: TypeScale,
    #[serde(default)]
    pub timeline: Timeline,
    #[serde(default, rename = "act_title")]
    pub plate: Plate,
    #[serde(default)]
    pub speaker_sticker: Option<SpeakerSticker>,
    /// The composition's order. Only its sprites are read here: the rest of the
    /// look is composed by name, from the palette, zones and type above.
    #[serde(default, rename = "layers")]
    pub layers: Vec<Layer>,
}

/// The document as it is on disk: `Look` plus the keys this plugin also reads.
#[derive(Deserialize)]
struct Doc {
    #[serde(default)]
    canvas: Canvas,
    #[serde(default)]
    zones: BTreeMap<String, Zone>,
    #[serde(default)]
    palette: BTreeMap<String, String>,
    #[serde(default)]
    fonts: BTreeMap<String, String>,
    #[serde(default)]
    font_files: BTreeMap<String, String>,
    #[serde(default, rename = "type")]
    type_: TypeScale,
    #[serde(default)]
    timeline: Timeline,
    #[serde(default, rename = "act_title")]
    plate: Plate,
    #[serde(default)]
    speaker_sticker: Option<SpeakerSticker>,
    #[serde(default, rename = "layers")]
    layers: Vec<Layer>,
}

/// A resolved pixel box.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// The bar's pixel geometry: the track, and one segment per act.
#[derive(Clone, Debug, PartialEq)]
pub struct BarGeom {
    pub left: f64,
    pub width: f64,
    pub track_h: f64,
    pub cy: f64,
    pub segs: Vec<(f64, f64)>,
}

/// The thumb's disc in pixels: across, and how wide the ring is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ThumbDisc {
    pub dia: f64,
    pub ring_w: f64,
}

impl Look {
    pub fn load(path: &Path) -> Result<Look> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading template {}", path.display()))?;
        let doc: Doc = serde_json::from_str(&text)
            .with_context(|| format!("parsing {}", path.display()))?;
        if doc.canvas.size[0] == 0 || doc.canvas.size[1] == 0 {
            bail!("template canvas.size must be at least one pixel");
        }
        Ok(Look {
            canvas: doc.canvas,
            zones: doc.zones,
            palette: doc.palette,
            fonts: doc.fonts,
            font_files: doc.font_files,
            type_: doc.type_,
            timeline: doc.timeline,
            plate: doc.plate,
            speaker_sticker: doc.speaker_sticker,
            layers: doc.layers,
        })
    }

    /// The pictures the template places and animates itself.
    pub fn sprites(&self) -> impl Iterator<Item = &Layer> {
        self.layers.iter().filter(|l| l.kind == "sprite")
    }

    /// The portraits' configuration, if the template has one.
    pub fn sticker(&self) -> Option<&SpeakerSticker> {
        self.speaker_sticker.as_ref()
    }

    /// The picture the thumb is drawn from, if the template names one and it is
    /// on disk: the same file the badge's scale is taken from.
    pub fn thumb_picture(&self, root: &Path) -> Option<PathBuf> {
        let thumb = self.timeline.thumb.as_ref()?;
        let rel = thumb
            .single
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| thumb.sheet.as_deref().filter(|s| !s.is_empty()))?;
        let path = if Path::new(rel).is_absolute() {
            PathBuf::from(rel)
        } else {
            root.join(rel)
        };
        path.exists().then_some(path)
    }

    pub fn fps(&self) -> f64 {
        self.canvas.fps
    }

    /// One type size in pixels, rounded the way `bm-video` rounds it.
    pub fn px(&self, role: &str, width: u32) -> f64 {
        let scale = match role {
            "act_label" => self.type_.act_label,
            "act_title" => self.type_.act_title,
            "subtitle" => self.type_.subtitle,
            _ => self.type_.unit,
        };
        (self.unit(width) * scale).round()
    }

    pub fn unit(&self, width: u32) -> f64 {
        width as f64 * self.type_.unit / 100.0
    }

    pub fn font(&self, role: &str) -> Option<&str> {
        self.fonts.get(role).map(String::as_str)
    }

    /// How much taller than the track the playing act's segment stands.
    pub fn active_y_scale(&self) -> f64 {
        self.timeline.segment.active_y_scale
    }

    /// The thumb's disc, if the template has one, in this frame's pixels.
    pub fn thumb_disc(&self, height: u32) -> Option<ThumbDisc> {
        let th = self.timeline.thumb.as_ref()?;
        let dia = (height as f64 * th.radius * 2.0 * th.scale / self.canvas.size[1] as f64)
            .round()
            .max(1.0);
        // The fill reaches 68% of the radius; the rest is the ring.
        Some(ThumbDisc { dia, ring_w: (dia * 0.16).max(1.0) })
    }

    /// The act plate's label with its chapter number filled in.
    pub fn label(&self, act: u32) -> String {
        self.plate.label.replace('#', &act.to_string())
    }

    /// Every zone as a pixel box, `align_*` resolved to a fixed point.
    pub fn boxes(&self, width: u32, height: u32) -> BTreeMap<String, Rect> {
        let (w, h) = (width as f64, height as f64);
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
                    x: round3(z.at[0] * w - bw * w * hh),
                    y: round3(z.at[1] * h - bh * h * vv),
                    w: round3(bw * w),
                    h: round3(bh * h),
                },
            );
        }
        // A zone may align to one visited after it; iterate instead of trusting
        // the document's key order.
        for _ in 0..=self.zones.len() {
            let mut moved = false;
            for (name, z) in &self.zones {
                let base = out[name];
                let top = match z.align_top.as_ref().and_then(|n| out.get(n)) {
                    Some(r) => r.y,
                    None => base.y,
                };
                let bottom = match z.align_bottom.as_ref().and_then(|n| out.get(n)) {
                    Some(r) => r.y + r.h - base.h,
                    None => base.y,
                };
                let y = if z.align_bottom.is_some() { bottom } else { top };
                if (z.align_top.is_some() || z.align_bottom.is_some()) && round3(y) != base.y {
                    out.insert(name.clone(), Rect { y: round3(y), ..base });
                    moved = true;
                }
            }
            if !moved {
                break;
            }
        }
        out
    }

    pub fn box_of(&self, name: &str, width: u32, height: u32) -> Option<Rect> {
        self.boxes(width, height).get(name).copied()
    }

    /// The bar: the track across the timeline zone, and one segment per act,
    /// sized by the act's length, spaced and rounded as `bm-video` does.
    pub fn bar(&self, durations: &[f64], width: u32, height: u32) -> Option<BarGeom> {
        let tz = self.box_of("timeline", width, height)?;
        if durations.is_empty() {
            return None;
        }
        let ch = self.canvas.size[1] as f64;
        let track_h = (height as f64 * self.timeline.track.thickness / ch).round();
        let gap = (height as f64 * 0.008).round();
        let sum: f64 = durations.iter().sum();
        let sum = if sum == 0.0 { 1.0 } else { sum };
        let usable = tz.w - gap * (durations.len() as f64 - 1.0);
        let mut x = tz.x;
        let mut segs = Vec::with_capacity(durations.len());
        for dur in durations {
            let sw = round3(usable * dur / sum);
            segs.push((round3(x), sw));
            x += sw + gap;
        }
        Some(BarGeom {
            left: tz.x,
            width: tz.w,
            track_h,
            cy: round3(tz.y + tz.h / 2.0),
            segs,
        })
    }

    /// A palette entry as FilmCraft takes a colour.
    ///
    /// The template's background is a CSS gradient; FilmCraft has no gradient
    /// fill, so the middle stop stands in for it — the other stops are white and
    /// near-white, and the wash they make is within a few levels of it.
    pub fn colour(&self, key: &str) -> Option<String> {
        css_colour(self.palette.get(key)?)
    }

    /// Template fonts the machine does not have, with the file each one is in.
    pub fn missing_fonts(&self, installed: &[String]) -> Vec<(String, String, String)> {
        let mut out = Vec::new();
        for (role, family) in &self.fonts {
            let have = installed.iter().any(|f| f.eq_ignore_ascii_case(family));
            if !have {
                let file = self.font_files.get(role).cloned().unwrap_or_default();
                out.push((role.clone(), family.clone(), file));
            }
        }
        out
    }
}

// Geometry in thousandths of a pixel: readable in the script, and a value that
// survives the round trip through JSON.
fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
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

/// `#rrggbb[aa]`, `rgba(r, g, b, a)` or a gradient's middle stop.
fn css_colour(value: &str) -> Option<String> {
    let s = value.trim();
    if let Some(rest) = s.strip_prefix("rgba(").and_then(|v| v.strip_suffix(')')) {
        let p: Vec<&str> = rest.split(',').map(str::trim).collect();
        if p.len() == 4 {
            let channel = |v: &str| v.parse::<u8>().ok();
            let alpha = p[3].parse::<f64>().ok()?;
            let a = (alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
            return Some(format!(
                "#{:02x}{:02x}{:02x}{a:02x}",
                channel(p[0])?,
                channel(p[1])?,
                channel(p[2])?
            ));
        }
    }
    if s.contains("gradient(") {
        // A stop is a colour and where it sits — `#fafafa 55%` — and only the
        // colour is ours.
        let stops: Vec<&str> = s
            .split([',', '('])
            .filter_map(|t| t.split_whitespace().next())
            .filter(|t| t.starts_with('#'))
            .collect();
        if !stops.is_empty() {
            return css_colour(stops[stops.len() / 2]);
        }
    }
    if s.starts_with('#') && s.len() >= 7 {
        return Some(s.to_string());
    }
    None
}
