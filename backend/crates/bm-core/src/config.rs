//! Runtime settings and LLM provider config.

use serde::{Deserialize, Serialize};

/// How many render takes one offer carries when the workspace does not say.
pub const DEFAULT_RENDER_BATCH: u32 = 5;

/// The largest batch a workspace may ask for.
pub const MAX_RENDER_BATCH: u32 = 64;

/// ONNX intra-op threads the TTS sidecar should open its sessions with.
pub fn tts_threads() -> usize {
    std::env::var("BM_TTS_THREADS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0)
}

mod crawl;
mod llm;
pub use crawl::CrawlSettings;
pub use llm::{LlmConfig, LlmKind, ProviderEntry, ResolvedLlm};
pub use settings::{
    DigestSettings, Settings, SshDefaults, DEFAULT_ANSWER_TOKENS, DEFAULT_GEMINI_URL,
};
mod settings;

#[cfg(test)]
mod tests;
