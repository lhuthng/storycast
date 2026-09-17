//! Stage 3/4 — segment planning, concatenation and the final mix.
//!
//! Ported from `synthesize.py`. The WAV handling is native (RIFF PCM 16-bit),
//! so the only external binary is `ffmpeg`, and only for ambience, tempo and
//! mp3 encoding — exactly as before.

mod mood;
mod plan;
mod wav;

pub use self::plan::{
    character_has_lines, drop_headline, expected_wavs, pick_exact, pick_rendered, plan_render,
    rendered_segments, runs, segment_miss, segments_complete, title_speech,
    title_speech_for_script, RenderedSegment, Run, MAX_SEGMENT_BYTES,
};
pub use self::wav::{read_wav, sample_rate_for, silent_wav, GEMINI_RATE, VIENEU_RATE};
use self::wav::{write_wav, Wav};
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

/// Concatenate a timeline, honouring each slot's own gap.
///
/// [`crate::ambience::timeline`] decided these offsets, so this writes exactly
/// the silence the layers believe is there. That is the point of the split: the
/// gap used to be one number read in two places (here and the span builder) and
/// stayed correct only because it was a constant. A planned beat is not a
/// constant, and a layers pass that assumed `gap_ms` would slide onto the wrong
/// turns by the length of every pause before it.
///
/// The gap after the last slot is not written: nothing follows it, and the
/// layer pass sizes its beds from the file it is given, so a trailing silence
/// would only make the chapter longer than the timeline claims.
pub fn concat_slots(slots: &[crate::ambience::Slot], out: &Path) -> Result<()> {
    let mut params: Option<(u16, u32, u16)> = None;
    let mut frames: Vec<u8> = Vec::new();

    for (i, slot) in slots.iter().enumerate() {
        let w = read_wav(&slot.wav)?;
        let p = (w.channels, w.sample_rate, w.bits);
        match params {
            None => params = Some(p),
            Some(prev) if prev != p => anyhow::bail!(
                "{}: {p:?} != {prev:?} (mixed engines/rates — use per-engine seg dirs)",
                slot.wav.display()
            ),
            _ => {}
        }
        frames.extend_from_slice(&w.data);
        if slot.gap_ms > 0 && i + 1 < slots.len() {
            let n = (w.sample_rate * slot.gap_ms / 1000) as usize;
            frames.resize(
                frames.len() + n * w.channels as usize * (w.bits as usize / 8),
                0,
            );
        }
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

/// Pair every wav the renderer produced with the scene, mood and speaker the
/// layers need.
///
/// Built from the same grouping [`expected_wavs`] used, in the same order, so
/// turn `i` is wav `i` by construction rather than by convention — a mismatch
/// here would silently slide the whole chapter's sound design onto the wrong
/// lines, so the count is checked instead of assumed.
///
/// The mood is resolved here rather than in the mixer because the grain differs
/// per engine: the local engine renders one wav per *run*, so a run's mood is
/// its majority value, while the cloud engine renders one wav per segment. Doing
/// it here keeps `Turn` a flat statement of fact for the layer pass.
fn plan_turns(
    segments: &[Value],
    wavs: &[PathBuf],
    local: bool,
    titled: bool,
    cfg: &crate::ambience::SceneMap,
) -> Result<Vec<crate::ambience::Turn>> {
    use crate::ambience::Turn;
    let segments = drop_headline(segments);
    let mut out: Vec<Turn> = Vec::new();

    if titled {
        // The headline opens the chapter before any scene is established: the
        // Narrator speaks it, and it carries no scene tag — so no reverb, no
        // bed, no music, and a scene change at the first real turn.
        out.push(Turn {
            wav: wavs[0].clone(),
            scene: String::new(),
            music: String::new(),
            speaker: "Narrator".into(),
        });
    }

    if local {
        let rs = runs(segments);
        let scenes = crate::ambience::run_scenes(segments, &rs);
        let musics = crate::ambience::run_music(segments, &rs, cfg);
        for (i, run) in rs.iter().enumerate() {
            out.push(Turn {
                wav: wavs[i + titled as usize].clone(),
                scene: scenes[i].clone(),
                music: musics[i].clone(),
                speaker: run.speaker.clone(),
            });
        }
    } else {
        for (i, s) in segments.iter().enumerate() {
            let scene = s
                .get("scene")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            out.push(Turn {
                wav: wavs[i + titled as usize].clone(),
                music: crate::ambience::resolve_music(
                    &scene,
                    s.get("music").and_then(|v| v.as_str()),
                    cfg,
                ),
                scene,
                speaker: s
                    .get("speaker")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            });
        }
    }

    if out.len() != wavs.len() {
        anyhow::bail!(
            "timeline has {} turns for {} rendered segments — the render plan and \
             the timeline disagree, so the layers would land on the wrong lines",
            out.len(),
            wavs.len()
        );
    }
    Ok(out)
}

/// Assemble cached segments into the chapter deliverable.
///
/// `scratch` is a directory the caller owns and may delete afterwards. Every
/// intermediate — the concat, the layer pass, the tempo pass — is written
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
    chapter: u32,
    gap_ms: u32,
    on: crate::ambience::LayerSwitch,
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
        .map(|w| {
            w.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string()
        })
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

    // The scene map is read here, not inside the layer pass, because it decides
    // two things the *timeline* needs: where the beats go, and what mood each
    // turn carries. A missing map degrades to "no sound design" rather than
    // failing a dry merge; a layered merge still fails loudly in `apply_layers`.
    let map = crate::ambience::load_map(&assets.join("scene-map.json")).unwrap_or_default();
    let turns = plan_turns(&segments, &wavs, local, title.is_some(), &map)?;
    // A beat is part of the sound design, so a dry chapter does not get one:
    // both layers off is an operator asking for a plain read of the text, and
    // silence inserted between the lines would be an edit they did not ask for.
    let pauses = if on.none() {
        std::collections::BTreeMap::new()
    } else {
        crate::ambience::plan_pauses(&turns, &map, speed)
    };
    let slots = crate::ambience::timeline(&turns, gap_ms, &pauses)?;

    let mut out_path = scratch.join("mix.wav");
    concat_slots(&slots, &out_path)?;

    // Tempo the SPEECH, then place the layers on the delivered clock.
    //
    // `atempo` used to run last, over the finished mix, so it sped the beds and
    // the music up along with the voice: a rain bed at 1.25x is a different
    // rain, and a loop stretched to fill its window no longer fits it. The
    // operator asked for faster *reading*, not a faster world.
    let mut slots = slots;
    if (speed - 1.0).abs() > f64::EPSILON {
        let sped = scratch.join("voice-sped.wav");
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
        // The layer pass reads `Slot::start`/`end`, so the timeline has to
        // become the one the listener will actually hear.
        crate::ambience::retime(&mut slots, speed);
    }

    if !on.none() {
        let amb_out = scratch.join("mix-amb.wav");
        out_path = crate::ambience::apply_layers(
            &out_path, &slots, chapter, on, &amb_out, scratch, assets,
        )?;
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
