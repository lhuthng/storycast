//! Post-process: the three sound-design layers under the voice mix.

use crate::assemble::{wav_info, wav_seconds, Run};
use crate::audio_pool::{self, ClipPool};
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

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
