//! Sound design: the three clip pools, what they are for, and what may not be

use crate::tui::style::Level;
use bm_core::ambience::{inject_mode, SceneMap, UseOf};
use bm_core::audio_pool::{self, ClipPool, PoolKind, Sound};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Which layer the editor is showing, and where the cursor is on it.
#[derive(Debug, Clone)]
pub(crate) struct SoundView {
    pub(crate) layer: PoolKind,
    pub(crate) cursor: usize,
    pub(crate) scroll: usize,
}

impl SoundView {
    pub(crate) fn new() -> Self {
        SoundView {
            layer: PoolKind::Effect,
            cursor: 0,
            scroll: 0,
        }
    }
}

/// Everything the screen reads, loaded off the UI thread in one go.
#[derive(Debug, Clone)]
pub(crate) struct SoundData {
    /// The checkout these registries came from. Kept so a save and a refresh
    pub(crate) root: PathBuf,
    pub(crate) pools: BTreeMap<PoolKind, ClipPool>,
    /// The scene map as loaded — its rules and palette are the first two
    pub(crate) map: SceneMap,
    /// What each pool's entries are still being used for, by layer. Derived,
    pub(crate) usage: BTreeMap<PoolKind, BTreeMap<String, Vec<UseOf>>>,
    /// Sound -> files the registry names that are not on disk, by layer. The
    pub(crate) missing: BTreeMap<PoolKind, BTreeMap<String, Vec<String>>>,
    /// Chapters whose script places each inject sound. Kept so a save can
    pub(crate) script_uses: BTreeMap<String, Vec<u32>>,
}

impl SoundData {
    /// Recompute everything derived from the pools and the map.
    pub(crate) fn refresh(&mut self) {
        let layout = bm_core::Layout::new(&self.root);
        self.usage.clear();
        self.usage.insert(
            PoolKind::Effect,
            bm_core::ambience::effect_usage(&self.map, &self.pools[&PoolKind::Effect]),
        );
        self.usage.insert(
            PoolKind::Music,
            bm_core::ambience::music_usage(&self.map, &self.pools[&PoolKind::Music]),
        );
        self.usage
            .insert(PoolKind::Inject, inject_usage_as_uses(&self.script_uses));
        self.missing.clear();
        for kind in PoolKind::ALL {
            self.missing
                .insert(kind, missing_files(&layout, &self.pools[&kind]));
        }
    }
}

/// One row of the pool table.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SoundRow {
    pub(crate) name: String,
    pub(crate) sound: Sound,
    /// Every rule, palette value or chapter that still reaches this sound.
    pub(crate) uses: Vec<UseOf>,
    /// Registry files that are not on disk.
    pub(crate) missing: Vec<String>,
}

impl SoundRow {
    pub(crate) fn in_use(&self) -> bool {
        !self.uses.is_empty()
    }

    /// How many takes the registry lists. Zero is not a sound the mix can play.
    pub(crate) fn takes(&self) -> usize {
        self.sound.files.len()
    }

    /// What the layer-specific columns say about this entry.
    pub(crate) fn shape(&self, layer: PoolKind) -> String {
        match layer {
            PoolKind::Effect => {
                if self.sound.looped {
                    "bed".to_string()
                } else {
                    "one-shot".to_string()
                }
            }
            PoolKind::Music => "loops".to_string(),
            PoolKind::Inject => {
                let mode = self.sound.mode.as_deref().unwrap_or("hit");
                let mut s = mode.to_string();
                if let Some(h) = self.sound.hold {
                    if mode == "trail" {
                        s.push_str(&format!(" {h}s"));
                    }
                }
                if self.sound.looped {
                    s.push_str(" · loops");
                }
                if let Some(d) = self.sound.dur_s {
                    s.push_str(&format!(" · {d}s"));
                }
                s
            }
        }
    }

    /// What the mix multiplies this entry's level by, or `None` when it plays at
    pub(crate) fn render_gain(&self) -> Option<f64> {
        inject_mode(self.sound.mode.as_deref().unwrap_or("hit"))
            .map(|m| m.gain())
            .filter(|g| *g != 1.0)
    }

    /// The compact form, for the table's status column: whether the entry is
    pub(crate) fn status(&self) -> (String, Level) {
        if !self.missing.is_empty() {
            return (
                format!("{} clip(s) missing", self.missing.len()),
                Level::Error,
            );
        }
        if self.in_use() {
            return (format!("in use by {}", self.uses.len()), Level::Warn);
        }
        ("removable".to_string(), Level::Ok)
    }

    /// The long form, for the sentence under the table: which rules, palette
    pub(crate) fn verdict(&self) -> (String, Level) {
        let mut parts: Vec<String> = Vec::new();
        if !self.missing.is_empty() {
            parts.push(format!("{} clip(s) missing", self.missing.len()));
        }
        if self.in_use() {
            parts.push(format!(
                "in use by {} — remove disabled",
                summarise(&self.uses, 2)
            ));
        }
        if parts.is_empty() {
            return ("unused — removable".to_string(), Level::Ok);
        }
        let level = if self.missing.is_empty() {
            Level::Warn
        } else {
            Level::Error
        };
        (parts.join(" · "), level)
    }
}

/// `scene rule "mountain, wind" [mountain]`, or the first few with a count.
pub(crate) fn summarise(uses: &[UseOf], keep: usize) -> String {
    let shown: Vec<String> = uses.iter().take(keep).map(UseOf::label).collect();
    let mut s = shown.join("; ");
    if uses.len() > keep {
        s.push_str(&format!(" (+{} more)", uses.len() - keep));
    }
    s
}

/// The rows for one layer, in name order (the pool is a `BTreeMap`).
pub(crate) fn rows(data: &SoundData, layer: PoolKind) -> Vec<SoundRow> {
    let empty_uses = BTreeMap::new();
    let empty_missing = BTreeMap::new();
    let usage = data.usage.get(&layer).unwrap_or(&empty_uses);
    let missing = data.missing.get(&layer).unwrap_or(&empty_missing);
    data.pools
        .get(&layer)
        .map(|pool| {
            pool.iter()
                .map(|(name, sound)| SoundRow {
                    name: name.clone(),
                    sound: sound.clone(),
                    uses: usage.get(name).cloned().unwrap_or_default(),
                    missing: missing.get(name).cloned().unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The layer's master gain, as the scene map has it. Shown beside the pool so
pub(crate) fn master_level(map: &SceneMap, layer: PoolKind) -> f64 {
    match layer {
        PoolKind::Effect => map.layers.effect.trim,
        PoolKind::Music => map.layers.music.level,
        PoolKind::Inject => map.layers.inject.level,
    }
}

// ---------------------------------------------------------------------------

/// Read every registry, the scene map and the scripts, and work out what each
pub(crate) fn load(layout: &bm_core::Layout) -> Result<SoundData, String> {
    if layout.root.as_os_str().is_empty() {
        return Err("no repo root — restart the TUI from a checkout".into());
    }
    if !layout.assets().is_dir() {
        return Err(format!(
            "no {} — the clip pools live beside the scene map",
            layout.assets().display()
        ));
    }
    let map = bm_core::ambience::load_map(&layout.scene_map())
        .map_err(|e| format!("scene map: {e:#}"))?;

    let mut pools = BTreeMap::new();
    for kind in PoolKind::ALL {
        pools.insert(kind, audio_pool::load_pool(&layout.pool(kind)));
    }

    let mut data = SoundData {
        root: layout.root.clone(),
        pools,
        map,
        usage: BTreeMap::new(),
        missing: BTreeMap::new(),
        script_uses: read_script_uses(layout),
    };
    data.refresh();
    Ok(data)
}

/// Chapter -> script, for the inject layer's usage. A script that will not
fn read_script_uses(layout: &bm_core::Layout) -> BTreeMap<String, Vec<u32>> {
    let mut scripts: Vec<(u32, serde_json::Value)> = Vec::new();
    for path in crate::tui::audition::script_files(layout) {
        let Some(n) = bm_core::paths::chapter_of(&path) else {
            continue;
        };
        if let Ok(doc) = bm_core::read_json::<serde_json::Value>(&path) {
            scripts.push((n, doc));
        }
    }
    bm_core::ambience::inject_usage(&scripts)
}

/// The inject usage in the same shape as the other two layers', so the screen
fn inject_usage_as_uses(script_uses: &BTreeMap<String, Vec<u32>>) -> BTreeMap<String, Vec<UseOf>> {
    script_uses
        .iter()
        .map(|(sound, chapters)| {
            let uses = chapters
                .iter()
                .map(|n| UseOf {
                    by: format!("ch{n:02}"),
                    tags: Vec::new(),
                })
                .collect();
            (sound.clone(), uses)
        })
        .collect()
}

fn missing_files(layout: &bm_core::Layout, pool: &ClipPool) -> BTreeMap<String, Vec<String>> {
    let mut out = BTreeMap::new();
    for (name, sound) in pool {
        let gone: Vec<String> = sound
            .files
            .iter()
            .filter(|f| !resolve_clip(layout, f).is_file())
            .cloned()
            .collect();
        if !gone.is_empty() {
            out.insert(name.clone(), gone);
        }
    }
    out
}

/// A registry path resolved against `assets/`, where every registry says its
pub(crate) fn resolve_clip(layout: &bm_core::Layout, file: &str) -> PathBuf {
    layout.assets().join(file)
}

// ---------------------------------------------------------------------------

/// The fields a layer's entry may carry, in the order the prompt shows them.
pub(crate) fn fields(layer: PoolKind) -> &'static [&'static str] {
    match layer {
        PoolKind::Effect => &["name", "files", "tags", "looped", "level"],
        PoolKind::Music => &["name", "files", "tags", "level"],
        PoolKind::Inject => &[
            "name", "files", "tags", "mode", "hold", "looped", "level", "dur_s",
        ],
    }
}

/// `name=… files=… tags=…` — the whole entry on one line.
pub(crate) fn describe(name: &str, sound: &Sound, layer: PoolKind) -> String {
    let mut parts = vec![
        format!("name={name}"),
        format!("files={}", sound.files.join(",")),
        format!("tags={}", sound.tags.join(",")),
    ];
    if let Some(m) = &sound.mode {
        if layer == PoolKind::Inject {
            parts.push(format!("mode={m}"));
        }
    }
    if layer == PoolKind::Inject {
        if let Some(h) = sound.hold {
            parts.push(format!("hold={h}"));
        }
    }
    if layer != PoolKind::Music {
        parts.push(format!("looped={}", sound.looped));
    }
    if let Some(l) = sound.level {
        parts.push(format!("level={l}"));
    }
    if layer == PoolKind::Inject {
        if let Some(d) = sound.dur_s {
            parts.push(format!("dur_s={d}"));
        }
    }
    parts.join(" ")
}

/// What the prompt's hint line lists, so an operator never has to guess which
pub(crate) fn fields_hint(layer: PoolKind) -> String {
    match layer {
        PoolKind::Effect => {
            "name=<sound> files=<a.mp3,b.mp3> tags=<t1,t2> [looped=true|false] [level=1.0]".into()
        }
        PoolKind::Music => "name=<sound> files=<a.mp3,b.mp3> tags=<t1,t2> [level=1.0]".into(),
        PoolKind::Inject => {
            "name=<sound> files=<a.mp3,b.mp3> tags=<t1,t2> [mode=hit|overlap|trail] [hold=2.0] [looped=true|false] [level=1.0] [dur_s=1.4]".into()
        }
    }
}

/// A level's legal range. Zero is excluded on purpose: `Sound::level` reads
pub(crate) const LEVEL_RANGE: (f64, f64) = (0.01, 4.0);

/// Parse one `key=value` line into a sound.
pub(crate) fn parse_entry(
    layer: PoolKind,
    buf: &str,
    renaming_from: Option<&str>,
) -> Result<(String, Sound), String> {
    let allowed = fields(layer);
    let mut name: Option<String> = None;
    let mut files: Option<Vec<String>> = None;
    let mut tags: Option<Vec<String>> = None;
    let mut looped: Option<bool> = None;
    let mut level: Option<f64> = None;
    let mut mode: Option<String> = None;
    let mut hold: Option<f64> = None;
    let mut dur_s: Option<f64> = None;

    for token in buf.split_whitespace() {
        let Some((key, value)) = token.split_once('=') else {
            return Err(format!(
                "“{token}” is not key=value — expected: {}",
                fields_hint(layer)
            ));
        };
        if !allowed.contains(&key) {
            return Err(format!(
                "{:?} is not a field of the {} layer — it takes {}",
                key,
                layer.label(),
                allowed.join(", ")
            ));
        }
        match key {
            "name" => {
                let n = value.trim();
                if n.is_empty() {
                    return Err("name is empty".into());
                }
                if n.starts_with('_') {
                    return Err(format!(
                        "a name may not start with “_” — that is how a registry marks its own notes ({n})"
                    ));
                }
                if n.contains(['/', '"', '\\']) {
                    return Err(format!("“{n}” may not contain a path separator or a quote"));
                }
                name = Some(n.to_string());
            }
            "files" => {
                let v = split_list(value);
                if v.is_empty() {
                    return Err("files is empty — a sound with no take can never play".into());
                }
                files = Some(v);
            }
            "tags" => {
                let v = split_list(value);
                if v.is_empty() {
                    return Err(
                        "tags is empty — nothing would ever reach this sound, so it could never play"
                            .into(),
                    );
                }
                tags = Some(v);
            }
            "looped" => looped = Some(parse_bool(value)?),
            "level" => level = Some(parse_level(value)?),
            "mode" => {
                // Asked of the mixer rather than of a list here: a fourth mode
                if inject_mode(value).is_none() {
                    return Err(format!(
                        "mode “{value}” unknown — hit (holds the whole clip), overlap (runs under the speech), trail (holds, then ducks). overlap and trail render at a tenth of their level"
                    ));
                }
                mode = Some(value.to_string());
            }
            "hold" => hold = Some(parse_positive(value, "hold")?),
            "dur_s" => dur_s = Some(parse_positive(value, "dur_s")?),
            _ => unreachable!("filtered by `allowed`"),
        }
    }

    let name = match (name, renaming_from) {
        (Some(n), Some(old)) if n != old => {
            return Err(format!(
                "the name is the pool's key — “{old}” cannot become “{n}” in place; remove it and add the new name"
            ))
        }
        (Some(n), _) => n,
        (None, Some(old)) => old.to_string(),
        (None, None) => return Err("name= is required when adding".into()),
    };
    let Some(files) = files else {
        return Err("files= is required — e.g. files=effects/rain-1.mp3".into());
    };
    let Some(tags) = tags else {
        return Err("tags= is required — what a scene matches on".into());
    };

    Ok((
        name,
        Sound {
            tags,
            files,
            looped: looped.unwrap_or(true),
            dur_s,
            mode,
            hold,
            level,
        },
    ))
}

/// A comma-separated list, trimmed, empty pieces dropped. A path never
fn split_list(v: &str) -> Vec<String> {
    v.split(',')
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .map(String::from)
        .collect()
}

fn parse_bool(v: &str) -> Result<bool, String> {
    match v {
        "true" | "yes" | "1" => Ok(true),
        "false" | "no" | "0" => Ok(false),
        _ => Err(format!("“{v}” is not true or false")),
    }
}

fn parse_level(v: &str) -> Result<f64, String> {
    let (lo, hi) = LEVEL_RANGE;
    let n: f64 = v
        .parse()
        .map_err(|_| format!("level “{v}” is not a number"))?;
    if !n.is_finite() || !(lo..=hi).contains(&n) {
        return Err(format!(
            "level must be {lo}–{hi}, got “{v}” (0 reads as “no trim”, so it is not a level)"
        ));
    }
    Ok(n)
}

fn parse_positive(v: &str, what: &str) -> Result<f64, String> {
    let n: f64 = v
        .parse()
        .map_err(|_| format!("{what} “{v}” is not a number"))?;
    if !n.is_finite() || n <= 0.0 {
        return Err(format!("{what} must be greater than 0, got “{v}”"));
    }
    Ok(n)
}

/// Parse the one-number level prompt. Empty clears the trim back to unity.
pub(crate) fn parse_level_prompt(buf: &str) -> Result<Option<f64>, String> {
    let t = buf.trim();
    if t.is_empty() {
        return Ok(None);
    }
    parse_level(t).map(Some)
}

/// Check the takes an edit *introduces*, under the layer's own clip directory.
pub(crate) fn check_files(root: &Path, layer: PoolKind, files: &[String]) -> Result<(), String> {
    let layout = bm_core::Layout::new(root);
    let prefix = format!("{}/", layer.dir());
    for f in files {
        if f.starts_with('/') || f.contains("..") {
            return Err(format!(
                "“{f}” must be relative to assets/ — e.g. {prefix}name-1.mp3"
            ));
        }
        if !f.starts_with(&prefix) {
            return Err(format!(
                "“{f}” is not in the {} layer's own directory — expected {prefix}…",
                layer.label()
            ));
        }
        let p = resolve_clip(&layout, f);
        if !p.is_file() {
            return Err(format!(
                "no such clip: {} — copy it into assets/{}/ first",
                p.display(),
                layer.dir()
            ));
        }
    }
    Ok(())
}

/// The takes an edit adds to an entry, i.e. the ones worth checking. Every take
pub(crate) fn introduced(old: Option<&Sound>, new: &Sound) -> Vec<String> {
    new.files
        .iter()
        .filter(|f| !old.map(|o| o.files.contains(f)).unwrap_or(false))
        .cloned()
        .collect()
}

/// Write one layer's registry back, and say what changed.
pub(crate) fn save(root: &Path, layer: PoolKind, pool: &ClipPool) -> Result<String, String> {
    if root.as_os_str().is_empty() {
        return Err("no repo root — restart the TUI from a checkout".into());
    }
    let path = bm_core::Layout::new(root).pool(layer);
    audio_pool::save_pool(&path, layer, pool).map_err(|e| format!("{e:#}"))?;
    Ok(format!(
        "{}: {} sound(s) written to {}",
        layer.label(),
        pool.len(),
        path.display()
    ))
}

/// Which take's length an inject's `dur_s` should be: the longest one.
pub(crate) fn probe_longest(root: &Path, sound: &Sound) -> Option<f64> {
    let layout = bm_core::Layout::new(root);
    let mut longest: Option<f64> = None;
    for f in &sound.files {
        let p = resolve_clip(&layout, f);
        let dur = std::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "csv=p=0",
                &p.to_string_lossy(),
            ])
            .output()
            .ok()
            .and_then(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .trim()
                    .parse::<f64>()
                    .ok()
            })
            .filter(|d| d.is_finite() && *d > 0.0);
        if let Some(d) = dur {
            longest = Some(longest.map_or(d, |l: f64| l.max(d)));
        }
    }
    longest
}

/// Fold one edit into a pool: insert or replace, and report what it was.
pub(crate) enum Edit {
    Added,
    Changed,
}

pub(crate) fn apply(pool: &mut ClipPool, name: &str, sound: Sound) -> Edit {
    match pool.insert(name.to_string(), sound) {
        None => Edit::Added,
        Some(_) => Edit::Changed,
    }
}

/// Write an edited entry to disk and into the screen, in that order.
pub(crate) fn commit(
    data: &mut SoundData,
    layer: PoolKind,
    name: String,
    sound: Sound,
) -> Result<String, String> {
    let mut pool = data.pools[&layer].clone();
    let edit = apply(&mut pool, &name, sound);
    save(&data.root, layer, &pool)?;
    data.pools.insert(layer, pool);
    data.refresh();
    Ok(match edit {
        Edit::Added => format!("{}: added “{name}”", layer.label()),
        Edit::Changed => format!("{}: “{name}” updated", layer.label()),
    })
}

/// Retune one sound, leaving every other field where it was. `None` clears the
pub(crate) fn set_level(
    data: &mut SoundData,
    layer: PoolKind,
    name: &str,
    level: Option<f64>,
) -> Result<String, String> {
    let mut pool = data.pools[&layer].clone();
    let Some(sound) = pool.get_mut(name) else {
        return Err(format!(
            "{name:?} is no longer in the {} pool",
            layer.label()
        ));
    };
    sound.level = level;
    let line = match level {
        Some(l) => format!("{}: “{name}” level {l}", layer.label()),
        None => format!("{}: “{name}” level cleared — back to 1.0", layer.label()),
    };
    save(&data.root, layer, &pool)?;
    data.pools.insert(layer, pool);
    data.refresh();
    Ok(line)
}

/// Take one entry out, on disk and in the screen.
pub(crate) fn remove(data: &mut SoundData, layer: PoolKind, name: &str) -> Result<String, String> {
    let mut pool = data.pools[&layer].clone();
    if pool.remove(name).is_none() {
        return Err(format!(
            "{name:?} is no longer in the {} pool",
            layer.label()
        ));
    }
    save(&data.root, layer, &pool)?;
    data.pools.insert(layer, pool);
    data.refresh();
    Ok(format!(
        "{}: removed “{name}” — its clips are still in assets/{}/",
        layer.label(),
        layer.dir()
    ))
}

#[cfg(test)]
mod tests;
