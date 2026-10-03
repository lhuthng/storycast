//! Runtime settings and LLM provider config.
//!
//! Settings live in the workspace's `settings.json` so the inductor can be
//! reconfigured from the TUI and survive restarts. LLM providers live in
//! `.bm/llm.json` (see [`LlmConfig`]) — machine-global like `machines.json`,
//! because a key is this machine's access, not a book's. The SSH key is a
//! *path*, which is config, not a secret: it lives here (per-machine in
//! `machines.json`, app-wide default below) and never with the keys.

use serde::{Deserialize, Serialize};

/// How many render takes one offer carries when the workspace does not say.
///
/// A render is scheduled per **take** (one `render:<ch>:<pos>` row each, so a
/// local edit costs one segment rather than a chapter), but a worker pays for
/// every offer: a process-to-process round trip, a heartbeat, a completion
/// report and a unit collection per take. Batching five takes into one offer
/// amortises that without changing what the ledger records — the batch is an
/// *assignment* detail, and each take still settles on its own row.
///
/// Five is a size that keeps an offer's JSON small (a take is text plus a few
/// numbers) and its lease meaningful, while being a large enough slice that
/// the per-offer overhead stops mattering — and a small enough one that a
/// worker cycles back to the scheduler quickly, where a ready merge outranks
/// its next render batch.
///
/// **The batch is also the unit of sharing.** It is the slice one box claims,
/// and takes are never pinned, so the rest of the chapter stays claimable
/// and the next worker to ask deepens this chapter instead of opening
/// another one. A smaller batch therefore does not give one box less work
/// overall — it lets more boxes work on the *same* chapter at once, which is
/// what makes the first artifact appear sooner.
pub const DEFAULT_RENDER_BATCH: u32 = 5;

/// The largest batch a workspace may ask for.
///
/// A batch is a **lease**, not a queue: the whole batch is `Assigned` to one
/// box for the render lease, so an absurd value would hold a chapter's worth of
/// work hostage on one machine for hours. The cap is what keeps a typo in
/// `settings.json` from being an outage; anything above it is clamped, not
/// refused, because a workspace that cannot run at all is a worse failure than
/// one that runs slower than it asked.
pub const MAX_RENDER_BATCH: u32 = 64;

/// ONNX intra-op threads the TTS sidecar should open its sessions with.
///
/// A property of the **box**, not of the book, and it rides the environment for
/// the same reason `bm-agent`'s `BM_TTS_*` memory guard does: a provisioned
/// worker has no `settings.json` at all. `BM_TTS_THREADS=0` (or unset) keeps the
/// sidecar's own default — **half the cores, capped at 8**, the reference's
/// choice — and a positive value is the count it opens with.
///
/// Raising it is the one lever that makes a *single* render use more of a box's
/// cores; it cannot buy parallelism on one model, because the sidecar serialises
/// inferences behind its `synth` mutex. Set it to `nproc` on a render box whose
/// cores sit idle, and leave it alone where the box also merges (a merge stops
/// the sidecar before ffmpeg, so there is no overlap to arbitrate).
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
