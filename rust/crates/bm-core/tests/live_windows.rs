//! Live check: what a **long chapter** does through the real backend.
//!
//! Not part of the suite. The split exists because the staging answer carries
//! the chapter's own words and every backend caps one answer at 16384 tokens, so
//! a chapter three times the corpus's longest cannot be answered in one call —
//! the reply is cut mid-JSON, the parse fails, and the one repair fails the same
//! way. Two runs, both by hand:
//!
//! 1. `digest.answer_tokens = 0` — the single-call digest, i.e. the behaviour
//!    every release before the split had. Expected to fail on a long chapter, and
//!    the failure is the evidence that the split is not a preference.
//! 2. the default settings — planned into parts, staged part by part, merged
//!    into one script. Expected to land, with the plan in its log.
//!
//! The chapter is **built from three real ones** rather than committed: 27.6 KB
//! of real Vietnamese prose with real dialogue, which is the shape a novel
//! chapter has and a hand-written fixture cannot fake. `BM_LONG_SOURCES` picks
//! which chapters — three *different* ones is the harder text, since their three
//! casts meet inside one digest; one chapter repeated twice is the clean
//! measurement of the split. The sandbox, once per machine:
//!
//! ```sh
//! mkdir -p tmp/window-live/data/chapters
//! for n in 238 239 240; do
//!   ln -sf "$PWD/workspaces/beyond-myriads/data/chapters/ch$n.txt" \
//!          "tmp/window-live/data/chapters/ch$n.txt"
//! done
//! cp workspaces/beyond-myriads/data/bible.json tmp/window-live/data/bible.json
//! # and `workspaces/beyond-myriads/settings.json` as `tmp/window-live/settings.json`
//! ```
//!
//! Then, from `rust/`, with the keys the digest reads from the environment:
//!
//! ```sh
//! set -a; . ../.env; set +a
//! cargo test -p bm-core --test live_windows -- --ignored --nocapture
//! ```
//!
//! `root` is the repository, because that is where `assets/` and the adapter's
//! `prompts/` are; `work` is the sandbox, so every write lands in a copy and the
//! real workspace is never touched.
#![allow(clippy::print_stdout)]

use bm_core::config::{DigestSettings, Settings};
use bm_core::digest::analyze_chapter;
use bm_core::paths::Layout;
use serde_json::Value;
use std::path::PathBuf;

/// The chapter the built text is digested under inside the sandbox. Never a
/// chapter of the real book.
const LONG: u32 = 51;

/// Three real chapters, concatenated: 27.6 KB, twice the corpus's longest
/// (ch238, 13.6 KB). Overridable with `BM_LONG_SOURCES=238,238,238`, which is
/// worth knowing about: three *different* chapters bring three casts into one
/// digest, and a model that mis-handles the meeting of them fails on the cast
/// rather than on the length. Repeating one chapter measures the split itself,
/// with the cast held still.
const SOURCES: [u32; 3] = [238, 239, 240];

fn sources() -> Vec<u32> {
    match std::env::var("BM_LONG_SOURCES") {
        Ok(list) => list
            .split(',')
            .filter_map(|s| s.trim().parse().ok())
            .collect(),
        Err(_) => SOURCES.to_vec(),
    }
}

fn layout() -> Layout {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .expect("crates/bm-core -> crates -> rust -> repo root")
        .to_path_buf();
    Layout {
        work: root.join("tmp/window-live"),
        root,
        adapter: "vi-VN".into(),
        engine: "vieneu".into(),
    }
}

/// The long chapter, written into the sandbox from the source chapters.
fn long_chapter(layout: &Layout) -> anyhow::Result<String> {
    let mut text = String::new();
    for n in sources() {
        text.push_str(&std::fs::read_to_string(layout.chapter_txt(n))?);
        text.push('\n');
    }
    std::fs::write(layout.chapter_txt(LONG), &text)?;
    Ok(text)
}

fn inputs(layout: &Layout) -> anyhow::Result<(Settings, Value, String)> {
    let settings: Settings = serde_json::from_str(&std::fs::read_to_string(
        layout.work.join("settings.json"),
    )?)?;
    let bible = bm_core::digest::load_bible(&layout.bible());
    let text = long_chapter(layout)?;
    println!(
        "chapter: {} bytes\n{}\n",
        text.len(),
        bm_core::digest::preview_split(&text)
    );
    Ok((settings, bible, text))
}

/// The control. What one call does with a chapter this long, today.
#[tokio::test]
#[ignore = "live: costs LLM calls, run by hand"]
async fn the_single_call_path_truncates_a_long_chapter() -> anyhow::Result<()> {
    let layout = layout();
    let (mut settings, bible, _) = inputs(&layout)?;
    settings.digest = DigestSettings {
        chunk_sentences: 0,
        chunk_chars: 0,
        // The escape hatch the split ships with: exactly the digest of every
        // release before windows existed.
        answer_tokens: 0,
    };
    let mut progress = |f: f32, s: String| println!("  [{:.0}%] {s}", f * 100.0);
    let analyzer = settings.analyzer.clone();
    println!("--- single call, digest.answer_tokens = 0 ---");
    match analyze_chapter(&layout, LONG, &bible, &settings, &analyzer, &mut progress).await {
        Ok(out) => {
            println!(
                "LANDED: {} segments, {} log lines — the cap was not hit on this chapter",
                out.segments,
                out.log.len()
            );
        }
        Err(e) => println!("FAILED, and this is the point of the split:\n{e:#}"),
    }
    Ok(())
}

/// The feature. The same chapter, planned and staged in parts.
#[tokio::test]
#[ignore = "live: costs LLM calls, run by hand"]
async fn a_real_model_stages_a_long_chapter_in_parts() -> anyhow::Result<()> {
    let layout = layout();
    let (mut settings, bible, text) = inputs(&layout)?;
    settings.digest = DigestSettings::default();
    // A clean run, so the numbers are the whole chapter's rather than the tail of
    // a previous one. (A real operator *wants* the checkpoint; this is a
    // measurement.)
    let checkpoint = layout.data().join(format!(".digest-parts-ch{LONG}.json"));
    let resumed = checkpoint.exists();
    let _ = std::fs::remove_file(&checkpoint);
    println!(
        "--- split: budget {} tokens of answer, checkpoint {} ---",
        settings.digest.answer_tokens,
        if resumed { "resumed, then cleared" } else { "none" },
    );

    let mut progress = |f: f32, s: String| println!("  [{:.0}%] {s}", f * 100.0);
    let analyzer = settings.analyzer.clone();
    let out = analyze_chapter(&layout, LONG, &bible, &settings, &analyzer, &mut progress).await?;

    println!("\n--- log ---");
    for line in &out.log {
        println!("{line}");
    }
    for w in &out.warnings {
        println!("WARN: {w}");
    }
    let segments = out
        .script
        .get("segments")
        .and_then(|s| s.as_array())
        .cloned()
        .unwrap_or_default();
    println!(
        "\nLANDED: {} segments, {} sound items, chapter is {} bytes",
        segments.len(),
        segments
            .iter()
            .filter(|i| bm_core::util::is_sound_item(i))
            .count(),
        text.len()
    );
    assert!(
        out.log.iter().any(|l| l.contains("staged in")),
        "the plan should be in the log: {:?}",
        out.log
    );
    assert!(
        !checkpoint.exists(),
        "a finished chapter leaves no checkpoint behind"
    );
    Ok(())
}
