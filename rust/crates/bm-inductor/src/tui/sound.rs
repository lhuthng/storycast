//! Sound design: the three clip pools, what they are for, and what may not be
//! taken out of them.
//!
//! The three layers under the voice are independent — beds chosen by a scene's
//! *place*, tracks chosen by its *mood*, and spot effects the script places by
//! name — but they are managed the same way, so they are one screen with three
//! tabs rather than three screens. Everything here is pure: the file I/O is
//! [`load`] and [`save`], and the rest is what a key press and a paint need.
//!
//! **The rule this module exists for is the removal guard.** A scene names
//! *tags*; the pool answers with a *sound*. Delete the sound a rule reaches and
//! the scene does not fail — it scores zero and goes quiet, silently, chapter
//! after chapter. So an entry something still reaches is not removable, and
//! [`SoundRow::uses`] is what the screen shows instead of the key doing
//! nothing. "In use" is always a *reference*: the scene map's rules and palette
//! for the two tag layers, the scripts for the layer that names sounds
//! directly. Nothing here consults whether a merge happens to be running —
//! that is a different question with a different answer, and conflating them
//! would block editing for the whole of a long run.

use crate::tui::style::Level;
use bm_core::ambience::{SceneMap, UseOf};
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
    /// need nothing but this struct — the screen cannot be pointed at a
    /// different tree from the one it read.
    pub(crate) root: PathBuf,
    pub(crate) pools: BTreeMap<PoolKind, ClipPool>,
    /// The scene map as loaded — its rules and palette are the first two
    /// layers' usage, and its layer knobs are the master gain the header shows.
    pub(crate) map: SceneMap,
    /// What each pool's entries are still being used for, by layer. Derived,
    /// never stored: see [`SoundData::refresh`].
    pub(crate) usage: BTreeMap<PoolKind, BTreeMap<String, Vec<UseOf>>>,
    /// Sound -> files the registry names that are not on disk, by layer. The
    /// merge degrades these to silence with one warning; the editor can say so
    /// before the chapter is run.
    pub(crate) missing: BTreeMap<PoolKind, BTreeMap<String, Vec<String>>>,
    /// Chapters whose script places each inject sound. Kept so a save can
    /// recompute the inject usage without re-reading a hundred scripts.
    pub(crate) script_uses: BTreeMap<String, Vec<u32>>,
}

impl SoundData {
    /// Recompute everything derived from the pools and the map.
    ///
    /// Called after every edit, so the removal guard is never read from a
    /// stale copy: an entry that has just gained a reference must lose its
    /// remove key in the same frame that added the reference, or the guard is
    /// theatre. `script_uses` is not re-read — it comes from the scripts, and
    /// no edit here changes a script.
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
    /// Empty means it is removable.
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

    /// The compact form, for the table's status column: whether the entry is
    /// removable, and how much is against it. The column is the scanning
    /// surface — a full rule list there clips mid-quote and reads as noise.
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
    /// values or chapters, and what that means for the remove key.
    ///
    /// Deliberately a second rendering rather than the column's text widened:
    /// the two answer different questions — "is this one safe to touch" and
    /// "what exactly still reaches it" — and both read `uses`/`missing`, so
    /// they cannot disagree about the facts, only about how much of them fits.
    ///
    /// A missing clip outranks the removal guard in *colour*, not in wording:
    /// it is the more urgent fact (the merge will go silent on the next run
    /// whatever the operator does here), and both are shown.
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
/// the operator can see the whole chain a level sits in; the operator's own
/// multiplier on top of it is `:mix`, and the two are separate knobs on purpose.
pub(crate) fn master_level(map: &SceneMap, layer: PoolKind) -> f64 {
    match layer {
        PoolKind::Effect => map.layers.effect.trim,
        PoolKind::Music => map.layers.music.level,
        PoolKind::Inject => map.layers.inject.level,
    }
}

// ---------------------------------------------------------------------------
// loading
// ---------------------------------------------------------------------------

/// Read every registry, the scene map and the scripts, and work out what each
/// entry is still used for.
///
/// A job rather than a keypress handler: it is three registries, the map and
/// every `data/script-*.json` — the same hundred file opens the audition index
/// makes, and the same reason for keeping them off the UI task.
pub(crate) fn load(root: &Path) -> Result<SoundData, String> {
    if root.as_os_str().is_empty() {
        return Err("no repo root — restart the TUI from a checkout".into());
    }
    let layout = bm_core::Layout::new(root);
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
        root: root.to_path_buf(),
        pools,
        map,
        usage: BTreeMap::new(),
        missing: BTreeMap::new(),
        script_uses: read_script_uses(root),
    };
    data.refresh();
    Ok(data)
}

/// Chapter -> script, for the inject layer's usage. A script that will not
/// parse is skipped, like the audition index: one corrupt chapter must not cost
/// the operator the whole screen.
fn read_script_uses(root: &Path) -> BTreeMap<String, Vec<u32>> {
    let mut scripts: Vec<(u32, serde_json::Value)> = Vec::new();
    for path in crate::tui::audition::script_files(root) {
        let Some(n) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_prefix("script-"))
            .and_then(|n| n.strip_suffix(".json"))
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        if let Ok(doc) = bm_core::read_json::<serde_json::Value>(&path) {
            scripts.push((n, doc));
        }
    }
    bm_core::ambience::inject_usage(&scripts)
}

/// The inject usage in the same shape as the other two layers', so the screen
/// and the removal guard read one type. A chapter is a reason with no tags —
/// the script names the sound itself.
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
/// files are relative to.
pub(crate) fn resolve_clip(layout: &bm_core::Layout, file: &str) -> PathBuf {
    layout.assets().join(file)
}

// ---------------------------------------------------------------------------
// editing
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
///
/// The same shape as `:mix` and the run config: one line, `key=value`, order
/// free. It is prefilled with the values actually in force, so an edit is a
/// change to something visible rather than a retype from memory.
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
/// keys this layer takes.
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
/// `0.0` as *unset*, so accepting it here would write a mute that plays at
/// unity — a knob that lies. Muting is what the layer switch is for.
pub(crate) const LEVEL_RANGE: (f64, f64) = (0.01, 4.0);

/// Parse one `key=value` line into a sound.
///
/// `renaming_from` is the entry being edited, if any. The name is the pool's
/// key — a script names an inject by it and a re-merge reproduces a pick from
/// it — so a rename is refused rather than silently performed: remove and add.
/// Everything else is optional and keeps the default the mix would use; the
/// music layer has no `looped` flag at all, so a music entry is written with
/// the one its mixer assumes rather than with a flag nothing reads.
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
                if !["hit", "overlap", "trail"].contains(&value) {
                    return Err(format!(
                        "mode “{value}” unknown — hit (holds the whole clip), overlap (runs under the speech), trail (holds, then ducks)"
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
/// contains a space, so a spaced token is a typo and the caller says so.
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
///
/// Two separate refusals, and both are worth making: a path outside
/// `assets/<layer>/` is a category error the file would carry forever, and a
/// path that is not there is a window the merge degrades to silence with a
/// warning nobody reads. Caught at the prompt, where it is one keystroke.
///
/// Deliberately takes a list rather than a whole `Sound`, so the caller can
/// hand it only the paths the edit adds. Re-checking a take that was already
/// registered and has since gone missing would make that entry *uneditable* —
/// a tag change refused because of a clip somebody moved — and the entry is
/// already flagged in red on the screen, which is where that belongs.
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
/// of a brand-new entry is one of them.
pub(crate) fn introduced<'a>(old: Option<&Sound>, new: &'a Sound) -> Vec<String> {
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
///
/// `dur_s` is what the digest prompt renders so the analyzer never `hit`s a
/// 51-second boil, and what the validator checks a placement against — so it is
/// a *fact about the clip*, and a typed number drifts from the file the moment
/// the file is replaced. Probed with ffprobe, the same probe the merge uses.
/// `None` means nothing could be probed: the field is left as it was rather
/// than set to a guess.
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
///
/// The file is written first and the in-memory pool is only replaced if that
/// succeeded: a save that failed must leave the screen showing what is actually
/// on disk, or the next edit builds on a fiction.
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
/// trim back to unity — the one edit that has to be expressible without
/// restating the whole entry.
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
///
/// The clip files are left alone: the registry is the pool, and a clip is not
/// deleted by unregistering it. Re-adding the name brings the same takes back.
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
mod tests {
    use super::*;

    fn sound(files: &[&str], tags: &[&str]) -> Sound {
        Sound {
            tags: tags.iter().map(|t| t.to_string()).collect(),
            files: files.iter().map(|f| f.to_string()).collect(),
            looped: true,
            dur_s: None,
            mode: None,
            hold: None,
            level: None,
        }
    }

    /// A checkout with the shipped assets copied in, so `check_files` and
    /// `load` see the real registries without ever writing to the repo.
    ///
    /// A `TempDir` rather than a named directory under the system temp root:
    /// twelve of these per run, and a named one is litter the next run has to
    /// remember to clear. The guard comes back with the path, so the caller has
    /// to hold it — dropping it deletes the tree mid-test.
    fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::create_dir_all(dir.join("prompts")).unwrap();
        std::fs::write(dir.join("prompts/analyze.txt"), "x").unwrap();
        for kind in PoolKind::ALL {
            std::fs::copy(
                repo.join("assets").join(kind.registry()),
                dir.join("assets").join(kind.registry()),
            )
            .unwrap();
            copy_dir(
                &repo.join("assets").join(kind.dir()),
                &dir.join("assets").join(kind.dir()),
            );
        }
        std::fs::copy(
            repo.join("assets/scene-map.json"),
            dir.join("assets/scene-map.json"),
        )
        .unwrap();
        (tmp, dir)
    }

    fn copy_dir(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for e in std::fs::read_dir(from).unwrap().flatten() {
            let p = e.path();
            if p.is_file() {
                let _ = std::fs::copy(&p, to.join(p.file_name().unwrap()));
            }
        }
    }

    #[test]
    fn an_entry_nothing_reaches_is_removable_and_one_the_map_reaches_is_not() {
        let (_d, dir) = fixture();
        let data = load(&dir).unwrap();
        let effect = rows(&data, PoolKind::Effect);
        assert!(!effect.is_empty());
        // Every shipped effect sound answers a shipped rule, so none is free.
        assert!(
            effect.iter().all(|r| r.in_use()),
            "a shipped effect sound reads as unused: {:?}",
            effect
                .iter()
                .filter(|r| !r.in_use())
                .map(|r| &r.name)
                .collect::<Vec<_>>()
        );
        let wind = effect.iter().find(|r| r.name == "wind").unwrap();
        assert!(
            wind.uses
                .iter()
                .any(|u| u.tags.iter().any(|t| t == "mountain")),
            "wind is reached by the mountain tag, not by its name: {:?}",
            wind.uses
        );
        assert!(wind.verdict().0.contains("remove disabled"));

        // The inject layer is reached by scripts, and there are none here.
        let inject = rows(&data, PoolKind::Inject);
        assert!(inject.iter().all(|r| !r.in_use()), "{inject:?}");
        assert!(inject.iter().all(|r| r.verdict().0.contains("removable")));
    }

    /// The two layers' usage comes from different files and must not be
    /// swapped: a palette value is not a scene rule.
    #[test]
    fn the_music_layer_reads_the_palette_not_the_rules() {
        let (_d, dir) = fixture();
        let data = load(&dir).unwrap();
        let music = rows(&data, PoolKind::Music);
        let tavern = music.iter().find(|r| r.name == "tavern").unwrap();
        assert!(tavern.in_use());
        assert!(
            tavern.uses.iter().all(|u| u.by.starts_with("palette ")),
            "{:?}",
            tavern.uses
        );
    }

    /// A script that places a sound is what makes an inject unremovable, and
    /// the reason names the chapter so the operator can go and look.
    #[test]
    fn a_script_placing_an_inject_is_what_makes_it_unremovable() {
        let (_d, dir) = fixture();
        std::fs::create_dir_all(dir.join("data")).unwrap();
        std::fs::write(
            dir.join("data/script-09.json"),
            r#"{"segments":[{"speaker":"A","text":"x"},{"sound":"coin"},{"stop":"cooking"}]}"#,
        )
        .unwrap();
        let data = load(&dir).unwrap();
        let inject = rows(&data, PoolKind::Inject);
        let coin = inject.iter().find(|r| r.name == "coin").unwrap();
        assert!(coin.in_use());
        assert_eq!(coin.uses[0].by, "ch09");
        assert_eq!(
            coin.uses[0].label(),
            "ch09",
            "a chapter has no tags to show"
        );
        let swoosh = inject.iter().find(|r| r.name == "swoosh").unwrap();
        assert!(!swoosh.in_use());
    }

    /// A registry naming a clip that is not there is the failure the merge only
    /// warns about; the screen says it up front.
    #[test]
    fn a_registry_naming_a_missing_clip_is_flagged() {
        let (_d, dir) = fixture();
        let mut pool = audio_pool::load_pool(&dir.join("assets/effect-pool.json"));
        pool.insert("ghost".into(), sound(&["effects/ghost-1.mp3"], &["ghost"]));
        audio_pool::save_pool(
            &dir.join("assets/effect-pool.json"),
            PoolKind::Effect,
            &pool,
        )
        .unwrap();
        let data = load(&dir).unwrap();
        let effect = rows(&data, PoolKind::Effect);
        let ghost = effect.iter().find(|r| r.name == "ghost").unwrap();
        assert_eq!(ghost.missing, vec!["effects/ghost-1.mp3"]);
        assert!(effect
            .iter()
            .find(|r| r.name == "rain")
            .unwrap()
            .missing
            .is_empty());
    }

    #[test]
    fn an_entry_round_trips_through_the_prompt_line() {
        for layer in PoolKind::ALL {
            let s = Sound {
                tags: vec!["a".into(), "b".into()],
                files: vec![format!("{}/x-1.mp3", layer.dir())],
                // The music layer has no `looped` flag at all — it always loops
                // — so a music entry is written with the layer's own default.
                // The other two carry a one-shot, which has to survive the trip.
                looped: layer == PoolKind::Music,
                dur_s: (layer == PoolKind::Inject).then_some(1.5),
                mode: (layer == PoolKind::Inject).then(|| "trail".into()),
                hold: (layer == PoolKind::Inject).then_some(2.0),
                level: Some(0.4),
            };
            let line = describe("probe", &s, layer);
            let (name, back) = parse_entry(layer, &line, Some("probe")).unwrap();
            assert_eq!(name, "probe");
            assert_eq!(back, s, "{layer:?} lost a field: {line}");
        }
    }

    #[test]
    fn the_prompt_refuses_what_the_mix_would_read_as_something_else() {
        let e = |buf: &str| parse_entry(PoolKind::Effect, buf, None).unwrap_err();
        // A level of zero is read as "unset" by the mixer, so it is not a level.
        assert!(e("name=a files=effects/a.mp3 tags=t level=0").contains("0 reads as"));
        assert!(e("name=a files=effects/a.mp3 tags=t level=9").contains("level must be"));
        // A sound nothing reaches can never play.
        assert!(e("name=a files=effects/a.mp3 tags=").contains("tags is empty"));
        // A sound with no take can never play either.
        assert!(e("name=a files= tags=t").contains("files is empty"));
        // A field the layer does not have.
        assert!(e("name=a files=effects/a.mp3 tags=t mode=hit").contains("not a field"));
        // Not a pair at all.
        assert!(e("name=a files=effects/a.mp3 tags=t oops").contains("not key=value"));
        // A missing name is only allowed when renaming an existing entry.
        assert!(e("files=effects/a.mp3 tags=t").contains("name= is required"));
        // `_` is how a registry marks its own notes.
        assert!(e("name=_note files=effects/a.mp3 tags=t").contains("may not start"));
    }

    /// The name is the pool's key: a script names an inject by it, and a pick
    /// is seeded from it. A rename in place would be a different sound wearing
    /// the old one's references.
    #[test]
    fn renaming_in_place_is_refused_and_keeping_the_name_is_not() {
        let same = parse_entry(
            PoolKind::Inject,
            "name=coin files=injects/coin-1.mp3 tags=coin",
            Some("coin"),
        );
        assert!(same.is_ok(), "{same:?}");
        let err = parse_entry(
            PoolKind::Inject,
            "name=penny files=injects/coin-1.mp3 tags=coin",
            Some("coin"),
        )
        .unwrap_err();
        assert!(err.contains("cannot become"), "{err}");
        // Omitting the name entirely keeps the one being edited.
        let (name, _) = parse_entry(
            PoolKind::Music,
            "files=music/market-bg-1.mp3 tags=market",
            Some("market"),
        )
        .unwrap();
        assert_eq!(name, "market");
    }

    #[test]
    fn the_music_layer_has_no_looped_flag_and_the_inject_layer_has_a_mode() {
        let err = parse_entry(
            PoolKind::Music,
            "name=a files=music/a.mp3 tags=t looped=false",
            None,
        )
        .unwrap_err();
        assert!(err.contains("not a field"), "{err}");
        let err = parse_entry(
            PoolKind::Inject,
            "name=a files=injects/a.mp3 tags=t mode=whatever",
            None,
        )
        .unwrap_err();
        assert!(err.contains("mode “whatever” unknown"), "{err}");
        let (_, s) = parse_entry(
            PoolKind::Inject,
            "name=a files=injects/a.mp3 tags=t mode=overlap hold=1.5",
            None,
        )
        .unwrap();
        assert_eq!(s.mode.as_deref(), Some("overlap"));
        assert_eq!(s.hold, Some(1.5));
    }

    #[test]
    fn a_level_prompt_accepts_a_number_and_clears_on_empty() {
        assert_eq!(parse_level_prompt(" 0.35 ").unwrap(), Some(0.35));
        assert_eq!(parse_level_prompt("").unwrap(), None);
        assert_eq!(parse_level_prompt("   ").unwrap(), None);
        assert!(parse_level_prompt("0").is_err());
        assert!(parse_level_prompt("loud").is_err());
    }

    /// The clip has to exist and to live in the layer's own directory — a
    /// registry line that points at another layer's clips is a category error
    /// that would survive forever.
    #[test]
    fn a_clip_must_exist_and_live_in_its_own_layers_directory() {
        let (_d, dir) = fixture();
        let ok = vec!["effects/rain-1.mp3".to_string()];
        assert!(check_files(&dir, PoolKind::Effect, &ok).is_ok());

        let elsewhere = vec!["music/market-bg-1.mp3".to_string()];
        let err = check_files(&dir, PoolKind::Effect, &elsewhere).unwrap_err();
        assert!(err.contains("own directory"), "{err}");

        let absent = vec!["effects/nope-1.mp3".to_string()];
        let err = check_files(&dir, PoolKind::Effect, &absent).unwrap_err();
        assert!(err.contains("no such clip"), "{err}");

        let escape = vec!["../secrets.mp3".to_string()];
        assert!(check_files(&dir, PoolKind::Effect, &escape)
            .unwrap_err()
            .contains("relative to assets/"));
    }

    /// Only what an edit *adds* is checked, so an entry whose clip has gone
    /// missing stays editable — it is already flagged in red, and refusing a
    /// tag change because somebody moved a file is how an entry becomes
    /// unfixable from the screen.
    #[test]
    fn an_edit_is_checked_on_what_it_introduces_not_on_what_was_already_there() {
        let (_d, dir) = fixture();
        let gone = Sound {
            tags: vec!["rain".into()],
            files: vec!["effects/gone-1.mp3".into()],
            looped: true,
            dur_s: None,
            mode: None,
            hold: None,
            level: None,
        };
        // The same (broken) take, retagged: nothing new to check.
        let retagged = Sound {
            tags: vec!["rain".into(), "storm".into()],
            ..gone.clone()
        };
        assert!(introduced(Some(&gone), &retagged).is_empty());
        assert!(check_files(&dir, PoolKind::Effect, &introduced(Some(&gone), &retagged)).is_ok());

        // A take the edit adds is checked, and a missing one is refused.
        let replaced = Sound {
            files: vec!["effects/gone-1.mp3".into(), "effects/absent-1.mp3".into()],
            ..gone.clone()
        };
        assert_eq!(
            introduced(Some(&gone), &replaced),
            vec!["effects/absent-1.mp3"]
        );
        assert!(check_files(&dir, PoolKind::Effect, &introduced(Some(&gone), &replaced)).is_err());

        // A brand-new entry has every take introduced, so all of them are.
        assert_eq!(introduced(None, &replaced).len(), 2);
        // ...and the one that is there passes.
        let good = Sound {
            files: vec!["effects/rain-1.mp3".into()],
            ..gone.clone()
        };
        assert!(check_files(&dir, PoolKind::Effect, &introduced(None, &good)).is_ok());
    }

    /// The write path end to end: add, save, reload — and the guard is read off
    /// the reloaded data, so the screen and the file cannot disagree.
    #[test]
    fn a_saved_pool_reloads_with_the_same_rows() {
        let (_d, dir) = fixture();
        let data = load(&dir).unwrap();
        let mut pool = data.pools[&PoolKind::Inject].clone();
        let (name, s) = parse_entry(
            PoolKind::Inject,
            "name=kettle files=injects/coin-1.mp3 tags=kettle,whistle mode=hit level=0.8",
            None,
        )
        .unwrap();
        check_files(&dir, PoolKind::Inject, &s.files).unwrap();
        assert!(matches!(apply(&mut pool, &name, s), Edit::Added));
        save(&dir, PoolKind::Inject, &pool).unwrap();

        let back = load(&dir).unwrap();
        let rows = rows(&back, PoolKind::Inject);
        assert_eq!(rows.len(), data.pools[&PoolKind::Inject].len() + 1);
        let kettle = rows.iter().find(|r| r.name == "kettle").unwrap();
        assert_eq!(kettle.sound.level, Some(0.8));
        assert_eq!(kettle.takes(), 1);
        assert!(!kettle.in_use(), "nothing places it yet");
    }

    /// `dur_s` is a fact about the clip, so it is probed, not typed. The fixture
    /// clips are the real ones, so this either reads a duration or says so.
    #[test]
    fn the_longest_take_is_probed_rather_than_guessed() {
        let (_d, dir) = fixture();
        let pool = audio_pool::load_pool(&dir.join("assets/inject-pool.json"));
        let coin = &pool["coin"];
        match probe_longest(&dir, coin) {
            Some(d) => assert!(d > 0.0 && d < 10.0, "coin probes as {d}s"),
            // ffprobe absent is not a failure — it means "leave the field".
            None => eprintln!("ffprobe unavailable; dur_s would be left as it was"),
        }
        assert_eq!(
            probe_longest(&dir, &sound(&["injects/nope.mp3"], &["x"])),
            None
        );
    }

    #[test]
    fn the_header_shows_the_layers_own_master_knob() {
        let (_d, dir) = fixture();
        let data = load(&dir).unwrap();
        // Read from the map, not invented: whatever ships is what is shown.
        assert_eq!(
            master_level(&data.map, PoolKind::Effect),
            data.map.layers.effect.trim
        );
        assert_eq!(
            master_level(&data.map, PoolKind::Music),
            data.map.layers.music.level
        );
        assert_eq!(
            master_level(&data.map, PoolKind::Inject),
            data.map.layers.inject.level
        );
        assert!(PoolKind::Effect.master_knob().starts_with("layers.effect"));
    }

    /// Every layer's own field list is what the parser accepts — the hint, the
    /// refusal message and the parser are one list, not three.
    #[test]
    fn the_field_list_is_the_parser_and_the_hint() {
        for layer in PoolKind::ALL {
            let hint = fields_hint(layer);
            for f in fields(layer) {
                assert!(
                    hint.contains(&format!("{f}=")),
                    "{layer:?}: {f} missing from the hint"
                );
            }
            assert!(!fields(layer).contains(&"mode") || layer == PoolKind::Inject);
            assert!(!fields(layer).contains(&"hold") || layer == PoolKind::Inject);
            assert!(!fields(layer).contains(&"dur_s") || layer == PoolKind::Inject);
            assert!(!fields(layer).contains(&"looped") || layer != PoolKind::Music);
        }
    }

    /// `summarise` is what the footer shows, so it has to be short and honest.
    #[test]
    fn the_use_summary_counts_what_it_does_not_show() {
        let uses: Vec<UseOf> = (0..4)
            .map(|i| UseOf {
                by: format!("rule{i}"),
                tags: vec!["t".into()],
            })
            .collect();
        assert_eq!(summarise(&uses, 2), "rule0 [t]; rule1 [t] (+2 more)");
        assert_eq!(summarise(&uses[..1], 2), "rule0 [t]");
    }
}
