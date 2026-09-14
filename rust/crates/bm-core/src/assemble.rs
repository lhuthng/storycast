//! Stage 3/4 — segment planning, concatenation and the final mix.
//!
//! Ported from `synthesize.py`. The WAV handling is native (RIFF PCM 16-bit),
//! so the only external binary is `ffmpeg`, and only for ambience, tempo and
//! mp3 encoding — exactly as before.

use crate::cast::Cast;
use crate::util::{atomic_write, head_chars};
use anyhow::{Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

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

// ---------------------------------------------------------------------------
// segment planning
// ---------------------------------------------------------------------------

/// A consecutive run of lines by one speaker — one TTS call.
#[derive(Debug, Clone)]
pub struct Run {
    pub speaker: String,
    pub idx: Vec<usize>,
}

/// Group consecutive same-speaker segments: one TTS call per run.
pub fn runs(segments: &[Value]) -> Vec<Run> {
    let mut out: Vec<Run> = Vec::new();
    for (i, seg) in segments.iter().enumerate() {
        let speaker = seg
            .get("speaker")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        match out.last_mut() {
            Some(last) if last.speaker == speaker => last.idx.push(i),
            _ => out.push(Run {
                speaker,
                idx: vec![i],
            }),
        }
    }
    out
}

fn seg_text(seg: &Value) -> &str {
    seg.get("text").and_then(|t| t.as_str()).unwrap_or("")
}

/// True when a segment is an embedded chapter headline ("Chương 12: ...").
/// ASCII-prefix scan only — safe on UTF-8 text.
pub fn is_headline(text: &str) -> bool {
    let rest = match text.trim_start().strip_prefix("Chương") {
        Some(r) => r,
        None => return false,
    };
    rest.trim_start().chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false)
}

/// Drop an embedded headline so it never plans twice: the synthetic title run
/// (see [`TitleSpeech`]) replaces it everywhere. Idempotent — every planning
/// entry point applies it to raw input, so all of them always agree.
pub fn drop_headline(segments: &[Value]) -> &[Value] {
    match segments.first() {
        Some(s) if is_headline(seg_text(s)) => &segments[1..],
        _ => segments,
    }
}

/// Filenames the renderer is expected to produce. Shared by the renderer, the
/// completeness check and the merger so they can never disagree.
pub fn expected_wavs(
    segments: &[Value],
    cast: &Cast,
    seg_dir: &Path,
    local: bool,
    title: Option<&TitleSpeech>,
) -> Result<Vec<PathBuf>> {
    let segments = drop_headline(segments);
    let mut out = Vec::new();
    if let Some(t) = title {
        out.push(seg_dir.join(format!("title_{}.wav", t.voice)));
    }
    if local {
        for run in runs(segments) {
            let a = run.idx[0];
            let b = run.idx[run.idx.len() - 1];
            let tag = if a == b {
                format!("{a:04}")
            } else {
                format!("{a:04}-{b:04}")
            };
            let voice = cast
                .get(&run.speaker)
                .ok_or_else(|| anyhow::anyhow!("cast has no voice for {:?}", run.speaker))?;
            out.push(seg_dir.join(format!("{tag}_{voice}.wav")));
        }
    } else {
        for (i, s) in segments.iter().enumerate() {
            let speaker = s.get("speaker").and_then(|v| v.as_str()).unwrap_or("");
            let voice = cast
                .get(speaker)
                .ok_or_else(|| anyhow::anyhow!("cast has no voice for {speaker:?}"))?;
            out.push(seg_dir.join(format!("{i:04}_{voice}.wav")));
        }
    }
    Ok(out)
}

/// One unit of render work, ready to hand to the TTS sidecar.
#[derive(Debug, Clone)]
pub struct RenderUnit {
    pub tag: String,
    pub dest: PathBuf,
    pub speaker: String,
    pub voice: String,
    pub text: String,
    pub temperature: f64,
    pub silence_p: f64,
    pub indices: Vec<usize>,
}

/// Mood -> (temperature, silence_p). Calm reads steady; hot moods swing wider
/// and pause harder. Retune here — filenames do not depend on these values.
pub const MOOD_TAKE: [(&str, f64, f64); 18] = [
    ("neutral", 0.80, 0.15),
    ("calm", 0.72, 0.12),
    ("reflective", 0.75, 0.18),
    ("grand", 0.85, 0.20),
    ("sarcastic", 0.85, 0.12),
    ("ironic", 0.85, 0.12),
    ("amused", 0.90, 0.12),
    ("excited", 0.92, 0.10),
    ("smug", 0.88, 0.12),
    ("happy", 0.90, 0.10),
    ("sad", 0.85, 0.22),
    ("angry", 0.90, 0.18),
    ("cold", 0.70, 0.15),
    ("stern", 0.72, 0.18),
    ("urgent", 0.92, 0.08),
    ("surprised", 0.90, 0.10),
    ("shocked", 0.90, 0.20),
    ("gossipy", 0.88, 0.10),
];

/// Normalize free-form digest moods onto the small acting vocabulary.
pub fn mood_cluster(mood: &str) -> String {
    let first = mood
        .split_whitespace()
        .next()
        .unwrap_or("neutral")
        .to_lowercase();
    let first = first.trim_matches(|c| c == ',' || c == '.').to_string();
    let first = if first.is_empty() {
        "neutral".to_string()
    } else {
        first
    };
    match first.as_str() {
        "flat" | "monotone" | "expository" | "narrative" | "matter-of-fact" | "mildly"
        | "observant" | "descriptive" | "indifferent" | "awkward" | "pragmatic" | "casual" => {
            "neutral".into()
        }
        "serene" | "unconcerned" | "reflective" => "calm".into(),
        "ironic" | "sarcastic" => "amused".into(),
        "helpless" | "resigned" | "subdued" | "pouting" => "sad".into(),
        "amazed" | "earnest" | "playful" | "pleading" | "happy" | "warm" | "grand" | "admiring" => {
            "excited".into()
        }
        "satisfied" | "self-satisfied" => "smug".into(),
        "hasty" => "urgent".into(),
        "aloof" | "arrogant" | "stern" | "strict" => "cold".into(),
        "annoyed" => "angry".into(),
        other => other.to_string(),
    }
}

pub fn take_for_mood(cluster: &str) -> (f64, f64) {
    MOOD_TAKE
        .iter()
        .find(|(m, _, _)| *m == cluster)
        .map(|(_, t, s)| (*t, *s))
        .unwrap_or((0.80, 0.15))
}

/// Hottest mood in the run wins — one expressive line should lift the whole breath.
/// No neutral floor: a uniformly calm run reads calm, not neutral.
pub fn mood_take(segments: &[Value], idx: &[usize]) -> (f64, f64) {
    let mut best: Option<(f64, f64)> = None;
    for i in idx {
        let mood = segments[*i].get("mood").and_then(|m| m.as_str()).unwrap_or("neutral");
        let cand = take_for_mood(&mood_cluster(mood));
        if best.is_none_or(|(t, _)| cand.0 > t) {
            best = Some(cand);
        }
    }
    best.unwrap_or((0.80, 0.15))
}

/// Plain joined text for local engines (the acting style is baked into the
/// voice, not the prompt).
pub fn run_text(segments: &[Value], idx: &[usize]) -> String {
    idx.iter()
        .map(|i| seg_text(&segments[*i]))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The spoken chapter headline ("Chương 46, <title>", Narrator). Digests
/// routinely drop the headline and concatenation would glue it to the first
/// line with no pause — so the headline is its own leading run with its own
/// cache file (`title_<voice>.wav`, never colliding with numeric tags) and
/// the normal inter-turn gap after it.
pub struct TitleSpeech {
    pub voice: String,
    pub text: String,
}

pub fn title_speech(
    layout: &crate::Layout,
    n: u32,
    cast: &Cast,
    first_text: &str,
) -> Option<TitleSpeech> {
    let title = layout.chapter_title(n);
    if title.is_empty() || title == format!("Chapter {n}") {
        return None; // no chapter text on disk — nothing truthful to say
    }
    // The planned first line already carries the headline (a second embedded
    // headline, or narration quoting the title): don't speak it twice.
    // Callers pass post-drop text; is_headline matches drop_headline exactly.
    if first_text.contains(title.as_str()) || is_headline(first_text) {
        return None;
    }
    let voice = cast.get("Narrator")?.clone();
    Some(TitleSpeech { voice, text: format!("Chương {n}, {title}") })
}

/// Same, when only the script path is known (merge path): the chapter number
/// comes from `script-NN.json`, the title from the sibling chapter text.
pub fn title_speech_for_script(script_path: &Path, cast: &Cast, segments: &[Value]) -> Option<TitleSpeech> {
    let stem = script_path.file_stem()?.to_str()?;
    let n: u32 = stem.strip_prefix("script-")?.parse().ok()?;
    let data_dir = script_path.parent()?;
    let layout = crate::Layout::new(data_dir.parent()?);
    let planned = drop_headline(segments);
    let first = planned.first().map(seg_text).unwrap_or("");
    title_speech(&layout, n, cast, first)
}

fn title_unit(seg_dir: &Path, title: &TitleSpeech) -> RenderUnit {
    RenderUnit {
        tag: "title".to_string(),
        dest: seg_dir.join(format!("title_{}.wav", title.voice)),
        speaker: "Narrator".to_string(),
        voice: title.voice.clone(),
        text: title.text.clone(),
        temperature: 0.80,
        silence_p: 0.15,
        indices: Vec::new(),
    }
}

/// Decide what to render, without rendering it. The agent turns each unit into
/// one call to the TTS sidecar.
pub fn plan_render(
    segments: &[Value],
    cast: &Cast,
    seg_dir: &Path,
    local: bool,
    title: Option<&TitleSpeech>,
) -> Result<Vec<RenderUnit>> {
    let segments = drop_headline(segments);
    let mut units = Vec::new();
    if let Some(t) = title {
        units.push(title_unit(seg_dir, t));
    }
    if local {
        for run in runs(segments) {
            let voice = cast
                .get(&run.speaker)
                .ok_or_else(|| anyhow::anyhow!("cast has no voice for {:?}", run.speaker))?
                .clone();
            let a = run.idx[0];
            let b = run.idx[run.idx.len() - 1];
            let tag = if a == b {
                format!("{a:04}")
            } else {
                format!("{a:04}-{b:04}")
            };
            let (temperature, silence_p) = mood_take(segments, &run.idx);
            units.push(RenderUnit {
                dest: seg_dir.join(format!("{tag}_{voice}.wav")),
                tag,
                speaker: run.speaker.clone(),
                voice,
                text: run_text(segments, &run.idx),
                temperature,
                silence_p,
                indices: run.idx.clone(),
            });
        }
    } else {
        for (i, s) in segments.iter().enumerate() {
            let speaker = s.get("speaker").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let voice = cast
                .get(&speaker)
                .ok_or_else(|| anyhow::anyhow!("cast has no voice for {speaker:?}"))?
                .clone();
            let mood = s.get("mood").and_then(|m| m.as_str()).unwrap_or("neutral");
            let (temperature, silence_p) = take_for_mood(&mood_cluster(mood));
            units.push(RenderUnit {
                dest: seg_dir.join(format!("{i:04}_{voice}.wav")),
                tag: format!("{i:04}"),
                speaker,
                voice,
                text: seg_text(s).to_string(),
                temperature,
                silence_p,
                indices: vec![i],
            });
        }
    }
    Ok(units)
}

/// True if every expected segment wav exists — the renderer's skip check and
/// the merger's ready check. Read-only.
pub fn segments_complete(
    script_path: &Path,
    cast_path: &Path,
    bible_path: &Path,
    seg_dir: &Path,
    engine: &str,
) -> bool {
    let Ok(text) = std::fs::read_to_string(script_path) else {
        return false;
    };
    let Ok(data) = serde_json::from_str::<Value>(&text) else {
        return false;
    };
    let Some(segments) = data.get("segments").and_then(|s| s.as_array()) else {
        return false;
    };
    if segments.is_empty() {
        return false;
    }
    let policy = crate::voices::policy_for(engine);
    let Ok(cast) = crate::cast::load_cast(script_path, cast_path, bible_path, &policy, false) else {
        return false;
    };
    let local = engine == "vieneu";
    let title = title_speech_for_script(script_path, &cast, segments);
    let Ok(wavs) = expected_wavs(segments, &cast, seg_dir, local, title.as_ref()) else {
        return false;
    };
    wavs.iter()
        .all(|w| w.metadata().map(|m| m.len() > 1000).unwrap_or(false))
}

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
/// Returns the mp3 when ffmpeg is available, otherwise the wav.
#[allow(clippy::too_many_arguments)]
pub fn assemble(
    script_path: &Path,
    cast_path: &Path,
    bible_path: &Path,
    seg_dir: &Path,
    out: &Path,
    gap_ms: u32,
    ambience: bool,
    speed: f64,
    engine: &str,
    assets: &Path,
) -> Result<PathBuf> {
    let policy = crate::voices::policy_for(engine);
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

    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    concat_wavs(&wavs, out, gap_ms)?;
    let mut out_path = out.to_path_buf();

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
        let amb_out = out_path.with_file_name(format!(
            "{}-amb.{}",
            out_path.file_stem().unwrap_or_default().to_string_lossy(),
            out_path.extension().unwrap_or_default().to_string_lossy()
        ));
        out_path = crate::ambience::apply_ambience(&out_path, &scenes, &wavs, gap_ms, &amb_out, assets)?;
    }

    if (speed - 1.0).abs() > f64::EPSILON {
        let sped = out_path.with_file_name(format!(
            "{}x{speed}{}",
            out_path.file_stem().unwrap_or_default().to_string_lossy(),
            out_path
                .extension()
                .map(|e| format!(".{}", e.to_string_lossy()))
                .unwrap_or_default()
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
    use serde_json::json;

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

    #[test]
    fn runs_group_consecutive_speakers() {
        let segs = vec![
            json!({"speaker": "A", "text": "1"}),
            json!({"speaker": "A", "text": "2"}),
            json!({"speaker": "B", "text": "3"}),
            json!({"speaker": "A", "text": "4"}),
        ];
        let r = runs(&segs);
        assert_eq!(r.len(), 3);
        assert_eq!(r[0].idx, vec![0, 1]);
        assert_eq!(r[1].idx, vec![2]);
        assert_eq!(r[2].idx, vec![3]);
    }

    #[test]
    fn expected_wavs_names_match_the_run_shape() {
        let segs = vec![
            json!({"speaker": "A", "text": "1"}),
            json!({"speaker": "A", "text": "2"}),
            json!({"speaker": "B", "text": "3"}),
        ];
        let mut cast = Cast::new();
        cast.insert("A".into(), "Đức Trí".into());
        cast.insert("B".into(), "Adam".into());
        let local = expected_wavs(&segs, &cast, Path::new("segs"), true, None).unwrap();
        assert!(local[0].ends_with("0000-0001_Đức Trí.wav"), "{:?}", local[0]);
        assert!(local[1].ends_with("0002_Adam.wav"), "{:?}", local[1]);
        let cloud = expected_wavs(&segs, &cast, Path::new("segs"), false, None).unwrap();
        assert!(cloud[0].ends_with("0000_Đức Trí.wav"));
        assert_eq!(cloud.len(), 3);
    }

    #[test]
    fn expected_wavs_errors_on_an_uncast_speaker() {
        let segs = vec![json!({"speaker": "Nobody", "text": "1"})];
        let err = expected_wavs(&segs, &Cast::new(), Path::new("s"), true, None).unwrap_err();
        assert!(err.to_string().contains("no voice for"), "{err}");
    }

    #[test]
    fn mood_cluster_normalizes_and_the_hottest_line_wins() {
        assert_eq!(mood_cluster("lazy calm"), "lazy");
        assert_eq!(mood_cluster("sarcastic"), "amused");
        assert_eq!(mood_cluster("descriptive, flat"), "neutral");
        assert_eq!(mood_cluster(""), "neutral");

        let segs = vec![
            json!({"speaker": "A", "text": "x", "mood": "calm"}),
            json!({"speaker": "A", "text": "y", "mood": "urgent"}),
        ];
        let (t, _s) = mood_take(&segs, &[0, 1]);
        assert_eq!(t, take_for_mood("urgent").0, "hottest mood must win");
    }

    #[test]
    fn plan_render_is_per_run_locally_and_per_line_in_the_cloud() {
        let segs = vec![
            json!({"speaker": "A", "text": "one", "mood": "calm"}),
            json!({"speaker": "A", "text": "two", "mood": "calm"}),
        ];
        let mut cast = Cast::new();
        cast.insert("A".into(), "Đức Trí".into());
        let local = plan_render(&segs, &cast, Path::new("s"), true, None).unwrap();
        assert_eq!(local.len(), 1);
        assert_eq!(local[0].tag, "0000-0001");
        assert_eq!(local[0].text, "one two");
        assert_eq!(local[0].temperature, take_for_mood("calm").0);

        let cloud = plan_render(&segs, &cast, Path::new("s"), false, None).unwrap();
        assert_eq!(cloud.len(), 2);
    }

    fn titled_layout(tag: &str, headline: &str) -> (PathBuf, crate::Layout) {
        let d = tmpdir(&format!("title-{tag}"));
        let l = crate::Layout::new(&d);
        std::fs::create_dir_all(l.chapters()).unwrap();
        std::fs::write(l.chapter_txt(7), format!("{headline}\n\nbody\n")).unwrap();
        (d, l)
    }

    #[test]
    fn headline_gets_its_own_leading_run_and_cache_file() {
        let (_d, l) = titled_layout("t", "Chương 7: Kiếm khí xung thiên. . .");
        let mut cast = Cast::new();
        cast.insert("Narrator".into(), "Đức Trí".into());
        cast.insert("A".into(), "Adam".into());
        let segs = vec![json!({"speaker": "A", "text": "mở đầu"})];
        let first = "mở đầu";
        let title = title_speech(&l, 7, &cast, first).unwrap();
        assert_eq!(title.text, "Chương 7, Kiếm khí xung thiên");
        assert_eq!(title.voice, "Đức Trí");
        let units = plan_render(&segs, &cast, Path::new("s"), true, Some(&title)).unwrap();
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].tag, "title");
        assert!(units[0].dest.ends_with("title_Đức Trí.wav"), "{:?}", units[0].dest);
        assert_eq!(units[1].tag, "0000");
        let wavs = expected_wavs(&segs, &cast, Path::new("s"), true, Some(&title)).unwrap();
        assert_eq!(wavs.len(), 2);
        assert!(wavs[0].ends_with("title_Đức Trí.wav"));
    }

    #[test]
    fn headline_skipped_when_the_digest_kept_its_own() {
        let (_d, l) = titled_layout("t", "Chương 7: Kiếm khí xung thiên");
        let mut cast = Cast::new();
        cast.insert("Narrator".into(), "Đức Trí".into());
        assert!(title_speech(&l, 7, &cast, "Kiếm khí xung thiên vang lên").is_none());
        // A quoted chapter number is dialogue, not a headline: title still spoken.
        assert!(title_speech(&l, 7, &cast, "\"Chương 7\" ai đó nói").is_some());
    }

    #[test]
    fn headline_skipped_without_chapter_text() {
        let d = tmpdir("title-missing");
        let l = crate::Layout::new(&d);
        let mut cast = Cast::new();
        cast.insert("Narrator".into(), "Đức Trí".into());
        assert!(title_speech(&l, 7, &cast, "mở đầu").is_none());
    }

    #[test]
    fn drop_headline_only_cuts_a_leading_chapter_heading() {
        let hl = || json!({"speaker": "Narrator", "text": "Chương 7: Kiếm khí xung thiên"});
        let body = || json!({"speaker": "A", "text": "mở đầu"});
        assert_eq!(drop_headline(&[hl(), body()]).len(), 1);
        assert_eq!(drop_headline(&[body(), hl()]).len(), 2); // headline later: kept
        assert_eq!(drop_headline(&[]).len(), 0);
        assert!(is_headline("  Chương 12: x"));
        assert!(!is_headline("Chương pháp này rất hay")); // no digits: content
        assert!(!is_headline("mở đầu"));
    }

    #[test]
    fn kept_headline_never_speaks_twice() {
        let (_d, l) = titled_layout("t2", "Chương 7: Kiếm khí xung thiên");
        let mut cast = Cast::new();
        cast.insert("Narrator".into(), "Đức Trí".into());
        cast.insert("A".into(), "Adam".into());
        // Digest kept "Chương 7: ..." as its first segment.
        let segs = vec![
            json!({"speaker": "Narrator", "text": "Chương 7: Kiếm khí xung thiên"}),
            json!({"speaker": "A", "text": "mở đầu"}),
        ];
        let planned = drop_headline(&segs);
        assert_eq!(planned.len(), 1);
        let first = planned[0].get("text").and_then(|t| t.as_str()).unwrap();
        let title = title_speech(&l, 7, &cast, first).unwrap();
        let units = plan_render(planned, &cast, Path::new("s"), true, Some(&title)).unwrap();
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].tag, "title");
        assert_eq!(units[0].text, "Chương 7, Kiếm khí xung thiên");
        // The embedded raw headline appears in no unit.
        assert!(!units.iter().any(|u| u.text.contains("Chương 7:")));
        let wavs = expected_wavs(&segs, &cast, Path::new("s"), true, Some(&title)).unwrap();
        assert_eq!(wavs.len(), 2);
        assert!(wavs[0].ends_with("title_Đức Trí.wav"));
    }

    #[test]
    fn stripped_and_kept_scripts_plan_the_same_runs() {
        let (_d, l) = titled_layout("t3", "Chương 7: Kiếm khí xung thiên");
        let mut cast = Cast::new();
        cast.insert("Narrator".into(), "Đức Trí".into());
        cast.insert("A".into(), "Adam".into());
        let body = vec![json!({"speaker": "A", "text": "mở đầu"})];
        let first = "mở đầu";
        let title = title_speech(&l, 7, &cast, first).unwrap();
        let a = plan_render(&body, &cast, Path::new("s"), true, Some(&title)).unwrap();
        let mut kept = vec![json!({"speaker": "Narrator", "text": "Chương 7: x"})];
        kept.extend(body.clone());
        let b = plan_render(&kept, &cast, Path::new("s"), true, Some(&title)).unwrap();
        assert_eq!(
            a.iter().map(|u| u.tag.clone()).collect::<Vec<_>>(),
            b.iter().map(|u| u.tag.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn segments_complete_is_false_until_every_wav_exists() {
        let d = tmpdir("complete");
        let script = d.join("script-01.json");
        std::fs::write(
            &script,
            r#"{"segments":[{"speaker":"A","text":"one"},{"speaker":"A","text":"two"}]}"#,
        )
        .unwrap();
        let cast = d.join("cast-vieneu.json");
        std::fs::write(&cast, r#"{"A":"Đức Trí"}"#).unwrap();
        let segs = d.join("segs");
        std::fs::create_dir_all(&segs).unwrap();
        assert!(!segments_complete(
            &script,
            &cast,
            &d.join("bible.json"),
            &segs,
            "vieneu"
        ));
        silent_wav(&segs.join("0000-0001_Đức Trí.wav"), 0.05, 48_000).unwrap();
        assert!(segments_complete(
            &script,
            &cast,
            &d.join("bible.json"),
            &segs,
            "vieneu"
        ));
    }

    #[test]
    fn sample_rates_are_engine_specific() {
        assert_eq!(sample_rate_for("vieneu"), 48_000);
        assert_eq!(sample_rate_for("gemini"), 24_000);
    }
}
