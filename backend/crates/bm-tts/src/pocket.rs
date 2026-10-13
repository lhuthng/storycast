//! Kyutai's Pocket TTS — the second engine, and no longer a port.

use anyhow::{bail, Context, Result};
use candle_core::Device;
use pocket_tts::{ModelState, TTSModel};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The variant name handed to the crate. Deliberately *not* `b6369a24`: that
const VARIANT: &str = "pocket";

/// The checkpoint's architecture, with paths filled in at load.
const CONFIG_TEMPLATE: &str = include_str!("../pocket-config.yaml");

/// Sampling temperature, fixed at load because the crate's API takes it in the
const TEMPERATURE: f32 = 0.3;

/// Flow-integration steps. The crate's own default, and what the measured
const LSD_DECODE_STEPS: usize = 1;

/// End-of-speech threshold, from the crate's defaults.
const EOS_THRESHOLD: f32 = -4.0;

/// The weights file, vendored beside the voices.
const WEIGHTS: &str = "pocket.safetensors";

/// The SentencePiece model this checkpoint's lookup table was trained with.
const TOKENIZER: &str = "tokenizer.model";

/// Target loudness for a rendered segment, in dBFS RMS.
const TARGET_RMS_DBFS: f32 = -23.0;

/// The most gain normalization will ever apply.
const MAX_GAIN_DB: f32 = 30.0;

/// One enrolled voice: a prepared state the crate can condition on.
pub struct PocketVoice {
    pub name: String,
    pub description: String,
    /// The file this state came from — reported, never re-read after load.
    pub source: String,
    state: ModelState,
}

#[derive(Debug, Deserialize)]
struct RawPocketVoice {
    #[serde(default)]
    description: String,
    /// A path relative to the models directory: either a `.safetensors`
    file: String,
}

#[derive(Debug, Deserialize)]
struct RawPocketStore {
    #[serde(default)]
    default_voice: Option<String>,
    #[serde(default)]
    presets: BTreeMap<String, RawPocketVoice>,
}

/// The loaded checkpoint plus every voice in the store.
pub struct Pocket {
    /// Held for the crate's `&self` generate call and never mutated after load.
    model: TTSModel,
    pub voices: BTreeMap<String, PocketVoice>,
    pub default_voice: Option<String>,
    sample_rate: usize,
}

impl Pocket {
    /// Load the checkpoint, then enrol every voice in the store.
    pub fn load(models_dir: &Path, voices_path: &Path, threads: usize) -> Result<Pocket> {
        let _ = threads;
        let raw: RawPocketStore = serde_json::from_str(
            &std::fs::read_to_string(voices_path)
                .with_context(|| format!("reading {}", voices_path.display()))?,
        )
        .with_context(|| format!("parsing {}", voices_path.display()))?;
        if raw.presets.is_empty() {
            bail!("{} has no presets", voices_path.display());
        }

        let model = load_model(models_dir)?;
        let sample_rate = model.sample_rate;

        let mut voices = BTreeMap::new();
        for (name, v) in raw.presets {
            let path = models_dir.join(&v.file);
            let state = voice_state(&model, &path)
                .with_context(|| format!("enrolling voice {name:?} from {}", path.display()))?;
            voices.insert(
                name.clone(),
                PocketVoice {
                    name,
                    description: v.description,
                    source: path.display().to_string(),
                    state,
                },
            );
        }

        Ok(Pocket {
            model,
            voices,
            default_voice: raw.default_voice,
            sample_rate,
        })
    }

    /// Resolve a requested voice — the same rule as [`crate::voice::Roster`]:
    pub fn resolve(&self, name: Option<&str>) -> Result<&PocketVoice> {
        let want = name.map(str::trim).filter(|n| !n.is_empty());
        let Some(n) = want else {
            return self
                .default_voice
                .as_ref()
                .and_then(|d| self.voices.get(d))
                .or_else(|| self.voices.values().next())
                .context("the voice store is empty");
        };
        if let Some(v) = self.voices.get(n) {
            return Ok(v);
        }
        let folded = norm(n);
        if let Some(v) = self
            .voices
            .iter()
            .find(|(k, _)| norm(k) == folded)
            .map(|(_, v)| v)
        {
            return Ok(v);
        }
        bail!(
            "unknown voice {n:?} on this box — enroll it in the engine's voices.json ({} known)",
            self.voices.len()
        )
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.voices.keys().map(|s| s.as_str())
    }

    /// Text → 24 kHz mono samples, whole.
    pub fn generate(
        &mut self,
        text: &str,
        voice: &str,
        temperature: f64,
        seed: u64,
    ) -> Result<(Vec<f32>, usize)> {
        let _ = (temperature, seed);
        // Cloned out because `resolve` borrows `self` immutably and the crate's
        let state = self.resolve(Some(voice))?.state.clone();
        let audio = self
            .model
            .generate(text, &state)
            .with_context(|| format!("generating {text:?} as {voice:?}"))?;
        let mut pcm = audio
            .flatten_all()
            .context("flattening the generated tensor")?
            .to_vec1::<f32>()
            .context("reading the generated tensor as f32 samples")?;
        normalize(&mut pcm);
        Ok((pcm, self.sample_rate))
    }
}

/// Scale a rendered segment to [`TARGET_RMS_DBFS`], staying short of clipping.
fn normalize(pcm: &mut [f32]) {
    if pcm.is_empty() {
        return;
    }
    let energy: f32 = pcm.iter().map(|s| s * s).sum();
    let rms = (energy / pcm.len() as f32).sqrt();
    if rms <= f32::EPSILON {
        return; // digital silence: nothing to scale, and no gain is right
    }
    let wanted = 10f32.powf((TARGET_RMS_DBFS - 20.0 * rms.log10()) / 20.0);
    let ceiling = 10f32.powf(MAX_GAIN_DB / 20.0);
    let mut gain = wanted.min(ceiling);
    let peak = pcm.iter().fold(0f32, |m, s| m.max(s.abs()));
    if peak * gain > 0.99 {
        gain = 0.99 / peak;
    }
    for s in pcm.iter_mut() {
        *s *= gain;
    }
}

/// Load the vendored checkpoint by staging the config the crate insists on
fn load_model(models_dir: &Path) -> Result<TTSModel> {
    let weights = models_dir.join(WEIGHTS);
    let tokenizer = models_dir.join(TOKENIZER);
    for (what, path) in [(WEIGHTS, &weights), (TOKENIZER, &tokenizer)] {
        if !path.is_file() {
            bail!(
                "{what} is missing from {} — Pocket TTS needs the checkpoint's \
                 {WEIGHTS}, its {TOKENIZER} and a voices.json beside them",
                models_dir.display()
            );
        }
    }
    // Absolute from here on. The crate reads the config below *after* the
    let weights = weights
        .canonicalize()
        .with_context(|| format!("resolving {}", weights.display()))?;
    let tokenizer = tokenizer
        .canonicalize()
        .with_context(|| format!("resolving {}", tokenizer.display()))?;
    let config = CONFIG_TEMPLATE
        .replace("__WEIGHTS__", &weights.display().to_string())
        .replace("__TOKENIZER__", &tokenizer.display().to_string());

    let stage = tempfile::tempdir().context("creating a directory to stage the config in")?;
    std::fs::create_dir(stage.path().join("config"))
        .context("creating the staged config directory")?;
    std::fs::write(
        stage.path().join("config").join(format!("{VARIANT}.yaml")),
        config,
    )
    .context("writing the staged config")?;

    // The crate looks for `config/<variant>.yaml` under the process working
    let cwd = std::env::current_dir().context("reading the working directory")?;
    std::env::set_current_dir(stage.path()).context("entering the staged config directory")?;
    let loaded = TTSModel::load_with_params_device(
        VARIANT,
        TEMPERATURE,
        LSD_DECODE_STEPS,
        EOS_THRESHOLD,
        None,
        &Device::Cpu,
    );
    let restored = std::env::set_current_dir(&cwd).context("restoring the working directory");
    let model = loaded.context("loading the Pocket TTS checkpoint")?;
    restored?;
    Ok(model)
}

/// A voice file → the state the model conditions on.
fn voice_state(model: &TTSModel, path: &Path) -> Result<ModelState> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        // Prepared conditioning — what the shipped catalogue uses.
        "safetensors" => model
            .get_voice_state_from_prompt_file(path)
            .with_context(|| format!("loading embeddings {}", path.display())),
        // Clone from raw audio. Requires the with-voice-cloning checkpoint;
        "wav" | "wave" => {
            let (audio, rate) = pocket_tts::audio::read_wav(path)
                .with_context(|| format!("reading clone source {}", path.display()))?;
            let pcm = audio
                .flatten_all()
                .context("flattening the clone source")?
                .to_vec1::<f32>()
                .context("reading the clone source as f32 samples")?;
            let cleaned = denoise(&pcm)?;
            // Through bytes rather than a path: the model's entry point takes a
            let wav = crate::codec::to_wav_bytes(&cleaned, rate);
            model
                .get_voice_state_from_bytes(&wav)
                .with_context(|| format!("cloning from {}", path.display()))
        }
        other => bail!(
            "voice {} has extension {other:?}; expected .safetensors (prepared \
             embeddings) or .wav (clone source)",
            path.display()
        ),
    }
}

/// STFT window and hop for [`denoise`]. 1024/256 at 24 kHz is 43 ms of context
const FFT: usize = 1024;
const HOP: usize = 256;

/// How hard the noise estimate is subtracted. Over-subtracting is the safe
const OVER_SUBTRACT: f32 = 2.0;

/// A floor under the subtraction, as a fraction of each frame's own magnitude.
const SPECTRAL_FLOOR: f32 = 0.02;

/// The quietest share of frames, in percent, used as the noise profile.
const NOISE_FRACTION: usize = 15;

/// Samples of reflected signal prepended and appended before the transform.
const EDGE_PAD: usize = FFT;

/// Remove steady noise from a clone source by spectral subtraction.
fn denoise(pcm: &[f32]) -> Result<Vec<f32>> {
    if pcm.len() < FFT * 4 {
        return Ok(pcm.to_vec());
    }
    let mut padded = Vec::with_capacity(pcm.len() + 2 * EDGE_PAD);
    for i in (1..=EDGE_PAD).rev() {
        padded.push(pcm[i]);
    }
    padded.extend_from_slice(pcm);
    for i in 1..=EDGE_PAD {
        padded.push(pcm[pcm.len() - 1 - i]);
    }
    let cleaned = spectral_subtract(&padded)?;
    Ok(cleaned[EDGE_PAD..EDGE_PAD + pcm.len()].to_vec())
}

/// The transform proper: the noise estimate is the clip's *own* quietest frames,
fn spectral_subtract(pcm: &[f32]) -> Result<Vec<f32>> {
    use realfft::num_complex::Complex;
    use realfft::RealFftPlanner;

    let frames = (pcm.len() - FFT) / HOP + 1;
    let bins = FFT / 2 + 1;
    let window: Vec<f32> = (0..FFT)
        .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / (FFT as f32 - 1.0)).cos())
        .collect();

    let mut planner = RealFftPlanner::<f32>::new();
    let fwd = planner.plan_fft_forward(FFT);
    let inv = planner.plan_fft_inverse(FFT);
    let mut buf = vec![0f32; FFT];
    let mut spec = vec![Complex::new(0f32, 0f32); bins];

    // Forward pass: one spectrum per frame, plus the magnitude sum that decides
    let mut spectra: Vec<Vec<Complex<f32>>> = Vec::with_capacity(frames);
    let mut energy: Vec<(f32, usize)> = Vec::with_capacity(frames);
    for f in 0..frames {
        let base = f * HOP;
        for i in 0..FFT {
            buf[i] = pcm[base + i] * window[i];
        }
        fwd.process(&mut buf, &mut spec)?;
        energy.push((spec.iter().map(|c| c.norm()).sum(), f));
        spectra.push(spec.clone());
    }

    // The noise profile: the per-bin mean of the quietest frames.
    energy.sort_by(|a, b| a.0.total_cmp(&b.0));
    let nfloor = (frames * NOISE_FRACTION / 100).max(1);
    let mut noise = vec![0f32; bins];
    for (_, f) in &energy[..nfloor] {
        for (b, c) in spectra[*f].iter().enumerate() {
            noise[b] += c.norm() / nfloor as f32;
        }
    }

    // Inverse pass: over-subtract, floor, keep phase, overlap-add.
    let mut out = vec![0f32; pcm.len() + FFT];
    let mut norm = vec![0f32; pcm.len() + FFT];
    let scale = 1.0 / FFT as f32;
    for (f, frame) in spectra.iter_mut().enumerate() {
        for (b, c) in frame.iter_mut().enumerate() {
            let mag = c.norm();
            let clean = (mag - OVER_SUBTRACT * noise[b]).max(SPECTRAL_FLOOR * mag);
            let keep = if mag > f32::EPSILON { clean / mag } else { 0.0 };
            *c = Complex::new(c.re * keep, c.im * keep);
        }
        inv.process(frame, &mut buf)?;
        let base = f * HOP;
        for i in 0..FFT {
            out[base + i] += buf[i] * scale * window[i];
            norm[base + i] += window[i] * window[i];
        }
    }
    out.truncate(pcm.len());
    for (o, n) in out.iter_mut().zip(&norm) {
        if *n > 1e-8 {
            *o /= n;
        }
    }
    Ok(out)
}

/// Case-folding for the folded voice lookup, matching [`crate::voice::norm`].
fn norm(s: &str) -> String {
    s.trim().to_ascii_lowercase().replace('_', "-")
}

/// Where the store lives for a models directory, for callers that need it.
pub fn store_path(models_dir: &Path) -> PathBuf {
    models_dir.join("voices.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(json: &str) -> Result<RawPocketStore> {
        Ok(serde_json::from_str(json)?)
    }

    #[test]
    fn a_store_needs_at_least_one_preset() {
        let s = store(r#"{"default_voice":"A","presets":{}}"#).unwrap();
        assert!(s.presets.is_empty());
        let s =
            store(r#"{"default_voice":"A","presets":{"A":{"file":"v/a.safetensors"}}}"#).unwrap();
        assert_eq!(s.presets.len(), 1);
        // `description` is optional metadata, not a required field.
        assert_eq!(s.presets["A"].description, "");
    }

    #[test]
    fn a_missing_weights_file_is_refused_before_anything_is_loaded() {
        // The point is the *message and the refusal*: this is the guard that
        let dir = tempfile::tempdir().unwrap();
        // Matched rather than `unwrap_err`ed: `TTSModel` has no `Debug`, so the
        let err = match load_model(dir.path()) {
            Ok(_) => panic!("a models directory with no checkpoint must not load"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains(WEIGHTS), "{err}");
        assert!(err.contains("tokenizer.model"), "{err}");
    }

    #[test]
    fn an_unknown_voice_extension_is_an_error_not_a_guess() {
        let err = voice_state_ext("mp3");
        assert!(err.contains("mp3"), "{err}");
        assert!(err.contains(".safetensors"), "{err}");
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt()
    }

    #[test]
    fn denoise_keeps_what_is_loud_and_quiets_what_is_not() {
        // The scaling check, on a signal the algorithm can actually reason about:
        let sr = 24_000f32;
        let n = 24_000usize;
        let block = n / 10;
        let bed = 0.01;
        let x: Vec<f32> = (0..n)
            .map(|i| {
                let level = if (i / block) % 2 == 0 { 0.3 } else { bed };
                level * (2.0 * std::f32::consts::PI * 220.0 * i as f32 / sr).sin()
            })
            .collect();
        let bands = |v: &[f32]| {
            let loud: Vec<f32> = (0..n)
                .filter(|i| (i / block) % 2 == 0)
                .map(|i| v[i])
                .collect();
            let quiet: Vec<f32> = (0..n)
                .filter(|i| (i / block) % 2 == 1)
                .map(|i| v[i])
                .collect();
            (rms(&loud), rms(&quiet))
        };
        let (loud_in, quiet_in) = bands(&x);
        let (loud_out, quiet_out) = bands(&denoise(&x).unwrap());
        assert!(
            (loud_in - loud_out).abs() / loud_in < 0.2,
            "loud bursts preserved, {loud_in:.4} -> {loud_out:.4}"
        );
        assert!(
            quiet_out < quiet_in * 0.5,
            "quiet bed fell, {quiet_in:.4} -> {quiet_out:.4}"
        );
    }

    #[test]
    fn denoise_quietens_the_gaps() {
        // The shape of the real failure: bursts of voice separated by stretches
        let sr = 24_000usize;
        let n = sr;
        let block = sr / 10;
        let mut x: Vec<f32> = (0..n)
            .map(|i| {
                if (i / block) % 2 == 0 {
                    0.3 * (2.0 * std::f32::consts::PI * 200.0 * i as f32 / sr as f32).sin()
                } else {
                    0.0
                }
            })
            .collect();
        let mut seed = 99u64;
        for s in x.iter_mut() {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            *s += 0.02 * (((seed >> 40) as f32 / (1u64 << 24) as f32) - 0.5);
        }
        let gaps = |v: &[f32]| {
            let g: Vec<f32> = (0..n)
                .filter(|i| (i / block) % 2 == 1)
                .map(|i| v[i])
                .collect();
            rms(&g)
        };
        let (before, after) = (gaps(&x), gaps(&denoise(&x).unwrap()));
        assert!(
            after < before * 0.5,
            "gap noise fell, {before:.5} -> {after:.5}"
        );
    }

    #[test]
    fn denoise_quietens_the_final_gap_too() {
        // The regression that the reflection pad exists for. Without it the last
        let sr = 24_000usize;
        let n = sr;
        let block = sr / 10;
        let mut x: Vec<f32> = (0..n)
            .map(|i| {
                if (i / block) % 2 == 0 {
                    0.3 * (2.0 * std::f32::consts::PI * 200.0 * i as f32 / sr as f32).sin()
                } else {
                    0.0
                }
            })
            .collect();
        let mut seed = 7u64;
        for s in x.iter_mut() {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            *s += 0.02 * (((seed >> 40) as f32 / (1u64 << 24) as f32) - 0.5);
        }
        let out = denoise(&x).unwrap();
        let tail = rms(&out[n - block..]);
        let input_tail = rms(&x[n - block..]);
        assert!(
            tail < input_tail,
            "the last block quietened, {input_tail:.5} -> {tail:.5}"
        );
    }

    #[test]
    fn denoise_leaves_a_too_short_clip_alone() {
        let tiny = vec![0.1f32; 100];
        assert_eq!(denoise(&tiny).unwrap(), tiny);
    }

    fn dbfs(x: &[f32]) -> f32 {
        let e: f32 = x.iter().map(|s| s * s).sum();
        20.0 * (e / x.len() as f32).sqrt().log10()
    }

    #[test]
    fn a_quiet_segment_is_brought_up_to_the_target() {
        // The actual failure: a segment some 20 dB below where it should be.
        let mut pcm: Vec<f32> = (0..1000).map(|i| 0.02 * (i as f32 * 0.05).sin()).collect();
        assert!(
            dbfs(&pcm) < TARGET_RMS_DBFS - 10.0,
            "starts well under the target, at {} dBFS",
            dbfs(&pcm)
        );
        normalize(&mut pcm);
        assert!(
            (dbfs(&pcm) - TARGET_RMS_DBFS).abs() < 0.5,
            "lands on the target, got {}",
            dbfs(&pcm)
        );
    }

    #[test]
    fn normalization_never_clips() {
        // A signal whose RMS is low but whose peaks are already near full scale:
        let mut pcm: Vec<f32> = (0..1000)
            .map(|i| if i % 100 == 0 { 0.95 } else { 0.001 })
            .collect();
        normalize(&mut pcm);
        let peak = pcm.iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!(peak <= 0.99, "peak {peak} stayed under full scale");
    }

    #[test]
    fn digital_silence_is_left_alone() {
        // Not a level problem, and 30 dB of gain on nothing is still nothing,
        let mut pcm = vec![0f32; 100];
        normalize(&mut pcm);
        assert!(pcm.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn a_nearly_dead_segment_is_bounded_rather_than_amplified() {
        // 60 dB below target: the cap must stop short, so an actually broken
        let mut pcm: Vec<f32> = (0..1000)
            .map(|i| 0.0005 * (i as f32 * 0.05).sin())
            .collect();
        let before = dbfs(&pcm);
        normalize(&mut pcm);
        let gained = dbfs(&pcm) - before;
        assert!(gained <= MAX_GAIN_DB + 0.5, "gain {gained} dB was capped");
    }

    #[test]
    fn voice_names_fold_like_the_rest_of_the_roster() {
        assert_eq!(norm("Caro_Davy"), "caro-davy");
        assert_eq!(norm("  ALBA  "), "alba");
    }

    /// The extension dispatch, without a model: this is the branch table that
    fn voice_state_ext(ext: &str) -> String {
        match ext {
            "safetensors" | "wav" | "wave" => String::new(),
            other => format!(
                "voice x.{other} has extension {other:?}; expected .safetensors \
                 (prepared embeddings) or .wav (clone source)"
            ),
        }
    }
}
