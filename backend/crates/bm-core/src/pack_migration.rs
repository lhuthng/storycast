//! Applying a pack's rename history to a workspace's scripts.
//!
//! A rename is the one change a content hash cannot express, so the pack ships
//! it as intent: a cumulative `migrations.json` beside `assets/`. Applying it is
//! verified against the live tree first — a workspace's own assets may have
//! kept the old name, and then the rewrite would be wrong.

use crate::audio_pool::{self, PoolKind};
use crate::paths::Layout;
use crate::util::write_json;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Where a bundle carries its rename history, beside `assets/`.
pub const MIGRATIONS_FILE: &str = "migrations.json";

/// The newest document this build understands.
pub const FORMAT: u32 = 1;

/// One pack's rename history. Cumulative: a workspace that skipped versions
/// still converges, because an old name maps straight to the current one.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Migrations {
    #[serde(default)]
    pub format: u32,
    #[serde(default)]
    pub pack: String,
    #[serde(default)]
    pub history: Vec<Step>,
}

/// One release's renames. Only names a script holds: palette values and inject
/// sounds. Pool keys are held by nobody, so they never appear here.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Step {
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub palette: BTreeMap<String, String>,
    #[serde(default)]
    pub inject: BTreeMap<String, String>,
}

/// The merged map every replaced pack agrees on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Renames {
    pub palette: BTreeMap<String, String>,
    pub inject: BTreeMap<String, String>,
}

impl Renames {
    pub fn is_empty(&self) -> bool {
        self.palette.is_empty() && self.inject.is_empty()
    }
}

/// Which layer a rename belongs to, as a message spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    Palette,
    Inject,
}

impl Layer {
    pub fn noun(self) -> &'static str {
        match self {
            Layer::Palette => "palette",
            Layer::Inject => "inject",
        }
    }
}

/// One rename, with the decision `apply` reached about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub layer: Layer,
    pub from: String,
    pub to: String,
}

impl Change {
    pub fn line(&self, verb: &str) -> String {
        format!("{verb} {} {} -> {}", self.layer.noun(), self.from, self.to)
    }
}

/// What an apply did. Empty when the workspace had no script holding an old name.
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub applied: Vec<Change>,
    pub skipped: Vec<Change>,
    pub missing: Vec<Change>,
    pub chapters: Vec<u32>,
    pub segments: usize,
}

impl Report {
    pub fn changed(&self) -> bool {
        !self.chapters.is_empty()
    }

    pub fn summary(&self) -> String {
        let mut parts = vec![format!(
            "{} rename(s), {} chapter(s), {} segment(s)",
            self.applied.len(),
            self.chapters.len(),
            self.segments
        )];
        if !self.skipped.is_empty() {
            parts.push(format!("{} kept by this workspace", self.skipped.len()));
        }
        if !self.missing.is_empty() {
            parts.push(format!("{} unresolved", self.missing.len()));
        }
        parts.join("; ")
    }

    /// One line per decision, for the operator.
    pub fn lines(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .applied
            .iter()
            .map(|c| c.line("renamed"))
            .collect();
        out.extend(self.skipped.iter().map(|c| c.line("kept")));
        out.extend(self.missing.iter().map(|c| c.line("unresolved")));
        out
    }
}

/// Read a bundle's `migrations.json`. A tree without one has no renames.
pub fn read(tree: &Path) -> Result<Option<Migrations>> {
    let path = tree.join(MIGRATIONS_FILE);
    if !path.is_file() {
        return Ok(None);
    }
    read_file(&path).map(Some)
}

/// [`read`] for a path the fetcher kept beside the tree.
pub fn read_file(path: &Path) -> Result<Migrations> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let doc: Migrations =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    if doc.format > FORMAT {
        bail!(
            "{} is format {}, newer than this build reads ({FORMAT}) — update the binary",
            path.display(),
            doc.format
        );
    }
    Ok(doc)
}

/// Merge every replaced pack's history. Two packs disagreeing about one old
/// name is refused, because no script can satisfy both.
pub fn merge(docs: &[Migrations]) -> Result<Renames> {
    let mut out = Renames::default();
    for doc in docs {
        for step in &doc.history {
            for (from, to) in &step.palette {
                put(
                    &mut out.palette,
                    from,
                    to,
                    &doc.pack,
                    &step.version,
                    "palette",
                )?;
            }
            for (from, to) in &step.inject {
                put(&mut out.inject, from, to, &doc.pack, &step.version, "inject")?;
            }
        }
    }
    out.palette = chained(&out.palette)?;
    out.inject = chained(&out.inject)?;
    Ok(out)
}

fn put(
    map: &mut BTreeMap<String, String>,
    from: &str,
    to: &str,
    pack: &str,
    version: &str,
    layer: &str,
) -> Result<()> {
    if from == to {
        bail!("{pack} {version}: {layer} rename {from} -> {to} names the same thing");
    }
    if let Some(was) = map.get(from) {
        if was != to {
            bail!(
                "{pack} {version}: {layer} {from} is renamed to both {was} and {to} — \
                 two packs in this composition disagree"
            );
        }
        return Ok(());
    }
    map.insert(from.to_string(), to.to_string());
    Ok(())
}

/// `a -> b` plus `b -> c` is `a -> c`: a history that appended one release at a
/// time still lands an old script on the current name in one pass.
fn chained(map: &BTreeMap<String, String>) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for from in map.keys() {
        let mut to = map[from].clone();
        let mut hops = 0;
        while let Some(next) = map.get(&to) {
            to = next.clone();
            hops += 1;
            if hops > 32 {
                bail!("rename history loops at {from} — refusing to guess where it ends");
            }
        }
        out.insert(from.clone(), to);
    }
    Ok(out)
}

/// Rewrite the workspace's scripts, but only where the live tree proves the
/// rename landed: `new` is there and `old` is not.
pub fn apply(layout: &Layout, renames: &Renames) -> Result<Report> {
    let mut report = Report::default();
    if renames.is_empty() {
        return Ok(report);
    }
    let map = crate::ambience::load_map(&layout.scene_map()).unwrap_or_default();
    let palette: BTreeSet<&str> = map.music_palette.keys().map(String::as_str).collect();
    let inject = audio_pool::load_pool(&layout.pool(PoolKind::Inject));
    let inject_keys: BTreeSet<&str> = inject.keys().map(String::as_str).collect();

    let mut active = Renames::default();
    for (from, to) in &renames.palette {
        let change = Change {
            layer: Layer::Palette,
            from: from.clone(),
            to: to.clone(),
        };
        match settle(from, to, &palette) {
            Verdict::Apply => {
                active.palette.insert(from.clone(), to.clone());
                report.applied.push(change);
            }
            Verdict::Keep => report.skipped.push(change),
            Verdict::Missing => report.missing.push(change),
        }
    }
    for (from, to) in &renames.inject {
        let change = Change {
            layer: Layer::Inject,
            from: from.clone(),
            to: to.clone(),
        };
        match settle(from, to, &inject_keys) {
            Verdict::Apply => {
                active.inject.insert(from.clone(), to.clone());
                report.applied.push(change);
            }
            Verdict::Keep => report.skipped.push(change),
            Verdict::Missing => report.missing.push(change),
        }
    }
    if active.is_empty() {
        return Ok(report);
    }

    for path in layout.scripts() {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let mut doc: Value = serde_json::from_str(&text)
            .with_context(|| format!("parsing {}", path.display()))?;
        let touched = rewrite(&mut doc, &active);
        if touched == 0 {
            continue;
        }
        write_json(&path, &doc)?;
        report.segments += touched;
        if let Some(chapter) = crate::paths::chapter_of(&path) {
            report.chapters.push(chapter);
        }
    }
    Ok(report)
}

enum Verdict {
    Apply,
    Keep,
    Missing,
}

fn settle(from: &str, to: &str, live: &BTreeSet<&str>) -> Verdict {
    if live.contains(to) && !live.contains(from) {
        Verdict::Apply
    } else if live.contains(from) {
        Verdict::Keep
    } else {
        Verdict::Missing
    }
}

/// Rewrite one script's `music` and its named injects. Returns the segments
/// touched. `sound_tags` / `stop_tags` are queries, not names, so they stay.
fn rewrite(doc: &mut Value, active: &Renames) -> usize {
    let Some(segments) = doc.get_mut("segments").and_then(Value::as_array_mut) else {
        return 0;
    };
    let mut touched = 0;
    for seg in segments {
        let Some(obj) = seg.as_object_mut() else {
            continue;
        };
        let mut changed = false;
        let music = obj
            .get("music")
            .and_then(Value::as_str)
            .and_then(|old| active.palette.get(old.trim()))
            .cloned();
        if let Some(new) = music {
            obj.insert("music".into(), Value::String(new));
            changed = true;
        }
        for key in ["sound", "stop"] {
            let next = obj
                .get(key)
                .and_then(Value::as_str)
                .and_then(|old| active.inject.get(old.trim()))
                .cloned();
            if let Some(new) = next {
                obj.insert(key.into(), Value::String(new));
                changed = true;
            }
        }
        if changed {
            touched += 1;
        }
    }
    touched
}

#[cfg(test)]
mod tests;
