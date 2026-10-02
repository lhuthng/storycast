//! Render text to wav, the way the server will.
//!
//!     bm-tts-render <models-dir> [--codec <dir>] [--dict <bin>] --voices <json>
//!                   --voice <name> [--temp 0] [--seed N] [--wav <prefix>]
//!                   [--raw <prefix>] < text.txt
//!
//! One output per input line: the chunk count, the total samples, and — with
//! `--raw` — the waveform as raw f32 for `tools/render-parity.py` to diff against
//! the reference engine's own output for the same text and voice.
//!
//! **The engine is decided by the models directory, not by this binary.** A
//! `bundle.json` there is the pocket bundle's contract, which is exactly the
//! rule `bm-tts` itself uses to pick a backend — so one command renders either
//! tree, and the flags an engine does not use (`--codec`, `--dict`) are
//! optional rather than required. This used to hardcode VieNeu and refuse a
//! pocket bundle outright, which made a pocket box impossible to render or
//! diagnose from a terminal: the one tool meant to say "is this text audible?"
//! could only answer for half the engines in the product.
//!
//! This is the whole pipeline in one place: text → sentences → chunks → phonemes
//! → codes → audio → joined with the pauses each boundary asks for. For pocket
//! the phoneme stage does not exist — it tokenizes subwords and carries a voice
//! state — so its line reports samples rather than chunks, which is the one
//! number both engines share.

use anyhow::{Context, Result};
use bm_tts::codec::to_wav_bytes;
use bm_tts::engine::Request;
use bm_tts::sample::{Rng, Sampling};
use bm_tts::synth::{gaps_to_silence, join_with_pauses, Synth, SAMPLE_RATE};
use bm_tts::text::FrontEnd;
use bm_tts::voice::Roster;
use std::io::BufRead;

/// Where a render's files go. One argument rather than two, so `emit` stays
/// under the arity a reader holds in their head.
struct Out<'a> {
    wav: &'a Option<String>,
    raw: &'a Option<String>,
}

/// Write one render's artefacts and print its line, in the shape every engine's
/// caller — including `tools/render-parity.py` — already parses.
fn emit(
    n: usize,
    pcm: &[f32],
    rate: u32,
    chunks: Option<usize>,
    gaps: Option<Vec<String>>,
    started: std::time::Instant,
    out: Out<'_>,
) -> Result<()> {
    // Independent on purpose: the raw f32 is what the parity check diffs, and
    // requiring `--wav` as well made a run that asked only for raw silently
    // write nothing.
    if let Some(prefix) = out.wav {
        std::fs::write(
            format!("{prefix}.{n}.wav"),
            to_wav_bytes(pcm, rate),
        )?;
    }
    if let Some(prefix) = out.raw {
        let mut b = Vec::with_capacity(pcm.len() * 4);
        for v in pcm {
            b.extend_from_slice(&v.to_le_bytes());
        }
        std::fs::write(format!("{prefix}.{n}.f32"), b)?;
    }
    let mut line = serde_json::Map::new();
    if let Some(c) = chunks {
        line.insert("chunks".into(), serde_json::json!(c));
    }
    if let Some(g) = gaps {
        line.insert("gaps".into(), serde_json::json!(g));
    }
    line.insert("samples".into(), serde_json::json!(pcm.len()));
    line.insert("seconds".into(), serde_json::json!(pcm.len() as f64 / rate as f64));
    println!("{}", serde_json::Value::Object(line));
    eprintln!(
        "  line {n}: {} samples ({:.2}s) in {:.2?}",
        pcm.len(),
        pcm.len() as f64 / rate as f64,
        started.elapsed()
    );
    Ok(())
}

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
        models.context("usage: bm-tts-render <models-dir> --voices <json> [--voice NAME]")?;
    let voices = voices.context("--voices is required")?;

    // The inputs, read before the model loads: a run asked for zero texts
    // should not pay two seconds of weight loading to say so.
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
    let inputs: Vec<String> = inputs
        .into_iter()
        .filter(|t| !t.trim().is_empty())
        .collect();
    if inputs.is_empty() {
        anyhow::bail!("no input texts — pipe one per line or pass --texts FILE");
    }

    // **Same rule as `bm-tts` itself:** a `pocket.safetensors` checkpoint in
    // the models directory is what makes it a Pocket TTS tree. Deciding it here
    // rather than with a flag is what makes one command render either tree —
    // and it is what stops the two binaries from disagreeing about which engine
    // a directory is.
    let is_pocket = std::path::Path::new(&models).join("pocket.safetensors").is_file();
    #[cfg(feature = "pocket")]
    if is_pocket {
        return render_pocket(
            &models,
            &voices,
            voice.as_deref(),
            temp,
            seed,
            threads,
            &inputs,
            &wav,
            &raw,
        );
    }
    if is_pocket {
        anyhow::bail!(
            "{models} is a Pocket TTS tree, but this bm-tts-render was built \
             without it — rebuild with `--features pocket`"
        );
    }

    let codec = codec.context("--codec is required for the vieneu engine")?;
    let dict = dict.context("--dict is required for the vieneu engine")?;

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

    let mut n = 0usize;
    for text in inputs {
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
        emit(
            n,
            &final_wav,
            SAMPLE_RATE as u32,
            Some(chunks.chunks.len()),
            Some(chunks.gaps),
            started,
            Out {
                wav: &wav,
                raw: &raw,
            },
        )?;
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

/// The pocket bundle's path: one `generate` per text, no phoneme stage.
///
/// **No `--dict`, no `--codec`, no chunk loop** — a pocket bundle carries its
/// codec and its tokenizer inside the models directory and has no lexicon,
/// which is exactly why `bm-tts` makes those two flags optional. The chunk
/// count is absent from the emitted line for the same reason: pocket splits
/// and re-joins internally, and a number the caller cannot act on is worse
/// than no number.
#[cfg(feature = "pocket")]
#[allow(clippy::too_many_arguments)]
fn render_pocket(
    models: &str,
    voices: &str,
    voice: Option<&str>,
    temp: f64,
    seed: u64,
    threads: usize,
    inputs: &[String],
    wav: &Option<String>,
    raw: &Option<String>,
) -> Result<()> {
    let started_load = std::time::Instant::now();
    let mut engine = bm_tts::pocket::Pocket::load(
        std::path::Path::new(models),
        std::path::Path::new(voices),
        threads,
    )?;
    eprintln!(
        "engine: pocket · {} preset voices (default {:?}) loaded in {:.1?}",
        engine.voices.len(),
        engine.default_voice.clone().unwrap_or_else(|| "?".into()),
        started_load.elapsed()
    );
    // Resolved once and the name cloned: `generate` needs `&mut self` for its
    // ONNX sessions, so holding the `&PocketVoice` it hands back would borrow
    // the engine for the whole loop.
    let chosen = engine.resolve(voice)?.name.clone();
    eprintln!("voice: {chosen}");

    for (n, text) in inputs.iter().enumerate() {
        let started = std::time::Instant::now();
        let (pcm, rate) = engine.generate(text, &chosen, temp, seed)?;
        emit(
            n,
            &pcm,
            rate as u32,
            None,
            None,
            started,
            Out { wav, raw },
        )?;
    }
    Ok(())
}
