//! Pointing a workspace's stage folders elsewhere: the plan, the settings
//! write, and moving what the old folder already holds.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;

/// The two folders an operator may move: finished pairs, rendered takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageDir {
    Output,
    Segments,
}

impl StageDir {
    /// The settings field this folder is kept in.
    pub fn key(self) -> &'static str {
        match self {
            StageDir::Output => "output_dir",
            StageDir::Segments => "segments_dir",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            StageDir::Output => "output",
            StageDir::Segments => "segment",
        }
    }

    /// What lands there, for a status line.
    pub fn lands(self) -> &'static str {
        match self {
            StageDir::Output => "merge",
            StageDir::Segments => "render",
        }
    }

    pub fn other(self) -> StageDir {
        match self {
            StageDir::Output => StageDir::Segments,
            StageDir::Segments => StageDir::Output,
        }
    }
}

/// One stage folder pointed somewhere else, resolved but not yet saved.
#[derive(Debug, Clone)]
pub struct StageDirChange {
    pub kind: StageDir,
    /// As typed: the settings file keeps this, and empty means the default.
    pub raw: String,
    pub from: PathBuf,
    pub to: PathBuf,
}

/// How many top-level entries a move carried.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Moved {
    pub entries: usize,
}

impl crate::Layout {
    /// The folder a stage writes into today.
    pub fn stage_dir(&self, kind: StageDir) -> PathBuf {
        match kind {
            StageDir::Output => self.output(),
            StageDir::Segments => self.audio(),
        }
    }

    fn default_stage_dir(&self, kind: StageDir) -> PathBuf {
        match kind {
            StageDir::Output => self.work.join("output"),
            StageDir::Segments => self.data().join("audio"),
        }
    }

    /// Resolve a typed folder for one stage. The two stages may not share a
    /// folder: a merge writes into the output folder while it reads takes
    /// from the segment folder.
    pub fn plan_stage_dir(&self, kind: StageDir, raw: &str) -> Result<StageDirChange> {
        let raw = raw.trim().to_string();
        let to =
            Self::resolve_dir(&self.work, &raw).unwrap_or_else(|| self.default_stage_dir(kind));
        let other = self.stage_dir(kind.other());
        if Self::same_dir(&to, &other) {
            bail!(
                "{} folder {} is the other stage's folder - point one of them elsewhere",
                kind.label(),
                to.display()
            );
        }
        Ok(StageDirChange {
            kind,
            raw,
            from: self.stage_dir(kind),
            to,
        })
    }

    /// Write the change into this workspace's settings, creating the folder:
    /// a stage pointed at a missing directory would fail on its first write.
    pub fn apply_stage_dir(&self, change: &StageDirChange) -> Result<()> {
        std::fs::create_dir_all(&change.to)
            .with_context(|| format!("creating {}", change.to.display()))?;
        let path = self.settings();
        let mut settings = crate::config::Settings::load(&path);
        match change.kind {
            StageDir::Output => settings.output_dir = change.raw.clone(),
            StageDir::Segments => settings.segments_dir = change.raw.clone(),
        }
        settings
            .save(&path)
            .with_context(|| format!("saving {}", path.display()))
    }

    /// Take everything the old folder holds into the new one, which must be
    /// absent or empty: two partial sets in one folder would read as one book
    /// with takes missing and repeated. Nothing moves when the pairing cannot
    /// work, and a failed entry puts back the ones already gone.
    pub fn move_stage_dir(&self, change: &StageDirChange) -> Result<Moved> {
        let (from, to) = (&change.from, &change.to);
        if Self::same_dir(from, to) || !from.is_dir() {
            return Ok(Moved::default());
        }
        if Self::overlaps(from, to) {
            bail!(
                "{} overlaps {} - pick a folder that is neither inside it nor holds it",
                to.display(),
                from.display()
            );
        }
        let other = self.stage_dir(change.kind.other());
        if Self::inside(&other, from) {
            bail!(
                "the {} folder {} sits inside {} - move it out first",
                change.kind.other().label(),
                other.display(),
                from.display()
            );
        }
        let entries: Vec<PathBuf> = std::fs::read_dir(from)
            .with_context(|| format!("reading {}", from.display()))?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        if entries.is_empty() {
            let _ = std::fs::remove_dir(from);
            std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
            return Ok(Moved::default());
        }
        if to.exists() {
            refuse_occupied(to)?;
        } else {
            // Nobody else has this name yet: one rename carries the whole
            // tree, atomic on one device and through the `.incoming` copy
            // across devices. A failure leaves `from` whole, so the entry
            // loop below is a clean second try.
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            if crate::util::move_entry(from, to).is_ok() {
                return Ok(Moved {
                    entries: entries.len(),
                });
            }
        }
        std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
        let mut moved: Vec<(PathBuf, PathBuf)> = Vec::new();
        for src in entries {
            let Some(name) = src.file_name() else {
                continue;
            };
            let dst = to.join(name);
            if let Err(e) = crate::util::move_entry(&src, &dst) {
                for (back_from, back_to) in moved.iter().rev() {
                    let _ = crate::util::move_entry(back_from, back_to);
                }
                return Err(e).with_context(|| format!("moving into {}", to.display()));
            }
            moved.push((dst, src));
        }
        // Every entry left, so the folder itself goes too: a move leaves no
        // empty husk where the stage used to write. It only succeeds when
        // nothing new appeared meanwhile.
        let _ = std::fs::remove_dir(from);
        Ok(Moved {
            entries: moved.len(),
        })
    }
}

fn refuse_occupied(to: &std::path::Path) -> Result<()> {
    let names: Vec<String> = std::fs::read_dir(to)
        .with_context(|| format!("reading {}", to.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    if names.is_empty() {
        return Ok(());
    }
    let mut listed = names.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
    if names.len() > 3 {
        listed.push_str(", ...");
    }
    bail!(
        "{} is not empty ({} item(s): {}) - clear it first, or point at a folder that is",
        to.display(),
        names.len(),
        listed
    )
}

#[cfg(test)]
mod tests;
