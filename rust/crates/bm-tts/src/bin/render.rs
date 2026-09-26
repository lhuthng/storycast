//! Render text to wav, the way the server will.
//!
//!     bm-tts-render <models-dir> --codec <dir> --dict <bin> --voices <json>
//!                   --voice <name> [--temp 0] [--seed N] [--wav <prefix>]
//!                   [--raw <prefix>] < text.txt
//!
//! One output per input line: the chunk count, the total samples, and — with
//! `--raw` — the waveform as raw f32 for `tools/render-parity.py` to diff against
//! the reference engine's own output for the same text and voice.
//!
//! This is the whole pipeline in one place: text → sentences → chunks → phonemes
//! → codes → audio → joined with the pauses each boundary asks for.

use anyhow::{Context, Result};
use bm_tts::codec::to_wav_bytes;
use bm_tts::engine::Request;
use bm_tts::sample::{Rng, Sampling};
use bm_tts::synth::{gaps_to_silence, join_with_pauses, Synth, SAMPLE_RATE};
use bm_tts::text::FrontEnd;
use bm_tts::voice::Roster;
use std::io::BufRead;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut models: Option<String> = None;
    let mut codec: Option<String> = None;
    let mut dict: Option<String> = None;
    let mut voices: Option<String> = None;
    let mut voice: Option<String> = None;
    let mut wav: Option<String> = None;
    let mut raw: Option<String> = None;
    let mut temp = 0.8f64;
    let mut seed = 0u64;
    let mut threads = 0usize;
    let mut texts_file: Option<String> = None;
    let mut anchor_file: Option<String> = None;
    // Print where the time went. The counters are always on; this only decides
    // whether to report them.
    let mut timing = false;

    let mut i = 0;
    while i < args.len() {
        let next = |i: &mut usize| -> Result<String> {
            *i += 1;
            args.get(*i)
                .cloned()
                .with_context(|| format!("{} needs a value", args[*i - 1]))
        };
        match args[i].as_str() {
            "--codec" => codec = Some(next(&mut i)?),
            "--dict" => dict = Some(next(&mut i)?),
            "--voices" => voices = Some(next(&mut i)?),
            "--voice" => voice = Some(next(&mut i)?),
            "--wav" => wav = Some(next(&mut i)?),
            "--raw" => raw = Some(next(&mut i)?),
            "--temp" => temp = next(&mut i)?.parse()?,
            "--seed" => seed = next(&mut i)?.parse()?,
            // Kept accepted for old command lines; sentence-level chunking is
            // now unconditional, so these packing controls no longer apply.
            "--max-chars" | "--min-chunk-chars" => {
                let _ = next(&mut i)?;
            }
            "--threads" => threads = next(&mut i)?.parse()?,
            "--texts" => texts_file = Some(next(&mut i)?),
            // Use this anchor instead of the voice's own. Diagnostic only:
            // it separates "the pipeline is wrong" from "the anchor's float
            // rounding is all that is left".
            "--anchor" => anchor_file = Some(next(&mut i)?),
            "--timing" => timing = true,
            other if other.starts_with("--") => anyhow::bail!("unknown flag {other}"),
            other => models = Some(other.to_string()),
        }
        i += 1;
    }
    let models =
        models.context("usage: bm-tts-render <models-dir> --codec … --dict … --voices …")?;
    let codec = codec.context("--codec is required")?;
    let dict = dict.context("--dict is required")?;
    let voices = voices.context("--voices is required")?;

    let front = FrontEnd::new(&dict)?;
    let roster = Roster::load(std::path::Path::new(&voices))?;
    let chosen = roster.resolve(voice.as_deref())?;
    eprintln!(
        "voice: {} ({} presets, {} reference frames)",
        chosen.name,
        roster.voices.len(),
        chosen.codes.len()
    );

    let anchor_override: Option<Vec<f32>> = match &anchor_file {
        Some(p) => Some(bm_tts::f32le::read(p)?),
        None => None,
    };

    let mut synth = Synth::load(
        std::path::Path::new(&models),
        std::path::Path::new(&codec),
        threads,
    )?;
    let mut rng = Rng::new(seed);

    // A JSON array of strings, or one per stdin line. The file form exists
    // because a text may contain newlines — a paragraph is two lines — and a
    // line-per-text protocol silently splits it into two renders.
    let inputs: Vec<String> = match &texts_file {
        Some(p) => serde_json::from_str(
            &std::fs::read_to_string(p).with_context(|| format!("reading {p}"))?,
        )
        .with_context(|| format!("parsing {p} as a JSON array of strings"))?,
        None => {
            let mut v = Vec::new();
            for line in std::io::stdin().lock().lines() {
                v.push(line?);
            }
            v
        }
    };

    let mut n = 0usize;
    for text in inputs {
        if text.trim().is_empty() {
            continue;
        }
        let started = std::time::Instant::now();
        let chunks = front.chunks_sentence_level(&text);
        let mut wavs = Vec::with_capacity(chunks.chunks.len());
        for ch in &chunks.chunks {
            let phonemes = front.phonemize_with_emotions(ch);
            let mut req = Request::new(&phonemes);
            req.sampling = Sampling {
                temperature: temp,
                ..Default::default()
            };
            req.anchor_override = anchor_override.as_deref();
            req.speaker_emb = Some(&chosen.speaker_emb);
            req.ref_codes = Some(&chosen.codes);
            let c = synth.chunk(&req, &mut rng)?;
            if c.retries > 0 {
                if let Some(v) = &c.verdict {
                    eprintln!("  {}", v.describe(c.retries, c.codes.len()));
                }
            }
            wavs.push(c.pcm);
        }
        let pauses = gaps_to_silence(&chunks.gaps);
        let final_wav = join_with_pauses(&wavs, &pauses, SAMPLE_RATE);

        // Independent on purpose: the raw f32 is what the parity check diffs, and
        // requiring `--wav` as well made a run that asked only for raw silently
        // write nothing.
        if let Some(prefix) = &wav {
            std::fs::write(
                format!("{prefix}.{n}.wav"),
                to_wav_bytes(&final_wav, SAMPLE_RATE as u32),
            )?;
        }
        if let Some(prefix) = &raw {
            let mut b = Vec::with_capacity(final_wav.len() * 4);
            for v in &final_wav {
                b.extend_from_slice(&v.to_le_bytes());
            }
            std::fs::write(format!("{prefix}.{n}.f32"), b)?;
        }
        println!(
            "{}",
            serde_json::json!({
                "chunks": chunks.chunks.len(),
                "gaps": chunks.gaps,
                "samples": final_wav.len(),
                "seconds": final_wav.len() as f64 / SAMPLE_RATE as f64,
            })
        );
        eprintln!(
            "  line {n}: {} chunks, {} samples ({:.2}s) in {:.2?}",
            chunks.chunks.len(),
            final_wav.len(),
            final_wav.len() as f64 / SAMPLE_RATE as f64,
            started.elapsed()
        );
        n += 1;
    }

    if timing {
        let total = bm_tts::stats::total_ms();
        eprintln!("timing: {n} renders");
        for row in bm_tts::stats::snapshot() {
            eprintln!(
                "  {:<9} {:>9.1} ms  {:>7} calls  {:>7.1} us/call  {:>5.1}%",
                row.name,
                row.ms(),
                row.calls,
                row.us_per_call(),
                100.0 * row.ms() / total.max(1e-9),
            );
        }
        eprintln!("  {:<9} {:>9.1} ms", "counted", total);
    }

    Ok(())
}
