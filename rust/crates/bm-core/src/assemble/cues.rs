//! The chapter's cue sheet: every line's interval on the delivered clock.
//!
//! Written beside the published mp3 so a video render never has to replay the
//! mix to find out when a line lands.

use anyhow::{Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// One script segment, in delivered seconds.
#[derive(Debug, Clone, Serialize)]
pub struct Cue {
    pub i: usize,
    pub speaker: String,
    pub start: f64,
    pub end: f64,
    pub text: String,
}

/// What lands in `output/Ch.N - Title.cues.json`.
#[derive(Debug, Clone, Serialize)]
pub struct Cues {
    pub version: u32,
    pub chapter: u32,
    pub duration_s: f64,
    pub cues: Vec<Cue>,
}

pub const CUES_VERSION: u32 = 2;

impl Cues {
    /// `texts` is parallel to `slots` and holds the script segments inside that
    /// turn: a local engine speaks consecutive lines of one speaker as a single
    /// wav, so a turn can carry several segments and the sheet splits the turn's
    /// span between them by length. A short list leaves the missing tails empty.
    pub fn build(chapter: u32, slots: &[crate::ambience::Slot], texts: &[Vec<String>]) -> Cues {
        let mut cues: Vec<Cue> = Vec::new();
        for (i, s) in slots.iter().enumerate() {
            let parts: &[String] = texts.get(i).map_or(&[], |p| p.as_slice());
            if parts.is_empty() {
                cues.push(Cue::new(cues.len() + 1, s, s.start, s.end, String::new()));
                continue;
            }
            let total: usize = parts.iter().map(|p| weight(p)).sum();
            let mut t = s.start;
            for (k, text) in parts.iter().enumerate() {
                let span = if k + 1 == parts.len() {
                    s.end - t
                } else {
                    (s.end - s.start) * weight(text) as f64 / total as f64
                };
                cues.push(Cue::new(cues.len() + 1, s, t, t + span, text.clone()));
                t += span;
            }
        }
        Cues {
            version: CUES_VERSION,
            chapter,
            duration_s: slots.last().map(|s| s.end).unwrap_or(0.0),
            cues,
        }
    }
}

impl Cue {
    fn new(i: usize, slot: &crate::ambience::Slot, start: f64, end: f64, text: String) -> Cue {
        Cue {
            i,
            speaker: slot.speaker.clone(),
            start,
            end,
            text,
        }
    }
}

/// Spoken weight of a segment: its letters, since a caption has to cover the
/// same stretch of speech the line does.
fn weight(text: &str) -> usize {
    text.chars().filter(|c| !c.is_whitespace()).count().max(1)
}

/// The sidecar that belongs to a published chapter: `Ch.N - Title.mp3` loses its
/// extension to `Ch.N - Title.cues.json`.
pub fn cues_path(mp3: &Path) -> PathBuf {
    mp3.with_extension("cues.json")
}

/// Write the sidecar atomically, so a killed write leaves no half a transcript.
pub fn write(path: &Path, cues: &Cues) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = path.with_file_name(format!("{name}.tmp"));
    std::fs::write(&tmp, serde_json::to_string_pretty(cues)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("placing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests;
