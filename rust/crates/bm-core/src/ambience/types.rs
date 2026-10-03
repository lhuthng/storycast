use super::*;

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
