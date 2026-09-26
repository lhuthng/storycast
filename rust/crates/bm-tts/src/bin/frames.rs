//! Generate one chunk's codes, for the parity check against the Python engine.
//!
//!     bm-tts-frames <models-dir> --speaker <f32-file> [--ref <json>] [--temp 0]
//!                    [--seed N] [--no-frame-cap] [--max-frames N] < phonemes.txt
//!
//! One JSON array of frames per input line, in the order read. The speaker
//! anchor and the reference codes come from files rather than from a reference
//! wav, on purpose: enrollment (fbank → speaker encoder → codec encode) is a
//! separate path with its own verification, and letting it into this comparison
//! would mean a mismatch could be either one. Both sides are handed the same
//! bytes and only the generator is under test.

use anyhow::{bail, Context, Result};
use bm_tts::codec::Codec;
use bm_tts::engine::{Engine, Request};
use bm_tts::sample::Rng;
use bm_tts::sample::Sampling;
use std::io::{BufRead, Write};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // `--logits <f32>` is a different mode: no model, no phonemes, just the
    // sampler's filter on a fixed vector. It is the only way to reach the
    // stochastic branch, which temperature 0 short-circuits — and the draw
    // itself cannot be compared, so this reports what the filter decided
    // *before* the draw: the candidate set and its probabilities.
    if let Some(at) = args.iter().position(|a| a == "--logits") {
        let path = args.get(at + 1).context("--logits needs a path")?;
        let logits: Vec<f32> = bm_tts::f32le::read(std::path::Path::new(path))?;
        let base = Sampling {
            temperature: 1.0,
            top_k: 25,
            top_p: 0.95,
            repetition_penalty: 1.2,
        };
        let nucleus = bm_tts::sample::candidates(&logits, &base);
        let plain = bm_tts::sample::candidates(&logits, &Sampling { top_p: 1.0, ..base });
        let out = serde_json::json!({
            "probs": nucleus.probs,
            "probs_no_nucleus": plain.probs,
        });
        println!("{}", serde_json::to_string(&out)?);
        return Ok(());
    }

    let mut models: Option<String> = None;
    let mut speaker: Option<String> = None;
    let mut ref_codes: Option<String> = None;
    let mut temp = 0.0f64;
    let mut seed = 0u64;
    let mut frame_cap = true;
    let mut max_frames = 300usize;
    let mut threads = 0usize;
    let mut dump: Option<String> = None;
    // Bypass the speaker projection and use this anchor verbatim. It exists to
    // separate two questions that look the same at the frame level: is the loop
    // right, and is the anchor's float rounding all that is left?
    let mut anchor_file: Option<String> = None;
    // Codec dir + an output prefix: with both, each line also writes the
    // decoded audio as raw f32, which is what the audio comparison needs.
    let mut codec_dir: Option<String> = None;
    let mut wav: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        let next = |i: &mut usize| -> Result<String> {
            *i += 1;
            args.get(*i)
                .cloned()
                .with_context(|| format!("{} needs a value", args[*i - 1]))
        };
        match args[i].as_str() {
            "--speaker" => speaker = Some(next(&mut i)?),
            "--ref" => ref_codes = Some(next(&mut i)?),
            "--temp" => temp = next(&mut i)?.parse()?,
            "--seed" => seed = next(&mut i)?.parse()?,
            "--max-frames" => max_frames = next(&mut i)?.parse()?,
            "--threads" => threads = next(&mut i)?.parse()?,
            "--dump" => dump = Some(next(&mut i)?),
            "--anchor" => anchor_file = Some(next(&mut i)?),
            "--codec" => codec_dir = Some(next(&mut i)?),
            "--wav" => wav = Some(next(&mut i)?),
            "--no-frame-cap" => frame_cap = false,
            other if other.starts_with("--") => bail!("unknown flag {other}"),
            other => models = Some(other.to_string()),
        }
        i += 1;
    }
    let models = models.context("usage: bm-tts-frames <models-dir> --speaker <f32-file> …")?;
    let speaker = speaker.context("--speaker is required: this model conditions on one")?;

    let anchor_in: Vec<f32> = bm_tts::f32le::read(&speaker)?;
    let ref_frames: Option<Vec<Vec<i64>>> = match &ref_codes {
        Some(p) => Some(
            serde_json::from_str(
                &std::fs::read_to_string(p).with_context(|| format!("reading {p}"))?,
            )
            .context("parsing --ref json")?,
        ),
        None => None,
    };

    eprintln!("loading {}", models);
    let mut engine = Engine::load(std::path::Path::new(&models), threads)?;
    eprintln!(
        "ready: n_vq={} hidden={} layers={}",
        engine.cfg.n_vq, engine.cfg.hidden_size, engine.cfg.num_hidden_layers
    );
    let mut codec = match (&codec_dir, &wav) {
        (Some(dir), Some(_)) => Some(Codec::load(std::path::Path::new(dir), threads)?),
        (Some(_), None) => {
            bail!("--codec without --wav would load the codec and use it for nothing")
        }
        _ => None,
    };

    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    let mut n = 0usize;
    for line in stdin.lock().lines() {
        let phonemes = line?;
        if phonemes.trim().is_empty() {
            continue;
        }
        // `--dump` writes the intermediate stages for the *first* line only, so
        // a divergence can be located at the stage that caused it rather than
        // argued about at the frame level.
        if let (Some(dir), true) = (&dump, n == 0) {
            let dir = std::path::Path::new(dir);
            std::fs::create_dir_all(dir)?;
            let anchor = match &anchor_file {
                Some(p) => bm_tts::f32le::read(p)?,
                None => engine.speaker_anchor(Some(&anchor_in))?.unwrap_or_default(),
            };
            let (rows, t0) = engine.build_rows(&phonemes, ref_frames.as_deref())?;
            let tprompt = rows.len() / (engine.cfg.n_vq + 1);
            let prompt = engine.embed_rows(&rows, tprompt, Some(&anchor));
            write_f32(&dir.join("anchor.f32"), &anchor)?;
            write_i64(&dir.join("rows.i64"), &rows)?;
            write_f32(&dir.join("prompt.f32"), &prompt)?;
            std::fs::write(dir.join("t0.txt"), t0.to_string())?;
            eprintln!(
                "dumped: anchor {} values, rows {} (t0={t0}), prompt {} values",
                anchor.len(),
                rows.len(),
                prompt.len()
            );
        }
        let forced: Option<Vec<f32>> = anchor_file
            .as_ref()
            .map(|p| bm_tts::f32le::read(p).expect("--anchor file"));
        let mut req = Request::new(&phonemes);
        req.sampling = Sampling {
            temperature: temp,
            ..Default::default()
        };
        req.max_new_frames = max_frames;
        req.frame_cap = frame_cap;
        let mut rng = Rng::new(seed);
        // With `--anchor`, the projection is skipped: the value handed to the
        // model is the one in the file, so the anchor's arithmetic is out of the
        // comparison entirely.
        req.anchor_override = forced.as_deref();
        req.speaker_emb = Some(&anchor_in);
        req.ref_codes = ref_frames.as_deref();
        let started = std::time::Instant::now();
        let frames = engine
            .generate(&req, &mut rng)
            .with_context(|| format!("generating line {n}"))?;
        eprintln!(
            "  line {n}: {} frames (cap {}, {}) in {:.2?}",
            frames.codes.len(),
            frames.cap,
            if frames.hit_cap { "hit cap" } else { "eos" },
            started.elapsed()
        );
        if let (Some(codec), Some(prefix)) = (codec.as_mut(), &wav) {
            let pcm = codec.decode(&frames.codes)?;
            write_f32(std::path::Path::new(&format!("{prefix}.{n}.f32")), &pcm)?;
            eprintln!(
                "  line {n}: {} samples ({:.2}s)",
                pcm.len(),
                pcm.len() as f64 / 48_000.0
            );
        }
        serde_json::to_writer(&mut out, &frames.codes)?;
        writeln!(out)?;
        n += 1;
    }
    Ok(())
}

fn write_f32(path: &std::path::Path, v: &[f32]) -> Result<()> {
    let mut b = Vec::with_capacity(v.len() * 4);
    for x in v {
        b.extend_from_slice(&x.to_le_bytes());
    }
    std::fs::write(path, b).with_context(|| format!("writing {}", path.display()))
}

fn write_i64(path: &std::path::Path, v: &[i64]) -> Result<()> {
    let mut b = Vec::with_capacity(v.len() * 8);
    for x in v {
        b.extend_from_slice(&x.to_le_bytes());
    }
    std::fs::write(path, b).with_context(|| format!("writing {}", path.display()))
}
