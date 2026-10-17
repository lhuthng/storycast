//! Re-measure the *shipped* judge question on real pairs.
//!
//!     cargo run -p bm-tts --example laya_probe -- <model_dir> gate <pairs.jsonl> [choice] [show]
//!
//! The question and the sequence layout live in `bm_tts::judge` — the same code
//! the sidecar runs. This only feeds it pairs and counts.
//!
//! Each `{stretch, gold, line}` pair is asked about one key at a time: the raw
//! line, and the line with that key's tag applied. The key the line actually
//! carries is the positive; a key it does not carry is the negative, and its
//! `no` is the whole job. `choice` selects the vendor card's neutral-key A/B
//! framing instead of the shipped `noul` claim, to compare them.

use anyhow::{anyhow, bail, Result};
use bm_tts::judge::{self, Judge};
use std::path::Path;

/// The three declared keys, and the phrase their gloss takes in a question.
const KEYS: [(&str, &str); 3] = [
    ("laugh", "laughter"),
    ("sigh", "a sigh"),
    ("throat-clear", "clearing of the throat"),
];

/// The tag each key wears in a script.
const TAGS: [(&str, &str); 3] = [
    ("[cười]", "laugh"),
    ("[thở dài]", "sigh"),
    ("[hắng giọng]", "throat-clear"),
];

fn gate(dir: &Path, pairs_path: &Path, choice: bool, show: bool) -> Result<()> {
    let mut judge = Judge::from_dir(dir)?
        .ok_or_else(|| anyhow!("no judge model in {}", dir.display()))?;
    let (mut pos_hit, mut pos_n) = (0usize, 0usize);
    let (mut neg_hit, mut neg_n) = (0usize, 0usize);
    let mut q = 0usize;
    for line in std::io::BufRead::lines(std::io::BufReader::new(std::fs::File::open(
        pairs_path,
    )?)) {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let pair: serde_json::Value = serde_json::from_str(&line)?;
        let raw = pair["line"].as_str().unwrap_or_default();
        let stretch = pair["stretch"].as_str().unwrap_or_default();
        let gold = pair["gold"].as_str().unwrap_or("none");
        // A mis-located stretch is an extraction defect, not a verdict case.
        if !KEYS.iter().any(|(k, _)| *k == gold) || !raw.contains(stretch) {
            continue;
        }
        for (tag, key) in TAGS {
            let phrase = KEYS
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, p)| *p)
                .unwrap_or(key);
            let tagged = raw.replacen(stretch, tag, 1);
            if show && q < 2 {
                println!("---- question {} ----", q + 1);
                println!(
                    "instructions:\n    {}",
                    if choice {
                        judge::question(raw, &tagged, stretch, phrase)
                    } else {
                        judge::claim(raw, &tagged, stretch, phrase)
                    }
                );
                println!("state:\n    {tagged}");
            }
            q += 1;
            let p_yes = if choice {
                judge.p_yes_choice(raw, &tagged, stretch, phrase)?
            } else {
                judge.p_yes(raw, &tagged, stretch, phrase)?
            };
            let says_yes = p_yes >= 0.5;
            let correct = says_yes == (key == gold);
            if key == gold {
                pos_n += 1;
                pos_hit += usize::from(correct);
            } else {
                neg_n += 1;
                neg_hit += usize::from(correct);
            }
            println!(
                "{gold:<14} asked {key:<13} says {:<4} {}  P(yes) {p_yes:.2}",
                if says_yes { "yes" } else { "no" },
                if correct { "ok" } else { "MISS" },
            );
        }
    }
    println!(
        "\ncorrect key  accepted: {pos_hit}/{pos_n} = {:.2}",
        pos_hit as f64 / pos_n.max(1) as f64
    );
    println!(
        "wrong key    rejected: {neg_hit}/{neg_n} = {:.2}",
        neg_hit as f64 / neg_n.max(1) as f64
    );
    println!(
        "overall {}/{} = {:.2}",
        pos_hit + neg_hit,
        pos_n + neg_n,
        (pos_hit + neg_hit) as f64 / (pos_n + neg_n).max(1) as f64
    );
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 || args[2] != "gate" {
        bail!("usage: laya_probe <model_dir> gate <pairs.jsonl> [choice] [show]");
    }
    gate(
        Path::new(&args[1]),
        Path::new(&args[3]),
        args.iter().any(|a| a == "choice"),
        args.iter().any(|a| a == "show"),
    )
}
