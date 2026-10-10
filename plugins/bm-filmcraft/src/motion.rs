//! The template's animation vocabulary: a property's segment, its easing, and
//! where it sits at an instant.
//!
//! `bm-video` evaluates the same keys of the same document, so one motion is one
//! thing in both renderers: this is a port of its `render/layers.rs`, not a
//! second language.

use anyhow::{bail, Result};
use serde::Deserialize;

/// The easing names a track may use. Both renderers know all of them.
pub const EASE_NAMES: &[&str] = &[
    "linear",
    "in",
    "out",
    "in-out",
    "in-quad",
    "out-quad",
    "in-out-quad",
    "in-cubic",
    "out-cubic",
    "in-out-cubic",
    "in-sin",
    "out-sin",
    "in-out-sin",
];

/// The properties a track may animate.
pub const PROPS: [&str; 5] = ["x", "y", "scale", "rotate", "opacity"];

/// One property's animation: a segment eased `from` → `to` over `dur_s`, and
/// optionally repeated. Several tracks may name the same `prop`; they are then
/// segments in time order, each taking over where the one before it ended, so a
/// fade in and a fade out at the end are two `opacity` tracks.
#[derive(Clone, Debug, Deserialize)]
pub struct MotionTrack {
    /// `x`, `y`, `scale`, `rotate` or `opacity`.
    pub prop: String,
    #[serde(default)]
    pub from: f64,
    #[serde(default)]
    pub to: f64,
    /// One pass. Defaults to the motion's `period_s`.
    #[serde(default)]
    pub dur_s: Option<f64>,
    #[serde(default = "d_linear")]
    pub ease: String,
    /// Passes: `0` runs to the end of the video, `1` is a one-shot.
    #[serde(default = "d_one_pass")]
    pub repeat: u32,
    /// Reverse direction on each pass: an oscillation rather than a sawtooth.
    #[serde(default)]
    pub pingpong: bool,
    /// Where a one-shot segment begins: `start` (the default) or `end`.
    #[serde(default = "d_start")]
    pub align: String,
}

fn d_linear() -> String {
    "linear".into()
}
fn d_one_pass() -> u32 {
    1
}
fn d_start() -> String {
    "start".into()
}

/// A motion: named presets, or tracks of your own. `period_s` defaults to the
/// whole book, so omitting it means one pass from start to finish.
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
    /// The animation itself, when it is not one of the presets.
    #[serde(default)]
    pub tracks: Vec<MotionTrack>,
}

/// One segment's pace, `u` in 0..1.
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

/// Refuse a track the renderers cannot draw, rather than quietly holding still.
pub fn check(tracks: &[MotionTrack]) -> Result<()> {
    for tr in tracks {
        if !PROPS.contains(&tr.prop.as_str()) {
            bail!("a template animates {:?}, which is not one of {PROPS:?}", tr.prop);
        }
        if !EASE_NAMES.contains(&canonical(&tr.ease).as_str()) {
            bail!("a template eases with {:?}, which is not one of {EASE_NAMES:?}", tr.ease);
        }
    }
    Ok(())
}

/// What a property sits at when no track names it: an opacity or a scale of one,
/// and no offset anywhere else.
pub fn dflt(prop: &str) -> f64 {
    if prop == "scale" || prop == "opacity" {
        1.0
    } else {
        0.0
    }
}

/// The value a property is at, `t` seconds in: its tracks in time order, each
/// within its own window, and the last value held past the end of them all.
pub fn value(tracks: &[MotionTrack], prop: &str, t: f64, total: f64, period: f64) -> f64 {
    let mut held: Option<f64> = None;
    for tr in tracks.iter().filter(|tr| tr.prop == prop) {
        let dur = tr.dur_s.unwrap_or(period).max(1e-3);
        let start = if tr.align.eq_ignore_ascii_case("end") {
            (total - dur).max(0.0)
        } else {
            0.0
        };
        if t < start {
            return held.unwrap_or(tr.from);
        }
        let passes = if tr.repeat == 0 { f64::INFINITY } else { f64::from(tr.repeat) };
        let at = (t - start) / dur;
        if at < passes {
            let i = at.floor();
            let (a, b) = if tr.pingpong && (i as i64) % 2 == 1 {
                (tr.to, tr.from)
            } else {
                (tr.from, tr.to)
            };
            return a + (b - a) * ease(&tr.ease, at - i);
        }
        held = Some(if tr.pingpong && passes % 2.0 == 0.0 { tr.from } else { tr.to });
    }
    held.unwrap_or_else(|| dflt(prop))
}

/// The presets, as the tracks they always were: one pass, or a ping-pong that
/// loops. A `bob` is a sine — an `in-out-sin` segment ping-ponged is
/// `-cos(2πt/P)`, a quarter period behind `bm-video`'s, at the same amplitude
/// and period.
pub fn preset_tracks(
    m: &Motion,
    zone_w: f64,
    fitted_w: f64,
    period: f64,
) -> Result<Vec<MotionTrack>> {
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
            (zone_w - fitted_w).max(0.0) / zone_w.max(1.0),
            period,
            "linear",
            false,
        )],
        "bob" => vec![track("y", -amplitude, amplitude, period / 2.0, "in-out-sin", true)],
        "spin" => vec![track("rotate", 0.0, sign * 360.0, period, "linear", false)],
        "" => Vec::new(),
        other => bail!(
            "template motion {other:?} is not one `bm-video` draws — name it `travel`, `bob`, \
             `spin`, or animate it with tracks"
        ),
    })
}
