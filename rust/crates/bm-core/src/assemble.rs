//! Stage 3/4 — segment planning, concatenation and the final mix.
//!
//! Ported from `synthesize.py`. The WAV handling is native (RIFF PCM 16-bit),
//! so the only external binary is `ffmpeg`, and only for ambience, tempo and
//! mp3 encoding — exactly as before.

mod mood;
mod plan;
mod wav;

pub use self::plan::{Run, character_has_lines, drop_headline, pick_exact, pick_rendered, plan_render, rendered_segments, runs, segment_miss, segments_complete, title_speech, RenderedSegment};
pub use self::wav::{GEMINI_RATE, VIENEU_RATE, read_wav, sample_rate_for, silent_wav};
use self::plan::{expected_wavs, title_speech_for_script};
use self::wav::{Wav, write_wav};
use crate::util::{atomic_write, head_chars};
use anyhow::{Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// concatenation and the final mix
// ---------------------------------------------------------------------------

/// Concatenate WAVs, optionally inserting `gap_ms` of silence between turns.
pub fn concat_wavs(files: &[PathBuf], out: &Path, gap_ms: u32) -> Result<()> {
    let mut params: Option<(u16, u32, u16)> = None;
    let mut frames: Vec<u8> = Vec::new();
    let mut gap: Vec<u8> = Vec::new();

    for f in files {
        let w = read_wav(f)?;
        let p = (w.channels, w.sample_rate, w.bits);
        match params {
            None => params = Some(p),
            Some(prev) if prev != p => anyhow::bail!(
                "{}: {p:?} != {prev:?} (mixed engines/rates — use per-engine seg dirs)",
                f.display()
            ),
            _ => {}
        }
        if gap_ms > 0 && !frames.is_empty() {
            if gap.is_empty() {
                let n = (w.sample_rate * gap_ms / 1000) as usize;
                gap = vec![0u8; n * w.channels as usize * (w.bits as usize / 8)];
            }
            frames.extend_from_slice(&gap);
        }
        frames.extend_from_slice(&w.data);
    }

    let (channels, sample_rate, bits) =
        params.ok_or_else(|| anyhow::anyhow!("no segments to concatenate"))?;
    write_wav(
        out,
        &Wav {
            channels,
            sample_rate,
            bits,
            data: frames,
        },
    )?;
    Ok(())
}

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn run_ffmpeg(args: &[&str]) -> Result<()> {
    let out = Command::new("ffmpeg")
        .args(args)
        .output()
        .context("spawning ffmpeg")?;
    if !out.status.success() {
        anyhow::bail!(
            "ffmpeg failed: {}",
            head_chars(&String::from_utf8_lossy(&out.stderr), 300)
        );
    }
    Ok(())
}

/// Assemble cached segments into the chapter deliverable.
///
/// `scratch` is a directory the caller owns and may delete afterwards. Every
/// intermediate — the concat, the ambience pass, the tempo pass — is written
/// there, and only the returned file is meant to survive. Pass
/// `Layout::scratch_ch(n)` so intermediates never land in `output/`.
///
/// Returns the mp3 when ffmpeg is available, otherwise the wav.
#[allow(clippy::too_many_arguments)]
pub fn assemble(
    script_path: &Path,
    cast_path: &Path,
    bible_path: &Path,
    seg_dir: &Path,
    scratch: &Path,
    gap_ms: u32,
    ambience: bool,
    speed: f64,
    engine: &str,
    assets: &Path,
) -> Result<PathBuf> {
    let policy = crate::cast::policy_for_bible(engine, bible_path)?;
    let cast = crate::cast::load_cast(script_path, cast_path, bible_path, &policy, false)?;
    let text = std::fs::read_to_string(script_path)
        .with_context(|| format!("reading {}", script_path.display()))?;
    let data: Value = serde_json::from_str(&text)?;
    let segments = data
        .get("segments")
        .and_then(|s| s.as_array())
        .cloned()
        .unwrap_or_default();
    let local = engine == "vieneu";
    let title = title_speech_for_script(script_path, &cast, &segments);

    let wavs = expected_wavs(&segments, &cast, seg_dir, local, title.as_ref())?;
    let missing: Vec<String> = wavs
        .iter()
        .filter(|w| !w.metadata().map(|m| m.len() > 1000).unwrap_or(false))
        .map(|w| w.file_name().unwrap_or_default().to_string_lossy().to_string())
        .collect();
    if !missing.is_empty() {
        anyhow::bail!(
            "{} segments missing in {} (e.g. {}): run the render stage first",
            missing.len(),
            seg_dir.display(),
            missing[0]
        );
    }

    std::fs::create_dir_all(scratch)
        .with_context(|| format!("creating scratch {}", scratch.display()))?;
    let mut out_path = scratch.join("mix.wav");
    concat_wavs(&wavs, &out_path, gap_ms)?;

    if ambience {
        let mut scenes = if local {
            crate::ambience::run_scenes(&segments, &runs(&segments))
        } else {
            segments
                .iter()
                .map(|s| s.get("scene").and_then(|v| v.as_str()).unwrap_or("").to_string())
                .collect()
        };
        if title.is_some() {
            // The headline run leads the wav list; keep scenes aligned with
            // a dry span so ambience never slides onto the wrong turn.
            scenes.insert(0, String::new());
        }
        let amb_out = scratch.join("mix-amb.wav");
        out_path = crate::ambience::apply_ambience(
            &out_path, &scenes, &wavs, gap_ms, &amb_out, scratch, assets,
        )?;
    }

    if (speed - 1.0).abs() > f64::EPSILON {
        let sped = scratch.join(format!(
            "{}-x{speed}.wav",
            out_path.file_stem().unwrap_or_default().to_string_lossy()
        ));
        run_ffmpeg(&[
            "-y",
            "-loglevel",
            "error",
            "-i",
            &out_path.to_string_lossy(),
            "-filter:a",
            &format!("atempo={speed}"),
            &sped.to_string_lossy(),
        ])?;
        out_path = sped;
    }

    if ffmpeg_available() {
        let mp3 = out_path.with_extension("mp3");
        run_ffmpeg(&[
            "-y",
            "-loglevel",
            "error",
            "-i",
            &out_path.to_string_lossy(),
            &mp3.to_string_lossy(),
        ])?;
        Ok(mp3)
    } else {
        Ok(out_path)
    }
}

/// Rewrite a chapter's title into the output filename, matching the legacy
/// `Ch.N - Title.mp3` convention.
pub fn publish(assembled: &Path, layout: &crate::Layout, n: u32) -> Result<PathBuf> {
    let final_path = layout.final_mp3(n);
    if let Some(parent) = final_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(assembled, &final_path)
        .with_context(|| format!("publishing to {}", final_path.display()))?;
    Ok(final_path)
}

/// Record a manifest line (JSONL) so every render is auditable, as before.
pub fn manifest_append(path: &Path, record: &Value) -> Result<()> {
    let mut line = serde_json::to_string(record)?;
    line.push('\n');
    let mut existing = std::fs::read_to_string(path).unwrap_or_default();
    existing.push_str(&line);
    atomic_write(path, &existing)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bm-assemble-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn concat_inserts_the_requested_gap() {
        let d = tmpdir("concat");
        let a = d.join("a.wav");
        let b = d.join("b.wav");
        silent_wav(&a, 0.10, 24_000).unwrap();
        silent_wav(&b, 0.10, 24_000).unwrap();
        let o = d.join("o.wav");
        concat_wavs(&[a, b], &o, 100).unwrap();
        let w = read_wav(&o).unwrap();
        // 0.10s + 0.10s gap + 0.10s
        assert_eq!(w.frames(), 24_000 * 3 / 10);
    }

    #[test]
    fn concat_refuses_mixed_rates() {
        let d = tmpdir("mixed");
        let a = d.join("a.wav");
        let b = d.join("b.wav");
        silent_wav(&a, 0.05, 24_000).unwrap();
        silent_wav(&b, 0.05, 48_000).unwrap();
        let err = concat_wavs(&[a, b], &d.join("o.wav"), 0).unwrap_err();
        assert!(err.to_string().contains("mixed engines"), "{err}");
    }
}
