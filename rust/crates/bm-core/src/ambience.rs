//! Post-process: the three sound-design layers under the voice mix.
//!
//! Exactly three layers, and the split is the point:
//!
//! * **effect**, per-scene beds and one-shot stingers, deliberately *sparse*:
//!   gated windows bounded by `max_window_s`, `cooldown_s` and `max_coverage`.
//!   A bed running under 100% of a chapter is a wall, not a bed.
//! * **music**, background tracks, continuous and far below the effects. A
//!   scene with no music tags, or with `music_off`, or whose tags match nothing
//!   in the pool, gets no music at all: silence is a valid answer here.
//! * **inject**, spot effects the script places itself, as items between the
//!   lines. A *hit* holds its clip as silence, an *overlap* runs under the
//!   following speech, a *trail* holds briefly and tails under it; a *stop*
//!   fades a running tail out, never a cut.
//!
//! Room reverb is not a layer: it is applied to the voice before the layers
//! exist, and it is left alone here. All three layers are ducked by **one**
//! sidechain compressor keyed on the voice track, applied as a single bus, so
//! "every layer drops whenever anybody speaks" is a property of the signal
//! path. The lift the music gets inside a planned pause is that same
//! compressor releasing.
//!
//! Offline, deterministic, no API: the same script, scene map and pools always
//! produce the same mix, and a missing clip degrades to silence for that span
//! rather than failing a chapter.

use crate::assemble::{wav_info, wav_seconds, Run};
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
/// track. One string doing both jobs is how `martial-shop-morning`, a place
/// came out with a hearth under it, because the keyword `shop` matched a fire
/// rule before the keyword `morning` reached the daylight rule.
#[derive(Debug, Clone, Default, serde::Serialize, Deserialize)]
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
#[derive(Debug, Clone, Default, serde::Serialize, Deserialize)]
pub struct PaletteEntry {
    /// Pool tags for assets/music-pool.json. Empty is the `none` value, no
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

/// Read a palette object, skipping `_note`-style keys, the same convention the
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
#[derive(Debug, Clone, Default, serde::Serialize, Deserialize)]
pub struct LegacyMusicRule {
    #[serde(rename = "match", default)]
    pub matches: Vec<String>,
    #[serde(default)]
    pub music: String,
}

/// Where a script that predates the `music` field gets its mood from.
#[derive(Debug, Clone, Default, serde::Serialize, Deserialize)]
pub struct LegacyMusic {
    #[serde(default)]
    pub rules: Vec<LegacyMusicRule>,
    #[serde(default)]
    pub default: LegacyMusicRule,
}

#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub struct Duck {
    #[serde(default = "default_threshold")]
    pub threshold: f64,
    #[serde(default = "default_ratio")]
    pub ratio: f64,
    #[serde(default = "default_attack")]
    pub attack: u32,
    #[serde(default = "default_release")]
    pub release: u32,
    /// Gain the key is held at while the chapter's **headline** is spoken.
    ///
    /// The headline is the one stretch where the beds are *meant* to arrive:
    /// the music starts at the top of the chapter and fades up under it, and
    /// a duck that pulls the beds to nothing there is how that fade-in came
    /// out inaudible. `0.0` mutes the key for those few seconds, so the beds
    /// sit at their own level under the title; `1.0` is ducking everywhere,
    /// which is what shipped before this field existed. The exemption costs
    /// nothing elsewhere: an effect window cannot open on the headline, so the
    /// music is the only layer with anything to say there.
    #[serde(default = "default_head_key")]
    pub head_key: f64,
}

fn default_head_key() -> f64 {
    0.0
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
            head_key: default_head_key(),
        }
    }
}

/// How sparse the effect layer must be.
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
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
    /// Fade at the end of a window that runs to the *end of the chapter*, the
    /// last thing the layer has to say. A window that stops mid-chapter keeps
    /// `fade_s`: there is speech after it, and a three-second fade would only
    /// bleed the bed into the next scene. A chapter that closes on a bed used
    /// to end on a 0.3 s edge, which is a cut in everything but name.
    #[serde(default = "d_end_fade")]
    pub end_fade_s: f64,
    /// Master gain for the layer, multiplied into every rule's own `level`.
    ///
    /// A rule's `level` is the *relative* balance between scenes, a storm
    /// against a hearth, and is the wrong place to say "the layer as a whole
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
fn d_end_fade() -> f64 {
    3.0
}

impl Default for EffectLayer {
    fn default() -> Self {
        EffectLayer {
            max_coverage: d_max_coverage(),
            cooldown_s: d_cooldown(),
            min_span_s: d_min_span(),
            max_window_s: d_max_window(),
            fade_s: d_fade(),
            end_fade_s: d_end_fade(),
            trim: d_effect_trim(),
        }
    }
}

/// How loud the music sits, and what it does inside a pause.
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub struct MusicLayer {
    /// Quiet on purpose: 0.06, against an effect layer whose rules reach 0.11
    /// once `layers.effect.trim` has been applied.
    #[serde(default = "d_music_level")]
    pub level: f64,
    /// Level inside a planned pause. The compressor has released by then, so
    /// this is a *further* lift on top of an already-unducked track.
    #[serde(default = "d_music_pause_level")]
    pub pause_level: f64,
    /// Fade at the head and tail of the whole layer: the opening cue rises out
    /// of silence *under the headline* and the closing one falls away at the
    /// chapter's end. Three seconds, not the 0.3 s the short edges of the other
    /// two layers use, a cue that arrives or leaves inside a third of a second
    /// reads as a cut, which is exactly what a chapter's first and last moments
    /// of music must not read as. Clamped to half a run by [`place`], so a
    /// chapter whose only cue is two seconds long still can't invert.
    #[serde(default = "d_music_fade")]
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
fn d_music_fade() -> f64 {
    3.0
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
            fade_s: d_music_fade(),
            xfade_s: d_xfade(),
            ramp_s: d_ramp(),
        }
    }
}

/// Where a beat fits, and how long it lasts.
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub struct PausePlan {
    /// Delivered seconds, the merge scales it by `speed` before writing it.
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

#[derive(Debug, Clone, Default, serde::Serialize, Deserialize)]
pub struct Layers {
    #[serde(default)]
    pub effect: EffectLayer,
    #[serde(default)]
    pub music: MusicLayer,
    /// Spot effects the script places itself. A map section like the other
    /// two, so the whole layer retunes in one place.
    #[serde(default)]
    pub inject: InjectLayer,
}

/// Knobs for the inject layer: script-placed spot effects on their own track.
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub struct InjectLayer {
    /// Master gain over the -20 LUFS foreground contract. 1.0 speaks hits at
    /// voice level; the operator's `inject_volume` multiplies this.
    #[serde(default = "d_inject_level")]
    pub level: f64,
    /// Fade that ends a retriggered instance when its sound starts again.
    #[serde(default = "d_fade")]
    pub fade_s: f64,
    /// Fade a `stop` takes to silence its sound. A stop is an ending, and
    /// endings fade, never a cut. Three seconds, not one and a half: at 1.5 s
    /// a sizzle bed's end read as a cut, which is the one thing a stop exists
    /// to avoid.
    #[serde(default = "d_stop_fade")]
    pub stop_fade_s: f64,
    /// `trail` hold when the directive names none: how long the solo part
    /// lasts before the tail ducks under the speech.
    #[serde(default = "d_hold")]
    pub default_hold_s: f64,
    /// Fade at the natural end of every tail, against a click. Clamped to half
    /// the clip, so it stays a *tail*: at 3 s a 6.9 s bed spent 44% of its life
    /// fading and read as though it had stopped early. A deliberate ending is
    /// [`Self::stop_fade_s`], which is the long one on purpose.
    #[serde(default = "d_tail_fade")]
    pub tail_fade_s: f64,
    /// Crossfade at each seam of a looped bed. Short on purpose, it is there
    /// to hide the join, not to be heard. Clamped to a quarter of the clip.
    #[serde(default = "d_loop_xfade")]
    pub loop_xfade_s: f64,
}

fn d_inject_level() -> f64 {
    1.0
}
fn d_stop_fade() -> f64 {
    3.0
}
fn d_hold() -> f64 {
    2.0
}
fn d_tail_fade() -> f64 {
    1.0
}
fn d_loop_xfade() -> f64 {
    0.25
}

impl Default for InjectLayer {
    fn default() -> Self {
        InjectLayer {
            level: d_inject_level(),
            fade_s: d_fade(),
            stop_fade_s: d_stop_fade(),
            default_hold_s: d_hold(),
            tail_fade_s: d_tail_fade(),
            loop_xfade_s: d_loop_xfade(),
        }
    }
}

/// Which of the two layers are on for this chapter.
///
/// A struct rather than two positional `bool`s so a call site cannot swap them,
/// and so the answer to "can this chapter come out dry?" is one `none()` rather
/// than a compound condition that has to stay in step with the argument list.
/// The two switches are independent on purpose: a book can want the effect beds
/// and no music, which is `Settings::ambience` and `Settings::music` doing
/// exactly what they say.
///
/// Injects ride with the effects switch: they are sound design, so an operator
/// asking for a plain read gets neither beds nor spot effects. Their volume is
/// independent, a third gain, not a third switch.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct LayerSwitch {
    pub effects: bool,
    pub music: bool,
    pub effect_volume: f64,
    pub music_volume: f64,
    pub inject_volume: f64,
}

impl LayerSwitch {
    pub fn new(
        effects: bool,
        music: bool,
        effect_volume: f64,
        music_volume: f64,
        inject_volume: f64,
    ) -> Self {
        LayerSwitch {
            effects,
            music,
            effect_volume: effect_volume.max(0.0),
            music_volume: music_volume.max(0.0),
            inject_volume: inject_volume.max(0.0),
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
    /// the analyzer prompt, and [`plan_music`] resolves a value to a track, so
    /// the prompt and the pool cannot disagree about what `warm` sounds like.
    #[serde(default, deserialize_with = "de_palette")]
    pub music_palette: MusicPalette,
    /// Where a script that predates the `music` field gets its mood from.
    /// A migration shim, kept so chapters already on disk keep merging; a
    /// freshly digested script declares `music` per segment and never reads it.
    #[serde(default)]
    pub legacy_scene_music: LegacyMusic,
    #[serde(default)]
    pub reverb_presets: BTreeMap<String, VoiceFx>,
    #[serde(default)]
    pub duck: Duck,
    #[serde(default)]
    pub layers: Layers,
    #[serde(default)]
    pub pause: PausePlan,
    /// The sound a thought fires at its seam, declared by the pack. See
    /// [`ThoughtRule`].
    #[serde(default)]
    pub thought: ThoughtRule,
}

/// The spot sound a thought fires at its seam, declared by the pack:
/// `"thought": {"sound": "thought-chime"}`.
///
/// A thought is marked in the script by code (`"kind": "thought"`) and is
/// otherwise just another line to the mixer. This names the one clip that goes
/// with that marker, and the digest *lifts* it into a sibling `{"sound": …}`
/// item at the thought's seam — so no render path learns a new rule: the inject
/// layer already places a spot effect the script names, and this is simply a
/// name the pack chooses once instead of the analyzer guessing per line. The
/// name must exist in `inject-pool.json`: a declared sound nobody has a clip
/// for is refused at digest time, not mixed down to silence.
///
/// Absent or empty means thoughts carry no sound, which is every pack that has
/// not asked for one.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct ThoughtRule {
    #[serde(default)]
    pub sound: String,
}

/// How much of a scene's treatment the Narrator takes: a tenth of a
/// character's depth — in the room, never standing in it.
pub const NARRATOR_DEPTH: f64 = 0.1;

/// Which engine runs a voice treatment's chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FxEngine {
    Ffmpeg,
    Sox,
}

/// One voice treatment: what a slot's voice is run through, and how much decay
/// to **reserve** after it so a reverb is not cut mid-tail.
///
/// Two shapes, so a pack migrates at its own pace:
///
/// * a bare string — a legacy ffmpeg `-af` chain, no reserved tail;
/// * an object — `{"sox": "reverb 45 45 80", "tail_s": 1.2}`.
///
/// A `sox` chain is a SoX effect list (`reverb 45 45 80`, `overdrive gain -3`),
/// run on the slot's piece by the `sox` binary; an `ffmpeg` chain is the same
/// `-af` string the presets used to be. The engine is chosen by which key is
/// set, never inferred from the text: the two grammars overlap but are not the
/// same, and guessing is how one gets run by the other.
///
/// A `narrator` key still parses where an older map carries one, and is
/// ignored: the Narrator takes a tenth of every treatment (see
/// [`NARRATOR_DEPTH`]).
#[derive(Debug, Clone, Deserialize, serde::Serialize, PartialEq)]
#[serde(untagged)]
pub enum VoiceFx {
    Chain(String),
    Spec(VoiceFxSpec),
}

#[derive(Debug, Clone, Deserialize, serde::Serialize, PartialEq)]
pub struct VoiceFxSpec {
    /// SoX effect chain, run by `sox` (no `sox`/input/output — just the effects).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sox: Option<String>,
    /// Legacy ffmpeg `-af` chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ffmpeg: Option<String>,
    /// Seconds of decay to reserve after the slot, so the tail rings out
    /// instead of being cut at the next line. Zero is "no tail".
    #[serde(default)]
    pub tail_s: f64,
}

impl VoiceFx {
    /// The engine and chain this treatment runs.
    pub fn engine_and_chain(&self) -> (FxEngine, &str) {
        match self {
            VoiceFx::Chain(c) => (FxEngine::Ffmpeg, c.as_str()),
            VoiceFx::Spec(s) => match (&s.sox, &s.ffmpeg) {
                (Some(c), _) => (FxEngine::Sox, c.as_str()),
                (None, Some(c)) => (FxEngine::Ffmpeg, c.as_str()),
                (None, None) => (FxEngine::Sox, ""),
            },
        }
    }

    /// Seconds of decay reserved after the slot, clamped to something sane.
    pub fn tail_s(&self) -> f64 {
        match self {
            VoiceFx::Chain(_) => 0.0,
            VoiceFx::Spec(s) => s.tail_s.clamp(0.0, 10.0),
        }
    }
}

/// The palette's keys, sorted, what a script's `music` value is checked
/// against, and what a rejection message lists back to the analyzer.
pub fn palette_names(map: &SceneMap) -> Vec<String> {
    map.music_palette.keys().cloned().collect()
}

/// The palette rendered for the digest prompt: `name (tags; gloss), ...`.
///
/// Built from the map rather than written into the prompt text, so adding a
/// value (and the clip that answers it) is one edit to one file. A prompt that
/// listed its own vocabulary would drift the moment the pool changed. The tags
/// ride along so the analyzer sees what each mood *means* in pool terms, the
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

/// The scene map's rule vocabulary rendered for the digest prompt: every match
/// word the resolved rules can match, sorted and deduped.
///
/// **The place vocabulary, which is not the bed vocabulary.** A `scene` label
/// is matched against [`Rule::matches`], so those words are what the analyzer
/// has to write for a rule to fire at all; [`effect_tags`] is the pool's answer
/// to the tag sets the rules hand it. The two were being conflated: the prompt
/// injected the effect tags, called them "the vocabulary it answers to", and
/// warned that a label built from anything else "gets silence" — so a rule
/// whose match word is not an effect tag (`palace`, `hall`, `garden`, `gate`,
/// `morning`, `dusk`) matched a word the model had been told not to use. On the
/// shipped map 15 of 61 match words are effect tags, so the rules were running
/// on the intersection.
///
/// This is the same arrangement as [`palette_prompt`]: the vocabulary is
/// rendered from the map rather than written into a prompt, so a pack that adds
/// a rule reaches the analyzer without a pack being able to edit a prompt —
/// which is the whole reason the music palette lives pack-side. Without it a
/// rule is decoration the operator cannot see.
///
/// Every rule contributes, `default` does not: it has no match set, it is what
/// a label matches when nothing else did.
pub fn scene_prompt(map: &SceneMap) -> String {
    let mut words: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for rule in &map.rules {
        words.extend(
            rule.matches
                .iter()
                .filter(|w| !w.trim().is_empty())
                .cloned(),
        );
    }
    words.into_iter().collect::<Vec<String>>().join(", ")
}

/// Every tag any effect-pool sound answers to, sorted and deduped: the **bed**
/// vocabulary the digest prompt offers the analyzer. A scene built from these
/// words resolves to a pooled sound by tag overlap instead of by keyword luck.
///
/// The *place* vocabulary is [`scene_prompt`], and the two are not the same
/// list. A place word that is no bed word is still the right thing to write:
/// the rules route it, and the bed is chosen by tag overlap, not by the label.
pub fn effect_tags(pool: &ClipPool) -> Vec<String> {
    let mut out = std::collections::BTreeSet::new();
    for sound in pool.values() {
        for t in &sound.tags {
            out.insert(t.clone());
        }
    }
    out.into_iter().collect()
}

// ---------------------------------------------------------------------------
// what a pool is still being used for
// ---------------------------------------------------------------------------

/// One reason a pooled sound cannot be removed: a thing that names it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct UseOf {
    /// The scene map's rule or palette value, or `ch09` for a script.
    pub by: String,
    /// The tags that reach the sound. Empty for a script, which names the sound
    /// itself rather than a tag.
    pub tags: Vec<String>,
}

impl UseOf {
    /// `mountain rule [mountain, wind]`, what a screen shows beside the entry.
    pub fn label(&self) -> String {
        if self.tags.is_empty() {
            self.by.clone()
        } else {
            format!("{} [{}]", self.by, self.tags.join(", "))
        }
    }
}

/// Sounds at least one tag set can reach, and the tag sets that reach them.
///
/// A scene names *tags*; the pool answers with a *sound*. So "is this sound in
/// use" is a question about the tag sets the map names, not about the sound's
/// own name: `wind` is in use because the mountain rule asks for `mountain`,
/// and the map never says `wind` anywhere. Deleting it would make every
/// mountain scene score zero and go quiet, the failure the effect pool's own
/// note warns about, so the editor refuses, and this is what it refuses on.
///
/// The test is the weakest one `pick` applies before narrowing: at least one
/// tag in common. A one-tag sound may lose the overlap contest on most
/// chapters and still win on a thin one, and a guard that lets you delete
/// something that sometimes plays is not a guard.
fn reachable(pool: &ClipPool, sets: impl Iterator<Item = (String, Vec<String>)>) -> Usage {
    let mut out: Usage = BTreeMap::new();
    for (by, tags) in sets {
        if tags.is_empty() {
            continue;
        }
        for (name, sound) in pool {
            if sound.tags.iter().any(|t| tags.contains(t)) {
                out.entry(name.clone()).or_default().push(UseOf {
                    by: by.clone(),
                    tags: tags.clone(),
                });
            }
        }
    }
    for uses in out.values_mut() {
        uses.sort();
        uses.dedup();
    }
    out
}

/// Sound -> what the scene map's rules still reach. See [`reachable`].
pub fn effect_usage(map: &SceneMap, pool: &ClipPool) -> Usage {
    let named = map.rules.iter().map(|r| {
        (
            format!("scene rule {:?}", r.matches.join(", ")),
            r.effect.clone(),
        )
    });
    let dflt = std::iter::once(("scene default".to_string(), map.default.effect.clone()));
    reachable(pool, named.chain(dflt))
}

/// Sound -> what the palette still reaches. See [`reachable`].
pub fn music_usage(map: &SceneMap, pool: &ClipPool) -> Usage {
    reachable(
        pool,
        map.music_palette
            .iter()
            .map(|(mood, e)| (format!("palette {mood:?}"), e.tags.clone())),
    )
}

/// Sound -> the chapters whose script places it.
///
/// Unlike the other two layers this one is a direct lookup, because the script
/// names the *sound*, `{"sound": "coin"}`, rather than a tag. An item is a
/// sound when it carries `sound` or `stop` and no `text`
/// ([`crate::util::is_sound_item`]); a `stop` counts, because it is placed for
/// the same sound and would be left fading nothing.
pub fn inject_usage(scripts: &[(u32, Value)]) -> BTreeMap<String, Vec<u32>> {
    let mut out: BTreeMap<String, Vec<u32>> = BTreeMap::new();
    for (chapter, doc) in scripts {
        let Some(items) = doc.get("segments").and_then(|s| s.as_array()) else {
            continue;
        };
        for item in items {
            if !crate::util::is_sound_item(item) {
                continue;
            }
            for key in ["sound", "stop"] {
                let Some(name) = item.get(key).and_then(|v| v.as_str()).map(str::trim) else {
                    continue;
                };
                if name.is_empty() {
                    continue;
                }
                let chapters = out.entry(name.to_string()).or_default();
                if !chapters.contains(chapter) {
                    chapters.push(*chapter);
                }
            }
        }
    }
    for chapters in out.values_mut() {
        chapters.sort_unstable();
    }
    out
}

/// Sound -> every reason it is still in use, per layer.
pub type Usage = BTreeMap<String, Vec<UseOf>>;

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
/// that is only part of a compound label, which is why the rules are ordered
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
/// label, the shim that keeps chapters already on disk mergeable. Mixed runs
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
/// plays). Two fields, two jobs, see this module's docs.
#[derive(Debug, Clone)]
pub struct Turn {
    pub wav: PathBuf,
    pub scene: String,
    /// A palette value, or `""` for none. Resolved by the caller from the
    /// script's own `music` field, or from the legacy shim.
    pub music: String,
    pub speaker: String,
    /// Script-placed spot effects, anchored at this turn's end.
    pub injects: Vec<Inject>,
}

/// One position in the mix: what plays, when it starts and ends, and how much
/// air follows it.
///
/// The timeline is built once and read twice, [`crate::assemble::concat_slots`]
/// writes these gaps and the layers read these offsets, which is the whole
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
    /// Script-placed spot effects, anchored at this slot's end.
    pub injects: Vec<Inject>,
    pub start: f64,
    pub end: f64,
    /// Silence written after this turn: the uniform gap plus any beat.
    pub gap_ms: u32,
    /// How much of `gap_ms` is a planned beat, held between this turn and the
    /// next. The layers need the beat on its own, it is where the music lifts.
    pub pause_ms: u32,
    /// How much of `gap_ms` is the injects' own solo time, in pre-tempo
    /// milliseconds, the room a hit or a trail's hold plays in.
    ///
    /// Tracked separately because it is the one part of a gap that outlives the
    /// last slot: an inject anchored at the final line's end has nowhere else
    /// to sound, so the concat writes this much silence after it even though it
    /// writes no bare gap there. Without that the hit is placed past the end of
    /// the mix and dropped in silence, with the plan log still claiming it ran.
    pub inject_ms: u32,
}

/// Lay the turns out on the mix clock, inserting the planned pauses.
///
/// `pauses` maps a turn index to the beat held *before* that turn, in pre-tempo
/// milliseconds, the scene map authors it as `pause_before_s` on the scene
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
        // Header probe, not a full read: only the params and the duration are
        // needed here, and the samples are read again by the concat anyway.
        let info = wav_info(&turn.wav)?;
        let p = (info.channels, info.sample_rate, info.bits);
        match params {
            None => params = Some(p),
            Some(prev) if prev != p => anyhow::bail!(
                "{}: {p:?} != {prev:?} (mixed engines/rates — use per-engine seg dirs)",
                turn.wav.display()
            ),
            _ => {}
        }
        let dur = info.seconds();
        let pause_ms = pauses.get(&(i + 1)).copied().unwrap_or(0);
        out.push(Slot {
            wav: turn.wav.clone(),
            scene: turn.scene.clone(),
            music: turn.music.clone(),
            speaker: turn.speaker.clone(),
            injects: turn.injects.clone(),
            start: t,
            end: t + dur,
            gap_ms: gap_ms + pause_ms,
            pause_ms,
            inject_ms: 0,
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
/// clock slides further behind the voice with every line, by the end of a
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
        s.inject_ms = (s.inject_ms as f64 / speed).round() as u32;
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

/// Where the chapter's headline ends, in delivered seconds, the stretch where
/// the duck lets go (see [`Duck::head_key`]).
///
/// The headline is the opening turn, and `plan_turns` builds it with neither a
/// place nor a mood: that pairing is its signature, which makes this a property
/// of the chapter rather than a number of seconds somebody has to guess at and
/// keep in step with the writing. A chapter that opens on a scene, or on a
/// line the analyzer gave a label but no mood, has no headline and no
/// exemption.
fn headline_end(slots: &[Slot]) -> Option<f64> {
    let first = slots.first()?;
    (first.end > 0.0 && first.scene.is_empty() && first.music.is_empty()).then_some(first.end)
}

/// Where a beat fits in this chapter, as `turn index -> pre-tempo milliseconds`.
///
/// A beat belongs where a *scene changes and the change is narrated*: the
/// incoming or the outgoing turn must be the Narrator, so the pause lands on
/// narration handing over rather than in the middle of an exchange. Narration
/// *resuming* is the stronger signal, a new scene establishing itself, so it
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
        // boundary, that would put a beat in the middle of a continuous scene.
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
    // earliest, so the choice is a decision, not whichever rule happened to
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
/// from, the previous window's cooldown pushes it. The report used to match
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
            // still compares raw rule levels, the trim is a property of the
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
    /// The palette value that chose this track, what the log reports and what
    /// a re-merge is compared against.
    pub mood: String,
    /// The *sound* the palette resolved to (`soft-relax`). Consecutive slots
    /// that land on the same sound are one run, because a repeated mood must be
    /// continuous music rather than a crossfade into the same tune.
    pub sound: String,
    /// The take that answers it. Which of `soft-relax-bg-1/2` plays is an
    /// implementation detail of the pick, so the mix reads it from here rather
    /// than looking the sound up a second time, one lookup, one answer.
    pub file: String,
    /// The sound's own trim (`Sound::level`), carried the same way and for the
    /// same reason as `file`: the mix multiplies it into the layer level, and
    /// re-looking it up would be a second answer to a question already asked.
    pub level: f64,
    /// Audible coverage ends here; the slice may extend past it into a
    /// crossfade with the next run. The chapter's first run is the exception at
    /// the other end: it starts at the head of the timeline, under the headline
    /// (see [`plan_music`]), and the slice is extended back to meet it.
    pub start: f64,
    pub end: f64,
    pub pauses: Vec<(f64, f64)>,
}

/// Which track plays when, and where it lifts.
///
/// Read per *slot*, not per span: the mood is a property of the line being
/// spoken, and a cue breaks exactly where the mood changes. The pick is seeded
/// from the palette *value* rather than its tags, so a change of value is a
/// change of track by construction; the chapter goes into the seed too, so two
/// chapters in the same mood still differ.
///
/// A slot with no value, with `none`, or whose palette entry names tags nothing
/// in the pool answers contributes nothing. `none` is a choice; a pool that
/// has lost its last clip for a mood is a degraded mix, and both are reported
/// once each.
///
/// The chapter's *first* run is pulled back to the head of the timeline (the
/// slot the headline is spoken in) so the music comes up under the title; every
/// later cue keeps the offset its own slot gave it. The layer's own knobs
/// (`level`, `xfade_s`, `ramp_s`) are not read here: they shape how a run is
/// *rendered*, which is the caller's job.
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
        // digest validator, say so rather than silently going quiet.
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
            // take anyway, merging on the sound is what keeps a scene change
            // inside one mood from cutting the music.
            Some(last) if last.sound == picked.sound => {
                last.end = slot.end;
                last.pauses.extend(inside);
            }
            _ => out.push(MusicRun {
                mood: mood.to_string(),
                sound: picked.sound,
                file: picked.file,
                level: picked.level,
                start: slot.start,
                end: slot.end,
                pauses: inside,
            }),
        }
    }
    for m in &unpooled {
        eprintln!("music: {m} -> no music there");
    }
    // Pulled back, never pushed forward: `min` against the head keeps a cue
    // that somehow starts before the first slot where it is.
    if let (Some(first), Some(head)) = (out.first_mut(), slots.first()) {
        first.start = first.start.min(head.start);
    }
    out
}

// ---------------------------------------------------------------------------
// injects: script-placed spot effects
// ---------------------------------------------------------------------------

/// How one injected sound sits on the timeline. Per *directive*, not per
/// sound: the same boil can underscore one scene (`overlap`) and punctuate
/// another (`trail`), and the filename never says which.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectMode {
    /// Inline: the narration waits out the whole clip.
    Hit,
    /// Parallel: zero timeline time, runs under the following speech until the
    /// clip ends or a `stop` fades it.
    Overlap,
    /// Both: `hold_s` of solo time, then the tail runs under the speech.
    Trail,
}

impl InjectMode {
    /// How much of its own level this mode plays at.
    ///
    /// A `hit` owns the silence it was written into, so it plays at the level
    /// the pool gives it. An `overlap` and a `trail` own nothing: they run
    /// *under* the speech, and at full level they stop being a bed and start
    /// competing with the voice. The library had already voted on this, every
    /// overlap or trail clip that sat right had been hand-trimmed to 0.05–0.2 in
    /// its own pool entry, which is a per-clip workaround for a property of the
    /// mode. So the mode carries the trim, and the pool's `level` goes back to
    /// being the balance *between* clips of one mode.
    pub const fn gain(self) -> f64 {
        match self {
            InjectMode::Hit => 1.0,
            InjectMode::Overlap | InjectMode::Trail => 0.1,
        }
    }
}

/// The mode a pool entry's string names, or `None` for a string that names
/// none, which the caller reads as *skip this directive*, never as a default.
///
/// The one place the string is parsed. `injects_of` needs the mode to place the
/// clip; the `:sound` editor needs it to say how loud the clip will be, and a
/// second `match` there would be a second answer to the same question, the kind
/// that goes stale silently when a fourth mode is added.
pub fn inject_mode(name: &str) -> Option<InjectMode> {
    match name {
        "hit" => Some(InjectMode::Hit),
        "overlap" => Some(InjectMode::Overlap),
        "trail" => Some(InjectMode::Trail),
        _ => None,
    }
}

/// One inject directive: start a sound at the place it was cut into, or fade
/// out a running one from there.
#[derive(Debug, Clone, PartialEq)]
pub enum Inject {
    Start {
        sound: String,
        mode: InjectMode,
        hold_s: f64,
        level: f64,
    },
    Stop {
        sound: String,
    },
}

/// The directives that fire at one place on the timeline, in listed order.
///
/// A directive is the script's own **sound item**, `{"sound": "page-turn"}`
/// a sibling of the lines rather than a field on one. All it carries is the
/// *name*; **how the sound behaves comes from the pool**, because that is a
/// property of the clip and not of the chapter (a per-use mode is not
/// something a reader of prose can know, and the analyzer guessed at it).
///
/// Lenient on purpose: the digest validator is the strict gate (it can ask
/// the analyzer for a repair), while a merge must survive a hand edit the way
/// it survives a missing clip, malformed entries are skipped, and an unknown
/// sound resolves to no take at [`plan_inject_takes`] with one warning rather
/// than a dead chapter.
pub fn injects_of(directives: &[Value], pool: &ClipPool, default_hold: f64) -> Vec<Inject> {
    let mut out = Vec::new();
    for e in directives {
        let Some(obj) = e.as_object() else { continue };
        if let Some(stop) = obj.get("stop").and_then(|v| v.as_str()).map(str::trim) {
            if !stop.is_empty() {
                out.push(Inject::Stop {
                    sound: stop.to_string(),
                });
            }
            continue;
        }
        let Some(sound) = obj.get("sound").and_then(|v| v.as_str()).map(str::trim) else {
            continue;
        };
        if sound.is_empty() {
            continue;
        }
        // An unknown sound is not the mixer's problem to guess at: it has no
        // entry, so it has no mode either, and `plan_inject_takes` warns once
        // and plays nothing. Defaulting it to `hit` here would give a sound
        // nobody registered a length of silence nobody asked for.
        let Some(entry) = pool.get(sound) else {
            continue;
        };
        let Some(mode) = inject_mode(entry.mode.as_deref().unwrap_or("hit")) else {
            continue;
        };
        let hold_s = entry.hold.filter(|h| *h > 0.0).unwrap_or(default_hold);
        // The pool's trim, then the mode's. A `hit` is an event and keeps its
        // level; an `overlap`/`trail` is a bed and renders at a tenth of it.
        let level = entry.level.filter(|l| *l > 0.0).unwrap_or(1.0) * mode.gain();
        out.push(Inject::Start {
            sound: sound.to_string(),
            mode,
            hold_s,
            level,
        });
    }
    out
}

/// The inject registry rendered for the digest prompt:
/// `sound (mode; tags; Ns)`.
///
/// Everything the analyzer needs to choose a sound and nothing it has to
/// decide: the *name* it writes, the mode so it knows whether the narration
/// will pause for it (`hit`) or carry on over it (`overlap`/`trail`), the tags
/// that say what it sounds like, and the length. The mode is rendered rather
/// than left to the analyzer because it is a property of the clip, the script
/// says *which* sound and *where*, never how it behaves.
pub fn inject_prompt(pool: &ClipPool) -> String {
    pool.iter()
        .map(|(name, s)| {
            let mut inner = s.mode.clone().unwrap_or_else(|| "hit".into());
            if let Some(h) = s.hold.filter(|h| *h > 0.0) {
                inner.push_str(&format!(" {}s", trim_num(h)));
            }
            // `loop` is the one piece of behaviour the analyzer has to *act* on
            // beyond naming the sound: a looped bed runs until it is stopped, so
            // starting one without a `stop` means it plays once.
            if s.looped {
                inner.push_str(", loop");
            }
            inner.push_str("; ");
            inner.push_str(&s.tags.join(", "));
            if let Some(d) = s.dur_s {
                inner.push_str(&format!("; {}s", trim_num(d)));
            }
            format!("{name} ({inner})")
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// `0.6` and `51.3` read better than `0.600000` and `51.300000` in a prompt.
fn trim_num(v: f64) -> String {
    if v >= 10.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.1}")
    }
}

/// Which take of an injected sound plays at one slot.
///
/// A direct registry lookup, not a tag pick: the analyzer names the *sound*,
/// and sounds are disjoint by construction, so scoring tags could only answer
/// a question nobody asked. The take rolls on the chapter, the slot and the
/// sound, a re-merge reproduces it, and neighbouring chapters vary. `None`
/// is an unknown sound (a hand edit past the validator), warned once here so
/// the chapter degrades to skipping it rather than dying on it.
pub fn inject_take(
    pool: &ClipPool,
    chapter: u32,
    slot: usize,
    sound: &str,
) -> Option<audio_pool::Picked> {
    let entry = pool.get(sound)?;
    if entry.files.is_empty() {
        return None;
    }
    let seed = audio_pool::seed(chapter, slot, &[sound.to_string()]);
    let file = entry.files[(seed % entry.files.len() as u64) as usize].clone();
    Some(audio_pool::Picked {
        sound: sound.to_string(),
        file,
        looped: entry.looped,
        level: audio_pool::sound_level(entry),
    })
}

/// The take each slot's directives resolve to, slot for slot. Computed once
/// and shared by the hold planner and the event planner, so the silence the
/// concat writes and the sounds the layers place agree on which take plays.
pub fn plan_inject_takes(
    slots: &[Slot],
    pool: &ClipPool,
    chapter: u32,
) -> Vec<Vec<Option<audio_pool::Picked>>> {
    let mut warned: Vec<String> = Vec::new();
    slots
        .iter()
        .enumerate()
        .map(|(i, slot)| {
            slot.injects
                .iter()
                .map(|inj| match inj {
                    Inject::Stop { .. } => None,
                    Inject::Start { sound, .. } => {
                        let take = inject_take(pool, chapter, i, sound);
                        if take.is_none() && !warned.contains(sound) {
                            warned.push(sound.clone());
                            eprintln!("inject: sound {sound:?} not in the pool -> skipped");
                        }
                        take
                    }
                })
                .collect()
        })
        .collect()
}

/// Durations of the takes one chapter's injects picked, by pool path. One
/// ffprobe per file; a file gone missing since the registry was written is
/// absent from the map, and both planners read absence as zero with a warning
///, a renamed clip degrades to a skipped inject, not a dead merge.
pub fn probe_inject_durs(
    takes: &[Vec<Option<audio_pool::Picked>>],
    assets: &Path,
) -> BTreeMap<String, f64> {
    let mut files: Vec<String> = Vec::new();
    for slot in takes {
        for take in slot.iter().flatten() {
            if !files.contains(&take.file) {
                files.push(take.file.clone());
            }
        }
    }
    probe_durs(&files, assets, "inject")
}

/// Duration in seconds of each named pool file, keyed by the same `assets/`-
/// relative path the registry writes. One ffprobe per file, and a file that
/// cannot be read is simply absent — every caller reads absence as zero and
/// degrades, which is what a renamed clip should do rather than kill a merge.
///
/// `what` names the layer in the warning, because the two layers do not fail
/// the same way: an unprobeable inject is a skipped spot effect, an
/// unprobeable track is a run that cannot be told from one that fits.
pub fn probe_durs(files: &[String], assets: &Path, what: &str) -> BTreeMap<String, f64> {
    let mut out = BTreeMap::new();
    let mut missing: Vec<String> = Vec::new();
    for file in files {
        if out.contains_key(file) {
            continue;
        }
        let p = clip_path(assets, file);
        let dur = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "csv=p=0",
                &p.to_string_lossy(),
            ])
            .output()
            .ok()
            .and_then(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .trim()
                    .parse::<f64>()
                    .ok()
            })
            .filter(|d| *d > 0.0);
        match dur {
            Some(d) => {
                out.insert(file.clone(), d);
            }
            None => {
                if !missing.contains(file) {
                    missing.push(file.clone());
                    eprintln!("{what}: cannot probe {file} -> treated as a single pass");
                }
            }
        }
    }
    out
}

/// Solo time each slot's injects need, written into the gap that follows the
/// slot, the same place a planned pause goes, in the same pre-tempo
/// milliseconds, so [`retime`] keeps them honest for free. Hits cost their
/// whole clip, trails their hold (never more than the clip), overlaps nothing.
/// Multiple directives queue in listed order; the event planner replays the
/// same queue, so the silence and the sounds agree.
///
/// The gaps are not the whole story. Writing a hold makes the concat longer,
/// so every slot after it starts later than the clock [`timeline`] laid out,
/// and every layer is placed by reading `Slot::start` and `Slot::end`. A hold
/// early in a chapter therefore slid everything after it late while the layers
/// stayed on the old clock. So the timeline is re-laid here, in the same call
/// that moved it: a caller cannot have the gaps without the clock that
/// matches them.
pub fn plan_inject_holds(
    slots: &mut [Slot],
    takes: &[Vec<Option<audio_pool::Picked>>],
    durs: &BTreeMap<String, f64>,
    speed: f64,
) {
    for (slot, slot_takes) in slots.iter_mut().zip(takes.iter()) {
        let mut solo = 0.0f64;
        for (inj, take) in slot.injects.iter().zip(slot_takes.iter()) {
            let (mode, hold_s) = match inj {
                Inject::Start { mode, hold_s, .. } => (*mode, *hold_s),
                Inject::Stop { .. } => continue,
            };
            let dur = take
                .as_ref()
                .and_then(|t| durs.get(&t.file))
                .copied()
                .unwrap_or(0.0);
            solo += match mode {
                InjectMode::Hit => dur,
                InjectMode::Trail => hold_s.min(dur),
                InjectMode::Overlap => 0.0,
            };
        }
        if solo > 0.0 {
            let ms = (solo * speed * 1000.0).round() as u32;
            slot.gap_ms += ms;
            // Kept on its own too: the concat writes no bare gap after the last
            // slot, and this is the part of it that has to survive that.
            slot.inject_ms = ms;
        }
    }
    relayout(slots);
}

/// Re-accumulate `start`/`end` from each slot's own duration and gap.
///
/// A slot's duration is `end - start`, the only copy of it the struct holds,
/// and the one thing a gap can never change. [`timeline`] lays the clock out
/// once; this is what re-lays it after something moves the gaps, so the two
/// cannot disagree about where a slot begins.
fn relayout(slots: &mut [Slot]) {
    let mut t = 0.0f64;
    for s in slots.iter_mut() {
        let dur = s.end - s.start;
        s.start = t;
        s.end = t + dur;
        t = s.end + s.gap_ms as f64 / 1000.0;
    }
}

/// One placed inject: what plays, when, and how it ends. The level is the
/// directive's own, the pool's trim with [`InjectMode::gain`] already folded
/// in, and the layer and operator gains are applied at render.
#[derive(Debug, Clone, PartialEq)]
pub struct InjectEvent {
    pub sound: String,
    pub file: String,
    pub start: f64,
    pub end: f64,
    pub level: f64,
    pub mode: InjectMode,
    pub fade_in: f64,
    pub fade_out: f64,
    /// The pool says this sound is a bed, so the window it was given is filled
    /// by repeating it rather than by playing once and leaving silence. False
    /// for every one-shot, and false for a bed with no `stop`, see
    /// [`loop_copies`].
    pub looped: bool,
}

/// A running overlap/trail tail (and, transiently, a hit): what is sounding,
/// and when the mix stops hearing it.
struct InjectActive {
    sound: String,
    file: String,
    start: f64,
    end: f64,
    level: f64,
    mode: InjectMode,
    fade_out: f64,
    looped: bool,
}

/// How many copies of a clip a crossfaded loop needs to fill `window`.
///
/// `n` copies joined by `n-1` crossfades of `xfade` seconds run
/// `n*clip - (n-1)*xfade`, so solving that for `>= window` gives
/// `n >= (window - xfade) / (clip - xfade)`. `None` when one play already
/// covers the window: a one-shot is not a loop, and a clip that needs no repeat
/// must not be handed a seam it never had.
///
/// A window only exists when something ends the sound, the `stop` in the
/// script. A bed with no `stop` gets the clip's own length and is therefore
/// never looped, which is why the digest has to place one.
pub fn loop_copies(window: f64, clip: f64, xfade: f64) -> Option<usize> {
    if clip <= 0.0 || window <= clip {
        return None;
    }
    // A crossfade a quarter of the clip long is already a blend, not a seam.
    let x = xfade.clamp(0.0, clip / 4.0);
    let step = (clip - x).max(1e-6);
    Some((((window - x) / step).ceil().max(1.0) as usize).max(2))
}

/// The filter graph that turns input 0 into a crossfaded loop of `copies`.
///
/// The seam is the whole point: a hard repeat of a sizzle clicks at every join,
/// and a bed that clicks four times a minute is worse than no bed. `acrossfade`
/// overlaps each pair into a short equal-power blend, `c1=tri:c2=tri` is
/// ffmpeg's default curve, which is what a short seam wants. The caller
/// truncates with an output `-t`, so the loop is allowed to run past the window
/// and no `atrim` is needed here.
pub fn loop_filter(copies: usize, xfade: f64, volume: f64) -> String {
    loop_filter_with_tail(copies, xfade, &format!("volume={volume:.4}"))
}

/// [`loop_filter`] with the post-crossfade chain supplied, which is what lets the
/// two callers share one graph builder: the inject layer wants a scalar gain
/// and the music layer wants a time-varying `volume` expression for its pause
/// lift, and both want the same `aformat` and the same `[out]` label. A
/// hand-spliced string is how a graph ends up with the gain filter *before* the
/// fade, which ducks the crossfade instead of the loop.
pub fn loop_filter_with_tail(copies: usize, xfade: f64, tail: &str) -> String {
    let ins: String = (0..copies).map(|i| format!("[c{i}]")).collect();
    let mut f = format!("[0:a]asplit={copies}{ins}");
    let mut prev = "c0".to_string();
    for i in 1..copies {
        let out = format!("o{i}");
        f.push_str(&format!(
            ";[{prev}][c{i}]acrossfade=d={xfade:.3}:c1=tri:c2=tri[{out}]"
        ));
        prev = out;
    }
    format!("{f};[{prev}]{tail},aformat=sample_rates=48000:channel_layouts=mono[out]")
}

/// End every still-sounding instance of `sound` at `at + fade`, eased rather
/// than cut. A stop for a sound with nothing running, or one already over, is
/// a no-op: the script outliving its sounds is ordinary, not an error.
fn stop_actives(actives: &mut [InjectActive], sound: &str, at: f64, fade: f64) {
    for a in actives.iter_mut().filter(|a| a.sound == sound) {
        if at >= a.end {
            continue;
        }
        let end = a.end.min(at + fade);
        a.end = end;
        a.fade_out = (end - at).max(0.0);
    }
}
/// Lay a chapter's injects on the delivered clock.
///
/// Every directive anchors at its slot's end, it plays when the segment's
/// speech ends, in listed order, hits queuing inside the silence
/// [`plan_inject_holds`] already wrote. Overlap and trail tails keep sounding
/// under the following speech until the clip ends or a `stop` names them: a
/// stop fades from its anchor over `stop_fade_s`, and starting a sound
/// retriggers it (the old instance fades in `fade_s`, so two boils never
/// stack +6 dB). A tail nobody stops ends with the clip, eased by
/// `tail_fade_s` against a click.
pub fn plan_injects(
    slots: &[Slot],
    takes: &[Vec<Option<audio_pool::Picked>>],
    durs: &BTreeMap<String, f64>,
    cfg: &InjectLayer,
) -> Vec<InjectEvent> {
    let mut actives: Vec<InjectActive> = Vec::new();
    for (slot, slot_takes) in slots.iter().zip(takes.iter()) {
        let mut cursor = slot.end;
        for (inj, take) in slot.injects.iter().zip(slot_takes.iter()) {
            match inj {
                Inject::Stop { sound } => {
                    stop_actives(&mut actives, sound, cursor, cfg.stop_fade_s);
                }
                Inject::Start {
                    sound,
                    mode,
                    hold_s,
                    level,
                } => {
                    let Some(take) = take else { continue };
                    let dur = durs.get(&take.file).copied().unwrap_or(0.0);
                    if dur <= 0.0 {
                        continue;
                    }
                    // The queue is serial: every directive anchors at the
                    // cursor, and hits and trails advance it by their solo
                    // time. That is the only anchor that keeps the sounds
                    // where the silence is, `plan_inject_holds` wrote the
                    // *sum* of those solos into the gap, so a trail that
                    // started back at `slot.end` would play its hold under a
                    // queued hit and leave its own tail in dead air.
                    let anchor = cursor;
                    // Retrigger: the old instance gets out of the way before
                    // the new one starts, so the same sound never stacks.
                    stop_actives(&mut actives, sound, anchor, cfg.fade_s);
                    // A looped bed has no natural end: the clip is a length of
                    // texture, not a statement, and the script's `stop` is what
                    // says when the scene moved on. So it opens *unbounded*
                    // `stop_actives` can only shorten, and a stop that arrives
                    // past the clip's own end would otherwise be ignored, which
                    // is exactly how a 7 s bed ended in a 142 s kitchen.
                    let open_end = if take.looped {
                        f64::INFINITY
                    } else {
                        anchor + dur
                    };
                    match mode {
                        InjectMode::Hit => {
                            actives.push(InjectActive {
                                sound: sound.clone(),
                                file: take.file.clone(),
                                start: anchor,
                                end: open_end,
                                level: *level,
                                mode: *mode,
                                fade_out: cfg.tail_fade_s,
                                looped: take.looped,
                            });
                            cursor += dur;
                        }
                        InjectMode::Overlap => {
                            actives.push(InjectActive {
                                sound: sound.clone(),
                                file: take.file.clone(),
                                start: anchor,
                                end: open_end,
                                level: *level,
                                mode: *mode,
                                fade_out: cfg.tail_fade_s,
                                looped: take.looped,
                            });
                        }
                        InjectMode::Trail => {
                            let solo = hold_s.min(dur);
                            actives.push(InjectActive {
                                sound: sound.clone(),
                                file: take.file.clone(),
                                start: anchor,
                                end: open_end,
                                level: *level,
                                mode: *mode,
                                fade_out: cfg.tail_fade_s,
                                looped: take.looped,
                            });
                            cursor += solo;
                        }
                    }
                }
            }
        }
    }
    // Anything still open never met its `stop`. Fall back to one play, the
    // behaviour a bed had before looping existed, so a chapter that forgets the
    // stop loses the loop, not the sound.
    for a in actives.iter_mut().filter(|a| a.end.is_infinite()) {
        a.end = a.start + durs.get(&a.file).copied().unwrap_or(0.0);
        a.looped = false;
    }
    actives
        .into_iter()
        .filter(|a| a.end - a.start > 0.01)
        .map(|a| InjectEvent {
            sound: a.sound,
            file: a.file,
            start: a.start,
            looped: a.looped,
            end: a.end,
            level: a.level,
            mode: a.mode,
            fade_in: if a.mode == InjectMode::Hit { 0.0 } else { 0.05 },
            fade_out: a.fade_out,
        })
        .collect()
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

/// The edge fade every spoken slot gets: a line must not begin or end on a hard
/// sample. A tenth of a second is a click guard, not an attack.
pub const FADE_S: f64 = 0.1;

/// `sox`, the second audio engine the merge shells out to. A voice treatment
/// that names a `sox` chain needs it, exactly as the beds need ffmpeg.
fn sox(args: &[String]) -> Result<()> {
    let out = Command::new("sox")
        .args(args)
        .output()
        .context("spawning sox")?;
    if !out.status.success() {
        anyhow::bail!(
            "sox failed: {}",
            crate::util::head_chars(&String::from_utf8_lossy(&out.stderr), 300)
        );
    }
    Ok(())
}

/// The longest decay any span in this chapter asks for. Zero when nothing
/// reserves a tail, which is when the mix is exactly as long as the voice.
fn voice_reserve(spans: &[Span], presets: &BTreeMap<String, VoiceFx>) -> f64 {
    spans
        .iter()
        .filter_map(|s| s.reverb.as_ref())
        .filter_map(|r| presets.get(r))
        .map(VoiceFx::tail_s)
        .fold(0.0_f64, f64::max)
}

/// Pad `raw` out to `span_len` and put a [`FADE_S`] fade at each edge. Used for
/// a slot with no treatment, which still must not start or end on a click.
fn fade_edges(raw: &Path, out: &Path, span_len: f64) -> Result<()> {
    let af = format!(
        "apad=whole_dur={span_len:.3},atrim=0:{span_len:.3},\
         afade=t=in:st=0:d={FADE_S:.3},afade=t=out:st={:.3}:d={FADE_S:.3}",
        (span_len - FADE_S).max(0.0)
    );
    ffmpeg(&[
        "-y".into(),
        "-loglevel".into(),
        "error".into(),
        "-i".into(),
        s(raw.display()),
        "-ar".into(),
        "48000".into(),
        "-ac".into(),
        "1".into(),
        "-af".into(),
        af,
        "-c:a".into(),
        "pcm_s16le".into(),
        s(out.display()),
    ])
}

/// The whole voice track: every slot's piece treated and placed at its own
/// offset, then summed.
///
/// **Placement, not concatenation.** A concat grew the track by every reserved
/// tail and slid the speech against the beds; placing each piece where the
/// script put it keeps the turn fixed, with the decay ringing under the next
/// line.
fn build_voice_track(
    voice_wav: &Path,
    slots: &[Slot],
    spans: &[Span],
    presets: &BTreeMap<String, VoiceFx>,
    total: f64,
    work: &Path,
) -> Result<PathBuf> {
    let mut pieces: Vec<(PathBuf, f64)> = Vec::new();
    for (n, slot) in slots.iter().enumerate() {
        let len = (slot.end - slot.start).max(0.0);
        if len <= 0.0 {
            continue;
        }
        // Seek BEFORE the input: `-ss` as an input option seeks (PCM is
        // sample-accurate for this), so each piece decodes only its own span.
        let raw = work.join(format!("v{n}.raw.wav"));
        ffmpeg(&[
            "-y".into(),
            "-loglevel".into(),
            "error".into(),
            "-ss".into(),
            format!("{:.3}", slot.start),
            "-t".into(),
            format!("{len:.3}"),
            "-i".into(),
            s(voice_wav.display()),
            "-ar".into(),
            "48000".into(),
            "-ac".into(),
            "1".into(),
            "-c:a".into(),
            "pcm_s16le".into(),
            s(raw.display()),
        ])?;
        let fx = slot_effect(slot, spans, presets);
        let tail = fx.map(|(f, _)| f.tail_s()).unwrap_or(0.0);
        let span_len = len + tail;
        let p = work.join(format!("v{n}.wav"));
        match fx {
            Some((f, narrator)) => {
                let depth = if narrator { NARRATOR_DEPTH } else { 1.0 };
                apply_voice_fx(f, depth, &raw, &p, span_len, work)?;
            }
            None => fade_edges(&raw, &p, span_len)?,
        }
        pieces.push((p, slot.start));
    }
    let voice_fx = work.join("voice_fx.wav");
    place_voice(&pieces, &voice_fx, total)?;
    Ok(voice_fx)
}

/// Run one slot's treatment: the effect, the reserved tail, the edge fades, and
/// (for the Narrator) a tenth of the depth by blending back toward dry.
///
/// `depth` is 1.0 for a character and [`NARRATOR_DEPTH`] for the Narrator: a
/// blend against the dry piece, because "in the room but not standing in it"
/// is a mix of two signals rather than a knob the effect has.
fn apply_voice_fx(
    fx: &VoiceFx,
    depth: f64,
    raw: &Path,
    out: &Path,
    span_len: f64,
    work: &Path,
) -> Result<()> {
    if depth <= 0.0 {
        return fade_edges(raw, out, span_len);
    }
    let stem = out
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("v")
        .to_string();
    let (engine, chain) = fx.engine_and_chain();
    let chain = chain.trim();
    let processed = work.join(format!("{stem}.wet.wav"));
    match engine {
        FxEngine::Ffmpeg => {
            let mut af: Vec<String> = Vec::new();
            if !chain.is_empty() {
                af.push(chain.to_string());
            }
            af.push(format!("apad=whole_dur={span_len:.3}"));
            af.push(format!("atrim=0:{span_len:.3}"));
            af.push(format!("afade=t=in:st=0:d={FADE_S:.3}"));
            af.push(format!(
                "afade=t=out:st={:.3}:d={FADE_S:.3}",
                (span_len - FADE_S).max(0.0)
            ));
            ffmpeg(&[
                "-y".into(),
                "-loglevel".into(),
                "error".into(),
                "-i".into(),
                s(raw.display()),
                "-ar".into(),
                "48000".into(),
                "-ac".into(),
                "1".into(),
                "-af".into(),
                af.join(","),
                "-c:a".into(),
                "pcm_s16le".into(),
                s(processed.display()),
            ])?;
        }
        FxEngine::Sox => {
            // SoX's `reverb` never extends its own output, so the room has to
            // ring into silence that already exists: pad the reserved tail on
            // *first*, run the chain, then land the edges on the result.
            let padded = work.join(format!("{stem}.pad.wav"));
            ffmpeg(&[
                "-y".into(),
                "-loglevel".into(),
                "error".into(),
                "-i".into(),
                s(raw.display()),
                "-ar".into(),
                "48000".into(),
                "-ac".into(),
                "1".into(),
                "-af".into(),
                format!("apad=whole_dur={span_len:.3}"),
                "-c:a".into(),
                "pcm_s16le".into(),
                s(padded.display()),
            ])?;
            let wet = work.join(format!("{stem}.sox.wav"));
            if chain.is_empty() {
                std::fs::copy(&padded, &wet)?;
            } else {
                let mut args = vec!["-q".to_string(), s(padded.display()), s(wet.display())];
                args.extend(chain.split_whitespace().map(str::to_string));
                sox(&args)?;
            }
            let af = format!(
                "atrim=0:{span_len:.3},afade=t=in:st=0:d={FADE_S:.3},\
                 afade=t=out:st={:.3}:d={FADE_S:.3}",
                (span_len - FADE_S).max(0.0)
            );
            ffmpeg(&[
                "-y".into(),
                "-loglevel".into(),
                "error".into(),
                "-i".into(),
                s(wet.display()),
                "-ar".into(),
                "48000".into(),
                "-ac".into(),
                "1".into(),
                "-af".into(),
                af,
                "-c:a".into(),
                "pcm_s16le".into(),
                s(processed.display()),
            ])?;
        }
    }
    if depth >= 1.0 {
        std::fs::rename(&processed, out)?;
        return Ok(());
    }
    // The dry leg is the un-treated piece: it is mixed back in for the
    // Narrator, so it needs the same edge fades the wet piece got, or the
    // blend would put the click back at full amplitude over a faded tail.
    let dry_len = (span_len - fx.tail_s()).max(0.0);
    let dry_fade_out = (dry_len - FADE_S).max(0.0);
    ffmpeg(&[
        "-y".into(),
        "-loglevel".into(),
        "error".into(),
        "-i".into(),
        s(raw.display()),
        "-i".into(),
        s(processed.display()),
        "-filter_complex".into(),
        format!(
            "[0:a]afade=t=in:st=0:d={FADE_S:.3},\
             afade=t=out:st={dry_fade_out:.3}:d={FADE_S:.3},\
             volume={:.4}[d];[1:a]volume={depth:.4}[w];\
             [d][w]amix=inputs=2:normalize=0,atrim=0:{span_len:.3}[m]",
            1.0 - depth
        ),
        "-map".into(),
        "[m]".into(),
        "-ar".into(),
        "48000".into(),
        "-ac".into(),
        "1".into(),
        "-c:a".into(),
        "pcm_s16le".into(),
        s(out.display()),
    ])
}

/// Sum the slot pieces at their absolute offsets, then pad the whole track out
/// to `total` (the voice plus the reserved tail).
fn place_voice(pieces: &[(PathBuf, f64)], out: &Path, total: f64) -> Result<()> {
    if pieces.is_empty() {
        return ffmpeg(&[
            "-y".into(),
            "-loglevel".into(),
            "error".into(),
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            "anullsrc=r=48000:cl=mono".into(),
            "-t".into(),
            format!("{total:.3}"),
            "-c:a".into(),
            "pcm_s16le".into(),
            s(out.display()),
        ]);
    }
    let mut args: Vec<String> = vec!["-y".into(), "-loglevel".into(), "error".into()];
    for (p, _) in pieces {
        args.push("-i".into());
        args.push(s(p.display()));
    }
    let mut graph = String::new();
    for (i, (_, start)) in pieces.iter().enumerate() {
        let ms = (start * 1000.0).round().max(0.0) as i64;
        graph.push_str(&format!("[{i}:a]adelay={ms}[p{i}];"));
    }
    let labels: String = (0..pieces.len()).map(|i| format!("[p{i}]")).collect();
    if pieces.len() == 1 {
        graph.push_str(&format!(
            "{labels}apad=whole_dur={total:.3},atrim=0:{total:.3}[v]"
        ));
    } else {
        graph.push_str(&format!(
            "{labels}amix=inputs={}:normalize=0,apad=whole_dur={total:.3},atrim=0:{total:.3}[v]",
            pieces.len()
        ));
    }
    args.push("-filter_complex".into());
    args.push(graph);
    args.push("-map".into());
    args.push("[v]".into());
    args.push("-ar".into());
    args.push("48000".into());
    args.push("-ac".into());
    args.push("1".into());
    args.push("-c:a".into());
    args.push("pcm_s16le".into());
    args.push(s(out.display()));
    ffmpeg(&args)
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
/// middle of a two-second overlap, cheaper than a filter chain that would have
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
/// ramps. The alternative, slicing the track at each level change and
/// concatenating, restarts the loop at every boundary, which is audible; and
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
/// directory the scene map came from, one root, so a pool and its clips cannot
/// be read from different places.
fn clip_path(assets: &Path, file: &str) -> PathBuf {
    assets.join(file)
}

/// Effective start offset per music run, so adjacent tracks crossfade.
///
/// The planner covers only speech (`run.end` is the slot's end), while the
/// renderer extends each run by `xfade_s`, leaving the two fades misaligned
/// by the inter-slot gap and dipping the mix mid-transition. Starting the
/// next track where the previous one's audible coverage ended aligns them.
/// Gaps wider than the crossfade plus a beat are intentional silence (`none`,
/// unpooled moods) and keep their hole.
///
/// The first run is passed through: [`plan_music`] already moved it to the
/// head of the timeline, so the chapter opens on it rather than on the first
/// line that names a mood.
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

/// The two edge fades of one music run: `(fade_in, fade_out)`.
///
/// The chapter's first slice rises and its last falls over `fade_s` (3 s). An
/// opening and a closing are *heard*, and a third of a second of either reads
/// as a cut, the one thing the layer's first and last moments must not read
/// as. Everywhere the track changes mid-chapter the edge is the short
/// `xfade_s` instead, because that seam is covered by the next track arriving.
fn music_fades(n: usize, last: bool, cfg: &MusicLayer) -> (f64, f64) {
    (
        if n == 0 { cfg.fade_s } else { cfg.xfade_s },
        if last { cfg.fade_s } else { cfg.xfade_s },
    )
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
/// throwaway path, the merge passes its per-chapter scratch directory.
/// The voice treatment for one slot, and whether the slot is the Narrator —
/// who takes the same room at a tenth of its depth, never as a full wet.
fn slot_effect<'a>(
    slot: &Slot,
    spans: &[Span],
    presets: &'a BTreeMap<String, VoiceFx>,
) -> Option<(&'a VoiceFx, bool)> {
    let span = spans
        .iter()
        .find(|s| slot.start >= s.start && slot.start < s.end)?;
    let fx = span.reverb.as_ref().and_then(|r| presets.get(r))?;
    Some((fx, slot.speaker == "Narrator"))
}

#[allow(clippy::too_many_arguments)]
pub fn apply_layers(
    voice_wav: &Path,
    slots: &[Slot],
    chapter: u32,
    on: LayerSwitch,
    out: &Path,
    work: &Path,
    assets: &Path,
    inj_durs: &BTreeMap<String, f64>,
) -> Result<PathBuf> {
    let mut cfg = load_map(&assets.join("scene-map.json"))?;
    cfg.layers.effect.trim *= on.effect_volume.max(0.0);
    cfg.layers.music.level *= on.music_volume.max(0.0);
    cfg.layers.music.pause_level *= on.music_volume.max(0.0);
    cfg.layers.inject.level *= on.inject_volume.max(0.0);
    let effect_pool = audio_pool::load_pool(&assets.join("effect-pool.json"));
    let music_pool = audio_pool::load_pool(&assets.join("music-pool.json"));
    let spans = build_spans(slots, &cfg);
    let pauses = pause_intervals(slots);

    // 1. the voice track: one treatment per slot, a short edge fade on every
    //    line, and a reserved decay so a reverb rings out. Not a layer: it is
    //    applied to the voice itself, before anything is mixed under it. It
    //    rides the effect switch because it is that layer's scene treatment —
    //    effects off is a plain read, not a plain read in a cave.
    let work = work.join("layers");
    std::fs::create_dir_all(&work)?;
    // Header probe: the mix WAV is the biggest file in the merge, and the
    // layers need only its length.
    let voice_total = wav_seconds(voice_wav)?;
    // Room for the longest decay any slot asks for, so a tail at the end of the
    // chapter rings out instead of being cut by the mix edge. Only when the
    // treatment runs: effects off is a plain read, not a read plus a silence.
    let reserve = if on.effects {
        voice_reserve(&spans, &cfg.reverb_presets)
    } else {
        0.0
    };
    let total = voice_total + reserve;
    let voice_fx = if on.effects {
        build_voice_track(voice_wav, slots, &spans, &cfg.reverb_presets, total, &work)?
    } else {
        voice_wav.to_path_buf()
    };

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
        // Three rungs, multiplied: the rule's balance against other scenes, the
        // layer's own trim, and this sound's trim. A sound with no `level` is
        // 1.0, so a registry that predates the field mixes byte for byte as it
        // did, which `the_shipped_registries_are_all_at_unity_today` keeps
        // honest.
        let vol = w.level * clip.level;
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
                vol
            )
        } else {
            format!(
                "volume={:.4},areverse,afade=t=in:st=0:d=0.4,areverse,\
                 aformat=sample_rates=48000:channel_layouts=mono",
                vol
            )
        };
        args.push("-af".into());
        args.push(af);
        args.push(s(p.display()));
        ffmpeg(&args)?;
        // A window that reaches the end of the chapter is the last thing this
        // layer says, so a looped bed closes over `end_fade_s` instead of being
        // cut on the mix's edge. Everywhere else the edge is the short
        // `fade_s`: there is speech after it, and the next scene's bed may
        // follow.
        let closing = w.end >= total - 0.05;
        fx_slices.push(Slice {
            path: p,
            start: w.start,
            dur,
            fade_in: cfg.layers.effect.fade_s,
            fade_out: match (clip.looped, closing) {
                (true, true) => cfg.layers.effect.end_fade_s,
                (true, false) => cfg.layers.effect.fade_s,
                (false, _) => 0.0,
            },
        });
        fx_log.push(FxReport {
            span: w.span,
            start: w.start,
            end: w.end,
            // The sound's name, not the take's: `day`, never `day-2`. The log
            // is read by a human asking which sound answered.
            name: clip.sound.clone(),
            // What was actually applied, not the rule's share of it: the report
            // is read against the audio that came out.
            level: vol,
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
    // How long each take is, so a run longer than its track can be rendered as
    // a crossfaded loop rather than a butt-jointed repeat. Probed once for the
    // chapter, not once per run: the same track usually runs twice, and a
    // second ffprobe is a second answer to a question already asked.
    let mu_durs = probe_durs(
        &runs.iter().map(|r| r.file.clone()).collect::<Vec<String>>(),
        assets,
        "music",
    );
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
            // Both levels carry the run's own trim, so a track that sits quiet
            // still lifts inside a pause by the same ratio as every other.
            cfg.layers.music.level * run.level,
            cfg.layers.music.pause_level * run.level,
            cfg.layers.music.ramp_s,
            start,
            &run.pauses,
        );
        // A run longer than its track is the common case — a 2-minute bed under
        // a 20-minute chapter — and the seam is the whole point of the choice.
        // `-stream_loop -1` butt-joins the tail onto the head, and every clip
        // entering a pool is trimmed with a short fade at each end (see
        // `tools/normalize-audio.sh`), so that seam is a small hole once every
        // couple of minutes for the length of the chapter. `loop_filter` is the
        // same crossfade the inject layer already uses for its looped beds, and
        // the gain expression rides on it rather than replacing it, so a track
        // that loops still lifts inside a pause.
        //
        // Below one clip length this is `-stream_loop` exactly as before, so a
        // run that fits never moves.
        let copies = loop_copies(dur, mu_durs.get(&run.file).copied().unwrap_or(0.0), 2.0);
        match copies {
            Some(k) => {
                let graph =
                    loop_filter_with_tail(k, 2.0, &format!("volume=volume='{expr}':eval=frame"));
                ffmpeg(&[
                    "-y".into(),
                    "-loglevel".into(),
                    "error".into(),
                    "-i".into(),
                    s(src.display()),
                    "-filter_complex".into(),
                    graph,
                    "-map".into(),
                    "[out]".into(),
                    "-t".into(),
                    format!("{dur:.3}"),
                    s(p.display()),
                ])?;
            }
            None => {
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
            }
        }
        let (fade_in, fade_out) = music_fades(n, last, &cfg.layers.music);
        mu_slices.push(Slice {
            path: p,
            start,
            dur,
            fade_in,
            fade_out,
        });
    }
    let music_mix = if mu_slices.is_empty() {
        None
    } else {
        let p = work.join("music.wav");
        place(&mu_slices, &p, total)?;
        Some(p)
    };

    // 4. the inject layer: script-placed spot effects on their own track.
    //    Planned against the same delivered clock the other layers read, so a
    //    hit lands in the silence `plan_inject_holds` wrote for it and a tail
    //    runs under the speech that follows. Takes are re-picked here rather
    //    than threaded through: the pick is a pure function of the chapter,
    //    the slot and the sound, so this is the same answer, not a second one.
    let events: Vec<InjectEvent> = if on.effects {
        let pool = audio_pool::load_pool(&assets.join("inject-pool.json"));
        let takes = plan_inject_takes(slots, &pool, chapter);
        plan_injects(slots, &takes, inj_durs, &cfg.layers.inject)
    } else {
        Vec::new()
    };
    let inject_mix: Option<PathBuf> = match events.is_empty() {
        true => None,
        false => {
            let mut ij_slices: Vec<Slice> = Vec::new();
            for (n, e) in events.iter().enumerate() {
                let dur = e.end - e.start;
                if dur <= 0.01 {
                    continue;
                }
                let src = clip_path(assets, &e.file);
                if !src.is_file() {
                    eprintln!("inject: pooled clip missing ({}) -> skipped", e.file);
                    continue;
                }
                let p = work.join(format!("ij{n}.wav"));
                let vol = e.level * cfg.layers.inject.level;
                let clip_s = inj_durs.get(&e.file).copied().unwrap_or(0.0);
                // A bed the pool marks `looped` fills its window by repeating,
                // with a short crossfade at each seam. The window is only longer
                // than the clip when a `stop` ended it, with no stop there is
                // nothing to fill and `loop_copies` returns None, so the sound
                // plays once exactly as it did before.
                let copies = if e.looped {
                    loop_copies(dur, clip_s, cfg.layers.inject.loop_xfade_s)
                } else {
                    None
                };
                match copies {
                    Some(k) => {
                        let graph = loop_filter(k, cfg.layers.inject.loop_xfade_s, vol);
                        ffmpeg(&[
                            "-y".into(),
                            "-loglevel".into(),
                            "error".into(),
                            "-i".into(),
                            s(src.display()),
                            "-filter_complex".into(),
                            graph,
                            "-map".into(),
                            "[out]".into(),
                            "-t".into(),
                            format!("{dur:.3}"),
                            s(p.display()),
                        ])?;
                    }
                    None => {
                        ffmpeg(&[
                            "-y".into(),
                            "-loglevel".into(),
                            "error".into(),
                            "-i".into(),
                            s(src.display()),
                            "-t".into(),
                            format!("{dur:.3}"),
                            "-af".into(),
                            format!(
                                "volume={vol:.4},aformat=sample_rates=48000:channel_layouts=mono"
                            ),
                            s(p.display()),
                        ])?;
                    }
                }
                ij_slices.push(Slice {
                    path: p,
                    start: e.start,
                    dur,
                    fade_in: e.fade_in,
                    fade_out: e.fade_out,
                });
            }
            if ij_slices.is_empty() {
                None
            } else {
                let p = work.join("inject.wav");
                // The track is at least as long as what it carries, never
                // shorter: an event past the voice's end would otherwise be
                // delayed off the end of its own track and vanish, with the
                // plan log still naming it. The concat writes the silence this
                // normally lands in; this is the net under that.
                let end = ij_slices
                    .iter()
                    .map(|s| s.start + s.dur)
                    .fold(total, f64::max);
                place(&ij_slices, &p, end)?;
                Some(p)
            }
        }
    };

    // 5. one duck for the beds, keyed on the voice, and the inject layer
    //    mixed in *after* it.
    //
    //    The inject registry's own contract is foreground: `-20 LUFS / -3 dBTP`,
    //    "voice territory, not the -26 bed contract". It was riding the beds'
    //    ducked bus anyway, and the duck keys on the voice while a spot effect
    //    fires at the instant the voice stops, so the compressor was at full
    //    reduction with a 400 ms release exactly when the sound began. Measured
    //    on ch9: a `cooking` bed the script asked for played at **-34.8 dB**,
    //    15 dB under the speech, and was inaudible. A bed has to get out of the
    //    way of the voice; a spot effect is the thing the voice is getting out
    //    of the way *for*, and `inject_volume` is the knob for balancing it.
    let beds: Vec<&PathBuf> = [effect_mix.as_ref(), music_mix.as_ref()]
        .into_iter()
        .flatten()
        .collect();
    if beds.is_empty() && inject_mix.is_none() {
        eprintln!("sound design: no layer produced anything, skipped");
        log_plan(&spans, &pauses, &fx_log, &runs, &[], &cfg);
        // The voice track IS the mix when nothing plays under it. Promote it to
        // the caller's path before the scratch dir that holds it is removed:
        // returning the path inside `work` handed back a file this line deletes.
        if voice_fx.as_path() != out {
            std::fs::copy(&voice_fx, out)?;
        }
        cleanup(&work);
        return Ok(out.to_path_buf());
    }
    let duck = &cfg.duck;
    let sc = format!(
        "sidechaincompress=threshold={}:ratio={}:attack={}:release={}",
        duck.threshold, duck.ratio, duck.attack, duck.release
    );
    // Input 0 is the voice key; 1..=beds are the beds in order; the inject
    // track, when there is one, is the last input and never enters `[under]`.
    // The headline keeps its own level: the music is under it by design, and
    // the duck is what was hiding it there.
    let headline = headline_end(slots).map(|end| (end, duck.head_key));
    let graph = layer_graph(beds.len(), inject_mix.is_some(), &sc, headline);
    let mut args: Vec<String> = vec!["-y".into(), "-loglevel".into(), "error".into()];
    args.push("-i".into());
    args.push(s(voice_fx.display()));
    for l in beds.iter().copied().chain(inject_mix.as_ref()) {
        args.push("-i".into());
        args.push(s(l.display()));
    }
    args.push("-filter_complex".into());
    args.push(graph);
    args.push("-map".into());
    args.push("[a]".into());
    args.push(s(out.display()));
    ffmpeg(&args)?;

    log_plan(&spans, &pauses, &fx_log, &runs, &events, &cfg);
    cleanup(&work);
    Ok(out.to_path_buf())
}

/// The mix graph: the beds ducked under the voice, the inject layer on top, and
/// a true-peak limiter on the sum.
///
/// Split out and pure because it is the one part of the signal path that is
/// invisible in every artifact: a wrong bus assignment does not fail, it just
/// makes a layer quiet, so it is asserted on instead of eyeballed. `beds` is
/// how many bed tracks are inputs 1..=beds; the inject track, if present, is
/// the input right after them.
///
/// `headline` is `(end, key gain)`: the seconds at the head of the chapter
/// where the sidechain key is held down ([`Duck::head_key`]) so the beds
/// arrive with the title. The key listens to a *copy* of the voice, never the
/// voice that reaches the mix: a key that also changed what the listener hears
/// would be a level edit wearing a compressor's name.
///
/// The limiter is not decoration. Clips are normalized to a -3 dBTP ceiling
/// and the layer gains multiply on top, but nothing was enforcing the ceiling
/// on the *sum*: ch9 measured -0.11 dBFS with the inject layer off entirely.
/// `alimiter` is a lookahead limiter, so it caps the peak without a clipper's
/// distortion, and `level=0` keeps it from re-normalizing behind the
/// operator's back.
fn layer_graph(beds: usize, inject: bool, sc: &str, headline: Option<(f64, f64)>) -> String {
    let inject_in = beds + 1;
    let lim = "alimiter=limit=0.589:attack=5:release=100:level=0";
    let tail = |n: usize| format!("amix=inputs={n}:normalize=0[mixed];[mixed]{lim}[a]");
    // Nothing to duck: just sum whatever is there.
    if beds == 0 {
        return format!("[0:a][{inject_in}:a]{}", tail(2));
    }
    // The key, and the voice the mix keeps: the same stream twice, unless the
    // headline holds the key down, then the voice is split and only the copy
    // the compressor listens to is attenuated. A gain of 1.0 (or no headline)
    // is the key taken as-is, which is exactly the graph this used to emit.
    let (prologue, key, vox) = match headline {
        Some((end, gain)) if end > 0.0 && gain < 1.0 => (
            format!(
                "[0:a]asplit=2[vox][sc];\
                 [sc]volume=volume='if(lt(t,{end:.3}),{gain:.4},1)':eval=frame[key];"
            ),
            "[key]",
            "[vox]",
        ),
        _ => (String::new(), "[0:a]", "[0:a]"),
    };
    let ins: String = (1..=beds).map(|i| format!("[{i}:a]")).collect();
    let bed_bus = if beds == 1 {
        // A single bed needs no summing before the compressor.
        "[1:a]anull[under]".to_string()
    } else {
        format!("{ins}amix=inputs={beds}:normalize=0[under]")
    };
    if inject {
        format!(
            "{prologue}{bed_bus};[under]{key}{sc}[duck];\
             {vox}[duck][{inject_in}:a]{}",
            tail(3)
        )
    } else {
        format!(
            "{prologue}{bed_bus};[under]{key}{sc}[duck];{vox}[duck]{}",
            tail(2)
        )
    }
}

/// Build the plan report. The mix is otherwise invisible in the logs, and a
/// chapter that came out silent should say *why* it came out silent.
///
/// Three lists, because the three things have three different clocks. A *span*
/// is a place and carries the reverb; a *window* is an effect and may open
/// later than its span (the cooldown can push it); a *run* is a mood, and
/// `none` emits no run at all, so silence shows up as a gap between two runs'
/// ranges rather than as a line. Folding these into one span line is what hid
/// exactly the behaviours the per-window and per-slot designs exist to
/// express.
fn plan_lines(
    spans: &[Span],
    pauses: &[(f64, f64)],
    fx: &[FxReport],
    runs: &[MusicRun],
    inj: &[InjectEvent],
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
        // The level that was actually applied, the layer's, times this track's
        // own trim, so the log reads the same way the effect line above it
        // does, and a per-sound trim is visible in the one place a human looks
        // to ask what the mix did.
        out.push(format!(
            "music [{:.0}-{:.0}s] {} <- {}@{:.3}",
            r.start,
            r.end,
            r.sound,
            r.mood,
            cfg.layers.music.level * r.level
        ));
    }
    for e in inj {
        let mode = match e.mode {
            InjectMode::Hit => "hit",
            InjectMode::Overlap => "overlap",
            InjectMode::Trail => "trail",
        };
        // The take's basename, not the pool path: `blood-spatter-2`, never
        // `injects/blood-spatter-2.mp3`. The log answers "what played", and
        // the directory is not part of that answer.
        let take = e.file.rsplit('/').next().unwrap_or(&e.file);
        out.push(format!(
            "inject [{:.1}-{:.1}s] {} ({mode}, {:.1}s, take {take})",
            e.start,
            e.end,
            e.sound,
            e.end - e.start,
        ));
    }
    out
}

fn log_plan(
    spans: &[Span],
    pauses: &[(f64, f64)],
    fx: &[FxReport],
    runs: &[MusicRun],
    inj: &[InjectEvent],
    cfg: &SceneMap,
) {
    for line in plan_lines(spans, pauses, fx, runs, inj, cfg) {
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
mod tests;
