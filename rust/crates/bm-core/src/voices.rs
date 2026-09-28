//! Voice rosters.
//!
//! These mirror the VieNeu preset store and the Gemini pools from the legacy
//! `synthesize.py`. The TTS sidecar is the authority at runtime — the inductor
//! asks it for `/policy` — but the workers need a usable fallback so a scheduled
//! render never depends on the sidecar being up just to decide who speaks.
//!
//! **The shipped roster states no preference.** There is no machine-local
//! overlay: the catalogue is the whole roster.

mod catalogue;
mod consts;
mod label;

pub use catalogue::{
    effective_engine, effective_engine_lenient, effective_offline_voices, effective_policy,
    key_for_name, name_for_key, resolve_voice_name, EngineRoster, RosterFile, RosterVoice,
    CATALOGUE_JSON,
};
pub use consts::{
    adapter_language, clones, declaration, dictionary, gemini_policy, languages, nonverbals,
    output_format, policy_for, supports_nonverbal, vieneu_policy, voices_language, EngineDecl,
    VoicePolicy, ENGINES, GEMINI, GEMINI_FEMALE, GEMINI_MALE, GEMINI_NEUTRAL, VIENEU,
    VIENEU_FEMALE, VIENEU_MALE,
};
pub use label::{enrolled_voices, offline_voices, voice_name, voices_from_labels};
