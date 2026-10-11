//! Manual live check: run the real proofread prompt for one chapter against the
//! workspace's configured analyzer and report the chapter it returned. Not run by
//! CI: it needs `BM_LIVE_ROOT` and the analyzer's key in the environment.

use bm_core::config::Settings;
use bm_core::digest::{generate, manual_accept, manual_prompt, preview_split, quote_findings, Round};
use bm_core::Layout;

/// The sidecar keeps the model's change list first, its chapter after it.
fn text_of(sidecar: &str) -> &str {
    sidecar
        .split_once("\n]\n")
        .map(|(_, t)| t)
        .unwrap_or(sidecar)
}

fn marks(text: &str) -> usize {
    text.chars()
        .filter(|c| matches!(c, '"' | '\u{201c}' | '\u{201d}'))
        .count()
}

fn paras(text: &str) -> usize {
    text.lines().filter(|l| !l.trim().is_empty()).count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_repair_one_chapter() {
    // No analyzer needed to read a chapter that is already on disk: point this
    // at a sidecar or a chapter and it reports what the scan makes of it.
    if let Ok(path) = std::env::var("BM_LIVE_TEXT") {
        let raw = std::fs::read_to_string(&path).unwrap();
        let text = if path.contains("repaired") {
            text_of(&raw)
        } else {
            &raw
        };
        println!("marks {} paragraphs {}", marks(text), paras(text));
        for f in quote_findings(text) {
            let head: String = f.text.chars().take(90).collect();
            println!("  [{}] para {}: {}", f.kind, f.paragraph, head.replace('\n', "|"));
        }
        println!("{}", preview_split(text));
        return;
    }
    let Ok(root) = std::env::var("BM_LIVE_ROOT") else {
        return;
    };
    let n: u32 = std::env::var("BM_LIVE_CH")
        .ok()
        .and_then(|c| c.parse().ok())
        .unwrap_or(1);
    // The checkout, so the active workspace and the shipped adapters both resolve
    // the way the worker resolves them, bound to the task's own adapter.
    let layout = Layout::resolve(std::path::Path::new(&root)).unwrap();
    let settings = Settings::load(&layout.settings());
    let layout = layout.rebind("vi-VN", &settings.engine);
    let original = std::fs::read_to_string(layout.chapter_txt(n)).unwrap();

    println!("=== ch{n} as crawled");
    println!("findings: {}", quote_findings(&original).len());
    println!("marks {} paragraphs {}", marks(&original), paras(&original));
    println!("{}", preview_split(&original));

    let step = manual_prompt(&layout, &settings.engine, n, None).expect("a first round");
    assert_eq!(step.round, Round::Repair, "the proofread must open the chapter");
    println!("=== prompt 1 is {:?}, {} chars", step.round, step.text.len());

    let (raw, backend) = generate(&step.text, &settings.analyzer, &settings)
        .await
        .expect("the analyzer answered");
    println!("backend {backend:?}, answer {} chars", raw.len());
    std::fs::write(
        std::env::temp_dir().join(format!("bm-live-answer-{n}.json")),
        &raw,
    )
    .unwrap();

    match manual_accept(&layout, &settings.engine, n, Round::Repair, &raw, None) {
        Ok(_) => {
            let sidecar =
                std::fs::read_to_string(layout.data().join(format!("ch{n:02}-repaired.txt")))
                    .expect("the accepted proofread lands in the sidecar");
            let fixed = text_of(&sidecar);
            println!("=== ACCEPTED — the chapter the digest will read");
            println!(
                "marks {} -> {}, paragraphs {} -> {}, findings after {}",
                marks(&original),
                marks(fixed),
                paras(&original),
                paras(fixed),
                quote_findings(fixed).len()
            );
            println!("{}", preview_split(fixed));
        }
        Err(e) => {
            println!("=== REFUSED: {e:#}");
            let head: String = raw.chars().take(400).collect();
            println!("raw head: {head}");
        }
    }
}
