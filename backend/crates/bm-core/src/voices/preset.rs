//! Voice presets: a tagged list of slots, with no dependencies of its own. A
//! slot names a built clone and brings its take, or is `unnamed-N`: a role the
//! tags describe while the voice for it is still to be picked.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::tags;

/// The only schema this build writes.
pub const VERSION: u32 = 1;

/// One named selection of voices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoicePreset {
    #[serde(default = "default_version")]
    pub version: u32,
    /// Human label, shown by the picker and the manager.
    #[serde(default)]
    pub label: String,
    /// Advisory discovery metadata: `vi`, `en`, `adult`, `vieneu`, … It never
    /// gates a voice or an engine.
    #[serde(default)]
    pub category: Vec<String>,
    #[serde(default)]
    pub voices: Vec<PresetVoice>,
}

fn default_version() -> u32 {
    VERSION
}

/// One slot the preset offers. `voice` is a clone under the pack's
/// `voices/clones/`, or `unnamed-N` when the slot is a role rather than a
/// voice; resolving a name is the caller's job because it depends on what the
/// engine has enrolled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresetVoice {
    pub voice: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

impl Default for VoicePreset {
    fn default() -> Self {
        Self {
            version: VERSION,
            label: String::new(),
            category: Vec::new(),
            voices: Vec::new(),
        }
    }
}

impl VoicePreset {
    /// Parse, normalize, and validate one preset.
    pub fn parse(text: &str) -> Result<Self> {
        let mut preset: Self = serde_json::from_str(text).context("parsing a voice preset")?;
        preset.normalize();
        preset.validate()?;
        Ok(preset)
    }

    /// Trim and canonicalize every tag, and dedupe voices.
    pub fn normalize(&mut self) {
        self.category = tags::normalized(std::mem::take(&mut self.category));
        let mut seen: Vec<String> = Vec::new();
        self.voices.retain_mut(|voice| {
            voice.voice = voice.voice.trim().to_string();
            voice.tags = tags::normalized(std::mem::take(&mut voice.tags));
            if voice.voice.is_empty() || seen.iter().any(|v| v == &voice.voice) {
                return false;
            }
            seen.push(voice.voice.clone());
            true
        });
    }

    /// What a save must refuse: no schema for this build, no label, an empty
    /// voice, or two tags that cannot describe one voice.
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.version == VERSION,
            "voice preset version {} is not supported (this build writes {VERSION})",
            self.version
        );
        anyhow::ensure!(
            !self.label.trim().is_empty(),
            "a voice preset needs a label"
        );
        let mut seen: Vec<&str> = Vec::new();
        for (i, voice) in self.voices.iter().enumerate() {
            anyhow::ensure!(
                !voice.voice.trim().is_empty(),
                "voice {} has no name",
                i + 1
            );
            anyhow::ensure!(
                !seen.contains(&voice.voice.as_str()),
                "voice {:?} is listed twice",
                voice.voice
            );
            seen.push(voice.voice.as_str());
            if let Some((a, b)) = tags::conflict(&voice.tags) {
                anyhow::bail!("voice {:?} carries both {a:?} and {b:?}", voice.voice);
            }
        }
        Ok(())
    }

    /// Read one preset from disk.
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("validating {}", path.display()))
    }

    /// Write one preset, normalized and atomically.
    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut doc = self.clone();
        doc.normalize();
        crate::util::atomic_write(path, &serde_json::to_string_pretty(&doc)?)
            .with_context(|| format!("writing {}", path.display()))
    }

    /// Whether this preset names no voices yet.
    pub fn is_empty(&self) -> bool {
        self.voices.is_empty()
    }
}

/// The file a preset of `name` lives in.
pub fn path(dir: &Path, name: &str) -> Result<PathBuf> {
    ensure_slug(name)?;
    Ok(dir.join(format!("{name}.json")))
}

/// Every readable preset under `dir`, keyed by file stem. A missing directory
/// is an empty set; a file that does not parse is an error naming the file.
pub fn read_dir(dir: &Path) -> Result<BTreeMap<String, VoicePreset>> {
    let mut out = BTreeMap::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|n| n.to_str()) else {
            continue;
        };
        if !slug_ok(name) {
            continue;
        }
        out.insert(name.to_string(), VoicePreset::load(&path)?);
    }
    Ok(out)
}

/// A preset name is a file name and a URL segment.
pub fn ensure_slug(name: &str) -> Result<()> {
    anyhow::ensure!(
        slug_ok(name),
        "not a voice preset name: {name:?} (lowercase letters, digits and hyphens)"
    );
    Ok(())
}

/// [`ensure_slug`], as a predicate.
pub fn slug_ok(name: &str) -> bool {
    !name.is_empty()
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

#[cfg(test)]
mod tests;
