//! Pipeline inputs: the acts manifest and the cue sidecars.
//!
//! The manifest names the grouping and the act titles; the sidecars carry every
//! line's interval on the delivered clock, and the delivered length.

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
    pub start: f64,
    pub end: f64,
    pub text: String,
    /// Who is reading this line. The sidecar spells it; the template maps it to
    /// a portrait.
    #[serde(default)]
    pub speaker: String,
}

/// `output/Ch.N - Title.cues.json`. Unknown keys (`version`, `chapter`, the
/// cues' `i`) are the pipeline's business, not this plugin's.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Cues {
    #[serde(default)]
    pub duration_s: f64,
    #[serde(default)]
    pub cues: Vec<Cue>,
}

impl Cues {
    /// The delivered length. `duration_s` is the mixer's own figure; the last
    /// cue's end covers a sidecar written before it was recorded.
    pub fn span(&self) -> f64 {
        let last = self.cues.last().map(|c| c.end).unwrap_or(0.0);
        self.duration_s.max(last).max(0.0)
    }
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

    /// The output stem: the manifest's name, else `acts-NN-NN`.
    pub fn default_name(&self) -> String {
        if let Some(named) = self.name.as_ref().filter(|n| !n.is_empty()) {
            return named.clone();
        }
        let numbers: Vec<u32> = self.acts.iter().map(|a| a.act).collect();
        format!(
            "acts-{:02}-{:02}",
            numbers.iter().min().copied().unwrap_or(1),
            numbers.iter().max().copied().unwrap_or(1)
        )
    }
}

pub fn load_cues(path: &Path) -> Result<Cues> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading cue sidecar {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
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
