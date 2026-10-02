// Runs the *shipped* sample crawler over a real book through the real Lua
// engine, and prints the chapters it hands back, so a human can read the
// result instead of a test's name.
//
//   BM_BOOK=tmp/epub/some.epub cargo test -p bm-core --test zz_real_book_dump -- --nocapture
use bm_core::crawl::{CrawlOutcome, Provider};
use std::path::PathBuf;

#[test]
fn crawl_a_real_book() {
    let Some(p) = std::env::var_os("BM_BOOK") else { return };
    let book = PathBuf::from(p);
    // The book is named relative to the workspace root, which is the root the
    // crawl is confined to — the same arrangement an operator gets.
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .canonicalize()
        .expect("the workspace root");
    let book = book.canonicalize().expect("the book");
    let named = book
        .strip_prefix(&root)
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|_| {
            println!("note: {} is not under {}, naming it absolutely", book.display(), root.display());
            book.clone()
        });

    let mut spec = bm_proto::CrawlSpec {
        engine: "lua".into(),
        script: "epub.lua".into(),
        source: std::fs::read_to_string(format!(
            "{}/../../../crawlers/examples/epub.lua",
            env!("CARGO_MANIFEST_DIR")
        ))
        .expect("crawlers/examples/epub.lua"),
        params: serde_json::Map::new(),
        ..Default::default()
    };
    spec.read_root = root.clone();
    spec.params
        .insert("epub".into(), serde_json::json!(named.to_string_lossy()));
    let provider = Provider::new(&spec);

    println!("\n\x1b[1m{}\x1b[0m\n", book.display());
    match provider.discover(1, 40).unwrap() {
        None => println!("discover found nothing\n"),
        Some(found) => {
            println!(
                "discover: total = {:?}, {} chapters queued in the range 1..40\n",
                found.total,
                found.chapters.len()
            );
        }
    }

    for n in 1..=3u32 {
        println!("\n\x1b[1m========== chapter {n} ==========\x1b[0m");
        match provider.crawl(n, None, 1).unwrap().outcome {
            CrawlOutcome::Text { text, url, .. } => {
                let paras: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
                println!(
                    "{} paragraphs, {} chars, from {:?}",
                    paras.len(),
                    text.chars().count(),
                    url
                );
                // What the digest's own preparer makes of it — the split report
                // `analyze_chapter` prints before it spends an LLM call, so the
                // segment count is visible without a model.
                println!("{}", bm_core::digest::preview_split(&text));
                for p in paras.iter().take(8) {
                    println!("  {}", &p[..p.chars().take(150).count()]);
                }
                if paras.len() > 8 {
                    println!("  \x1b[2m… {} more paragraphs\x1b[0m", paras.len() - 8);
                }
                println!();
            }
            CrawlOutcome::Absent { reason } => println!("absent: {reason}\n"),
            other => println!("{other:?}\n"),
        }
    }
}
