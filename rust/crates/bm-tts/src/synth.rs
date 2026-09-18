//! One chunk, end to end, and joining chunks into a paragraph.
//!
//! The reference keeps the guard inside `infer`, but the pieces are separable and
//! separating them is what makes each testable: [`Engine`] produces codes,
//! [`Codec`] produces audio, and the babble guard needs *audio* to decide — so it
//! can only live above both.
//!
//! A chunk is generated, decoded, and judged. If the judge is unhappy it is
//! generated **again** — and the RNG continues rather than restarting, because
//! regenerating the identical chunk would make the loop pointless. At most
//! [`babble::MAX_RETRIES`] times; the best attempt is what survives, which is not
//! necessarily the last one.

use crate::babble::{self, Verdict};
use crate::codec::Codec;
use crate::engine::{Engine, Frames, Request};
use crate::sample::Rng;
use anyhow::Result;

/// 48 kHz, the engine's native rate. Not configurable: the codec and the model
/// are both trained for it, and the reference hard-codes it too.
pub const SAMPLE_RATE: usize = 48_000;

pub struct Chunk {
    pub codes: Vec<Vec<i64>>,
    pub pcm: Vec<f32>,
    /// `None` when the guard did not run (the caller turned the frame cap off).
    pub verdict: Option<Verdict>,
    /// How many extra generations the guard asked for.
    pub retries: usize,
}

impl Chunk {
    pub fn seconds(&self) -> f64 {
        self.pcm.len() as f64 / SAMPLE_RATE as f64
    }
}

pub struct Synth {
    pub engine: Engine,
    pub codec: Codec,
    pub max_retries: usize,
}

impl Synth {
    pub fn load(
        models_dir: &std::path::Path,
        codec_dir: &std::path::Path,
        threads: usize,
    ) -> Result<Synth> {
        Ok(Synth {
            engine: Engine::load(models_dir, threads)?,
            codec: Codec::load(codec_dir, threads)?,
            max_retries: babble::MAX_RETRIES,
        })
    }

    /// Generate one chunk and, if the guard is suspicious of it, generate again.
    pub fn chunk(&mut self, req: &Request, rng: &mut Rng) -> Result<Chunk> {
        let first = self.engine.generate(req, rng)?;
        let mut pcm = self.codec.decode(&first.codes)?;
        let mut codes = first.codes;
        let cap = first.cap;

        if self.max_retries == 0 || !req.frame_cap {
            return Ok(Chunk {
                codes,
                pcm,
                verdict: None,
                retries: 0,
            });
        }

        let mut best = babble::suspect(&pcm, SAMPLE_RATE, req.phonemes, cap, codes.len());
        let mut retries = 0;
        while best.suspect && retries < self.max_retries {
            let again: Frames = self.engine.generate(req, rng)?;
            retries += 1;
            if again.codes.is_empty() {
                continue;
            }
            let wav = self.codec.decode(&again.codes)?;
            let cand = babble::suspect(&wav, SAMPLE_RATE, req.phonemes, cap, again.codes.len());
            if cand.better_than(&best) {
                codes = again.codes;
                pcm = wav;
                best = cand;
            }
        }
        Ok(Chunk {
            codes,
            pcm,
            verdict: Some(best),
            retries,
        })
    }
}

// ── joining chunks ──────────────────────────────────────────────────────────

/// Silence by boundary kind: paragraph, sentence end, or a break inside one.
pub const GAP_PARA_S: f64 = 0.70;
pub const GAP_SENTENCE_S: f64 = 0.50;
pub const GAP_MINOR_S: f64 = 0.30;

/// Map boundary labels to pause lengths. An unknown label is treated as a
/// sentence break, like the reference's `.get(g, ...[ "sentence"])`.
pub fn gaps_to_silence(gaps: &[String]) -> Vec<f64> {
    gaps.iter()
        .map(|g| match g.as_str() {
            "para" => GAP_PARA_S,
            "minor" => GAP_MINOR_S,
            _ => GAP_SENTENCE_S,
        })
        .collect()
}

/// "This is sound" on a mean-|x| envelope, absolute rather than relative to the
/// chunk's peak. Chunks from one render are all at a similar level, so an
/// absolute threshold is stable across them and a relative one would call a quiet
/// chunk's speech silence.
const EDGE_THRESH_DB: f32 = -45.0;

/// Samples of leading and trailing silence, on a 10 ms window.
///
/// An all-silent waveform returns `(len, 0)` — every sample is lead, nothing is
/// tail. That asymmetry is in the reference and it matters downstream:
/// `pause_pad_samples` special-cases it to avoid counting the same silence twice.
pub fn edge_silence(wav: &[f32], sample_rate: usize) -> (usize, usize) {
    let win = ((0.01 * sample_rate as f32) as usize).max(1);
    let n_win = wav.len() / win;
    if n_win == 0 {
        return (wav.len(), 0);
    }
    let env: Vec<f32> = (0..n_win)
        .map(|i| {
            let b = &wav[i * win..(i + 1) * win];
            b.iter().map(|v| v.abs()).sum::<f32>() / b.len() as f32
        })
        .collect();
    let thresh = 10f32.powf(EDGE_THRESH_DB / 20.0);
    let first = env.iter().position(|v| *v > thresh);
    let last = env.iter().rposition(|v| *v > thresh);
    match (first, last) {
        (Some(f), Some(l)) => (f * win, wav.len() - (l + 1) * win),
        _ => (wav.len(), 0),
    }
}

/// Zeros needed between two chunks so the *real* pause — the previous chunk's
/// trailing silence, plus zeros, plus the next chunk's leading silence — comes to
/// `pause_s`. Zero when the chunks already leave that much.
///
/// The pause is topped up rather than appended, which is what keeps the rhythm
/// even: a cloned voice with a short tail gets padded out, a preset with a long
/// natural tail is left alone. Appending instead makes every pause uneven by
/// however much silence each chunk happened to carry.
pub fn pause_pad_samples(prev: &[f32], next: &[f32], sample_rate: usize, pause_s: f64) -> usize {
    let (lead_prev, mut tail) = edge_silence(prev, sample_rate);
    if lead_prev == prev.len() {
        // The whole previous chunk is silence, so its "tail" is all of it.
        tail = prev.len();
    }
    let (lead, _) = edge_silence(next, sample_rate);
    let want = (pause_s * sample_rate as f64) as usize;
    want.saturating_sub(tail + lead)
}

/// Concatenate chunks, inserting only the missing silence at each boundary.
///
/// Chunks are **not** trimmed and **not** faded. That is deliberate in the
/// reference (09/2026): trimming each chunk's edges removes the natural breath a
/// preset voice ends on.
pub fn join_with_pauses(chunks: &[Vec<f32>], pauses_s: &[f64], sample_rate: usize) -> Vec<f32> {
    let Some(first) = chunks.first() else {
        return Vec::new();
    };
    let mut out = first.clone();
    for i in 1..chunks.len() {
        let pause = pauses_s.get(i - 1).copied().unwrap_or(0.0);
        let pad = pause_pad_samples(&chunks[i - 1], &chunks[i], sample_rate, pause);
        if pad > 0 {
            out.extend(std::iter::repeat_n(0.0f32, pad));
        }
        out.extend_from_slice(&chunks[i]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(n: usize) -> Vec<f32> {
        vec![0.5f32; n]
    }

    #[test]
    fn one_chunk_is_returned_untouched() {
        let c = tone(100);
        assert_eq!(join_with_pauses(std::slice::from_ref(&c), &[], 48_000), c);
    }

    /// Both chunks are pure tone, so neither contributes silence and the whole
    /// pause is inserted.
    #[test]
    fn a_pause_is_inserted_when_the_chunks_have_none() {
        let out = join_with_pauses(&[tone(4800), tone(4800)], &[0.2], 48_000);
        assert_eq!(out.len(), 4800 + 9600 + 4800);
        assert!(out[4800..4800 + 9600].iter().all(|v| *v == 0.0));
        assert_eq!(out[0], 0.5);
        assert_eq!(out[out.len() - 1], 0.5);
    }

    /// A chunk that already ends in silence needs less padding — the point of
    /// measuring rather than appending.
    #[test]
    fn existing_silence_counts_towards_the_pause() {
        let mut a = tone(4800);
        a.extend(std::iter::repeat_n(0.0f32, 4800)); // 100 ms tail
        let b = tone(4800);
        let pad = pause_pad_samples(&a, &b, 48_000, 0.2);
        // 200 ms wanted, 100 ms already there.
        assert!((pad as i64 - 4800).abs() < 500, "pad {pad}");
        // With no silence at all it would be the full 200 ms.
        assert_eq!(
            pause_pad_samples(&tone(4800), &tone(4800), 48_000, 0.2),
            9600
        );
    }

    #[test]
    fn an_all_silent_previous_chunk_is_counted_once() {
        let silent = vec![0.0f32; 4800];
        // Without the special case, `tail` would be 0 and the pad would ignore
        // the 100 ms already present.
        let pad = pause_pad_samples(&silent, &tone(4800), 48_000, 0.2);
        assert!((pad as i64 - 4800).abs() < 500, "pad {pad}");
    }

    #[test]
    fn a_pause_never_goes_negative() {
        let mut a = tone(4800);
        a.extend(std::iter::repeat_n(0.0f32, 48_000)); // 1 s tail
        assert_eq!(pause_pad_samples(&a, &tone(4800), 48_000, 0.2), 0);
    }

    #[test]
    fn edge_silence_finds_both_ends() {
        let mut w = vec![0.0f32; 4800];
        w.extend(tone(4800));
        w.extend(std::iter::repeat_n(0.0f32, 4800));
        let (lead, tail) = edge_silence(&w, 48_000);
        assert_eq!((lead, tail), (4800, 4800));
    }

    #[test]
    fn an_all_silent_chunk_is_all_lead() {
        assert_eq!(edge_silence(&vec![0.0f32; 9600], 48_000), (9600, 0));
    }

    #[test]
    fn boundary_labels_map_to_the_shipped_pauses() {
        let g = vec![
            "para".to_string(),
            "minor".to_string(),
            "sentence".to_string(),
            "?".to_string(),
        ];
        assert_eq!(gaps_to_silence(&g), vec![0.70, 0.30, 0.50, 0.50]);
    }
}
