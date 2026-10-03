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
mod inject;
mod mix;
mod plan;
mod prompt;
mod scene;
mod timeline;
mod track;
mod types;
mod usage;

pub use inject::{
    inject_mode, inject_prompt, inject_take, injects_of, loop_copies, loop_filter,
    loop_filter_with_tail, plan_inject_holds, plan_inject_takes, plan_injects, probe_durs,
    probe_inject_durs, Inject, InjectEvent, InjectMode,
};
pub use mix::apply_layers;
pub use plan::{build_spans, plan_music, plan_windows, MusicRun, Span, Window};
pub use prompt::{effect_tags, palette_names, palette_prompt, scene_prompt};
pub use scene::{legacy_music, load_map, match_scene, resolve_music, run_music, run_scenes};
pub use timeline::{pause_intervals, plan_pauses, retime, timeline, Slot, Turn};
pub use track::FADE_S;
pub use types::{
    Duck, EffectLayer, FxEngine, InjectLayer, LayerSwitch, Layers, LegacyMusic, LegacyMusicRule,
    MusicLayer, MusicPalette, PaletteEntry, PausePlan, Rule, SceneMap, SceneRule, ThoughtRule,
    VoiceFx, VoiceFxSpec, NARRATOR_DEPTH,
};
pub use usage::{effect_usage, inject_usage, music_usage, Usage, UseOf};

#[cfg(test)]
mod tests;
