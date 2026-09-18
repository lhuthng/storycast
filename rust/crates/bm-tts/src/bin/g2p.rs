//! Phonemize stdin, one line per output — the reference half of the G2P parity
//! check.
//!
//!     bm-tts-g2p <sea_g2p.bin> [punc] < lines.txt > phonemes.txt
//!
//! Paired with `tools/g2p-parity.py`, which runs the same lines through the
//! Python wheel and diffs the two. The wheel is the reference until the port
//! lands; after that this binary is the only implementation and the script
//! becomes a regression check against a recorded corpus.
//!
//! Two things here are load-bearing and easy to get wrong:
//!
//! * The normalizer is constructed with **no** dictionary path. That is what
//!   `vieneu_utils.PuncNormalizer` and `sea_g2p.SEAPipeline` do, so
//!   `init_norm_dict` is never called and the normalizer runs on its built-in
//!   whitelist. Passing the dictionary here — which looks like the more
//!   careful thing to do — makes the output *differ* from the reference.
//! * `punc_norm` applies at the **normalizer** only. `SEAPipeline.run` passes
//!   it there and then calls `g2p.convert` with its default of `false`, so
//!   normalizing punctuation twice is not the same as normalizing it once.

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
