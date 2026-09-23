//! Voice rosters and the accent policy.
//!
//! These mirror the VieNeu preset store and the Gemini pools from the legacy
//! `synthesize.py`. The TTS sidecar is the authority at runtime — the inductor
//! asks it for `/policy` — but the workers need a usable fallback so a scheduled
//! render never depends on the sidecar being up just to decide who speaks.
//!
//! **The shipped roster states no preference.** There is no machine-local
//! overlay: the catalogue is the whole policy.

mod catalogue;
mod consts;
mod label;

pub use catalogue::{
    effective_engine, effective_engine_lenient, effective_offline_voices, effective_policy,
    key_for_name, name_for_key, resolve_voice_name, EnginePolicy, EngineRoster, RosterFile,
    RosterVoice, CATALOGUE_JSON,
};
pub use consts::{
    gemini_policy, policy_for, vieneu_policy, VoicePolicy, GEMINI_FEMALE, GEMINI_MALE,
    GEMINI_NEUTRAL, VIENEU_FEMALE, VIENEU_MALE,
};
pub use label::{enrolled_voices, offline_voices, policy_note, voices_from_labels};
