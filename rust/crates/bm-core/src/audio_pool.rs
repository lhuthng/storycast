//! Sound-design clip pools: the effect layer's beds, the music layer's tracks
//! and the inject layer's spot effects.
//!
//! The voice pool (`pool.rs`) answers "which of these clips may stand in for
//! this character". This is the same shape one level down: a JSON registry
//! `sound -> {tags, files, ...}`, the filename only the *suggestion* and the
//! registry the truth — so a scene asks for the tag `rain` and the pool decides
//! which rain clip answers. Three registries exist, one per layer
//! (`assets/effect-pool.json`, `assets/music-pool.json`,
//! `assets/inject-pool.json`), and the code is the same for all of them: a
//! layer is a pool plus a level. [`PoolKind`] is which one, and it is the only
//! place the three filenames are spelled.
//!
//! **A pool entry is a sound, and a sound has one or more files.** The clips are
//! named `<sound>-<n>.mp3` by hand — `day-1`, `day-2`, `day-3` are three takes
//! of *day*, not three sounds called "day one", "day two", "day three". The
//! number is a file index inside the family, and it carries no meaning: nothing
//! in the mix reads it. An earlier revision made each file its own entry, which
//! promoted the index into part of the identity and then needed invented tags
//! (`stinger`, `calm`, `street`) to tell the siblings apart — tags that
//! described the *files* while claiming to describe the sound. `pick` therefore
//! returns the sound's name, never a filename.
//!
//! Picks are deterministic. A merge that reruns must reproduce the same audio,
//! so the seed is derived from the chapter and the tag set rather than from the
//! clock — the opposite of the audition picker, where a new sample on every
//! press is the point.
//!
//! All three registries sit in `assets/`, not at the repo root beside the voice
//! pool, and that placement is load-bearing: `assets/` is what provisioning
//! ships to a worker, so a clip and its registry travel together and a worker
//! merging a chapter can resolve the same tags the inductor would.

use anyhow::{anyhow, Context, Result};
use std::collections::BTreeMap;
use std::path::Path;

/// Filename tags are shared with the voice pool: one parser, three pools.
pub use crate::pool::parse_sample_tags;

/// Which registry a pool is.
///
/// The three layers differ in more than their filename: an effect entry carries
/// `looped` and no `mode`, a music entry carries neither, an inject entry
/// carries both plus `hold` and `dur_s`. Anything that reads or writes a
/// registry by name — the path, the clip directory, the field order in the
/// file — asks this rather than hard-coding the strings, so "add a layer" is
/// one enum arm instead of a search for every place `music-pool.json` appears.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PoolKind {
    /// Place beds, chosen by the scene map's rules.
    Effect,
    /// Tracks, chosen by the palette.
    Music,
    /// Script-placed spot effects.
    Inject,
}

impl PoolKind {
    /// In the order the layers sit under the voice, and the order the editor
    /// tabs through them.
    pub const ALL: [PoolKind; 3] = [PoolKind::Effect, PoolKind::Music, PoolKind::Inject];

    /// The registry filename, relative to `assets/`.
    pub fn registry(self) -> &'static str {
        match self {
            PoolKind::Effect => "effect-pool.json",
            PoolKind::Music => "music-pool.json",
            PoolKind::Inject => "inject-pool.json",
        }
    }

    /// The directory its clips live in, relative to `assets/`. `files` in the
    /// registry are written relative to `assets/` too, so a clip is addressed
    /// as `<dir>/<name>.mp3`.
    pub fn dir(self) -> &'static str {
        match self {
            PoolKind::Effect => "effects",
            PoolKind::Music => "music",
            PoolKind::Inject => "injects",
        }
    }

    /// The layer as an operator says it.
    pub fn label(self) -> &'static str {
        match self {
            PoolKind::Effect => "effects",
            PoolKind::Music => "music",
            PoolKind::Inject => "injects",
        }
    }

    /// What the layer's master gain is called in the scene map, for a screen
    /// that shows the whole gain chain. `None` for the effect layer, whose
    /// master is `layers.effect.trim` — named here rather than in the caller
    /// so the two spellings cannot drift.
    pub fn master_knob(self) -> &'static str {
        match self {
            PoolKind::Effect => "layers.effect.trim",
            PoolKind::Music => "layers.music.level",
            PoolKind::Inject => "layers.inject.level",
        }
    }
}

/// One pooled sound: every file that answers for it, and how they play.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Sound {
    /// What a scene or a palette entry matches on.
    #[serde(default)]
    pub tags: Vec<String>,
    /// The takes, in the registry's order — `day-1`, `day-2`, `day-3`. The
    /// order is the tie-break, so a reordered list is a different mix.
    #[serde(default)]
    pub files: Vec<String>,
    /// A loop is stretched to fill its window; a one-shot plays once and its
    /// window falls silent after it (a sword clash). Per sound, because every
    /// take of a bed is a bed.
    #[serde(default = "loops")]
    pub looped: bool,
    /// Longest take in seconds, post-trim. Only the inject registry sets it:
    /// the digest prompt renders it so the analyzer never `hit`s a 51 s boil,
    /// and the validator refuses one that tries. A whole-clip number, not
    /// per-take precision — the merge probes the actual take it picked.
    #[serde(default)]
    pub dur_s: Option<f64>,
    /// `hit` | `overlap` | `trail`. Only the inject registry sets it, and it is
    /// the reason the script does not: how a clip behaves is a property of the
    /// *clip* — a splat punctuates, a rustle runs under, a bed keeps going —
    /// and a per-chapter copy of it was a second source of truth the analyzer
    /// had to guess at (and got wrong: a water spell `overlap`ped under a
    /// kitchen sink). Kept as a string here because the mixer owns the enum;
    /// `ambience::injects_of` is the one place it is parsed.
    #[serde(default)]
    pub mode: Option<String>,
    /// Solo seconds before a `trail`'s tail ducks under the speech. `None`
    /// falls back to `layers.inject.default_hold_s`.
    #[serde(default)]
    pub hold: Option<f64>,
    /// Optional trim for this sound alone. `None` is 1.0.
    ///
    /// The third rung of the same ladder the layer already has, and the same
    /// argument: a rule's `level` is the balance *between scenes* and the
    /// layer's `trim`/`level` is the balance between layers, so neither is the
    /// right place to say "this one clip sits hot". Before this field existed
    /// that opinion had nowhere to go for the effect and music layers — the
    /// inject layer has read it from the start (`injects_of`), which is how
    /// `food-prep` came down to 0.05 without disturbing anything else. A value
    /// of `0.0` or less is read as `None`, so a pool can never mute a sound by
    /// arithmetic accident; muting is what removing it, or the layer switch, is
    /// for.
    #[serde(default)]
    pub level: Option<f64>,
}

fn loops() -> bool {
    true
}

/// `sound -> sound`. Ordered so the file on disk diffs cleanly — and so a pick
/// over a tie is reproducible without a second sort.
pub type ClipPool = BTreeMap<String, Sound>;

/// A resolved pick: which sound answered, and which of its takes.
#[derive(Debug, Clone, PartialEq)]
pub struct Picked {
    /// The sound's name — `day`. This is what the log reports, because it is
    /// what the scene map asked for and the only part of the answer that means
    /// anything to a reader.
    pub sound: String,
    /// The take, relative to `assets/` (`effects/day-2.mp3`) — the same
    /// convention the registry uses, resolved by the caller against the
    /// directory the registry itself came from. Implementation detail: chosen so
    /// a chapter's audio is reproducible, not so it can be referred to by name.
    pub file: String,
    pub looped: bool,
    /// The sound's own trim, `Sound::level` with `None` already resolved to
    /// 1.0. Rides on the pick for the same reason `looped` does: the caller
    /// that places the clip is the one that needs it, and looking the sound up
    /// again would be a second answer to a question already answered.
    ///
    /// Not `Eq` any more, because of this field — nothing compares picks for
    /// identity, only for equality.
    pub level: f64,
}

/// Read a registry. A missing or broken file is "no pool", not an error: a
/// bookkeeping file must never fail a render, and an empty pool already means
/// "this layer is silent here".
///
/// An entry with no `files` cannot answer and is kept out of the pick, so a
/// registry left in the old one-file-per-entry shape resolves to silence rather
/// than to a guess. `every_shipped_*` in `ambience` is what catches that, by
/// asserting every shipped rule and palette value still finds a clip.
pub fn load_pool(path: &Path) -> ClipPool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return ClipPool::new();
    };
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(&text) else {
        return ClipPool::new();
    };
    let Some(obj) = doc.as_object() else {
        return ClipPool::new();
    };
    obj.iter()
        .filter(|(k, _)| !k.starts_with('_'))
        .filter_map(|(k, v)| {
            serde_json::from_value::<Sound>(v.clone())
                .ok()
                .map(|s| (k.clone(), s))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// writing a registry
// ---------------------------------------------------------------------------

/// Longest a field may be and still stay on one line, in columns. Not an
/// invention: it is the habit the shipped registries already have — `night`'s
/// five takes are expanded, `rain`'s two are not — so reproducing it keeps the
/// files hand-editable instead of turning every list into eight lines.
const INLINE_MAX: usize = 80;

/// One top-level member of a JSON object, with the byte span of its value.
pub(crate) struct RawEntry {
    pub(crate) key: String,
    pub(crate) value_start: usize,
    pub(crate) value_end: usize,
}

/// Index just past the string starting at `i` (which must be a `"`).
pub(crate) fn skip_string(b: &[u8], mut i: usize) -> Option<usize> {
    if b.get(i) != Some(&b'"') {
        return None;
    }
    i += 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// Index just past the value starting at `i`: a string, a nested object or
/// array, or a bare literal. Nesting is tracked so a `}` inside a value is
/// not mistaken for the object's own end.
pub(crate) fn skip_value(b: &[u8], mut i: usize) -> Option<usize> {
    match b.get(i)? {
        b'"' => skip_string(b, i),
        b'{' | b'[' => {
            let mut depth = 0usize;
            while i < b.len() {
                match b[i] {
                    b'"' => i = skip_string(b, i)?,
                    b'{' | b'[' => {
                        depth += 1;
                        i += 1;
                    }
                    b'}' | b']' => {
                        depth -= 1;
                        i += 1;
                        if depth == 0 {
                            return Some(i);
                        }
                    }
                    _ => i += 1,
                }
            }
            None
        }
        _ => {
            while i < b.len() && !matches!(b[i], b',' | b'}' | b']') && !b[i].is_ascii_whitespace()
            {
                i += 1;
            }
            Some(i)
        }
    }
}

/// Walk a JSON array's top level, recording the byte span of each element.
///
/// The same scanner as [`scan_entries`], for the one layered member that is a
/// list. Withdrawing an inherited rule means finding the entries a previous
/// resolve appended and dropping exactly those, and a parsed tree cannot do it:
/// re-emitting a rule reorders its fields and reflows its list, so an
/// append-then-withdraw would rewrite rules nobody touched.
pub(crate) fn scan_array(text: &str) -> Option<Vec<(usize, usize)>> {
    let b = text.as_bytes();
    let mut i = 0usize;
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    if b.get(i) != Some(&b'[') {
        return None;
    }
    i += 1;
    let mut out = Vec::new();
    loop {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if b.get(i)? == &b']' {
            return Some(out);
        }
        let start = i;
        let end = skip_value(b, i)?;
        if end <= start {
            return None;
        }
        out.push((start, end));
        i = end;
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        match b.get(i)? {
            b',' => i += 1,
            b']' => return Some(out),
            _ => return None,
        }
    }
}

/// Walk a JSON object's top level, recording where each value begins and ends.
///
/// Deliberately a scanner and not a parser: the point is to recover the *raw
/// text* of values, which a parsed tree has already thrown away. It only has to
/// be right about two things — where a string ends (escapes included) and how
/// deep the braces go — and it refuses rather than guesses, so a file this
/// cannot read is a file the writer will not touch.
pub(crate) fn scan_entries(text: &str) -> Option<Vec<RawEntry>> {
    let b = text.as_bytes();
    let mut i = 0usize;
    let skip_ws = |i: &mut usize| {
        while *i < b.len() && b[*i].is_ascii_whitespace() {
            *i += 1;
        }
    };
    skip_ws(&mut i);
    if b.get(i) != Some(&b'{') {
        return None;
    }
    i += 1;
    let mut out = Vec::new();
    loop {
        skip_ws(&mut i);
        match b.get(i)? {
            b'}' => return Some(out),
            b'"' => {}
            _ => return None,
        }
        let key_end = skip_string(b, i)?;
        let key: String = serde_json::from_str(&text[i..key_end]).ok()?;
        i = key_end;
        skip_ws(&mut i);
        if b.get(i) != Some(&b':') {
            return None;
        }
        i += 1;
        skip_ws(&mut i);
        let value_start = i;
        let value_end = skip_value(b, i)?;
        // A value that consumed nothing is `{"a": }` — a file this writer has
        // no business rewriting.
        if value_end <= value_start {
            return None;
        }
        out.push(RawEntry {
            key,
            value_start,
            value_end,
        });
        i = value_end;
        skip_ws(&mut i);
        match b.get(i)? {
            b',' => i += 1,
            b'}' => return Some(out),
            _ => return None,
        }
    }
}

/// Rewrite a registry, touching only the entries that changed.
///
/// A whole-file re-serialise would reflow every list and re-emit the `_note`
/// through a JSON encoder — and that note is the only written record of why the
/// pool is shaped the way it is, in prose, in one line, exactly as its author
/// left it. So the writer keeps the **raw text** of every member it was not
/// asked to change, notes included, and renders only what is new or edited.
/// An untouched pool therefore round-trips byte for byte, which is what
/// `saving_an_untouched_registry_rewrites_nothing` asserts.
///
/// Order is the file's own: `_`-prefixed members first in the order they
/// appear, then the sounds sorted by name (`ClipPool` is a `BTreeMap`). That is
/// also the order the shipped registries already use, so the first real edit
/// produces a diff of one entry rather than one file.
pub fn save_pool(path: &Path, kind: PoolKind, pool: &ClipPool) -> Result<()> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|_| "{\n}\n".to_string());
    let entries = scan_entries(&text).ok_or_else(|| {
        anyhow!(
            "{} is not a JSON object this writer can read — refusing to rewrite it",
            path.display()
        )
    })?;
    let raw: BTreeMap<&str, &str> = entries
        .iter()
        .map(|e| (e.key.as_str(), &text[e.value_start..e.value_end]))
        .collect();

    let mut out = String::from("{\n");
    let mut members: Vec<(String, String)> = Vec::new();
    for e in entries.iter().filter(|e| e.key.starts_with('_')) {
        // Verbatim, including any whitespace inside the value.
        members.push((
            serde_json::to_string(&e.key)?,
            raw[e.key.as_str()].to_string(),
        ));
    }
    for (name, sound) in pool {
        let key = serde_json::to_string(name)?;
        let body = match raw.get(name.as_str()) {
            // Unchanged: keep the author's own bytes, layout and all.
            Some(t) if parses_as(t, sound) => (*t).to_string(),
            // Edited: re-render, in the shape it already had, so an expanded
            // entry does not silently collapse under a one-number edit.
            Some(t) => render_entry(sound, kind, t.contains('\n'))?,
            None => render_entry(sound, kind, false)?,
        };
        members.push((key, body));
    }
    for (i, (key, body)) in members.iter().enumerate() {
        if i > 0 {
            out.push_str(",\n");
        }
        out.push_str("  ");
        out.push_str(key);
        out.push_str(": ");
        out.push_str(body);
    }
    out.push_str("\n}\n");
    crate::util::atomic_write(path, &out).with_context(|| format!("writing {}", path.display()))
}

/// Whether an entry's own text still means exactly this sound. Round-trips
/// through `Sound` rather than comparing strings, so a hand-added space or a
/// reordered key does not count as a change and does not provoke a rewrite.
fn parses_as(raw: &str, sound: &Sound) -> bool {
    serde_json::from_str::<Sound>(raw)
        .map(|s| s == *sound)
        .unwrap_or(false)
}

/// One entry, in the field order its layer's file uses.
///
/// The order is not the struct's: it is what the shipped registries read as, so
/// a re-rendered entry looks like the ones around it. Fields whose value is the
/// registry's default are omitted where the layer's own note says to omit them
/// (`looped` on a bed) and kept where the file keeps them (`looped` on an
/// inject, where a one-shot and a bed are the same shape).
fn render_entry(sound: &Sound, kind: PoolKind, expand: bool) -> Result<String> {
    use serde_json::json;
    let mut fields: Vec<(&str, serde_json::Value)> = vec![("tags", json!(sound.tags))];
    if !sound.files.is_empty() {
        fields.push(("files", json!(sound.files)));
    }
    match kind {
        PoolKind::Effect => {
            if !sound.looped {
                fields.push(("looped", json!(false)));
            }
            if let Some(l) = sound.level {
                fields.push(("level", json!(l)));
            }
        }
        PoolKind::Music => {
            if let Some(l) = sound.level {
                fields.push(("level", json!(l)));
            }
        }
        PoolKind::Inject => {
            if let Some(m) = &sound.mode {
                fields.push(("mode", json!(m)));
            }
            if let Some(h) = sound.hold {
                fields.push(("hold", json!(h)));
            }
            if let Some(l) = sound.level {
                fields.push(("level", json!(l)));
            }
            fields.push(("looped", json!(sound.looped)));
            if let Some(d) = sound.dur_s {
                fields.push(("dur_s", json!(d)));
            }
        }
    }

    let mut body = String::new();
    for (i, (k, v)) in fields.iter().enumerate() {
        if i > 0 {
            body.push_str(",\n    ");
        }
        let key = serde_json::to_string(k)?;
        let inline = inline_value(v)?;
        // `    "files": [...]` is what the line will look like, so the budget
        // is measured against that, not against the value alone.
        let line = 4 + key.len() + 2 + inline.len();
        if !expand && line <= INLINE_MAX {
            body.push_str(&format!("{key}: {inline}"));
        } else {
            body.push_str(&format!("{key}: {}", expand_value(v, 4)?));
        }
    }
    Ok(format!("{{\n    {body}\n  }}"))
}

/// A value on one line, with `", "` between list items.
///
/// Not `serde_json::to_string`: that emits `["a","b"]`, and every registry in
/// the tree is written `["a", "b"]`. A new entry has to look like the ones
/// around it, or the file stops reading as one document.
fn inline_value(v: &serde_json::Value) -> Result<String> {
    match v {
        serde_json::Value::Array(items) => {
            let parts = items
                .iter()
                .map(inline_value)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(format!("[{}]", parts.join(", ")))
        }
        other => Ok(serde_json::to_string(other)?),
    }
}

/// A value, one element per line. Only ever called for a field that has been
/// decided to be too long for one line, or for an entry that was already
/// expanded — the inline case never reaches here.
fn expand_value(v: &serde_json::Value, indent: usize) -> Result<String> {
    match v {
        serde_json::Value::Array(items) if !items.is_empty() => {
            let inner = " ".repeat(indent + 2);
            let close = " ".repeat(indent);
            let parts = items
                .iter()
                .map(inline_value)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(format!(
                "[\n{inner}{}\n{close}]",
                parts.join(&format!(",\n{inner}"))
            ))
        }
        other => Ok(serde_json::to_string(other)?),
    }
}

/// The sounds `pick` can return for `tags`: the maximum-overlap set, before
/// `seed` decides which sound and which take.
///
/// Empty when nothing overlaps — which is the same question as `pick` returning
/// `None`, so the two share this answer instead of each computing it. That
/// matters beyond tidiness: a caller that needs to know *what a scene can
/// reach* (the sound-design fingerprint, `crate::design`) has to ask the same
/// question the mix asks, and a second implementation of "which clip answers
/// this tag" would be free to disagree with the mix about a chapter being
/// current.
///
/// Name-sorted, because `ClipPool` is a `BTreeMap` — so the set is a stable
/// value a caller may hash.
pub fn candidates<'a>(pool: &'a ClipPool, tags: &[String]) -> Vec<(&'a str, &'a Sound)> {
    if tags.is_empty() {
        return Vec::new();
    }
    let mut best = 0usize;
    let mut cands: Vec<(&str, &Sound)> = Vec::new();
    for (name, sound) in pool {
        if sound.files.is_empty() {
            continue;
        }
        let score = sound.tags.iter().filter(|t| tags.contains(t)).count();
        // Only the maximum-overlap set is a candidate. Both halves of this guard
        // are load-bearing. `score < best` is the one that was missing: without
        // it a sound sharing *one* tag still lands in `cands` whenever it sorts
        // after the winner, so a 2-of-2 match can lose a seed roll to a 1-of-2
        // one — the exact property this function exists to provide. `score == 0`
        // is the same guard at the bottom of the range, where `best` is still 0
        // and `score < best` cannot see it.
        if score == 0 || score < best {
            continue;
        }
        if score > best {
            best = score;
            cands.clear();
        }
        cands.push((name.as_str(), sound));
    }
    cands
}

/// Best-overlap pick among the sounds matching `tags`, then one take of it.
///
/// Scoring rather than set intersection: a scene tagged `[night, calm]` should
/// prefer a sound tagged `[night, calm]` over one tagged `[night, dark]`, and
/// `[night]` alone should still find either. A tie is broken by `seed`, so the
/// choice varies between chapters without varying between two runs of the same
/// chapter. The same seed then picks the take, so which of `day-1/2/3` plays is
/// reproducible too.
///
/// `None` means "no suitable track", which the caller must read as *the layer
/// is absent here* — never as "use a silent file".
pub fn pick(pool: &ClipPool, tags: &[String], seed: u64) -> Option<Picked> {
    let cands = candidates(pool, tags);
    // The guard has to come *before* the index, not inside it: `seed % 0` panics,
    // and an empty `cands` is the ordinary case of a scene naming tags no sound
    // answers. `cands.get(..)?` looks like it handles this and does not, because
    // the modulo is evaluated to build the argument.
    if cands.is_empty() {
        return None;
    }
    let (sound, entry) = cands[(seed % cands.len() as u64) as usize];
    // A second, decorrelated roll picks the take: two chapters that land on the
    // same sound should not also land on the same file, or the extra takes would
    // never play.
    let roll = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 33;
    let file = entry.files[(roll % entry.files.len() as u64) as usize].clone();
    Some(Picked {
        sound: sound.to_string(),
        file,
        looped: entry.looped,
        level: sound_level(entry),
    })
}

/// A sound's trim with `None` resolved. One place, so the mix and the editor
/// cannot disagree about what an absent `level` means.
pub fn sound_level(sound: &Sound) -> f64 {
    sound.level.filter(|l| *l > 0.0).unwrap_or(1.0)
}

/// Seed for one pick: FNV-1a over the chapter and the tag set.
///
/// Deliberately not over the clock, and deliberately over the *tags* rather
/// than the span index for music: two consecutive spans that ask for the same
/// mood then resolve to the same track, so a scene change inside one mood is
/// continuous music instead of a crossfade into the same tune. Effects seed
/// with the span index as well (see the caller) so a chapter's two night scenes
/// do not both get the same night sound.
pub fn seed(chapter: u32, salt: usize, tags: &[String]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |s: &str| {
        for b in s.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    };
    eat(&chapter.to_string());
    eat(&salt.to_string());
    for t in tags {
        eat(t);
    }
    h
}

#[cfg(test)]
mod tests;
