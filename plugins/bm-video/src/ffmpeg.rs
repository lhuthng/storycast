//! The ffmpeg surface: probe, audio build, chunk encode, concat, mux.
//!
//! Every call takes `-nostdin`, so a stray read of the caller's terminal can
//! never eat a frame; a chunk is one x264 encode fed rgb24 on stdin.

use anyhow::{bail, Context, Result};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::model::Chapter;

fn ffmpeg() -> Command {
    let mut c = Command::new("ffmpeg");
    c.args(["-y", "-hide_banner", "-loglevel", "error", "-nostdin"]);
    c
}

fn check(out: &Path, status: std::process::ExitStatus) -> Result<()> {
    if status.success() {
        Ok(())
    } else {
        bail!("ffmpeg failed ({status}) writing {}", out.display())
    }
}

/// Media duration in seconds, off the container.
pub fn duration(path: &Path) -> Result<f64> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .with_context(|| format!("probing {}", path.display()))?;
    if !out.status.success() {
        bail!("ffprobe failed for {}", path.display());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.trim()
        .parse::<f64>()
        .with_context(|| format!("ffprobe returned {:?} for {}", text.trim(), path.display()))
}

/// The chapter audio track, with `gap` seconds of silence between chapters.
pub fn build_audio(chapters: &[Chapter], gap: f64, total: f64, out: &Path) -> Result<()> {
    if chapters.is_empty() {
        bail!("no chapters to build audio from");
    }
    let n = chapters.len();
    let mut cmd = ffmpeg();
    for ch in chapters {
        cmd.arg("-i").arg(&ch.mp3);
    }
    let padded: Vec<String> = (0..n).map(|i| format!("[{i}:a]apad=pad_dur={gap:.3}[a{i}]")).collect();
    let mut filter = padded.join(";");
    let map = if n == 1 {
        "[a0]".to_string()
    } else {
        let labels: String = (0..n).map(|i| format!("[a{i}]")).collect();
        filter.push_str(&format!(";{labels}concat=n={n}:v=0:a=1[aud]"));
        "[aud]".to_string()
    };
    let span = format!("{total:.3}");
    cmd.args(["-filter_complex", &filter, "-map", &map, "-c:a", "aac", "-b:a", "256k", "-t", &span])
        .arg(out);
    let status = cmd.status().with_context(|| format!("spawning ffmpeg for {}", out.display()))?;
    check(out, status)
}

/// What one chunk is encoded with.
pub struct ChunkSpec<'a> {
    pub w: u32,
    pub h: u32,
    pub fps: u32,
    pub preset: &'a str,
    pub crf: u32,
    pub threads: usize,
}

/// One chunk: rgb24 frames on stdin, H.264 on disk, no audio.
pub fn encode_chunk<I: Iterator<Item = Vec<u8>>>(
    frames: I,
    spec: &ChunkSpec,
    out: &Path,
) -> Result<()> {
    let (w, h, fps) = (spec.w, spec.h, spec.fps);
    let size = format!("{w}x{h}");
    let rate = fps.to_string();
    let q = spec.crf.to_string();
    let jobs = spec.threads.to_string();
    // A chunk is concatenated by stream copy, so it must open on a keyframe and
    // end on a closed GOP with no scene-cut surprises at the seam.
    let params = format!("keyint={fps}:min-keyint={fps}:scenecut=0:open-gop=0");
    let mut child = ffmpeg()
        .args(["-f", "rawvideo", "-pix_fmt", "rgb24", "-s", &size, "-r", &rate, "-i", "pipe:0"])
        .args(["-c:v", "libx264", "-preset", spec.preset, "-crf", &q])
        .args(["-pix_fmt", "yuv420p", "-r", &rate, "-threads", &jobs, "-x264-params", &params])
        .arg(out)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("spawning ffmpeg to encode {}", out.display()))?;
    let mut pipe = child.stdin.take().context("ffmpeg stdin was not piped")?;
    for data in frames {
        if let Err(e) = pipe.write_all(&data) {
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                break; // the encoder gave up; its exit status is the real news
            }
            return Err(e).with_context(|| format!("writing frames to {}", out.display()));
        }
    }
    drop(pipe); // close stdin so ffmpeg flushes and exits
    let status = child.wait().with_context(|| format!("waiting for ffmpeg on {}", out.display()))?;
    check(out, status)
}

/// Concatenate the chunk files listed in `list`, stream-copied.
pub fn concat(list: &Path, out: &Path) -> Result<()> {
    let status = ffmpeg()
        .args(["-f", "concat", "-safe", "0", "-i"])
        .arg(list)
        .args(["-c", "copy"])
        .arg(out)
        .status()
        .with_context(|| format!("spawning ffmpeg to concat into {}", out.display()))?;
    check(out, status)
}

/// Mux the finished silent video with its audio track, both stream-copied.
pub fn mux(video: &Path, audio: &Path, out: &Path, total: f64) -> Result<()> {
    let span = format!("{total:.3}");
    let status = ffmpeg()
        .arg("-i")
        .arg(video)
        .arg("-i")
        .arg(audio)
        .args([
            "-map",
            "0:v:0",
            "-map",
            "1:a:0",
            "-c",
            "copy",
            "-t",
            &span,
            "-movflags",
            "+faststart",
        ])
        .arg(out)
        .status()
        .with_context(|| format!("spawning ffmpeg to mux {}", out.display()))?;
    check(out, status)
}
