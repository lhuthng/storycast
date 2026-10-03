//! Voice rosters.

mod catalogue;
mod consts;
mod label;

pub use catalogue::{
    effective_engine, effective_engine_lenient, effective_offline_voices, effective_policy,
    key_for_name, name_for_key, resolve_voice_name, EngineRoster, RosterFile, RosterVoice,
    CATALOGUE_JSON,
};
pub use consts::{
    adapter_language, clamp_temperature, clones, declaration, dictionary, gemini_policy, languages,
    max_temperature, nonverbals, output_format, pocket_policy, policy_for, store_kind,
    supports_nonverbal, tts_features, vieneu_policy, voices_language, EngineDecl, VoicePolicy,
    VoiceStore, ENGINES, GEMINI, GEMINI_FEMALE, GEMINI_MALE, GEMINI_NEUTRAL, POCKET, VIENEU,
    VIENEU_FEMALE, VIENEU_MALE,
};
pub use label::{enrolled_voices, offline_voices, voice_name, voices_from_labels};
