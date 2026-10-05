//! Pipeline inputs: the acts manifest and the cue sidecars.
//!
//! The manifest names the grouping and the presentation (an act title); the cue
//! sidecars carry every line's interval on the delivered clock.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Deserialize)]
pub struct Act {
    pub act: u32,
    pub title: String,
    pub chapters: Vec<u32>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Manifest {
    #[serde(default)]
    pub name: Option<String>,
    pub acts: Vec<Act>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Cue {
    pub speaker: String,
    pub start: f64,
    pub end: f64,
    pub text: String,
}

/// One published chapter: its audio, its cues, and where it lands on the clock.
#[derive(Clone, Debug)]
pub struct Chapter {
    pub act: u32,
    pub mp3: PathBuf,
    pub cues: Vec<Cue>,
    pub dur: f64,
    pub start: f64,
}

impl Manifest {
    /// A list of acts, or a single act (`{act, title, chapters}`).
    pub fn load(path: &Path) -> Result<Manifest> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading acts manifest {}", path.display()))?;
        let value: serde_json::Value =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if value.get("acts").is_some() {
            return serde_json::from_str(&text)
                .with_context(|| format!("parsing {}", path.display()));
        }
        #[derive(Deserialize)]
        struct One {
            #[serde(default)]
            act: Option<u32>,
            #[serde(default)]
            name: Option<String>,
            title: String,
            chapters: Vec<u32>,
        }
        let one: One = serde_json::from_str(&text)
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok(Manifest {
            name: one.name,
            acts: vec![Act { act: one.act.unwrap_or(1), title: one.title, chapters: one.chapters }],
        })
    }
}

#[derive(Deserialize)]
struct CueDoc {
    cues: Vec<Cue>,
}

pub fn load_cues(path: &Path) -> Result<Vec<Cue>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading cue sidecar {}", path.display()))?;
    Ok(serde_json::from_str::<CueDoc>(&text)
        .with_context(|| format!("parsing {}", path.display()))?
        .cues)
}

/// The published mp3 for chapter `n`, and its cue sidecar beside it.
pub fn find_chapter(workspace: &Path, n: u32) -> Result<(PathBuf, PathBuf)> {
    let dir = workspace.join("output");
    let prefix = format!("Ch.{n} - ");
    let mut hits: Vec<PathBuf> = Vec::new();
    let entries = std::fs::read_dir(&dir)
        .with_context(|| format!("no published output under {}", dir.display()))?;
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with(&prefix) && name.ends_with(".mp3") {
            hits.push(e.path());
        }
    }
    hits.sort();
    let mp3 = hits
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no published mp3 for chapter {n} under {}", dir.display()))?;
    let stem = mp3
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let cues = mp3.with_file_name(format!("{stem}.cues.json"));
    if !cues.exists() {
        bail!(
            "chapter {n} has no cue sidecar ({}) — merge it first",
            cues.file_name().unwrap_or_default().to_string_lossy()
        );
    }
    Ok((mp3, cues))
}
