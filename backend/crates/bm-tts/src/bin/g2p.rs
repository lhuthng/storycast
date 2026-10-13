//! Phonemize stdin, one line per output — the reference half of the G2P parity

use std::io::{self, BufRead, Write};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let dict = args.next().unwrap_or_else(|| {
        eprintln!("usage: bm-tts-g2p <sea_g2p.bin> [punc]");
        std::process::exit(2);
    });
    let punc = args.next().as_deref() == Some("punc");

    let engine = sea_g2p_rs::g2p::G2PEngine::new(&dict)?;
    let norm = sea_g2p_rs::lang::vi::Normalizer::new("vi", None);

    let stdin = io::stdin();
    let mut out = io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        let normalized = norm.normalize(&line, punc);
        writeln!(out, "{}", engine.phonemize(&normalized))?;
    }
    Ok(())
}
