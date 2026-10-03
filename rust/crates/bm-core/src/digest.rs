//! Stage 2, digest a chapter into `script-NN.json`.
//!
//! Ported from `analyze.py`. Two behavioural changes are forced by running
//! across a cluster:
//!
//! 1. The worker never writes the authoritative bible. It receives a snapshot
//!    in its task offer, uses it to build the prompt, and returns a *delta*
//!    (new characters, new aliases, who spoke) which the inductor merges as the
//!    single writer. Concurrent digests therefore cannot clobber each other.
//! 2. The snapshot is mirrored to the worker's local `data/bible.json` so the
//!    cast assigner can still read voice hints.

use crate::config::Settings;
use crate::paths::Layout;
use crate::util::{atomic_write, head_chars, squeeze_ws};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;
use window::{plan_windows, tokens, weight, Window};

mod canon;
mod llm;
mod reconcile;
mod tags;
mod window;

pub use excerpt::{build_excerpt_prompt, excerpt_chain, parse_excerpt, write_excerpt};
pub use manual::{manual_accept, manual_prompt, ManualAnswer, ManualPart, ManualPrompt};
pub(crate) use parse::is_anonymous_speaker;
#[cfg(test)]
pub(crate) use prepare::prepare_chapter;
pub use prepare::preview_split;
pub use prompts::{build_prompt, build_script_prompt};
pub(crate) use prompts::{PreparedChapter, PreparedEvent};
pub use quotes::{quote_findings, QuoteFinding};
pub use run::{analyze_chapter, digest_chapter, Complaint, Round};

pub use canon::{
    apply_merges, canon_key, canonicalize_script, merge_bible, resolve_speaker,
    scrub_ambiguous_aliases, BibleMerge,
};
pub use llm::{fetch_models, generate, parse_retry_delay, GenError};
pub use reconcile::{cast_only_folds, parse_reconcile_merges, reconcile_plan, ReconcilePlan};
pub use tags::{
    apply_tag_aliases, discard_unknown_effect_tags, retag_text, tags_of, validate_context,
    validate_effect_tags, validate_injects, validate_script, validate_title, warn_vietnamese,
    TagAliases,
};

#[cfg(test)]
mod tests;

#[cfg(test)]
mod quote_gate_tests;

#[cfg(test)]
mod repair_template_tests;

/// What a digest produces: the per-chapter script plus the bible delta.
#[derive(Debug, Clone)]
pub struct DigestOutcome {
    pub script: Value,
    /// `{new_characters, new_aliases, roster, speakers}`, merged by the inductor.
    pub delta: Value,
    pub segments: usize,
    pub log: Vec<String>,
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// bible
// ---------------------------------------------------------------------------

pub fn load_bible(path: &Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .filter(|v| v.get("characters").is_some())
        .unwrap_or_else(|| json!({"characters": []}))
}

pub fn save_bible(bible: &Value, path: &Path) -> Result<()> {
    atomic_write(path, &serde_json::to_string_pretty(bible)?)
}

/// Lean context for the prompt: identity only, no chapter baggage.
pub fn bible_context(bible: &Value) -> String {
    let lean: Vec<Value> = bible
        .get("characters")
        .and_then(|c| c.as_array())
        .map(|chars| {
            chars
                .iter()
                .map(|c| {
                    json!({
                        "name": c.get("name"),
                        "personality": c.get("personality"),
                        "voice_hint": c.get("voice_hint"),
                        "tags": c.get("tags").cloned().unwrap_or(json!([])),
                        "proper_aliases": c.get("proper_aliases"),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    serde_json::to_string(&lean).unwrap_or_else(|_| "[]".to_string())
}

pub fn write_script(layout: &Layout, n: u32, script: &Value) -> Result<()> {
    atomic_write(&layout.script(n), &serde_json::to_string_pretty(script)?)
}
/// Dump a round's raw answer when `BM_DIGEST_RAW` is set.
///
/// The digest throws the model's text away once it parses, which is right for a
/// run and useless for a post-mortem: "the analyzer placed no sounds" is a
/// symptom, and the raw is the only place the cause is visible, whether it
/// reasoned about the layer and dropped it, or never considered it at all.
///
/// `pub` because the backup digestor asks its own rounds outside the worker's
/// [`call`], and it is precisely the dry run that has no other record: a
/// `--dry-run` reported a chapter's segment count and kept nothing, so a
/// question about what the model actually said could only be answered by
/// re-spending the call.
pub fn dump_raw(layout: &Layout, round: &str, raw: &str) {
    if std::env::var("BM_DIGEST_RAW").is_err() {
        return;
    }
    let path = layout.data().join(format!(".last-{round}-raw.json"));
    let _ = atomic_write(&path, raw);
    eprintln!("{round} raw -> {}", path.display());
}

mod attribution;
mod excerpt;
mod gate;
mod json;
mod manual;
mod parse;
mod parts;
mod prepare;
mod prompts;
mod quotes;
mod run;
mod sound_fields;
mod validate;
