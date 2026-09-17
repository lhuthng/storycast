//! Post-process: the two sound-design layers under the voice mix.
//!
//! Exactly two layers, and the split is the point:
//!
//! * **effect** — per-scene beds and one-shot stingers, deliberately *sparse*.
//!   A window opens on a scene that names effect tags, lasts at most
//!   `max_window_s`, waits `cooldown_s` of silence after the previous window,
//!   and the chapter may not spend more than `max_coverage` of its runtime on
//!   the layer. A bed running under 100% of a chapter is a wall, not a bed.
//! * **music** — background tracks, continuous and far below the effects. A
//!   scene with no music tags, or with `music_off`, or whose tags match nothing
//!   in the pool, gets no music at all: silence is a valid answer here, not a
//!   failure to be papered over.
//!
//! A third thing the scene map controls — room reverb — is *not* a layer. It is
//! applied to the voice before the layers exist, and it is left alone here.
//!
//! Both layers are ducked by **one** sidechain compressor keyed on the voice
//! track, applied to them as a single bus. That makes "every layer drops
//! whenever anybody speaks" a property of the signal path rather than a rule
//! each layer has to remember — and because the key is the whole voice track,
//! the narrator ducks them exactly as a character does. The lift the music gets
//! inside a planned pause is the same compressor releasing: nothing special is
//! done for it beyond holding the beat long enough for the release to finish.
//!
//! Ported from `ambience.py`. Offline, deterministic, no API: the same script,
//! scene map and pools always produce the same mix, and a missing clip degrades
//! to silence for that span rather than failing a chapter.

use crate::assemble::{read_wav, Run};
use crate::audio_pool::{self, ClipPool};
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// the scene map
// ---------------------------------------------------------------------------

/// What a *place* sounds like. Field names are the scene map's, and the same
/// struct is produced both by a `rules` entry and by `default`.
///
/// Deliberately no music field. A place is where we are; what it feels like is
/// the script's `music` value, and the palette is the only thing that decides a
/// track. One string doing both jobs is how `martial-shop-morning` — a place —
/// came out with a hearth under it, because the keyword `shop` matched a fire
/// rule before the keyword `morning` reached the daylight rule.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SceneRule {
    /// Pool tags, not filenames: `["rain"]` lets the effect pool decide which
    /// rain clip answers. Empty means dry voice.
    #[serde(default)]
    pub effect: Vec<String>,
    /// Linear gain over a clip normalized to -26 LUFS, so it means the same
    /// thing for every clip in the pool.
    #[serde(default)]
    pub level: f64,
    #[serde(default)]
    pub reverb: Option<String>,
    /// A beat held *before* this scene's first line, in delivered seconds.
    /// `0.0` means "use `pause.pause_s`"; see [`plan_pauses`].
    #[serde(default)]
    pub pause_before_s: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Rule {
    #[serde(rename = "match", default)]
    pub matches: Vec<String>,
    #[serde(default)]
    pub effect: Vec<String>,
    #[serde(default)]
    pub level: f64,
    #[serde(default)]
    pub reverb: Option<String>,
    #[serde(default)]
    pub pause_before_s: f64,
}

/// One value of the closed music vocabulary: the pool tags it means, plus the
/// gloss the digest prompt shows the analyzer.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PaletteEntry {
    /// Pool tags for assets/music-pool.json. Empty is the `none` value — no
    /// track at all, which is a choice, not a missing one.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Shown to the analyzer in the prompt. Not read by the mix.
    #[serde(default)]
    pub note: String,
}

/// Mood -> what it sounds like. Closed by construction: a script value that is
/// not a key here is rejected at digest time.
pub type MusicPalette = BTreeMap<String, PaletteEntry>;

/// Read a palette object, skipping `_note`-style keys — the same convention the
/// clip pools use, so a section can document itself in place without becoming
/// an entry the analyzer could emit.
fn de_palette<'de, D>(d: D) -> Result<MusicPalette, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize as _;
    let v = Value::deserialize(d)?;
    let mut out = MusicPalette::new();
    if let Some(obj) = v.as_object() {
        for (k, val) in obj {
            if k.starts_with('_') {
                continue;
            }
            if let Ok(e) = serde_json::from_value::<PaletteEntry>(val.clone()) {
                out.insert(k.clone(), e);
            }
        }
    }
    Ok(out)
}

/// One keyword rule of the migration shim; see [`SceneMap::legacy_scene_music`].
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LegacyMusicRule {
    #[serde(rename = "match", default)]
    pub matches: Vec<String>,
    #[serde(default)]
    pub music: String,
}

/// Where a script that predates the `music` field gets its mood from.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LegacyMusic {
    #[serde(default)]
    pub rules: Vec<LegacyMusicRule>,
    #[serde(default)]
    pub default: LegacyMusicRule,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Duck {
    #[serde(default = "default_threshold")]
    pub threshold: f64,
    #[serde(default = "default_ratio")]
    pub ratio: f64,
    #[serde(default = "default_attack")]
    pub attack: u32,
    #[serde(default = "default_release")]
    pub release: u32,
}

fn default_threshold() -> f64 {
    0.02
}
fn default_ratio() -> f64 {
    6.0
}
fn default_attack() -> u32 {
    20
}
fn default_release() -> u32 {
    400
}

impl Default for Duck {
    fn default() -> Self {
        Duck {
            threshold: default_threshold(),
            ratio: default_ratio(),
            attack: default_attack(),
            release: default_release(),
        }
    }
}

/// How sparse the effect layer must be.
#[derive(Debug, Clone, Deserialize)]
pub struct EffectLayer {
    /// Fraction of the chapter's runtime the layer may occupy in total.
    #[serde(default = "d_max_coverage")]
    pub max_coverage: f64,
    /// Silence required after a window closes before the next may open.
    #[serde(default = "d_cooldown")]
    pub cooldown_s: f64,
    /// A scene shorter than this is not worth a window.
    #[serde(default = "d_min_span")]
    pub min_span_s: f64,
    /// Longest single window.
    #[serde(default = "d_max_window")]
    pub max_window_s: f64,
    /// Fade at each window edge, against a click.
    #[serde(default = "d_fade")]
    pub fade_s: f64,
    /// Master gain for the layer, multiplied into every rule's own `level`.
    ///
    /// A rule's `level` is the *relative* balance between scenes — a storm
    /// against a hearth — and is the wrong place to say "the layer as a whole
    /// is too hot": that is one opinion about the whole layer, and expressing
    /// it per rule means retuning the layer is thirteen edits that can drift
    /// apart. Defaults to 1.0, so every map written before this field existed
    /// keeps its exact mix.
    #[serde(default = "d_effect_trim")]
    pub trim: f64,
}

fn d_effect_trim() -> f64 {
    1.0
}

fn d_max_coverage() -> f64 {
    0.35
}
fn d_cooldown() -> f64 {
    45.0
}
fn d_min_span() -> f64 {
    20.0
}
fn d_max_window() -> f64 {
    75.0
}
fn d_fade() -> f64 {
    0.3
}

impl Default for EffectLayer {
    fn default() -> Self {
        EffectLayer {
            max_coverage: d_max_coverage(),
            cooldown_s: d_cooldown(),
            min_span_s: d_min_span(),
            max_window_s: d_max_window(),
            fade_s: d_fade(),
            trim: d_effect_trim(),
        }
    }
}

/// How loud the music sits, and what it does inside a pause.
#[derive(Debug, Clone, Deserialize)]
pub struct MusicLayer {
    /// Quiet on purpose: 0.06, against an effect layer whose rules reach 0.11
    /// once `layers.effect.trim` has been applied.
    #[serde(default = "d_music_level")]
    pub level: f64,
    /// Level inside a planned pause. The compressor has released by then, so
    /// this is a *further* lift on top of an already-unducked track.
    #[serde(default = "d_music_pause_level")]
    pub pause_level: f64,
    /// Fade at the head and tail of the whole layer.
    #[serde(default = "d_fade")]
    pub fade_s: f64,
    /// Crossfade where the track changes.
    #[serde(default = "d_xfade")]
    pub xfade_s: f64,
    /// How long the lift into and out of a pause takes. Must stay well under
    /// the shortest pause or the lift never arrives.
    #[serde(default = "d_ramp")]
    pub ramp_s: f64,
}

fn d_music_level() -> f64 {
    0.06
}
fn d_music_pause_level() -> f64 {
    0.085
}
fn d_xfade() -> f64 {
    2.0
}
fn d_ramp() -> f64 {
    0.6
}

impl Default for MusicLayer {
    fn default() -> Self {
        MusicLayer {
            level: d_music_level(),
            pause_level: d_music_pause_level(),
            fade_s: d_fade(),
            xfade_s: d_xfade(),
            ramp_s: d_ramp(),
        }
    }
}

/// Where a beat fits, and how long it lasts.
#[derive(Debug, Clone, Deserialize)]
pub struct PausePlan {
    /// Delivered seconds — the merge scales it by `speed` before writing it.
    #[serde(default = "d_pause")]
    pub pause_s: f64,
    #[serde(default = "d_max_pauses")]
    pub max_per_chapter: usize,
    /// Only hold a beat where a scene change is narrated (either side of the
    /// boundary is the Narrator). A scene change mid-exchange is not a beat,
    /// it is a hole in a conversation.
    #[serde(default = "d_true")]
    pub require_narration: bool,
}

fn d_pause() -> f64 {
    1.5
}
fn d_max_pauses() -> usize {
    1
}
fn d_true() -> bool {
    true
}

impl Default for PausePlan {
    fn default() -> Self {
        PausePlan {
            pause_s: d_pause(),
            max_per_chapter: d_max_pauses(),
            require_narration: d_true(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Layers {
    #[serde(default)]
    pub effect: EffectLayer,
    #[serde(default)]
    pub music: MusicLayer,
}

/// Which of the two layers are on for this chapter.
///
/// A struct rather than two positional `bool`s so a call site cannot swap them,
/// and so the answer to "can this chapter come out dry?" is one `none()` rather
/// than a compound condition that has to stay in step with the argument list.
/// The two switches are independent on purpose: a book can want the effect beds
/// and no music, which is `Settings::ambience` and `Settings::music` doing
/// exactly what they say.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayerSwitch {
    pub effects: bool,
    pub music: bool,
    pub effect_volume: f64,
    pub music_volume: f64,
}

impl LayerSwitch {
    pub fn new(effects: bool, music: bool, effect_volume: f64, music_volume: f64) -> Self {
        LayerSwitch {
            effects,
            music,
            effect_volume: effect_volume.max(0.0),
            music_volume: music_volume.max(0.0),
        }
    }

    pub fn none(&self) -> bool {
        !self.effects && !self.music
    }
}

/// Every knob the two layers read, plus the palette that decides the music.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SceneMap {
    #[serde(default)]
    pub rules: Vec<Rule>,
    #[serde(default)]
    pub default: SceneRule,
    /// The closed music vocabulary the digest may emit, and the pool tags each
    /// value means. One table, read twice: [`palette_prompt`] renders it into
    /// the analyzer prompt, and [`plan_music`] resolves a value to a track — so
    /// the prompt and the pool cannot disagree about what `warm` sounds like.
    #[serde(default, deserialize_with = "de_palette")]
    pub music_palette: MusicPalette,
    /// Where a script that predates the `music` field gets its mood from.
    /// A migration shim, kept so chapters already on disk keep merging; a
    /// freshly digested script declares `music` per segment and never reads it.
    #[serde(default)]
    pub legacy_scene_music: LegacyMusic,
    #[serde(default)]
    pub reverb_presets: BTreeMap<String, String>,
    #[serde(default)]
    pub duck: Duck,
    #[serde(default)]
    pub layers: Layers,
    #[serde(default)]
    pub pause: PausePlan,
}

/// The palette's keys, sorted — what a script's `music` value is checked
/// against, and what a rejection message lists back to the analyzer.
pub fn palette_names(map: &SceneMap) -> Vec<String> {
    map.music_palette.keys().cloned().collect()
}

/// The palette rendered for the digest prompt: `name (tags; gloss), ...`.
///
/// Built from the map rather than written into the prompt text, so adding a
/// value (and the clip that answers it) is one edit to one file. A prompt that
/// listed its own vocabulary would drift the moment the pool changed. The tags
/// ride along so the analyzer sees what each mood *means* in pool terms — the
/// merge scores those same tags, so a mood picked for its tags resolves to the
/// track the analyzer had in mind.
pub fn palette_prompt(map: &SceneMap) -> String {
    map.music_palette
        .iter()
        .map(|(name, e)| {
            let mut parts = Vec::new();
            if !e.tags.is_empty() {
                parts.push(e.tags.join(", "));
            }
            let note = e.note.trim();
            if !note.is_empty() {
                parts.push(note.to_string());
            }
            if parts.is_empty() {
                name.clone()
            } else {
                format!("{name} ({})", parts.join("; "))
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Every tag any effect-pool sound answers to, sorted and deduped: the effect
/// vocabulary the digest prompt offers the analyzer. A scene built from these
/// words resolves to a pooled sound by tag overlap instead of by keyword luck.
pub fn effect_tags(pool: &ClipPool) -> Vec<String> {
    let mut out = std::collections::BTreeSet::new();
    for sound in pool.values() {
        for t in &sound.tags {
            out.insert(t.clone());
        }
    }
    out.into_iter().collect()
}

pub fn load_map(path: &Path) -> Result<SceneMap> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading scene map {}", path.display()))?;
    let map: SceneMap = serde_json::from_str(&text)
        .with_context(|| format!("parsing scene map {}", path.display()))?;
    Ok(map)
}

/// First matching rule wins (ordered specific -> general).
///
/// Substring matching, not token matching, and on purpose: `market-stall-morning`
/// has to reach the `market` rule. The cost is that a keyword can fire on a word
/// that is only part of a compound label — which is why the rules are ordered
/// most-specific-first and why the generic place nouns that used to sit in a
/// catch-all (`shop`, `room`) are no longer keywords here.
pub fn match_scene(scene: &str, cfg: &SceneMap) -> SceneRule {
    let s = scene.to_lowercase();
    for rule in &cfg.rules {
        if rule.matches.iter().any(|k| s.contains(&k.to_lowercase())) {
            return SceneRule {
                effect: rule.effect.clone(),
                level: rule.level,
                reverb: rule.reverb.clone(),
                pause_before_s: rule.pause_before_s,
            };
        }
    }
    cfg.default.clone()
}

/// The majority value of a field across a run, ties to the last seen.
///
/// Shared by the scene and the music summary so a run cannot end up summarised
/// two different ways, and so "which line wins a tie" is decided once.
fn majority(vals: impl Iterator<Item = String>) -> String {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for v in vals {
        if v.is_empty() {
            continue;
        }
        match counts.iter_mut().find(|(t, _)| *t == v) {
            Some((_, n)) => *n += 1,
            None => counts.push((v, 1)),
        }
    }
    counts
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .map(|(t, _)| t)
        .unwrap_or_default()
}

fn seg_field(segments: &[Value], i: usize, key: &str) -> String {
    segments
        .get(i)
        .and_then(|s| s.get(key))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Majority scene tag per run (runs are consecutive same-speaker lines).
pub fn run_scenes(segments: &[Value], runs: &[Run]) -> Vec<String> {
    runs.iter()
        .map(|run| majority(run.idx.iter().map(|i| seg_field(segments, *i, "scene"))))
        .collect()
}

/// Majority `music` value per run, falling back to the legacy keyword lookup.
///
/// A run that declares no value anywhere is a run from a script written before
/// the field existed, so it is scored by `legacy_scene_music` off its scene
/// label — the shim that keeps chapters already on disk mergeable. Mixed runs
/// resolve the same way per run, which is the graceful reading: a hand-edited
/// old script still merges rather than losing its music entirely.
pub fn run_music(segments: &[Value], runs: &[Run], cfg: &SceneMap) -> Vec<String> {
    let mut musics = run_scenes(segments, runs);
    for (run, scene) in runs.iter().zip(musics.iter_mut()) {
        let declared = majority(run.idx.iter().map(|i| seg_field(segments, *i, "music")));
        *scene = if declared.is_empty() {
            legacy_music(scene, cfg)
        } else {
            declared
        };
    }
    musics
}

/// One segment's mood: its declared `music` value, else the legacy lookup over
/// its scene label. The single-segment form of [`run_music`], for the cloud
/// engine path where one turn *is* one segment.
pub fn resolve_music(scene: &str, declared: Option<&str>, cfg: &SceneMap) -> String {
    match declared.map(str::trim).filter(|m| !m.is_empty()) {
        Some(m) => m.to_string(),
        None => legacy_music(scene, cfg),
    }
}

/// The migration shim: keyword rules over a scene label, to a palette value.
pub fn legacy_music(scene: &str, cfg: &SceneMap) -> String {
    let s = scene.to_lowercase();
    for rule in &cfg.legacy_scene_music.rules {
        if rule.matches.iter().any(|k| s.contains(&k.to_lowercase())) {
            return rule.music.clone();
        }
    }
    cfg.legacy_scene_music.default.music.clone()
}

// ---------------------------------------------------------------------------
// the timeline
// ---------------------------------------------------------------------------

/// A planned turn: the wav the renderer produced, and what the layers need to
/// know about it.
///
/// `scene` is the place (effect + reverb) and `music` is the mood (which track
/// plays). Two fields, two jobs — see this module's docs.
#[derive(Debug, Clone)]
pub struct Turn {
    pub wav: PathBuf,
    pub scene: String,
    /// A palette value, or `""` for none. Resolved by the caller from the
    /// script's own `music` field, or from the legacy shim.
    pub music: String,
    pub speaker: String,
}

/// One position in the mix: what plays, when it starts and ends, and how much
/// air follows it.
///
/// The timeline is built once and read twice — [`crate::assemble::concat_slots`]
/// writes these gaps and the layers read these offsets — which is the whole
/// reason it exists. The gap arithmetic used to live in two places (the concat
/// and the span builder) and stayed correct only because the gap was a
/// constant; a variable-length pause would have let them drift apart silently,
/// and the layers would have slid onto the wrong turns.
#[derive(Debug, Clone, PartialEq)]
pub struct Slot {
    pub wav: PathBuf,
    pub scene: String,
    pub music: String,
    pub speaker: String,
    pub start: f64,
    pub end: f64,
    /// Silence written after this turn: the uniform gap plus any beat.
    pub gap_ms: u32,
    /// How much of `gap_ms` is a planned beat, held between this turn and the
    /// next. The layers need the beat on its own — it is where the music lifts.
    pub pause_ms: u32,
}

/// Lay the turns out on the mix clock, inserting the planned pauses.
///
/// `pauses` maps a turn index to the beat held *before* that turn, in pre-tempo
/// milliseconds — the scene map authors it as `pause_before_s` on the scene
/// being entered. The mix has no place to hold a beat before the first thing in
/// the chapter, so a beat is written into the gap that *follows* turn `i-1`:
/// the same silence, named from the other side. [`Slot::pause_ms`] therefore
/// reads as "the beat after this turn", and [`pause_intervals`] can hand the
/// music layer an interval without knowing which side it was authored from.
pub fn timeline(turns: &[Turn], gap_ms: u32, pauses: &BTreeMap<usize, u32>) -> Result<Vec<Slot>> {
    let mut out: Vec<Slot> = Vec::with_capacity(turns.len());
    let mut params: Option<(u16, u32, u16)> = None;
    let mut t = 0.0f64;
    for (i, turn) in turns.iter().enumerate() {
        let w = read_wav(&turn.wav)?;
        let p = (w.channels, w.sample_rate, w.bits);
        match params {
            None => params = Some(p),
            Some(prev) if prev != p => anyhow::bail!(
                "{}: {p:?} != {prev:?} (mixed engines/rates — use per-engine seg dirs)",
                turn.wav.display()
            ),
            _ => {}
        }
        let dur = w.seconds();
        let pause_ms = pauses.get(&(i + 1)).copied().unwrap_or(0);
        out.push(Slot {
            wav: turn.wav.clone(),
            scene: turn.scene.clone(),
            music: turn.music.clone(),
            speaker: turn.speaker.clone(),
            start: t,
            end: t + dur,
            gap_ms: gap_ms + pause_ms,
            pause_ms,
        });
        t += dur + (gap_ms + pause_ms) as f64 / 1000.0;
    }
    Ok(out)
}

/// Rescale a timeline from pre-tempo seconds to delivered seconds.
///
/// [`timeline`] lays the slots out from the raw voice wavs, so every offset is
/// on the *pre-tempo* clock. Once the speech has been through `atempo` the
/// delivered clock is `pre / speed`, and a layer placed against the pre-tempo
/// clock slides further behind the voice with every line — by the end of a
/// 7-minute chapter the music is a minute and a half out of place.
///
/// Scaling the whole timeline by one factor is exact rather than approximate:
/// `t` accumulates `duration + gap`, and `atempo` divides both by `speed`.
/// The gaps are scaled too, even though the concat has already written them,
/// because a `Slot` whose `start` is delivered seconds and whose `gap_ms` is
/// pre-tempo milliseconds is a struct lying about itself.
pub fn retime(slots: &mut [Slot], speed: f64) {
    if speed <= 0.0 || (speed - 1.0).abs() <= f64::EPSILON {
        return;
    }
    for s in slots.iter_mut() {
        s.start /= speed;
        s.end /= speed;
        s.gap_ms = (s.gap_ms as f64 / speed).round() as u32;
        s.pause_ms = (s.pause_ms as f64 / speed).round() as u32;
    }
}

/// The pause intervals of a timeline, absolute seconds: where the music lifts.
pub fn pause_intervals(slots: &[Slot]) -> Vec<(f64, f64)> {
    slots
        .iter()
        .filter(|s| s.pause_ms > 0)
        .map(|s| (s.end, s.end + s.pause_ms as f64 / 1000.0))
        .collect()
}

/// Where a beat fits in this chapter, as `turn index -> pre-tempo milliseconds`.
///
/// A beat belongs where a *scene changes and the change is narrated*: the
/// incoming or the outgoing turn must be the Narrator, so the pause lands on
/// narration handing over rather than in the middle of an exchange. Narration
/// *resuming* is the stronger signal — a new scene establishing itself — so it
/// outranks narration handing off; the longest `pause_before_s` the scene map
/// declares breaks the remaining ties, and the earliest boundary breaks those.
///
/// `speed` is applied here because the pause is authored in *delivered*
/// seconds: at `atempo=1.25` a 1.5 s beat written as 1.5 s of silence would
/// arrive as 1.2 s, and every pause in the book would be quietly short by the
/// same factor.
pub fn plan_pauses(turns: &[Turn], map: &SceneMap, speed: f64) -> BTreeMap<usize, u32> {
    let mut out = BTreeMap::new();
    let cfg = &map.pause;
    if cfg.max_per_chapter == 0 || turns.len() < 2 {
        return out;
    }
    let mut cands: Vec<(u8, f64, usize)> = Vec::new();
    for i in 1..turns.len() {
        let (prev, cur) = (&turns[i - 1], &turns[i]);
        // A beat marks a *change of scene*, so both sides have to name one. An
        // untagged turn is not a scene: the chapter headline leads with none,
        // and a mid-chapter line the analyzer left blank must not manufacture a
        // boundary — that would put a beat in the middle of a continuous scene.
        if prev.scene.is_empty() || cur.scene.is_empty() || cur.scene == prev.scene {
            continue;
        }
        if cfg.require_narration && cur.speaker != "Narrator" && prev.speaker != "Narrator" {
            continue;
        }
        let rule = match_scene(&cur.scene, map);
        let secs = if rule.pause_before_s > 0.0 {
            rule.pause_before_s
        } else {
            cfg.pause_s
        };
        let rank = if cur.speaker == "Narrator" { 2u8 } else { 1 };
        cands.push((rank, secs, i));
    }
    // Best first: strongest narration signal, then longest declared beat, then
    // earliest — so the choice is a decision, not whichever rule happened to
    // come first in the file.
    cands.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.total_cmp(&a.1)).then(a.2.cmp(&b.2)));
    for (_, secs, i) in cands.into_iter().take(cfg.max_per_chapter) {
        let ms = (secs * speed * 1000.0).round();
        if ms > 0.0 {
            out.insert(i, ms as u32);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// spans: one rule, one stretch of the clock
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    pub effect: Vec<String>,
    pub level: f64,
    pub reverb: Option<String>,
    pub scene: String,
    pub start: f64,
    pub end: f64,
}

/// Tile the timeline into spans of identical *place* treatment. Adjacent turns
/// that resolve to the same rule merge, so a bed is not restarted every line.
///
/// Deliberately not keyed on music: the effect layer's windows and the music
/// layer's cues are independent timelines, and folding a mood change into this
/// merge would let a change of track cut an effect window short. Music is read
/// per slot by [`plan_music`].
pub fn build_spans(slots: &[Slot], cfg: &SceneMap) -> Vec<Span> {
    let mut spans: Vec<Span> = Vec::new();
    for slot in slots {
        let rule = match_scene(&slot.scene, cfg);
        match spans.last_mut() {
            Some(last)
                if last.effect == rule.effect
                    && last.level == rule.level
                    && last.reverb == rule.reverb =>
            {
                last.end = slot.end;
            }
            _ => spans.push(Span {
                effect: rule.effect,
                level: rule.level,
                reverb: rule.reverb,
                scene: slot.scene.clone(),
                start: slot.start,
                end: slot.end,
            }),
        }
    }
    spans
}

// ---------------------------------------------------------------------------
// the effect layer's windows
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub start: f64,
    pub end: f64,
    pub level: f64,
    pub tags: Vec<String>,
    /// Index into the span list, for the log line and the seeded pick.
    pub span: usize,
}

/// One effect window as the *report* needs it: the place it came from, when it
/// plays, and the clip that answered it.
///
/// The span index is the whole point. `plan_windows` opens a window at
/// `span.start.max(free_at)`, so a window can start later than the span it came
/// from — the previous window's cooldown pushes it. The report used to match
/// windows to spans by start offset, through a formatted string
/// (`l.starts_with("[110-")`), so any window the cooldown had pushed read as
/// "no effect" on its own span. Chapter 13 measured 75 s of night under
/// `courtyard-evening` that the log claimed was silent.
#[derive(Debug, Clone, PartialEq)]
struct FxReport {
    span: usize,
    start: f64,
    end: f64,
    name: String,
    level: f64,
    one_shot: bool,
}

/// Which stretches of the chapter carry an effect, and for how long.
///
/// Four gates, applied in this order: the span must name effect tags, must be
/// at least `min_span_s` long, may not start until `cooldown_s` after the
/// previous window closed, and the chapter's total may not exceed
/// `max_coverage`. The window is then clipped to `max_window_s`.
///
/// The budget is a *stop*, not a trim: once the chapter has spent its share,
/// later eligible scenes get nothing. Taking a sliver of the budget for a scene
/// that would only get a few seconds of it is worse than silence.
pub fn plan_windows(spans: &[Span], cfg: &EffectLayer, total: f64) -> Vec<Window> {
    let mut out: Vec<Window> = Vec::new();
    let budget = total * cfg.max_coverage.max(0.0);
    let mut used = 0.0f64;
    let mut free_at = 0.0f64;
    for (i, span) in spans.iter().enumerate() {
        if span.effect.is_empty() || span.level <= 0.0 {
            continue;
        }
        if span.end - span.start < cfg.min_span_s {
            continue;
        }
        let start = span.start.max(free_at);
        if start >= span.end {
            continue;
        }
        let room = budget - used;
        if room < cfg.min_span_s {
            break;
        }
        let len = (span.end - start).min(cfg.max_window_s).min(room);
        if len < cfg.min_span_s {
            continue;
        }
        out.push(Window {
            start,
            end: start + len,
            // The rule's relative balance, scaled by the layer's one master
            // gain. Read here rather than in `build_spans` so the span merge
            // still compares raw rule levels — the trim is a property of the
            // layer, not of a scene, and folding it in earlier would make two
            // rules that differ only by trim merge as one.
            level: span.level * cfg.trim,
            tags: span.effect.clone(),
            span: i,
        });
        used += len;
        free_at = start + len + cfg.cooldown_s;
    }
    out
}

// ---------------------------------------------------------------------------
// the music layer's runs
// ---------------------------------------------------------------------------

/// A stretch of the clock with one track under it, and the pauses inside it
/// where the music lifts.
#[derive(Debug, Clone, PartialEq)]
pub struct MusicRun {
    /// The palette value that chose this track — what the log reports and what
    /// a re-merge is compared against.
    pub mood: String,
    /// The *sound* the palette resolved to (`soft-relax`). Consecutive slots
    /// that land on the same sound are one run, because a repeated mood must be
    /// continuous music rather than a crossfade into the same tune.
    pub sound: String,
    /// The take that answers it. Which of `soft-relax-bg-1/2` plays is an
    /// implementation detail of the pick, so the mix reads it from here rather
    /// than looking the sound up a second time — one lookup, one answer.
    pub file: String,
    /// Audible coverage ends here; the slice may extend past it into a
    /// crossfade with the next run.
    pub start: f64,
    pub end: f64,
    pub pauses: Vec<(f64, f64)>,
}

/// Which track plays when, and where it lifts.
///
/// Read per *slot*, not per span: the mood is a property of the line being
/// spoken, and a cue breaks exactly where the mood changes. A place change
/// inside one mood therefore keeps one continuous track rather than crossfading
/// into the same tune — and, the other way round, a change of mood mid-scene
/// does change the track, which is the whole point of the field.
///
/// The pick is seeded from the palette *value* rather than its tags, so a change
/// of value is a change of track by construction: two values that happened to
/// share tags would otherwise crossfade into themselves. The chapter goes into
/// the seed too, so two chapters in the same mood still differ.
///
/// A slot with no value, with `none`, or whose palette entry names tags nothing
/// in the pool answers contributes nothing — no run, no silent file, no
/// gap-filling. `none` is a choice; a pool that has lost its last clip for a
/// mood is a degraded mix, and both are reported once each.
///
/// The layer's own knobs (`level`, `xfade_s`, `ramp_s`) are not read here: they
/// shape how a run is *rendered*, which is the caller's job.
pub fn plan_music(
    slots: &[Slot],
    pauses: &[(f64, f64)],
    chapter: u32,
    pool: &ClipPool,
    palette: &MusicPalette,
) -> Vec<MusicRun> {
    let mut out: Vec<MusicRun> = Vec::new();
    let mut unpooled: Vec<String> = Vec::new();
    for slot in slots {
        let mood = slot.music.trim();
        if mood.is_empty() || mood == "none" {
            continue;
        }
        // A value outside the palette is a script that never went through the
        // digest validator — say so rather than silently going quiet.
        let Some(entry) = palette.get(mood) else {
            let msg = format!("{mood} (not a palette value)");
            if !unpooled.contains(&msg) {
                unpooled.push(msg);
            }
            continue;
        };
        if entry.tags.is_empty() {
            continue;
        }
        let key = [mood.to_string()];
        let Some(picked) = audio_pool::pick(pool, &entry.tags, audio_pool::seed(chapter, 0, &key))
        else {
            let msg = format!("{mood} (no pooled clip for [{}])", entry.tags.join(", "));
            if !unpooled.contains(&msg) {
                unpooled.push(msg);
            }
            continue;
        };
        let inside: Vec<(f64, f64)> = pauses
            .iter()
            .copied()
            .filter(|(a, b)| *b > slot.start && *a < slot.end)
            .collect();
        match out.last_mut() {
            // Merge on the *sound*, not the take: the seed is derived from the
            // mood, so a repeated mood resolves to the same sound and the same
            // take anyway — merging on the sound is what keeps a scene change
            // inside one mood from cutting the music.
            Some(last) if last.sound == picked.sound => {
                last.end = slot.end;
                last.pauses.extend(inside);
            }
            _ => out.push(MusicRun {
                mood: mood.to_string(),
                sound: picked.sound,
                file: picked.file,
                start: slot.start,
                end: slot.end,
                pauses: inside,
            }),
        }
    }
    for m in &unpooled {
        eprintln!("music: {m} -> no music there");
    }
    out
}

// ---------------------------------------------------------------------------
// ffmpeg
// ---------------------------------------------------------------------------

fn ffmpeg(args: &[String]) -> Result<()> {
    let out = Command::new("ffmpeg")
        .args(args)
        .output()
        .context("spawning ffmpeg")?;
    if !out.status.success() {
        anyhow::bail!(
            "ffmpeg failed: {}",
            crate::util::head_chars(&String::from_utf8_lossy(&out.stderr), 300)
        );
    }
    Ok(())
}

fn s(v: impl ToString) -> String {
    v.to_string()
}

fn concat_files(parts: &[PathBuf], out: &Path) -> Result<()> {
    let list = out.with_file_name("parts.txt");
    let mut body = String::new();
    for p in parts {
        // absolute paths: the concat demuxer resolves relative ones against the playlist dir
        let abs = p.canonicalize().unwrap_or_else(|_| p.clone());
        body.push_str(&format!("file '{}'\n", abs.display()));
    }
    std::fs::write(&list, body)?;
    let r = ffmpeg(&[
        "-y".into(),
        "-loglevel".into(),
        "error".into(),
        "-f".into(),
        "concat".into(),
        "-safe".into(),
        "0".into(),
        "-i".into(),
        s(list.display()),
        "-c:a".into(),
        "pcm_s16le".into(),
        s(out.display()),
    ]);
    let _ = std::fs::remove_file(&list);
    r
}

/// One thing to place on the layer's own track.
#[derive(Debug, Clone)]
struct Slice {
    path: PathBuf,
    start: f64,
    /// How long the slice file actually is, for the fade-out position.
    dur: f64,
    fade_in: f64,
    fade_out: f64,
}

/// Place slices at exact offsets (`adelay` + `amix`, so no drift) with edge
/// fades against clicks, and pad the whole track out to `total`.
///
/// Slices may overlap: that is how the music layer crossfades between tracks.
/// Two complementary linear fades summing to unity is not a true equal-power
/// crossfade, but for uncorrelated beds the error is a fraction of a dB in the
/// middle of a two-second overlap — cheaper than a filter chain that would have
/// to know which slice comes next.
fn place(slices: &[Slice], out: &Path, total: f64) -> Result<()> {
    if slices.is_empty() {
        return ffmpeg(&[
            "-y".into(),
            "-loglevel".into(),
            "error".into(),
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            format!("anullsrc=r=48000:cl=mono:d={total:.3}"),
            s(out.display()),
        ]);
    }
    let mut args: Vec<String> = vec!["-y".into(), "-loglevel".into(), "error".into()];
    let mut filters: Vec<String> = Vec::new();
    for (n, sl) in slices.iter().enumerate() {
        args.push("-i".into());
        args.push(s(sl.path.display()));
        // Fades may not be longer than half the slice, or they would run past
        // each other and invert.
        let fi = sl.fade_in.clamp(0.0, sl.dur / 2.0);
        let fo = sl.fade_out.clamp(0.0, sl.dur / 2.0);
        let mut chain: Vec<String> = Vec::new();
        if fi > 0.0 {
            chain.push(format!("afade=t=in:st=0:d={fi:.3}"));
        }
        if fo > 0.0 {
            chain.push(format!(
                "afade=t=out:st={:.3}:d={fo:.3}",
                (sl.dur - fo).max(0.0)
            ));
        }
        let ms = (sl.start * 1000.0) as i64;
        chain.push(format!("adelay={ms}|{ms}"));
        filters.push(format!("[{n}:a]{}[s{n}]", chain.join(",")));
    }
    let labels: String = (0..slices.len()).map(|n| format!("[s{n}]")).collect();
    filters.push(format!(
        "{labels}amix=inputs={}:normalize=0,apad=whole_dur={total:.3},\
         aformat=sample_rates=48000:channel_layouts=mono[out]",
        slices.len()
    ));
    args.push("-filter_complex".into());
    args.push(filters.join(";"));
    args.push("-map".into());
    args.push("[out]".into());
    args.push("-t".into());
    args.push(format!("{total:.3}"));
    args.push(s(out.display()));
    ffmpeg(&args)
}

/// The music layer's gain over time, as an ffmpeg expression.
///
/// A flat level, plus one lift per pause built from two complementary `clip`
/// ramps. The alternative — slicing the track at each level change and
/// concatenating — restarts the loop at every boundary, which is audible; and
/// `t` is monotonic across `-stream_loop` boundaries, so an expression keyed on
/// it is safe on a looped source.
fn level_expr(base: f64, lift: f64, ramp: f64, run_start: f64, pauses: &[(f64, f64)]) -> String {
    let mut e = format!("{base:.6}");
    let d = lift - base;
    if d.abs() < 1e-9 || ramp <= 0.0 {
        return e;
    }
    for (a, b) in pauses {
        let ra = (a - run_start).max(0.0);
        let rb = (b - run_start).max(0.0);
        e.push_str(&format!(
            " + {d:.6}*clip((t-{ra:.3})/{ramp:.3},0,1) - {d:.6}*clip((t-{rb:.3})/{ramp:.3},0,1)"
        ));
    }
    e
}

/// Resolve a clip path. Pool files are `assets/`-relative, which is the same
/// directory the scene map came from — one root, so a pool and its clips cannot
/// be read from different places.
fn clip_path(assets: &Path, file: &str) -> PathBuf {
    assets.join(file)
}

/// Effective start offset per music run, so adjacent tracks crossfade.
///
/// The planner covers only speech (`run.end` is the slot's end), while the
/// renderer extends each run by `xfade_s` — leaving the two fades misaligned
/// by the inter-slot gap and dipping the mix mid-transition. Starting the
/// next track where the previous one's audible coverage ended aligns them.
/// Gaps wider than the crossfade plus a beat are intentional silence (`none`,
/// unpooled moods) and keep their hole.
fn music_starts(runs: &[MusicRun], xfade: f64) -> Vec<f64> {
    let mut out = Vec::with_capacity(runs.len());
    for (n, run) in runs.iter().enumerate() {
        let start = if n == 0 {
            run.start
        } else {
            let gap = run.start - runs[n - 1].end;
            if gap >= 0.0 && gap <= xfade.max(0.0) + 1.0 {
                runs[n - 1].end
            } else {
                run.start
            }
        };
        out.push(start);
    }
    out
}

// ---------------------------------------------------------------------------
// the pass
// ---------------------------------------------------------------------------

/// Mix the two sound-design layers under the voice track.
///
/// `slots` is the timeline the concat already wrote, so the layer offsets and
/// the voice's own gaps agree by construction. `chapter` seeds the pool picks,
/// which is what makes a re-merge reproduce the same audio.
///
/// `work` is a directory the caller owns, used for the per-span slices this
/// pass needs. It is created on demand and never cleaned up here, so pass a
/// throwaway path — the merge passes its per-chapter scratch directory.
#[allow(clippy::too_many_arguments)]
pub fn apply_layers(
    voice_wav: &Path,
    slots: &[Slot],
    chapter: u32,
    on: LayerSwitch,
    out: &Path,
    work: &Path,
    assets: &Path,
) -> Result<PathBuf> {
    let mut cfg = load_map(&assets.join("scene-map.json"))?;
    cfg.layers.effect.trim *= on.effect_volume.max(0.0);
    cfg.layers.music.level *= on.music_volume.max(0.0);
    cfg.layers.music.pause_level *= on.music_volume.max(0.0);
    let effect_pool = audio_pool::load_pool(&assets.join("effect-pool.json"));
    let music_pool = audio_pool::load_pool(&assets.join("music-pool.json"));
    let spans = build_spans(slots, &cfg);
    let pauses = pause_intervals(slots);

    // 1. the voice track, with per-scene reverb. Not a layer: it is applied to
    //    the voice itself, before anything is mixed under it. It rides with the
    //    effect switch because it is part of that layer's scene treatment — an
    //    operator turning the effects off is asking for a plain read, not a
    //    plain read in a cave.
    let work = work.join("layers");
    std::fs::create_dir_all(&work)?;
    let total = read_wav(voice_wav)?.seconds();
    let mut voice_fx = voice_wav.to_path_buf();
    if on.effects && spans.iter().any(|s| s.reverb.is_some()) {
        let mut parts = Vec::new();
        for (n, span) in spans.iter().enumerate() {
            let p = work.join(format!("v{n}.wav"));
            let mut args: Vec<String> = vec![
                "-y".into(),
                "-loglevel".into(),
                "error".into(),
                "-i".into(),
                s(voice_wav.display()),
                "-ss".into(),
                format!("{:.3}", span.start),
                "-to".into(),
                format!("{:.3}", span.end),
            ];
            if let Some(fx) = span.reverb.as_ref().and_then(|r| cfg.reverb_presets.get(r)) {
                args.push("-af".into());
                args.push(fx.clone());
            }
            args.push(s(p.display()));
            ffmpeg(&args)?;
            parts.push(p);
        }
        voice_fx = work.join("voice_fx.wav");
        concat_files(&parts, &voice_fx)?;
    }

    // 2. the effect layer: gated windows, sparse on purpose.
    let windows = if on.effects {
        plan_windows(&spans, &cfg.layers.effect, total)
    } else {
        Vec::new()
    };
    let mut missing: Vec<String> = Vec::new();
    let mut fx_slices: Vec<Slice> = Vec::new();
    let mut fx_log: Vec<FxReport> = Vec::new();
    for (n, w) in windows.iter().enumerate() {
        let seed = audio_pool::seed(chapter, n, &w.tags);
        let Some(clip) = audio_pool::pick(&effect_pool, &w.tags, seed) else {
            continue;
        };
        let src = clip_path(assets, &clip.file);
        if !src.is_file() {
            if !missing.contains(&clip.file) {
                missing.push(clip.file.clone());
            }
            continue;
        }
        let dur = w.end - w.start;
        let p = work.join(format!("fx{n}.wav"));
        let mut args: Vec<String> = vec!["-y".into(), "-loglevel".into(), "error".into()];
        if clip.looped {
            args.push("-stream_loop".into());
            args.push("-1".into());
        }
        args.push("-i".into());
        args.push(s(src.display()));
        args.push("-t".into());
        args.push(format!("{dur:.3}"));
        // A one-shot is faded at *its own* end, whatever that is: `areverse`
        // twice puts the fade on the tail of an unprobed clip. A loop just
        // takes the level; `place` fades its window edges.
        let af = if clip.looped {
            format!(
                "volume={:.4},aformat=sample_rates=48000:channel_layouts=mono",
                w.level
            )
        } else {
            format!(
                "volume={:.4},areverse,afade=t=in:st=0:d=0.4,areverse,\
                 aformat=sample_rates=48000:channel_layouts=mono",
                w.level
            )
        };
        args.push("-af".into());
        args.push(af);
        args.push(s(p.display()));
        ffmpeg(&args)?;
        fx_slices.push(Slice {
            path: p,
            start: w.start,
            dur,
            fade_in: cfg.layers.effect.fade_s,
            fade_out: if clip.looped {
                cfg.layers.effect.fade_s
            } else {
                0.0
            },
        });
        fx_log.push(FxReport {
            span: w.span,
            start: w.start,
            end: w.end,
            // The sound's name, not the take's: `day`, never `day-2`. The log
            // is read by a human asking which sound answered.
            name: clip.sound.clone(),
            level: w.level,
            one_shot: !clip.looped,
        });
    }
    for f in &missing {
        eprintln!("effect: pooled clip missing ({f}) -> no effect for its windows");
    }
    let effect_mix = if fx_slices.is_empty() {
        None
    } else {
        let p = work.join("effect.wav");
        place(&fx_slices, &p, total)?;
        Some(p)
    };

    // 3. the music layer: continuous, quiet, lifting inside a pause.
    let runs = if on.music {
        plan_music(slots, &pauses, chapter, &music_pool, &cfg.music_palette)
    } else {
        Vec::new()
    };
    let mut mu_slices: Vec<Slice> = Vec::new();
    let starts = music_starts(&runs, cfg.layers.music.xfade_s);
    for (n, run) in runs.iter().enumerate() {
        // The take was chosen once, in `plan_music`, and travels on the run:
        // re-picking here would be a second answer to a question already
        // answered, and the two could disagree.
        let src = clip_path(assets, &run.file);
        if !src.is_file() {
            eprintln!(
                "music: pooled clip missing ({}) -> that run is silent",
                run.file
            );
            continue;
        }
        let last = n + 1 == runs.len();
        // Every run but the last carries a tail long enough to overlap the next
        // one's fade-in, so a track change is a crossfade and not a hole.
        // `starts` bridges small inter-slot gaps so the two fades align: the
        // next track begins where the previous one's audible coverage ended.
        let start = starts[n];
        let tail = if last { 0.0 } else { cfg.layers.music.xfade_s };
        let dur = (run.end - run.start) + tail + (run.start - start);
        let p = work.join(format!("mu{n}.wav"));
        let expr = level_expr(
            cfg.layers.music.level,
            cfg.layers.music.pause_level,
            cfg.layers.music.ramp_s,
            start,
            &run.pauses,
        );
        ffmpeg(&[
            "-y".into(),
            "-loglevel".into(),
            "error".into(),
            "-stream_loop".into(),
            "-1".into(),
            "-i".into(),
            s(src.display()),
            "-t".into(),
            format!("{dur:.3}"),
            "-af".into(),
            format!(
                "volume=volume='{expr}':eval=frame,\
                 aformat=sample_rates=48000:channel_layouts=mono"
            ),
            s(p.display()),
        ])?;
        mu_slices.push(Slice {
            path: p,
            start,
            dur,
            fade_in: if n == 0 {
                cfg.layers.music.fade_s
            } else {
                cfg.layers.music.xfade_s
            },
            fade_out: if last {
                cfg.layers.music.fade_s
            } else {
                cfg.layers.music.xfade_s
            },
        });
    }
    let music_mix = if mu_slices.is_empty() {
        None
    } else {
        let p = work.join("music.wav");
        place(&mu_slices, &p, total)?;
        Some(p)
    };

    // 4. one duck for both layers, keyed on the voice.
    let layers: Vec<&PathBuf> = [effect_mix.as_ref(), music_mix.as_ref()]
        .into_iter()
        .flatten()
        .collect();
    if layers.is_empty() {
        eprintln!("sound design: no layer produced anything, skipped");
        log_plan(&spans, &pauses, &fx_log, &runs, &cfg);
        cleanup(&work);
        return Ok(voice_fx);
    }
    let duck = &cfg.duck;
    let sc = format!(
        "sidechaincompress=threshold={}:ratio={}:attack={}:release={}",
        duck.threshold, duck.ratio, duck.attack, duck.release
    );
    let graph = if layers.len() == 1 {
        format!("[1:a][0:a]{sc}[duck];[0:a][duck]amix=inputs=2:normalize=0[a]")
    } else {
        format!(
            "[1:a][2:a]amix=inputs=2:normalize=0[under];\
             [under][0:a]{sc}[duck];[0:a][duck]amix=inputs=2:normalize=0[a]"
        )
    };
    let mut args: Vec<String> = vec!["-y".into(), "-loglevel".into(), "error".into()];
    args.push("-i".into());
    args.push(s(voice_fx.display()));
    for l in &layers {
        args.push("-i".into());
        args.push(s(l.display()));
    }
    args.push("-filter_complex".into());
    args.push(graph);
    args.push("-map".into());
    args.push("[a]".into());
    args.push(s(out.display()));
    ffmpeg(&args)?;

    log_plan(&spans, &pauses, &fx_log, &runs, &cfg);
    cleanup(&work);
    Ok(out.to_path_buf())
}

/// Build the plan report. The mix is otherwise invisible in the logs, and a
/// chapter that came out silent should say *why* it came out silent.
///
/// Three lists, because the three things have three different clocks. A *span*
/// is a place and carries the reverb; a *window* is an effect and may open
/// later than its span (the cooldown can push it); a *run* is a mood, and
/// `none` emits no run at all, so silence shows up as a gap between two runs'
/// ranges rather than as a line.
///
/// This used to fold both the effect and the music into the span line, by
/// taking the first entry that matched on start offset. That hid every effect
/// the cooldown had pushed and every cue after the first in a span — i.e. it
/// hid precisely the two behaviours the per-window and per-slot designs exist
/// to express, in the only place a merged chapter is inspectable.
fn plan_lines(
    spans: &[Span],
    pauses: &[(f64, f64)],
    fx: &[FxReport],
    runs: &[MusicRun],
    cfg: &SceneMap,
) -> Vec<String> {
    let mut out = Vec::new();
    for (a, b) in pauses {
        out.push(format!(
            "pause [{a:.1}-{b:.1}s] {:.2}s — music lifts to {:.3}",
            b - a,
            cfg.layers.music.pause_level
        ));
    }
    for span in spans {
        out.push(format!(
            "span  [{:.0}-{:.0}s] {}{}",
            span.start,
            span.end,
            if span.scene.is_empty() {
                "?"
            } else {
                &span.scene
            },
            span.reverb
                .as_ref()
                .map(|r| format!(" | reverb: {r}"))
                .unwrap_or_default()
        ));
    }
    for f in fx {
        // Attribute by index, not by offset: the window may have been pushed
        // past its span's start, and it is still that place's sound.
        let place = spans.get(f.span).map(|s| s.scene.as_str()).unwrap_or("?");
        out.push(format!(
            "effect [{:.0}-{:.0}s] {}@{:.2}{} <- {}",
            f.start,
            f.end,
            f.name,
            f.level,
            if f.one_shot { " (one-shot)" } else { "" },
            if place.is_empty() { "?" } else { place }
        ));
    }
    if runs.is_empty() {
        out.push("music none — no cue resolved for this chapter".into());
    }
    for r in runs {
        out.push(format!(
            "music [{:.0}-{:.0}s] {} <- {}@{:.3}",
            r.start, r.end, r.sound, r.mood, cfg.layers.music.level
        ));
    }
    out
}

fn log_plan(
    spans: &[Span],
    pauses: &[(f64, f64)],
    fx: &[FxReport],
    runs: &[MusicRun],
    cfg: &SceneMap,
) {
    for line in plan_lines(spans, pauses, fx, runs, cfg) {
        eprintln!("{line}");
    }
}

fn cleanup(work: &Path) {
    if let Ok(entries) = std::fs::read_dir(work) {
        for e in entries.flatten() {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assemble::silent_wav;
    use serde_json::json;

    fn scene_map() -> SceneMap {
        serde_json::from_str(
            r#"{
              "rules": [
                {"match": ["storm","thunder"], "effect": ["rain","storm"], "level": 0.22, "reverb": null},
                {"match": ["rain"], "effect": ["rain"], "level": 0.18, "reverb": null},
                {"match": ["night","evening"], "effect": ["night"], "level": 0.15, "reverb": null},
                {"match": ["market","street"], "effect": ["market"], "level": 0.16, "reverb": null, "pause_before_s": 1.5},
                {"match": ["cave"], "effect": ["cave"], "level": 0.18, "reverb": "cave"},
                {"match": ["hall","sect"], "effect": [], "level": 0.0, "reverb": "hall", "pause_before_s": 1.5},
                {"match": ["courtyard","dining"], "effect": ["fire"], "level": 0.08, "reverb": null},
                {"match": ["day","morning"], "effect": ["day"], "level": 0.12, "reverb": null}
              ],
              "default": {"effect": [], "level": 0.0, "reverb": null},
              "music_palette": {
                "_note": "skipped by the loader",
                "quiet": {"tags": ["soft","calm"], "note": "low and unobtrusive"},
                "busy": {"tags": ["market","busy"], "note": "crowds"},
                "warm": {"tags": ["warm"], "note": "hearth"},
                "none": {"tags": [], "note": "silence"}
              },
              "legacy_scene_music": {
                "rules": [
                  {"match": ["market","street"], "music": "busy"},
                  {"match": ["night","cave"], "music": "quiet"}
                ],
                "default": {"music": "none"}
              },
              "layers": {
                "effect": {"max_coverage": 0.35, "cooldown_s": 45.0, "min_span_s": 20.0, "max_window_s": 75.0, "fade_s": 0.3},
                "music": {"level": 0.06, "pause_level": 0.085, "fade_s": 0.3, "xfade_s": 2.0, "ramp_s": 0.6}
              },
              "pause": {"pause_s": 1.5, "max_per_chapter": 1, "require_narration": true},
              "reverb_presets": {"hall": "aecho=0.8:0.65:40|60:0.35|0.25"},
              "duck": {"threshold": 0.02, "ratio": 6.0, "attack": 20, "release": 400}
            }"#,
        )
        .unwrap()
    }

    /// The map that actually ships. The chapter-1 regression this file exists
    /// for lived in *this* file, not in the code, so a fixture-only test would
    /// have passed while the shipped map stayed wrong.
    fn shipped_map() -> SceneMap {
        load_map(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../assets/scene-map.json"))
            .expect("assets/scene-map.json must load")
    }

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bm-layers-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn turn(wav: &Path, scene: &str, speaker: &str) -> Turn {
        Turn {
            wav: wav.to_path_buf(),
            scene: scene.into(),
            music: String::new(),
            speaker: speaker.into(),
        }
    }

    fn turn_m(wav: &Path, scene: &str, music: &str, speaker: &str) -> Turn {
        Turn {
            wav: wav.to_path_buf(),
            scene: scene.into(),
            music: music.into(),
            speaker: speaker.into(),
        }
    }

    fn slot(music: &str, start: f64, end: f64) -> Slot {
        Slot {
            wav: PathBuf::from("x.wav"),
            scene: "s".into(),
            music: music.into(),
            speaker: "A".into(),
            start,
            end,
            gap_ms: 0,
            pause_ms: 0,
        }
    }

    fn span(start: f64, end: f64, effect: &[&str], level: f64) -> Span {
        Span {
            effect: effect.iter().map(|s| s.to_string()).collect(),
            level,
            reverb: None,
            scene: "s".into(),
            start,
            end,
        }
    }

    #[test]
    fn first_matching_rule_wins_specific_before_general() {
        let cfg = scene_map();
        assert_eq!(
            match_scene("street-day-book-discovery", &cfg).effect,
            vec!["market"]
        );
        assert_eq!(match_scene("courtyard-rain-day", &cfg).effect, vec!["rain"]);
        assert_eq!(
            match_scene("great-hall-day", &cfg).reverb.as_deref(),
            Some("hall")
        );
        assert!(match_scene("something-unknown-xyz", &cfg).effect.is_empty());
    }

    #[test]
    fn run_scenes_takes_the_majority_tag() {
        let segments = vec![
            json!({"scene": "market-morning"}),
            json!({"scene": "market-morning"}),
            json!({"scene": "courtyard-evening"}),
        ];
        let runs = crate::assemble::runs(&[
            json!({"speaker": "A"}),
            json!({"speaker": "A"}),
            json!({"speaker": "A"}),
        ]);
        assert_eq!(run_scenes(&segments, &runs), vec!["market-morning"]);
    }

    #[test]
    fn run_scenes_ignores_blank_tags() {
        let segments = vec![json!({"scene": ""}), json!({"scene": "  "})];
        let runs = crate::assemble::runs(&[json!({"speaker": "A"}), json!({"speaker": "A"})]);
        assert_eq!(run_scenes(&segments, &runs), vec![""]);
    }

    #[test]
    fn the_timeline_places_pauses_and_keeps_the_gap() {
        let d = tmpdir("timeline");
        let a = d.join("a.wav");
        let b = d.join("b.wav");
        silent_wav(&a, 1.0, 48_000).unwrap();
        silent_wav(&b, 1.0, 48_000).unwrap();
        let turns = vec![
            turn(&a, "street-day", "Narrator"),
            turn_m(&b, "cave-x", "battle", "A"),
        ];

        let plain = timeline(&turns, 300, &BTreeMap::new()).unwrap();
        assert_eq!(plain.len(), 2);
        assert!((plain[1].start - 1.3).abs() < 0.01, "{plain:?}");
        assert!(pause_intervals(&plain).is_empty());
        assert_eq!(
            plain[1].music, "battle",
            "the mood rides through to the mix"
        );

        // A pause before turn 1 pushes it and is visible as an interval.
        let mut pauses = BTreeMap::new();
        pauses.insert(1usize, 1875u32);
        let paused = timeline(&turns, 300, &pauses).unwrap();
        assert!((paused[1].start - 3.175).abs() < 0.01, "{paused:?}");
        assert_eq!(paused[0].gap_ms, 300 + 1875);
        assert_eq!(paused[0].pause_ms, 1875);
        let iv = pause_intervals(&paused);
        assert_eq!(iv.len(), 1);
        assert!(
            (iv[0].0 - 1.0).abs() < 0.01 && (iv[0].1 - 2.875).abs() < 0.01,
            "{iv:?}"
        );
    }

    /// The merge tempoes the speech and then places the layers, so the timeline
    /// the layers read has to be the one the listener hears. Without this the
    /// music drifts behind the voice by a line's worth per line — the whole
    /// reason the tempo pass moved ahead of `apply_layers`.
    #[test]
    fn retime_puts_the_timeline_on_the_delivered_clock() {
        let d = tmpdir("retime");
        let a = d.join("a.wav");
        silent_wav(&a, 4.0, 48_000).unwrap();
        let turns = vec![turn(&a, "s", "Narrator"), turn(&a, "s", "Narrator")];
        // A beat authored *before* turn 1 is written into the gap that follows
        // turn 0 — the same silence, named from the other side.
        let mut pauses = BTreeMap::new();
        pauses.insert(1usize, 1875u32);
        let mut slots = timeline(&turns, 300, &pauses).unwrap();
        assert!((slots[1].start - 6.175).abs() < 0.01, "{slots:?}");

        retime(&mut slots, 1.25);

        assert!(
            (slots[0].end - 3.2).abs() < 0.01,
            "4 s of speech is 3.2 s delivered: {slots:?}"
        );
        assert!(
            (slots[1].start - 4.94).abs() < 0.01,
            "and the gap scaled with it: {slots:?}"
        );
        assert_eq!(slots[0].pause_ms, 1500, "the beat is 1.5 s to the listener");
        assert_eq!(slots[0].gap_ms, 1740, "uniform gap + beat, both scaled");

        // 1.0 is a no-op rather than a divide that rounds.
        let before = slots.clone();
        retime(&mut slots, 1.0);
        assert_eq!(slots, before);
    }

    #[test]
    fn the_timeline_refuses_mixed_rates_before_anything_is_mixed() {
        let d = tmpdir("timeline-rates");
        let a = d.join("a.wav");
        let b = d.join("b.wav");
        silent_wav(&a, 0.2, 24_000).unwrap();
        silent_wav(&b, 0.2, 48_000).unwrap();
        let turns = vec![turn(&a, "x", "A"), turn(&b, "x", "A")];
        let err = timeline(&turns, 0, &BTreeMap::new()).unwrap_err();
        assert!(err.to_string().contains("mixed engines"), "{err}");
    }

    #[test]
    fn spans_merge_adjacent_identical_scenes() {
        let d = tmpdir("spans");
        let a = d.join("a.wav");
        let b = d.join("b.wav");
        silent_wav(&a, 1.0, 48_000).unwrap();
        silent_wav(&b, 1.0, 48_000).unwrap();
        let cfg = scene_map();

        let turns = vec![turn(&a, "street-day", "A"), turn(&b, "night-x", "A")];
        let spans = build_spans(&timeline(&turns, 300, &BTreeMap::new()).unwrap(), &cfg);
        assert_eq!(spans.len(), 2);

        let turns = vec![turn(&a, "street-day", "A"), turn(&b, "street-day", "A")];
        let merged = build_spans(&timeline(&turns, 0, &BTreeMap::new()).unwrap(), &cfg);
        assert_eq!(merged.len(), 1);
        assert!((merged[0].end - 2.0).abs() < 0.01);
    }

    /// The complaint the effect gates exist for: a bed under the whole chapter.
    #[test]
    fn the_effect_layer_is_sparse_where_the_old_one_was_wall_to_wall() {
        let cfg = scene_map();
        let spans = vec![
            span(0.0, 400.0, &["market"], 0.16),
            span(400.0, 800.0, &["night"], 0.15),
        ];
        let w = plan_windows(&spans, &cfg.layers.effect, 800.0);
        assert_eq!(w.len(), 2, "{w:?}");
        let covered: f64 = w.iter().map(|x| x.end - x.start).sum();
        assert!(
            (covered - 150.0).abs() < 0.01,
            "two 75 s windows, not 800 s: {w:?}"
        );
        // Window 1 waits out the cooldown: it opens no earlier than 75 + 45.
        assert!(w[1].start >= 120.0, "{w:?}");
        assert!(covered / 800.0 <= cfg.layers.effect.max_coverage);
    }

    /// The layer's one master gain. A rule's `level` is a relative balance
    /// between scenes; `trim` is the operator saying the whole layer is too hot,
    /// and it must reach every window without anyone editing thirteen rules.
    #[test]
    fn the_layer_trim_scales_every_rule_and_defaults_to_no_change() {
        let spans = vec![span(0.0, 100.0, &["rain"], 0.20)];

        // A map written before the field existed keeps its exact mix.
        let untouched = scene_map();
        let w = plan_windows(&spans, &untouched.layers.effect, 200.0);
        assert_eq!(w.len(), 1);
        assert!(
            (w[0].level - 0.20).abs() < 1e-9,
            "default trim is 1.0, got {}",
            w[0].level
        );

        let mut quieter = scene_map();
        quieter.layers.effect.trim = 0.5;
        let w = plan_windows(&spans, &quieter.layers.effect, 200.0);
        assert_eq!(w.len(), 1);
        assert!(
            (w[0].level - 0.10).abs() < 1e-9,
            "half the layer is half every rule: {}",
            w[0].level
        );

        // The trim must not resurrect a rule that declared silence — the hall
        // rule's `level: 0.0` means "no bed here", not "quiet bed".
        let silent = vec![span(0.0, 100.0, &["night"], 0.0)];
        assert!(plan_windows(&silent, &quieter.layers.effect, 200.0).is_empty());
    }

    #[test]
    fn a_short_scene_and_a_spent_budget_both_get_nothing() {
        let cfg = scene_map();
        // 10 s is under min_span_s: not worth a window.
        assert!(plan_windows(
            &[span(0.0, 10.0, &["rain"], 0.18)],
            &cfg.layers.effect,
            100.0
        )
        .is_empty());

        // Budget spent by the first window stops the rest: 100 s of chapter
        // buys 35 s, which the first eligible scene takes.
        let w = plan_windows(
            &[
                span(0.0, 100.0, &["rain"], 0.18),
                span(200.0, 300.0, &["rain"], 0.18),
            ],
            &cfg.layers.effect,
            100.0,
        );
        assert_eq!(w.len(), 1, "{w:?}");
        assert!((w[0].end - w[0].start - 35.0).abs() < 0.01, "{w:?}");
    }

    #[test]
    fn a_beat_lands_on_narrated_scene_changes_only() {
        let cfg = scene_map();
        let d = tmpdir("pauses");
        let w = d.join("a.wav");
        silent_wav(&w, 1.0, 48_000).unwrap();

        // Narration handing off to a character, then narration resuming: the
        // resuming boundary wins, even though both are narrated.
        let turns = vec![
            turn(&w, "street-day", "Narrator"),
            turn(&w, "night-x", "Dịch Phong"),
            turn(&w, "cave-y", "Narrator"),
        ];
        let got = plan_pauses(&turns, &cfg, 1.25);
        assert_eq!(got.len(), 1);
        assert_eq!(got.keys().next(), Some(&2usize), "narration resuming wins");
        assert_eq!(got[&2], 1875, "1.5 s delivered at atempo 1.25");

        // An exchange that crosses a scene boundary gets no beat.
        let turns = vec![turn(&w, "street-day", "A"), turn(&w, "night-x", "B")];
        assert!(plan_pauses(&turns, &cfg, 1.25).is_empty());

        // No scene change, no beat.
        let turns = vec![
            turn(&w, "street-day", "Narrator"),
            turn(&w, "street-day", "A"),
        ];
        assert!(plan_pauses(&turns, &cfg, 1.25).is_empty());

        // The opening scene is not a scene *change*.
        let turns = vec![turn(&w, "", "Narrator"), turn(&w, "street-day", "Narrator")];
        assert!(plan_pauses(&turns, &cfg, 1.25).is_empty());
    }

    #[test]
    fn only_one_beat_per_chapter_however_many_boundaries_there_are() {
        let cfg = scene_map();
        let d = tmpdir("pauses-one");
        let w = d.join("a.wav");
        silent_wav(&w, 1.0, 48_000).unwrap();
        let turns = vec![
            turn(&w, "street-day", "Narrator"),
            turn(&w, "night-x", "Narrator"),
            turn(&w, "cave-y", "Narrator"),
            turn(&w, "rain-z", "Narrator"),
        ];
        assert_eq!(plan_pauses(&turns, &cfg, 1.0).len(), 1);
    }

    /// Sound-keyed, like the shipped registry: `soft` is one sound with two
    /// takes, and `soft-alt` is a *second* sound answering the same tags — the
    /// shape the real pool has (`soft-relax` and `generic-soft` both answer
    /// `[soft, calm]`), and the only way a mood change can resolve to a
    /// different track.
    fn music_pool() -> ClipPool {
        let mut p = ClipPool::new();
        for (name, tags, files) in [
            (
                "market",
                &["market", "busy"][..],
                &["music/market-bg-1.mp3"][..],
            ),
            (
                "soft",
                &["soft", "calm"][..],
                &["music/soft-bg-1.mp3", "music/soft-bg-2.mp3"][..],
            ),
            (
                "soft-alt",
                &["soft", "calm"][..],
                &["music/soft-alt-bg-1.mp3"][..],
            ),
        ] {
            p.insert(
                name.into(),
                audio_pool::Sound {
                    tags: tags.iter().map(|s| s.to_string()).collect(),
                    files: files.iter().map(|s| s.to_string()).collect(),
                    looped: true,
                },
            );
        }
        p
    }

    #[test]
    fn music_runs_break_on_a_sound_change_not_a_scene_change() {
        let cfg = scene_map();
        let pool = music_pool();
        let pal = &cfg.music_palette;

        // One mood across two slots: one cue, so no crossfade into itself.
        let runs = plan_music(
            &[slot("quiet", 0.0, 100.0), slot("quiet", 100.0, 200.0)],
            &[],
            1,
            &pool,
            pal,
        );
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert!((runs[0].end - 200.0).abs() < 0.01);
        assert_eq!(runs[0].mood, "quiet");

        // A change of mood is a change of sound — the whole point of the field.
        let runs = plan_music(
            &[slot("quiet", 0.0, 100.0), slot("busy", 100.0, 200.0)],
            &[],
            1,
            &pool,
            pal,
        );
        assert_eq!(runs.len(), 2, "{runs:?}");
        assert_ne!(runs[0].sound, runs[1].sound);
        assert_eq!(runs[1].mood, "busy");
    }

    #[test]
    fn a_mood_change_mid_scene_changes_the_track() {
        // The case the old scene-keyed system could not express: one place, one
        // span, two moods. Nothing about the scene changed, so nothing but the
        // `music` field can carry this.
        let cfg = scene_map();
        let pool = music_pool();
        let slots = [
            slot("quiet", 0.0, 60.0),
            slot("quiet", 60.0, 120.0),
            slot("busy", 120.0, 180.0),
            slot("quiet", 180.0, 240.0),
        ];
        let spans = build_spans(&slots, &cfg);
        assert_eq!(spans.len(), 1, "one place, so one span: {spans:?}");

        let runs = plan_music(&slots, &[], 1, &pool, &cfg.music_palette);
        assert_eq!(runs.len(), 3, "quiet/busy/quiet: {runs:?}");
        assert_eq!(
            runs.iter().map(|r| r.mood.as_str()).collect::<Vec<_>>(),
            vec!["quiet", "busy", "quiet"]
        );
    }

    #[test]
    fn the_log_shows_every_cue_in_a_span_not_just_the_first() {
        // Regression: the report used to fold music into the span line by
        // taking the first run that overlapped, so a span holding three cues
        // printed one. Since spans are places and runs are moods, that hid
        // exactly the in-chapter change the design exists to express — and the
        // log is the only place a merged chapter is inspectable.
        let cfg = scene_map();
        let pool = music_pool();
        let slots = [
            slot("quiet", 0.0, 60.0),
            slot("busy", 60.0, 120.0),
            slot("quiet", 120.0, 180.0),
        ];
        let spans = build_spans(&slots, &cfg);
        assert_eq!(spans.len(), 1, "one place, so one span: {spans:?}");
        let runs = plan_music(&slots, &[], 1, &pool, &cfg.music_palette);

        let lines = plan_lines(&spans, &[], &[], &runs, &cfg);
        let music: Vec<&String> = lines.iter().filter(|l| l.starts_with("music ")).collect();
        assert_eq!(music.len(), 3, "{lines:#?}");
        assert!(music[0].contains("quiet"), "{music:#?}");
        assert!(music[1].contains("busy"), "{music:#?}");
        assert!(music[2].contains("quiet"), "{music:#?}");
        // ...and the span line no longer claims a track, because a span does
        // not have one.
        let span = lines.iter().find(|l| l.starts_with("span ")).unwrap();
        assert!(!span.contains("music"), "{span}");
    }

    #[test]
    fn the_log_says_so_when_a_chapter_resolves_to_no_music() {
        // `none` emits no run, so the report has to say that explicitly rather
        // than print an empty section the reader has to interpret.
        let cfg = scene_map();
        let lines = plan_lines(&[], &[], &[], &[], &cfg);
        assert_eq!(lines, vec!["music none — no cue resolved for this chapter"]);
    }

    #[test]
    fn the_log_attributes_a_window_the_cooldown_pushed_to_its_own_place() {
        // `plan_windows` opens at `span.start.max(free_at)`, so the second
        // window starts *after* its span does. Matching windows to spans by
        // start offset — which the report did, through a formatted string —
        // then reported "no effect" for a span that had 75 s of one. Chapter 13
        // was measured that way: `courtyard-evening` looked silent while
        // carrying a night bed from 120 s to 195 s.
        let cfg = scene_map();
        let spans = vec![
            Span {
                effect: vec!["day".into(), "calm".into()],
                level: 0.12,
                reverb: None,
                scene: "courtyard-morning".into(),
                start: 0.0,
                end: 108.0,
            },
            Span {
                effect: vec!["night".into()],
                level: 0.15,
                reverb: None,
                scene: "courtyard-evening".into(),
                start: 110.0,
                end: 316.0,
            },
        ];
        let fx = vec![
            FxReport {
                span: 0,
                start: 0.0,
                end: 75.0,
                name: "day-2".into(),
                level: 0.12,
                one_shot: false,
            },
            FxReport {
                span: 1,
                start: 120.0,
                end: 195.0,
                name: "night-2".into(),
                level: 0.15,
                one_shot: false,
            },
        ];
        let lines = plan_lines(&spans, &[], &fx, &[], &cfg);
        let effects: Vec<&String> = lines.iter().filter(|l| l.starts_with("effect ")).collect();
        assert_eq!(effects.len(), 2, "{lines:#?}");
        assert!(effects[1].starts_with("effect [120-195s]"), "{effects:#?}");
        assert!(
            effects[1].ends_with("<- courtyard-evening"),
            "a pushed window still belongs to its own place: {effects:#?}"
        );
        // And the span line carries no effect of its own — a span does not have
        // one, so it must not imply it does.
        let span_line = lines.iter().find(|l| l.starts_with("span ")).unwrap();
        assert!(!span_line.contains("effect"), "{span_line}");
    }

    #[test]
    fn two_moods_that_name_the_same_tags_are_one_run_when_one_sound_answers() {
        // The old premise here — "a mood change always changes the track" — was
        // false the moment picks became sound-based, and it is not a bug. The
        // seed decides *among the candidates*; with one candidate there is
        // nothing to decide, so both moods resolve to the same sound and the
        // run merges. Rendering two runs of the same take with a crossfade
        // between them would be a hole with extra steps.
        let mut cfg = scene_map();
        cfg.music_palette.insert(
            "warm".into(),
            PaletteEntry {
                tags: vec!["soft".into(), "calm".into()],
                note: String::new(),
            },
        );
        let mut pool = ClipPool::new();
        pool.insert(
            "soft".into(),
            audio_pool::Sound {
                tags: vec!["soft".into(), "calm".into()],
                files: vec!["music/soft-bg-1.mp3".into()],
                looped: true,
            },
        );
        let runs = plan_music(
            &[slot("quiet", 0.0, 100.0), slot("warm", 100.0, 200.0)],
            &[],
            1,
            &pool,
            &cfg.music_palette,
        );
        assert_eq!(runs.len(), 1, "one sound, so one run: {runs:?}");
        assert_eq!(
            runs[0].mood, "quiet",
            "the run reports the mood that opened it"
        );
        assert!((runs[0].end - 200.0).abs() < 0.01, "and it is unbroken");
    }

    #[test]
    fn two_moods_that_name_the_same_tags_can_still_split() {
        // What seeding from the palette *value* actually buys. Two values naming
        // one tag set are two independent draws from the candidate set, so a
        // pool with two sounds for those tags spreads them across moods instead
        // of crossfading one into itself. The pool has to offer the choice —
        // this is not a guarantee plan_music can make on its own.
        let mut cfg = scene_map();
        cfg.music_palette.insert(
            "warm".into(),
            PaletteEntry {
                tags: vec!["soft".into(), "calm".into()],
                note: String::new(),
            },
        );
        // The mechanism, stated where it is visible: the seed is a function of
        // the mood value, so the two moods are not forced onto one answer.
        assert_ne!(
            audio_pool::seed(1, 0, &["quiet".to_string()]),
            audio_pool::seed(1, 0, &["warm".to_string()])
        );

        // ...and the consequence: `soft` and `soft-alt` both answer [soft, calm],
        // so some chapter splits the two moods across them.
        let pool = music_pool();
        let split = (1..=32).any(|c| {
            let runs = plan_music(
                &[slot("quiet", 0.0, 100.0), slot("warm", 100.0, 200.0)],
                &[],
                c,
                &pool,
                &cfg.music_palette,
            );
            runs.len() == 2 && runs[0].sound != runs[1].sound
        });
        assert!(
            split,
            "no chapter in 32 split two moods that share tags across the two \
             pooled sounds — the value is not reaching the seed"
        );
    }

    #[test]
    fn none_an_unpooled_mood_and_an_empty_value_all_mean_no_music() {
        let cfg = scene_map();
        let pool = music_pool();
        let pal = &cfg.music_palette;
        // `none` is a choice.
        assert!(plan_music(&[slot("none", 0.0, 100.0)], &[], 1, &pool, pal).is_empty());
        // No value at all.
        assert!(plan_music(&[slot("", 0.0, 100.0)], &[], 1, &pool, pal).is_empty());
        // A palette value whose tags nothing in the pool answers.
        assert!(plan_music(&[slot("grand", 0.0, 100.0)], &[], 1, &pool, pal).is_empty());
        // A value that is not in the palette at all — the validator's job to
        // catch, and the mix still refuses to guess.
        assert!(plan_music(&[slot("melancholy", 0.0, 100.0)], &[], 1, &pool, pal).is_empty());
    }

    #[test]
    fn a_pause_inside_a_run_is_carried_to_the_lift() {
        let cfg = scene_map();
        let pool = music_pool();
        let runs = plan_music(
            &[slot("quiet", 0.0, 300.0)],
            &[(120.0, 121.5)],
            1,
            &pool,
            &cfg.music_palette,
        );
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].pauses, vec![(120.0, 121.5)]);
    }

    #[test]
    fn the_declared_mood_wins_and_the_legacy_shim_covers_scripts_without_one() {
        let cfg = scene_map();
        let build = |scenes: &[(&str, &str)]| -> Vec<Value> {
            scenes
                .iter()
                .map(|(s, m)| json!({"speaker": "A", "scene": s, "music": m}))
                .collect()
        };
        let runs_of = |n: usize| crate::assemble::runs(&vec![json!({"speaker": "A"}); n]);

        // Declared values are used as-is.
        let segs = build(&[("street-morning", "battle"), ("street-morning", "battle")]);
        assert_eq!(run_music(&segs, &runs_of(2), &cfg), vec!["battle"]);

        // No value anywhere: the shim scores the scene label instead, so the
        // ~200 chapters already on disk keep merging.
        let segs = build(&[("street-morning", ""), ("street-morning", "")]);
        assert_eq!(run_music(&segs, &runs_of(2), &cfg), vec!["busy"]);
        let segs = build(&[("night-forest", ""), ("night-forest", "")]);
        assert_eq!(run_music(&segs, &runs_of(2), &cfg), vec!["quiet"]);
        // Nothing in the shim matches: silence, exactly as the old `default`
        // (no music tags) did.
        let segs = build(&[("somewhere-else", ""), ("somewhere-else", "")]);
        assert_eq!(run_music(&segs, &runs_of(2), &cfg), vec!["none"]);
    }

    #[test]
    fn the_palette_is_read_once_and_rendered_for_the_prompt() {
        let cfg = scene_map();
        // The `_note` key documents the section in place; it is not a value the
        // analyzer could be asked to emit.
        assert_eq!(
            palette_names(&cfg),
            vec!["busy", "none", "quiet", "warm"],
            "sorted keys, no _note"
        );
        let rendered = palette_prompt(&cfg);
        assert!(
            rendered.contains("quiet (soft, calm; low and unobtrusive)"),
            "{rendered}"
        );
        assert!(rendered.contains("none (silence)"), "{rendered}");
        assert!(!rendered.contains("_note"), "{rendered}");
    }

    #[test]
    fn the_effect_vocabulary_is_the_sorted_union_of_pool_tags() {
        let pool: ClipPool = serde_json::from_value(json!({
            "night": {"tags": ["night", "calm"], "files": ["effects/night-1.mp3"]},
            "rain": {"tags": ["rain", "calm"], "files": ["effects/rain-1.mp3"]},
            "empty": {"tags": ["ghost"], "files": []},
        }))
        .unwrap();
        // Sorted, deduped, and drawn from sounds — even one with no files,
        // because the vocabulary describes the pool, not one pick.
        assert_eq!(effect_tags(&pool), vec!["calm", "ghost", "night", "rain"]);
    }

    /// The defect that started this: chapter 1 opened with a hearth crackling
    /// under a martial-arts shop at dawn. The label `martial-shop-morning`
    /// matched the generic `shop` keyword of a catch-all fire rule, which sat
    /// *before* the daylight rule — so `morning` never got a say. Against the
    /// map that actually ships, not a fixture.
    #[test]
    fn the_shipped_map_no_longer_puts_a_hearth_under_a_shop_at_dawn() {
        let cfg = shipped_map();
        assert_eq!(
            match_scene("martial-shop-morning", &cfg).effect,
            vec!["day", "calm"],
            "a shop at dawn is calm daylight, not a hearth"
        );
        // A genuine hearth still gets fire, from the forge rule.
        assert_eq!(match_scene("forge", &cfg).effect, vec!["fire"]);
        assert_eq!(match_scene("kitchen", &cfg).effect, vec!["fire"]);
        // ...but a courtyard does not. `courtyard` used to sit in a fire rule,
        // which put a hearth under `courtyard-battle-moment` and
        // `courtyard-confrontation` — 20 labels and ~800 segments in the
        // corpus, the same absurdity as the shop at dawn, just louder. A
        // courtyard is not a place with a fire in it: the battle rule owns the
        // ones that say battle, the daylight rule owns the ones that name a
        // time, and a bare `courtyard` is silent like any other unlisted place.
        assert_eq!(
            match_scene("courtyard-battle-moment", &cfg).effect,
            vec!["battle", "sword"]
        );
        assert_eq!(
            match_scene("courtyard-morning", &cfg).effect,
            vec!["day", "calm"]
        );
        assert!(match_scene("courtyard", &cfg).effect.is_empty());
        // Chapter 1's other two labels: the shopfront rule owns them.
        assert_eq!(
            match_scene("shopfront-neighbor-chat", &cfg).effect,
            vec!["market"]
        );
        assert_eq!(
            match_scene("shopfront-sisters-encounter", &cfg).effect,
            vec!["market"]
        );
        // Ordering is a decision, not a detail: an earlier rule's keyword beats
        // a later rule's, so a label carrying a time of day is scored by the
        // time and not by the place. Pinned here because *that* mechanism is
        // what mis-scored chapter 1, and it still cuts both ways.
        assert_eq!(match_scene("forge-night", &cfg).effect, vec!["night"]);
        assert_eq!(match_scene("courtyard-dusk", &cfg).effect, vec!["night"]);
    }

    /// The shipped palette has to be answerable by the shipped pool, or a mood
    /// the prompt offers would silently mean silence.
    #[test]
    fn every_shipped_palette_value_but_none_has_a_pooled_track() {
        let cfg = shipped_map();
        let pool = audio_pool::load_pool(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../assets/music-pool.json"),
        );
        assert!(!pool.is_empty(), "the music pool must load");
        for (name, entry) in &cfg.music_palette {
            if name == "none" {
                assert!(entry.tags.is_empty(), "`none` means no tags");
                continue;
            }
            assert!(
                audio_pool::pick(
                    &pool,
                    &entry.tags,
                    audio_pool::seed(1, 0, std::slice::from_ref(name))
                )
                .is_some(),
                "palette value {name:?} names tags [{}] that no pooled clip answers",
                entry.tags.join(", ")
            );
        }
    }

    /// Every shipped rule must actually resolve, or the rule is decoration.
    ///
    /// A tie between interchangeable variants is the pool's designed behaviour —
    /// `night-1..4` are four nights, and the seed spreads them across chapters.
    /// A tie between *different sounds* is not: `["day"]` alone sat on six clips
    /// spanning birdsong, a calm bed and a market crowd, one of them a one-shot
    /// stinger, so the daylight rule drew its bed at random and chapter 1 got a
    /// crowd under a shop at dawn.
    #[test]
    fn every_shipped_effect_rule_resolves_and_daylight_is_a_bed() {
        let cfg = shipped_map();
        let pool = audio_pool::load_pool(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../assets/effect-pool.json"),
        );
        assert!(!pool.is_empty(), "the effect pool must load");
        for rule in &cfg.rules {
            if rule.effect.is_empty() {
                continue;
            }
            assert!(
                audio_pool::pick(&pool, &rule.effect, audio_pool::seed(1, 0, &rule.effect))
                    .is_some(),
                "rule {:?} names tags [{}] that no pooled clip answers",
                rule.matches,
                rule.effect.join(", ")
            );
        }

        // The rule this change was about, pinned by name. A 75 s window needs a
        // looped bed, and the *sound* is the decision — the take (`day-1/2/3`)
        // is the pool's business and must not appear here.
        let day = match_scene("martial-shop-morning", &cfg);
        assert_eq!(day.effect, vec!["day", "calm"], "the daylight rule owns it");
        let got =
            audio_pool::pick(&pool, &day.effect, audio_pool::seed(1, 0, &day.effect)).unwrap();
        assert_eq!(got.sound, "day", "the family, never `day-2`");
        assert!(
            got.looped,
            "daylight must be a bed, got {} (one-shot)",
            got.sound
        );
        assert!(
            got.file.starts_with("effects/day-"),
            "and the take comes from the family: {}",
            got.file
        );
    }

    /// Every file a shipped pool names must exist on disk.
    ///
    /// The merge resolves a pool `file` through `clip_path` and, when it is
    /// missing, prints a warning and skips the run — so a registry pointing at a
    /// renamed or deleted clip is not an error, it is *silence*. Nothing else in
    /// the suite reads `assets/`, and the rename above (`rain-light.mp3` ->
    /// `rain-2.mp3`) touched five files by hand in an untracked directory. Had
    /// one landed on the registry side only, the whole suite would still have
    /// been green and the only symptom would have been a chapter with no rain in
    /// it. This is the test that would have caught that.
    #[test]
    fn every_shipped_pool_file_exists() {
        let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../assets");
        for reg in ["effect-pool.json", "music-pool.json"] {
            let pool = audio_pool::load_pool(&assets.join(reg));
            assert!(!pool.is_empty(), "{reg} must load");
            for (sound, entry) in &pool {
                assert!(
                    !entry.files.is_empty(),
                    "{reg}: sound {sound:?} has no takes, so it can never be picked"
                );
                for file in &entry.files {
                    let p = clip_path(&assets, file);
                    assert!(
                        p.is_file(),
                        "{reg}: sound {sound:?} names {file:?}, which is not on disk ({})",
                        p.display()
                    );
                }
            }
        }
    }

    #[test]
    fn the_level_expression_is_flat_without_a_pause_and_lifts_inside_one() {
        let flat = level_expr(0.06, 0.085, 0.6, 100.0, &[]);
        assert_eq!(flat, "0.060000");

        let lifted = level_expr(0.06, 0.085, 0.6, 100.0, &[(120.0, 121.5)]);
        // Offsets are relative to the run, and there are exactly two ramps.
        assert!(lifted.contains("clip((t-20.000)/0.600,0,1)"), "{lifted}");
        assert!(lifted.contains("clip((t-21.500)/0.600,0,1)"), "{lifted}");
        assert_eq!(lifted.matches("clip").count(), 2, "{lifted}");

        // No lift to make: the expression stays flat rather than emitting a
        // pair of cancelling ramps.
        assert_eq!(level_expr(0.06, 0.06, 0.6, 0.0, &[(1.0, 2.0)]), "0.060000");
    }

    #[test]
    fn adjacent_tracks_share_one_crossfade_and_silence_keeps_its_hole() {
        let run = |start: f64, end: f64| MusicRun {
            mood: "m".into(),
            sound: "s".into(),
            file: "f".into(),
            start,
            end,
            pauses: vec![],
        };
        let runs = vec![run(0.0, 10.0), run(10.3, 20.0)];
        assert_eq!(music_starts(&runs, 2.0), vec![0.0, 10.0]);
        let gapped = vec![run(0.0, 10.0), run(30.0, 40.0)];
        assert_eq!(music_starts(&gapped, 2.0), vec![0.0, 30.0]);
    }

    #[test]
    fn negative_volumes_mute_instead_of_inverting() {
        let on = LayerSwitch::new(true, true, -1.0, -2.0);
        assert_eq!((on.effect_volume, on.music_volume), (0.0, 0.0));
    }

    #[test]
    fn a_fade_never_runs_past_half_a_slice() {
        // `place` clamps, so a 0.3 s fade on a 0.2 s one-shot cannot invert the
        // envelope. Exercised through the pure part of the arithmetic.
        let dur = 0.2f64;
        let fo = 0.3f64.clamp(0.0, dur / 2.0);
        assert!((fo - 0.1).abs() < 1e-9);
        assert!(
            (dur - fo).max(0.0) > 0.0,
            "the fade-out must start inside the slice"
        );
    }
}
