use anyhow::{Context, Result};
use std::io::{Read, Seek};
use std::path::Path;

/// Gemini returns 24 kHz; VieNeu renders at 48 kHz.
pub const GEMINI_RATE: u32 = 24_000;
pub const VIENEU_RATE: u32 = 48_000;

pub fn sample_rate_for(engine: &str) -> u32 {
    if engine == "vieneu" {
        VIENEU_RATE
    } else {
        GEMINI_RATE
    }
}

// ---------------------------------------------------------------------------
// minimal RIFF/WAVE
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Wav {
    pub channels: u16,
    pub sample_rate: u32,
    pub bits: u16,
    pub data: Vec<u8>,
}

impl Wav {
    pub fn frames(&self) -> usize {
        let block = self.channels.max(1) as usize * (self.bits.max(8) as usize / 8);
        self.data.len().checked_div(block).unwrap_or(0)
    }

    pub fn seconds(&self) -> f64 {
        if self.sample_rate == 0 {
            0.0
        } else {
            self.frames() as f64 / self.sample_rate as f64
        }
    }

    /// `(channels, bits)` — the params the concat step requires to match.
    pub fn params(&self) -> (u16, u16) {
        (self.channels, self.bits)
    }
}

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

pub fn read_wav(path: &Path) -> Result<Wav> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        anyhow::bail!("{} is not a RIFF/WAVE file", path.display());
    }
    let mut channels = 0u16;
    let mut sample_rate = 0u32;
    let mut bits = 0u16;
    let mut data: Option<Vec<u8>> = None;

    let mut pos = 12usize;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32_at(&bytes, pos + 4) as usize;
        let body = pos + 8;
        let end = (body + size).min(bytes.len());
        if id == b"fmt " && size >= 16 {
            channels = u16_at(&bytes, body + 2);
            sample_rate = u32_at(&bytes, body + 4);
            bits = u16_at(&bytes, body + 14);
        } else if id == b"data" {
            data = Some(bytes[body..end].to_vec());
        }
        // chunks are word-aligned
        pos = body + size + (size & 1);
    }

    let data = data.ok_or_else(|| anyhow::anyhow!("{}: no data chunk", path.display()))?;
    if channels == 0 || sample_rate == 0 || bits == 0 {
        anyhow::bail!("{}: incomplete fmt chunk", path.display());
    }
    Ok(Wav {
        channels,
        sample_rate,
        bits,
        data,
    })
}

pub fn write_wav(path: &Path, wav: &Wav) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let byte_rate = wav.sample_rate * wav.channels as u32 * (wav.bits as u32 / 8);
    let block_align = wav.channels * (wav.bits / 8);
    let mut out = Vec::with_capacity(44 + wav.data.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((36 + wav.data.len()) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&wav.channels.to_le_bytes());
    out.extend_from_slice(&wav.sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&wav.bits.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(wav.data.len() as u32).to_le_bytes());
    out.extend_from_slice(&wav.data);
    std::fs::write(path, out).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Silent 16-bit mono WAV — used by `--dry-run` rehearsals and tests.
pub fn silent_wav(path: &Path, seconds: f64, rate: u32) -> Result<()> {
    let frames = (rate as f64 * seconds) as usize;
    write_wav(
        path,
        &Wav {
            channels: 1,
            sample_rate: rate,
            bits: 16,
            data: vec![0u8; frames * 2],
        },
    )
}

/// Duration of a WAV in seconds, from the header plus the file length.
///
/// Reads kilobytes instead of the whole file: the merge path asks only this of
/// the mix (the layers' clock, the durations a plan needs), and a chapter's
/// mix WAV is tens of megabytes. Returns `Err` for a file that is not a WAV or
/// has no data chunk, exactly as [`read_wav`] would, so callers can fall back
/// to the full read where they actually need the samples.
pub fn wav_seconds(path: &Path) -> Result<f64> {
    wav_info(path).map(|i| i.seconds())
}

/// Header facts of a WAV: the params the concat step matches on, plus the
/// data length to turn into frames. A full [`read_wav`] copies every sample;
/// the merge path mostly needs only this.
pub struct WavInfo {
    pub channels: u16,
    pub sample_rate: u32,
    pub bits: u16,
    pub data_len: u64,
}

impl WavInfo {
    pub fn frames(&self) -> u64 {
        let block = self.channels.max(1) as u64 * (self.bits.max(8) as u64 / 8);
        self.data_len.checked_div(block).unwrap_or(0)
    }

    pub fn seconds(&self) -> f64 {
        if self.sample_rate == 0 {
            0.0
        } else {
            self.frames() as f64 / self.sample_rate as f64
        }
    }

    /// `(channels, bits)` — the params the concat step requires to match.
    pub fn params(&self) -> (u16, u16) {
        (self.channels, self.bits)
    }
}

pub fn wav_info(path: &Path) -> Result<WavInfo> {
    let mut f = std::fs::File::open(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let mut head = [0u8; 12];
    f.read_exact(&mut head)?;
    if &head[0..4] != b"RIFF" || &head[8..12] != b"WAVE" {
        anyhow::bail!("{} is not a RIFF/WAVE file", path.display());
    }
    let mut channels = 0u16;
    let mut sample_rate = 0u32;
    let mut bits = 0u16;
    let mut data_len: Option<u64> = None;
    let mut pos = 12u64;
    let file_len = f.metadata()?.len();
    let mut chunk = [0u8; 8];
    while f.seek(std::io::SeekFrom::Start(pos))? == pos
        && f.read(&mut chunk)? == 8
    {
        let id = &chunk[0..4];
        let size = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]) as u64;
        let body = pos + 8;
        if body + size > file_len {
            anyhow::bail!("{}: truncated chunk header", path.display());
        }
        if id == b"fmt " && size >= 16 {
            let mut fmt = [0u8; 16];
            f.read_exact(&mut fmt)?;
            channels = u16::from_le_bytes([fmt[2], fmt[3]]);
            sample_rate = u32::from_le_bytes([fmt[4], fmt[5], fmt[6], fmt[7]]);
            bits = u16::from_le_bytes([fmt[14], fmt[15]]);
        } else if id == b"data" {
            data_len = Some(size);
        }
        // chunks are word-aligned
        pos = body + size + (size & 1);
    }
    let data_len = data_len.ok_or_else(|| anyhow::anyhow!("{}: no data chunk", path.display()))?;
    if channels == 0 || sample_rate == 0 || bits == 0 {
        anyhow::bail!("{}: incomplete fmt chunk", path.display());
    }
    Ok(WavInfo {
        channels,
        sample_rate,
        bits,
        data_len,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bm-assemble-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn wav_roundtrip_preserves_params_and_samples() {
        let d = tmpdir("wav");
        let p = d.join("a.wav");
        let wav = Wav {
            channels: 1,
            sample_rate: 48_000,
            bits: 16,
            data: vec![1, 2, 3, 4, 5, 6],
        };
        write_wav(&p, &wav).unwrap();
        let back = read_wav(&p).unwrap();
        assert_eq!(back.channels, 1);
        assert_eq!(back.sample_rate, 48_000);
        assert_eq!(back.bits, 16);
        assert_eq!(back.data, wav.data);
        assert_eq!(back.frames(), 3);
        assert!((back.seconds() - 3.0 / 48_000.0).abs() < 1e-9);
    }

    #[test]
    fn sample_rates_are_engine_specific() {
        assert_eq!(sample_rate_for("vieneu"), 48_000);
        assert_eq!(sample_rate_for("gemini"), 24_000);
    }

    #[test]
    fn header_probe_matches_the_full_read() {
        let d = tmpdir("probe");
        let p = d.join("a.wav");
        silent_wav(&p, 2.5, 48_000).unwrap();
        let full = read_wav(&p).unwrap();
        let probe = wav_info(&p).unwrap();
        assert_eq!(probe.channels, full.channels);
        assert_eq!(probe.sample_rate, full.sample_rate);
        assert_eq!(probe.bits, full.bits);
        assert_eq!(probe.frames() as usize, full.frames());
        assert!((full.seconds() - probe.seconds()).abs() < 1e-6);
    }
}
