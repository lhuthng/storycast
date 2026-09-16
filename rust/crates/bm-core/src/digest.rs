//! Stage 2 — digest a chapter into `script-NN.json`.
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
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;

mod canon;
mod llm;
mod reconcile;
mod tags;

pub use canon::{
    apply_merges, canon_key, canonicalize_script, merge_bible, resolve_speaker, BibleMerge,
};
pub use llm::{generate, parse_retry_delay, GenError};
pub use reconcile::{cast_only_folds, parse_reconcile_merges, reconcile_plan, ReconcilePlan};
pub use tags::{retag_text, tags_of, validate, warn_vietnamese};

/// What a digest produces: the per-chapter script plus the bible delta.
#[derive(Debug, Clone)]
pub struct DigestOutcome {
    pub script: Value,
    /// `{new_characters, new_aliases, roster, speakers}` — merged by the inductor.
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

fn strip_fences(raw: &str) -> &str {
    let s = raw.trim();
    let s = s.strip_prefix("```json").unwrap_or(s);
    let s = s.strip_suffix("```").unwrap_or(s);
    s.trim()
}

// ---------------------------------------------------------------------------
// the stage entry point
// ---------------------------------------------------------------------------

pub fn build_prompt(layout: &Layout, bible: &Value, chapter_text: &str) -> Result<String> {
    let template = std::fs::read_to_string(layout.prompt())
        .with_context(|| format!("reading prompt template {}", layout.prompt().display()))?;
    Ok(template
        .replace("{bible_json}", &bible_context(bible))
        .replace("{chapter_text}", chapter_text))
}

/// Digest one chapter. `bible` is the inductor's snapshot; the returned delta is
/// merged by the inductor, never here.
pub async fn digest_chapter(
    layout: &Layout,
    n: u32,
    bible: &Value,
    settings: &Settings,
    analyzer: &str,
    progress: &mut (dyn FnMut(f32, String) + Send),
) -> Result<DigestOutcome> {
    let chapter_path = layout.chapter_txt(n);
    let text = std::fs::read_to_string(&chapter_path)
        .with_context(|| format!("reading {}", chapter_path.display()))?;
    let prompt = build_prompt(layout, bible, &text)?;

    progress(0.10, format!("digest ch{n} via {analyzer}"));
    let mut raw: Option<String> = None;
    let mut last_rl = String::new();
    for attempt in 0..6 {
        match generate(&prompt, analyzer, settings).await {
            Ok(t) => {
                raw = Some(t);
                break;
            }
            Err(GenError::RateLimited(msg)) => {
                let wait = parse_retry_delay(&msg)
                    .unwrap_or_else(|| (30.0 * 2f64.powi(attempt)).min(300.0));
                progress(
                    (0.10 + 0.05 * attempt as f32).min(0.30),
                    format!("rate-limited, sleeping {wait:.0}s"),
                );
                tokio::time::sleep(Duration::from_secs_f64(wait)).await;
                last_rl = msg;
            }
            Err(GenError::Fatal(e)) => return Err(e),
        }
    }
    let mut raw = raw.ok_or_else(|| {
        anyhow!("analyzer {analyzer} still rate-limited after retries: {last_rl}")
    })?;

    progress(0.60, "validating digest".to_string());
    let parsed = parse_and_validate(&raw, bible);
    let data = match parsed {
        Ok(d) => d,
        Err(e) => {
            progress(0.65, "invalid JSON, asking for one repair".to_string());
            let repair = format!(
                "{prompt}\n\nYour last output was invalid: {e}. Return ONLY the corrected JSON object."
            );
            let second = match generate(&repair, analyzer, settings).await {
                Ok(t) => t,
                Err(GenError::RateLimited(m)) => {
                    anyhow::bail!("repair attempt rate-limited: {m}")
                }
                Err(GenError::Fatal(e2)) => return Err(e2),
            };
            raw = second;
            match parse_and_validate(&raw, bible) {
                Ok(d) => d,
                Err(e2) => {
                    let dump = layout.data().join(".last-analyze-raw.json");
                    let _ = atomic_write(&dump, &raw);
                    anyhow::bail!("digest invalid ({e2}); raw saved to {}", dump.display());
                }
            }
        }
    };

    let mut log = Vec::new();
    let warnings = warn_vietnamese(&data, bible);

    // Grammar fixes must reference text that is actually in the chapter.
    let fixes = data
        .get("fixes")
        .and_then(|f| f.as_array())
        .cloned()
        .unwrap_or_default();
    for fx in &fixes {
        let before = fx.get("before").and_then(|b| b.as_str()).unwrap_or("");
        let after = fx.get("after").and_then(|a| a.as_str()).unwrap_or("");
        if before.is_empty() || after.is_empty() {
            anyhow::bail!("fix needs before+after: {fx}");
        }
        if !text.contains(before) {
            log.push(format!(
                "   WARN: fix source not found in chapter: {:?}",
                head_chars(before, 60)
            ));
        }
    }
    if !fixes.is_empty() {
        log.push(format!("   grammar fixes: {}", fixes.len()));
    }

    let segments = data
        .get("segments")
        .and_then(|s| s.as_array())
        .cloned()
        .unwrap_or_default();

    let script = json!({
        "atmosphere": data.get("atmosphere").cloned().unwrap_or(json!("")),
        "roster": data.get("roster").cloned().unwrap_or(json!([])),
        "mentions": data.get("mentions").cloned().unwrap_or(json!({})),
        "segments": segments,
        "fixes": fixes,
    });

    let script_path = layout.script(n);
    atomic_write(&script_path, &serde_json::to_string_pretty(&script)?)?;

    let delta = json!({
        "new_characters": data.get("new_characters").cloned().unwrap_or(json!([])),
        "new_aliases": data.get("new_aliases").cloned().unwrap_or(json!({})),
        "roster": data.get("roster").cloned().unwrap_or(json!([])),
        "segments": script.get("segments").cloned().unwrap_or(json!([])),
    });

    progress(1.0, format!("digest ch{n} done"));
    log.push(format!(
        "segments={} roster={} -> {}",
        script
            .get("segments")
            .and_then(|s| s.as_array())
            .map(|s| s.len())
            .unwrap_or(0),
        squeeze_ws(
            &script
                .get("roster")
                .map(|r| r.to_string())
                .unwrap_or_else(|| "[]".into())
        ),
        script_path.display()
    ));

    Ok(DigestOutcome {
        segments: script
            .get("segments")
            .and_then(|s| s.as_array())
            .map(|s| s.len())
            .unwrap_or(0),
        script,
        delta,
        log,
        warnings,
    })
}

fn parse_and_validate(raw: &str, bible: &Value) -> Result<Value> {
    let cleaned = strip_fences(raw);
    let data: Value = serde_json::from_str(cleaned).context("not valid JSON")?;
    validate(&data, bible)?;
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fences_are_stripped() {
        assert_eq!(strip_fences("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_fences("{\"a\":1}"), "{\"a\":1}");
    }

    #[test]
    fn bible_context_is_identity_only() {
        let bible = json!({"characters": [{
            "name": "A", "personality": "p", "voice_hint": "adult male",
            "proper_aliases": ["B"], "first_seen": "01", "chapters_seen": ["01"]
        }]});
        let ctx = bible_context(&bible);
        assert!(ctx.contains("\"name\":\"A\""));
        assert!(
            !ctx.contains("chapters_seen"),
            "context leaked chapter baggage: {ctx}"
        );
    }

    #[test]
    fn load_bible_defaults_when_missing_or_corrupt() {
        let missing = load_bible(Path::new("/nonexistent/bible.json"));
        assert_eq!(missing, json!({"characters": []}));
    }
}
