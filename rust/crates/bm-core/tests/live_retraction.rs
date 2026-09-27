//! Live check: does a real model actually use `not_speech`?
//!
//! Not part of the suite. Run by hand against a throwaway workspace copy, so
//! a real chapter goes through the real attribution prompt with the real
//! backend and the answer is inspected rather than asserted.
#![allow(clippy::print_stdout)]

use bm_core::config::Settings;
use bm_core::paths::Layout;
use serde_json::Value;
use std::path::PathBuf;

const SANDBOX: &str = "/private/tmp/claude-501/-Users-wwzz-Downloads-proxyclawd/88833871-9d1c-410a-8425-a5a54e5377ef/scratchpad/live";

/// `root` stays the repo, because that is where `prompts/` and `assets/` live.
/// `work` is the sandbox, so every write lands in a copy and the real
/// workspace is never touched.
fn layout() -> Layout {
    Layout {
        root: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            .expect("crates/bm-core -> crates -> rust -> repo root")
            .to_path_buf(),
        work: PathBuf::from(SANDBOX),
    }
}

#[tokio::test]
#[ignore = "live: costs an LLM call, run by hand"]
async fn a_real_model_uses_the_retraction() -> anyhow::Result<()> {
    let layout = layout();
    let settings: Settings =
        serde_json::from_str(&std::fs::read_to_string(layout.work.join("settings.json"))?)?;
    let bible: Value =
        serde_json::from_str(&std::fs::read_to_string(layout.data().join("bible.json"))?)?;
    let n: u32 = std::env::var("BM_CH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);

    // What the preparer decided, and what the model is about to be shown.
    let raw = std::fs::read_to_string(layout.chapter_txt(n))?;
    println!(
        "--- split report ---\n{}\n",
        bm_core::digest::preview_split(&raw)
    );

    let mut progress = |f: f32, s: String| println!("  [{:.0}%] {s}", f * 100.0);
    let outcome = bm_core::digest::analyze_chapter(
        &layout,
        n,
        &bible,
        &settings,
        &settings.analyzer,
        &mut progress,
    )
    .await?;

    println!("\n--- log ---\n{}\n", outcome.log.join("\n"));
    let speakers = outcome
        .script
        .get("speakers")
        .cloned()
        .unwrap_or(Value::Null);
    println!(
        "--- speakers ---\n{}\n",
        serde_json::to_string_pretty(&speakers)?
    );

    let segs = outcome
        .script
        .get("segments")
        .and_then(|s| s.as_array())
        .cloned()
        .unwrap_or_default();
    let voices: std::collections::BTreeMap<String, usize> = segs
        .iter()
        .filter(|s| s.get("source_id").is_some())
        .filter_map(|s| {
            s.get("speaker")
                .and_then(|v| v.as_str())
                .map(|sp| sp.to_string())
        })
        .fold(Default::default(), |mut m, sp| {
            *m.entry(sp).or_default() += 1;
            m
        });
    println!("--- voices used ---");
    for (sp, count) in &voices {
        println!("  {count:4}  {sp}");
    }

    // The one thing worth looking at by eye: the short segments that are not
    // narration, i.e. exactly the spans the retraction is meant to move.
    println!("\n--- short non-narration segments ---");
    for s in &segs {
        let (Some(t), Some(sp)) = (
            s.get("text").and_then(|v| v.as_str()),
            s.get("speaker").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        if t.chars().count() <= 24 && sp != "Narrator" {
            println!("  [{sp}] {t:?}");
        }
    }
    Ok(())
}
