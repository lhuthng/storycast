//! Voice rosters and the accent policy.
//!
//! These mirror the VieNeu preset store and the Gemini pools from the legacy
//! `synthesize.py`. The TTS sidecar is the authority at runtime — the inductor
//! asks it for `/policy` — but the workers need a usable fallback so a scheduled
//! render never depends on the sidecar being up just to decide who speaks.
//!
//! **The shipped roster states no preference.** It used to carry only the
//! Central/South presets, which made one operator's regional taste look like a
//! fact about the engine: the 13 Northern presets were excluded by *omission*,
//! which is harder to notice than an allow-list and harder to argue with. A
//! restriction is the operator's and lives in `.bm/voices.json`; see
//! `OperatorRoster`.

mod catalogue;
mod consts;
mod label;

pub use catalogue::{
    effective_engine, effective_engine_lenient, effective_offline_voices, effective_policy,
    key_for_name, name_for_key, resolve_voice_name, EnginePolicy, EngineRoster, OperatorEngine,
    OperatorPolicy, OperatorRoster, RosterFile, RosterVoice, CATALOGUE_JSON,
};
pub use consts::{
    gemini_policy, policy_for, vieneu_policy, VoicePolicy, GEMINI_FEMALE, GEMINI_MALE,
    GEMINI_NEUTRAL, VIENEU_FEMALE, VIENEU_MALE,
};
pub use label::{enrolled_voices, offline_voices, policy_note, voices_from_labels};
