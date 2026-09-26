//! Run the pure-signal helpers against jobs from Python, and print the answers.
//!
//!     bm-tts-check <job.json> <out.json>
//!
//! The babble guard and the chunk joiner are the parts of the pipeline that read
//! *audio* rather than tensors, so they cannot be checked by comparing codes. This
//! exists so `tools/audio-utils-parity.py` can hand both implementations the same
//! waveforms and diff the answers.
//!
//! A JSON contract rather than flags because each job is a list: a hundred
//! waveforms in, a hundred answers out, one process, one model-free run.

use anyhow::{bail, Context, Result};
use bm_tts::babble;
use bm_tts::synth;
use bm_tts::text::{split_sentences, FrontEnd};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
struct Job {
    #[serde(default)]
    bursts: Vec<WavJob>,
    #[serde(default)]
    edge: Vec<WavJob>,
    #[serde(default)]
    babble: Vec<BabbleJob>,
    #[serde(default)]
    pad: Vec<PadJob>,
    #[serde(default)]
    join: Vec<JoinJob>,
    #[serde(default)]
    sentences: Vec<String>,
    #[serde(default)]
    text: Vec<TextJob>,
}

#[derive(Deserialize)]
struct TextJob {
    text: String,
    max_chars: usize,
    min_chunk_chars: usize,
}

#[derive(Deserialize)]
struct WavJob {
    wav: String,
    sr: usize,
}

#[derive(Deserialize)]
struct BabbleJob {
    wav: String,
    sr: usize,
    phonemes: String,
    cap: usize,
    frames: usize,
}

#[derive(Deserialize)]
struct PadJob {
    prev: String,
    next: String,
    sr: usize,
    pause: f64,
}

#[derive(Deserialize)]
struct JoinJob {
    chunks: Vec<String>,
    pauses: Vec<f64>,
    sr: usize,
}

#[derive(Serialize)]
struct Out {
    bursts: Vec<usize>,
    edge: Vec<(usize, usize)>,
    babble: Vec<(bool, usize, usize, usize)>,
    pad: Vec<usize>,
    join: Vec<usize>,
    sentences: Vec<Vec<String>>,
    text: Vec<TextOut>,
}

#[derive(Serialize)]
struct TextOut {
    chunks: Vec<String>,
    gaps: Vec<String>,
    phonemes: Vec<String>,
}

/// Raw little-endian f32, which is what the Python side writes with `.tofile()`.
fn read_wav(path: &str) -> Result<Vec<f32>> {
    bm_tts::f32le::read(std::path::Path::new(path))
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `--dict` is only needed when the job asks for text work.
    let dict = args
        .iter()
        .position(|a| a == "--dict")
        .and_then(|i| args.get(i + 1).cloned());
    let positional: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    let (Some(job), Some(out)) = (positional.first(), positional.get(1)) else {
        bail!("usage: bm-tts-check <job.json> <out.json> [--dict <sea_g2p.bin>]");
    };
    let job: Job = serde_json::from_str(&std::fs::read_to_string(job).context("reading the job")?)
        .context("parsing the job")?;
    // Only built when the job asks for text work: it loads a 60 MB dictionary.
    let front = if job.text.is_empty() {
        None
    } else {
        let d = dict.context("the job asks for text work, so --dict is required")?;
        Some(FrontEnd::new(&d)?)
    };

    let mut o = Out {
        bursts: Vec::new(),
        edge: Vec::new(),
        babble: Vec::new(),
        pad: Vec::new(),
        join: Vec::new(),
        sentences: Vec::new(),
        text: Vec::new(),
    };
    for j in &job.bursts {
        o.bursts
            .push(babble::count_speech_bursts(&read_wav(&j.wav)?, j.sr));
    }
    for j in &job.edge {
        o.edge.push(synth::edge_silence(&read_wav(&j.wav)?, j.sr));
    }
    for j in &job.babble {
        let v = babble::suspect(&read_wav(&j.wav)?, j.sr, &j.phonemes, j.cap, j.frames);
        o.babble.push((v.suspect, v.syllables, v.bursts, v.frames));
    }
    for j in &job.pad {
        o.pad.push(synth::pause_pad_samples(
            &read_wav(&j.prev)?,
            &read_wav(&j.next)?,
            j.sr,
            j.pause,
        ));
    }
    for j in &job.join {
        let chunks: Vec<Vec<f32>> = j
            .chunks
            .iter()
            .map(|p| read_wav(p))
            .collect::<Result<_>>()?;
        o.join
            .push(synth::join_with_pauses(&chunks, &j.pauses, j.sr).len());
    }

    for s in &job.sentences {
        o.sentences.push(split_sentences(s));
    }
    for j in &job.text {
        let front = front.as_ref().expect("checked above");
        let c = front.chunks(&j.text, j.max_chars, j.min_chunk_chars);
        let phonemes = c
            .chunks
            .iter()
            .map(|ch| front.phonemize_with_emotions(ch))
            .collect();
        o.text.push(TextOut {
            chunks: c.chunks,
            gaps: c.gaps,
            phonemes,
        });
    }

    std::fs::write(out, serde_json::to_string(&o)?).with_context(|| format!("writing {out}"))?;
    Ok(())
}
