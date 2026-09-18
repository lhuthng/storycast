//! The MOSS audio codec, and the wav container the server hands out.
//!
//! Decoding is the last step of a render: the backbone produces `(frames, n_vq)`
//! codes and this turns them into 48 kHz mono samples. It is a port of
//! `_decode_codes`, which is three lines because the graph does the work:
//!
//! ```text
//! audio_codes (1, T, n_vq) int32  ->  (1, channels, samples)  ->  mean over channels
//! ```
//!
//! The channel mean is not an optimisation, it is part of the contract: the
//! codec is stereo and the pipeline is mono, so the two channels are averaged
//! rather than one being taken. Dropping a channel would change the audio.
//!
//! `encode` (the other direction, for enrollment reference codes) is deliberately
//! not here — it belongs with the rest of the enrollment path.

use crate::engine::open_session;
use anyhow::{bail, Context, Result};
use ort::session::Session;
use ort::value::Tensor;
use std::path::Path;

pub struct Codec {
    sess: Session,
}

impl Codec {
    /// Load `moss_audio_tokenizer_decode_full.onnx` from `dir`.
    pub fn load(dir: &Path, threads: usize) -> Result<Codec> {
        let path = dir.join("moss_audio_tokenizer_decode_full.onnx");
        Ok(Codec {
            sess: open_session(&path, threads)?,
        })
    }

    /// `(frames, n_vq)` codes to mono f32 samples at 48 kHz.
    pub fn decode(&mut self, codes: &[Vec<i64>]) -> Result<Vec<f32>> {
        if codes.is_empty() {
            return Ok(Vec::new());
        }
        let n_vq = codes[0].len();
        let mut flat: Vec<i32> = Vec::with_capacity(codes.len() * n_vq);
        for (i, f) in codes.iter().enumerate() {
            if f.len() != n_vq {
                bail!("frame {i} has {} codes, expected {n_vq}", f.len());
            }
            flat.extend(f.iter().map(|c| *c as i32));
        }
        let t = codes.len();

        let audio = Tensor::from_array((vec![1i64, t as i64, n_vq as i64], flat))
            .context("building audio_codes")?;
        let lens = Tensor::from_array((vec![1i64], vec![t as i32]))
            .context("building audio_code_lengths")?;

        let t_run = std::time::Instant::now();
        let outs = self
            .sess
            .run(ort::inputs!["audio_codes" => audio, "audio_code_lengths" => lens])
            .context("codec decode")?;
        crate::stats::add(crate::stats::Kind::Codec, t_run.elapsed());
        let (shape, data) = outs[0]
            .try_extract_tensor::<f32>()
            .context("codec output is not f32")?;
        let dims: Vec<i64> = shape.as_ref().to_vec();
        if dims.len() != 3 {
            bail!("codec output is {dims:?}, expected (1, channels, samples)");
        }
        let (channels, samples) = (dims[1] as usize, dims[2] as usize);
        let mut out = vec![0f32; samples];
        for c in 0..channels {
            let row = &data[c * samples..(c + 1) * samples];
            for (o, v) in out.iter_mut().zip(row) {
                *o += v;
            }
        }
        if channels > 1 {
            let inv = 1.0 / channels as f32;
            for o in out.iter_mut() {
                *o *= inv;
            }
        }
        Ok(out)
    }
}

/// A 16-bit mono PCM wav, byte for byte what `python/tts_server.py` sends.
///
/// Hand-rolled because it is 44 bytes of header and this is the only wav the
/// server produces; a dependency would be larger than the code. The clipping and
/// the `32767` scale match the reference's `_wav_bytes` — `32768` would be the
/// more usual choice and would change the output.
pub fn to_wav_bytes(pcm: &[f32], sample_rate: u32) -> Vec<u8> {
    let n = pcm.len();
    let data_len = (n * 2) as u32;
    let mut out = Vec::with_capacity(44 + n * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte rate
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for v in pcm {
        let clamped = v.clamp(-1.0, 1.0);
        let s = (clamped * 32767.0) as i16;
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wav_header_is_the_one_the_sidecar_sends() {
        let w = to_wav_bytes(&[0.0, 1.0, -1.0], 48_000);
        assert_eq!(&w[..4], b"RIFF");
        assert_eq!(&w[8..12], b"WAVE");
        assert_eq!(&w[12..16], b"fmt ");
        assert_eq!(u32::from_le_bytes([w[16], w[17], w[18], w[19]]), 16);
        assert_eq!(u16::from_le_bytes([w[20], w[21]]), 1, "PCM");
        assert_eq!(u16::from_le_bytes([w[22], w[23]]), 1, "mono");
        assert_eq!(u32::from_le_bytes([w[24], w[25], w[26], w[27]]), 48_000);
        assert_eq!(u32::from_le_bytes([w[28], w[29], w[30], w[31]]), 96_000);
        assert_eq!(&w[36..40], b"data");
        assert_eq!(u32::from_le_bytes([w[40], w[41], w[42], w[43]]), 6);
        assert_eq!(w.len(), 44 + 6);
    }

    #[test]
    fn samples_clip_rather_than_wrap() {
        let w = to_wav_bytes(&[2.0, -2.0], 48_000);
        assert_eq!(i16::from_le_bytes([w[44], w[45]]), 32767);
        assert_eq!(i16::from_le_bytes([w[46], w[47]]), -32767);
    }
}
