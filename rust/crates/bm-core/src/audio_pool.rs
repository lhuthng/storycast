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
struct RawEntry {
    key: String,
    value_start: usize,
    value_end: usize,
}

/// Walk a JSON object's top level, recording where each value begins and ends.
///
/// Deliberately a scanner and not a parser: the point is to recover the *raw
/// text* of values, which a parsed tree has already thrown away. It only has to
/// be right about two things — where a string ends (escapes included) and how
/// deep the braces go — and it refuses rather than guesses, so a file this
/// cannot read is a file the writer will not touch.
fn scan_entries(text: &str) -> Option<Vec<RawEntry>> {
    let b = text.as_bytes();
    let mut i = 0usize;
    let skip_ws = |i: &mut usize| {
        while *i < b.len() && b[*i].is_ascii_whitespace() {
            *i += 1;
        }
    };
    /// Index just past the string starting at `i` (which must be a `"`).
    fn skip_string(b: &[u8], mut i: usize) -> Option<usize> {
        debug_assert_eq!(b[i], b'"');
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
    fn skip_value(b: &[u8], mut i: usize) -> Option<usize> {
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
                while i < b.len()
                    && !matches!(b[i], b',' | b'}' | b']')
                    && !b[i].is_ascii_whitespace()
                {
                    i += 1;
                }
                Some(i)
            }
        }
    }

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
    if tags.is_empty() {
        return None;
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
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn tags(t: &[&str]) -> Vec<String> {
        t.iter().map(|s| s.to_string()).collect()
    }

    /// Two sounds, one of them with three takes — the shape the real registries
    /// have, where `day-1/2/3` is one sound.
    fn pool() -> ClipPool {
        let mut p = ClipPool::new();
        p.insert(
            "day".into(),
            Sound {
                tags: tags(&["day", "calm"]),
                files: vec![
                    "effects/day-1.mp3".into(),
                    "effects/day-2.mp3".into(),
                    "effects/day-3.mp3".into(),
                ],
                looped: true,
                dur_s: None,
                mode: None,
                hold: None,
                level: None,
            },
        );
        p.insert(
            "night".into(),
            Sound {
                tags: tags(&["night"]),
                files: vec!["effects/night-1.mp3".into()],
                looped: true,
                dur_s: None,
                mode: None,
                hold: None,
                level: None,
            },
        );
        p.insert(
            "rain".into(),
            Sound {
                tags: tags(&["rain", "calm"]),
                files: vec!["effects/rain-1.mp3".into()],
                looped: true,
                dur_s: None,
                mode: None,
                hold: None,
                level: None,
            },
        );
        p.insert(
            "sword-fight".into(),
            Sound {
                tags: tags(&["battle", "sword"]),
                files: vec!["effects/sword-fight-1.mp3".into()],
                looped: false,
                dur_s: None,
                mode: None,
                hold: None,
                level: None,
            },
        );
        p
    }

    /// The whole point of the shape: a scene asks for a *sound*, and the number
    /// on the file never becomes part of the answer.
    #[test]
    fn a_pick_names_the_sound_never_a_numbered_file() {
        let p = pool();
        let t = tags(&["day"]);
        for seed in 0..16 {
            let got = pick(&p, &t, seed).unwrap();
            assert_eq!(
                got.sound, "day",
                "seed {seed}: the sound is the family name"
            );
            assert!(
                got.file.starts_with("effects/day-"),
                "seed {seed}: the file is one of the family's, got {}",
                got.file
            );
        }
    }

    /// Every take in a family has to be reachable, or the extra ones are dead
    /// weight nobody notices. This is what a one-file-per-entry registry could
    /// not express and what made the numbering look like identity.
    #[test]
    fn every_take_of_a_sound_is_reachable() {
        let p = pool();
        let t = tags(&["day"]);
        let files: BTreeSet<String> = (0..64).map(|s| pick(&p, &t, s).unwrap().file).collect();
        assert_eq!(files.len(), 3, "all three takes must play: {files:?}");
    }

    #[test]
    fn the_best_overlap_wins_over_mere_intersection() {
        let p = pool();
        // Two shared tags beat one, whatever the seed.
        assert_eq!(pick(&p, &tags(&["day", "calm"]), 0).unwrap().sound, "day");
        assert_eq!(pick(&p, &tags(&["day", "calm"]), 9).unwrap().sound, "day");
        // `[night, dark]` shares only `night`, so it still finds the night sound
        // rather than nothing.
        assert_eq!(
            pick(&p, &tags(&["night", "dark"]), 7).unwrap().sound,
            "night"
        );
    }

    #[test]
    fn a_weaker_overlap_never_reaches_the_candidate_set() {
        // The winner must be decided by overlap, never by where its name sorts.
        // `zz-weak` sorts *after* `mm-strong`, so a candidate set that only
        // cleared on a strict improvement would offer both and let the seed roll
        // the loser.
        let mut p = ClipPool::new();
        for (name, t) in [
            ("aa-weak", &["night"][..]),
            ("mm-strong", &["night", "calm"][..]),
            ("zz-weak", &["night"][..]),
        ] {
            p.insert(
                name.into(),
                Sound {
                    tags: tags(t),
                    files: vec![format!("effects/{name}.mp3")],
                    looped: true,
                    dur_s: None,
                    mode: None,
                    hold: None,
                    level: None,
                },
            );
        }
        for seed in 0..32 {
            assert_eq!(
                pick(&p, &tags(&["night", "calm"]), seed).unwrap().sound,
                "mm-strong",
                "seed {seed}: a one-tag sound must never win against a two-tag one"
            );
        }
    }

    #[test]
    fn a_sound_with_no_files_is_not_a_candidate() {
        // A registry left in the old one-file-per-entry shape resolves to
        // silence, not to a guess. Pinned because the failure is quiet.
        let mut p = ClipPool::new();
        p.insert(
            "day".into(),
            Sound {
                tags: tags(&["day"]),
                files: vec![],
                looped: true,
                dur_s: None,
                mode: None,
                hold: None,
                level: None,
            },
        );
        assert!(pick(&p, &tags(&["day"]), 0).is_none());
    }

    /// Every one of these used to *panic*, not return `None`: the index was
    /// built as `seed % cands.len()`, and `% 0` traps before the `?` can see an
    /// empty vector. A scene naming tags nothing answers is ordinary, so this is
    /// the difference between a silent stretch and a dead merge.
    #[test]
    fn no_suitable_track_is_none_not_a_silent_file() {
        let p = pool();
        assert!(pick(&p, &tags(&["market"]), 0).is_none());
        assert!(pick(&p, &tags(&[]), 0).is_none(), "no tags, no opinion");
        assert!(pick(&ClipPool::new(), &tags(&["rain"]), 0).is_none());
    }

    #[test]
    fn a_pick_is_stable_for_a_chapter_and_moves_between_them() {
        let p = pool();
        let t = tags(&["day"]);
        let a = pick(&p, &t, seed(1, 0, &t)).unwrap();
        let again = pick(&p, &t, seed(1, 0, &t)).unwrap();
        assert_eq!(a, again, "same chapter, same tags, same take");

        let over: Vec<String> = (1..=8)
            .map(|c| pick(&p, &t, seed(c, 0, &t)).unwrap().file)
            .collect();
        assert!(
            over.iter().any(|n| *n != over[0]),
            "eight chapters must not all land on one take: {over:?}"
        );
    }

    #[test]
    fn one_shots_are_marked_by_the_registry_not_the_filename() {
        let p = pool();
        assert!(!p["sword-fight"].looped);
        assert!(p["day"].looped, "a bed loops by default");
        // The flag rides along on the pick, so the caller never has to look the
        // sound up a second time to learn how it plays.
        assert!(!pick(&p, &tags(&["battle", "sword"]), 0).unwrap().looped);
    }

    #[test]
    fn a_missing_or_broken_registry_is_an_empty_pool() {
        assert!(load_pool(Path::new("/nonexistent/effect-pool.json")).is_empty());
        let d = std::env::temp_dir().join("bm-clip-pool-broken");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("effect-pool.json"), "{ nope").unwrap();
        assert!(load_pool(&d.join("effect-pool.json")).is_empty());
        std::fs::write(d.join("effect-pool.json"), "[]").unwrap();
        assert!(load_pool(&d.join("effect-pool.json")).is_empty());
    }

    #[test]
    fn the_note_key_is_not_a_sound() {
        let d = std::env::temp_dir().join("bm-clip-pool-note");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("p.json"),
            r#"{"_note":"x","rain":{"tags":["rain"],"files":["effects/rain-1.mp3"]}}"#,
        )
        .unwrap();
        let p = load_pool(&d.join("p.json"));
        assert_eq!(p.len(), 1, "{p:?}");
        assert_eq!(p["rain"].files, vec!["effects/rain-1.mp3"]);
        assert!(p["rain"].looped, "looped defaults on");
    }

    #[test]
    fn filename_tags_come_from_the_one_shared_parser() {
        // The three pools must never disagree about what a filename means.
        assert_eq!(parse_sample_tags("night-1"), vec!["night"]);
        assert_eq!(parse_sample_tags("young-female-1"), vec!["young", "female"]);
    }

    // -----------------------------------------------------------------------
    // writing a registry
    // -----------------------------------------------------------------------

    /// A scratch copy of a shipped registry. Never write to `assets/` from a
    /// test: the writer's whole promise is that it leaves those files alone.
    fn shipped_copy(kind: PoolKind, tag: &str) -> (std::path::PathBuf, String) {
        let src = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../assets")
            .join(kind.registry());
        let original = std::fs::read_to_string(&src).expect("shipped registry");
        let dir = std::env::temp_dir().join(format!("bm-pool-write-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(kind.registry());
        std::fs::write(&path, &original).unwrap();
        (path, original)
    }

    /// The property the whole writer exists for: an edit must cost the diff of
    /// an edit, not the diff of a re-serialise. Every shipped registry is
    /// hand-formatted — some arrays inline, some expanded, and a `_note` in
    /// prose — so a round trip through the writer has to return the same bytes.
    #[test]
    fn saving_an_untouched_registry_rewrites_nothing() {
        for kind in PoolKind::ALL {
            let (path, original) = shipped_copy(kind, "noop");
            let pool = load_pool(&path);
            assert!(
                !pool.is_empty(),
                "{}: fixture did not load",
                kind.registry()
            );
            save_pool(&path, kind, &pool).unwrap();
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                original,
                "{} was rewritten by a save that changed nothing",
                kind.registry()
            );
        }
    }

    /// Removal is surgical: the entry goes, everything around it — including
    /// the notes and the neighbours' own spacing — is untouched.
    #[test]
    fn removing_an_entry_leaves_the_rest_byte_identical() {
        let (path, original) = shipped_copy(PoolKind::Music, "remove");
        let mut pool = load_pool(&path);
        assert!(pool.remove("market").is_some());
        save_pool(&path, PoolKind::Music, &pool).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();

        // The block, exactly as it stands in the file, is gone...
        let block = "  \"market\": {\n    \"tags\": [\"market\", \"busy\", \"day\"],\n    \"files\": [\"music/market-bg-1.mp3\"]\n  },\n";
        assert!(original.contains(block), "fixture moved; fix this test");
        assert!(!after.contains("\"market\""), "{after}");
        // ...and the file is the original with exactly that block cut out.
        assert_eq!(after, original.replace(block, ""), "{after}");
        // The registry still reads back as the pool we wrote.
        assert_eq!(load_pool(&path), pool);
    }

    /// A new entry is rendered in its layer's field order and lands in name
    /// order, so the file stays diffable against the ones around it.
    #[test]
    fn a_new_entry_is_written_in_its_layers_field_order() {
        let (path, _) = shipped_copy(PoolKind::Inject, "add");
        let mut pool = load_pool(&path);
        pool.insert(
            "kettle".into(),
            Sound {
                tags: vec!["kettle".into(), "whistle".into()],
                files: vec!["injects/kettle-1.mp3".into()],
                looped: false,
                dur_s: Some(2.5),
                mode: Some("hit".into()),
                hold: None,
                level: Some(0.8),
            },
        );
        save_pool(&path, PoolKind::Inject, &pool).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        // In name order, and in the file's field order: tags, files, mode,
        // level, looped, dur_s. Both lists are short enough to stay inline.
        assert!(
            after.contains(
                "  \"kettle\": {\n    \"tags\": [\"kettle\", \"whistle\"],\n    \"files\": [\"injects/kettle-1.mp3\"],\n    \"mode\": \"hit\",\n    \"level\": 0.8,\n    \"looped\": false,\n    \"dur_s\": 2.5\n  },\n"
            ),
            "{after}"
        );
        // ...and it sits between its neighbours by name, not at the end.
        let kettle = after.find("\"kettle\"").unwrap();
        assert!(after.find("\"intense\"").is_none());
        assert!(after.find("\"light-spell\"").unwrap() > kettle);
        assert!(after.find("\"fire-spell\"").unwrap() < kettle);
        assert_eq!(load_pool(&path)["kettle"].level, Some(0.8));
    }

    /// An edit to one number must not collapse an entry the author expanded.
    #[test]
    fn an_edited_entry_keeps_the_shape_it_had() {
        let (path, _) = shipped_copy(PoolKind::Inject, "shape");
        let mut pool = load_pool(&path);
        // `cooking` ships expanded, with two takes and a level of 0.8.
        pool.get_mut("cooking").unwrap().level = Some(0.35);
        save_pool(&path, PoolKind::Inject, &pool).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("  \"cooking\": {\n    \"tags\": [\n      \"cooking\",\n"),
            "an expanded entry came back inline:\n{after}"
        );
        assert!(after.contains("\"level\": 0.35"), "{after}");
        // And every other entry kept its own bytes.
        for other in ["\"boiling-water\"", "\"food-prep\"", "\"coin\""] {
            assert!(after.contains(other), "{other} went missing");
        }
    }

    /// A short list stays on one line when the entry is new — the same habit
    /// the hand-written effect and music registries have.
    #[test]
    fn a_short_list_stays_inline_but_a_long_one_does_not() {
        let (path, _) = shipped_copy(PoolKind::Effect, "inline");
        let mut pool = load_pool(&path);
        let mk = |files: Vec<String>| Sound {
            tags: vec!["probe".into()],
            files,
            looped: true,
            dur_s: None,
            mode: None,
            hold: None,
            level: None,
        };
        pool.insert(
            "short-probe".into(),
            mk(vec!["effects/short-probe-1.mp3".into()]),
        );
        pool.insert(
            "long-probe".into(),
            mk((1..=6)
                .map(|n| format!("effects/long-probe-{n}.mp3"))
                .collect()),
        );
        save_pool(&path, PoolKind::Effect, &pool).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("\"files\": [\"effects/short-probe-1.mp3\"]"),
            "a one-take entry was expanded:\n{after}"
        );
        assert!(
            after.contains("\"files\": [\n      \"effects/long-probe-1.mp3\","),
            "six takes were kept on one line:\n{after}"
        );
        // A bed omits `looped`; a one-shot states it.
        assert!(
            after.contains("\"files\": [\"effects/short-probe-1.mp3\"]\n  }"),
            "{after}"
        );
        assert_eq!(load_pool(&path).len(), pool.len());
    }

    /// The `_note` is prose an author wrote, and it is the only written record
    /// of why the pool is shaped the way it is. It must survive verbatim —
    /// including its own line, not re-encoded.
    #[test]
    fn the_note_survives_verbatim() {
        let (path, original) = shipped_copy(PoolKind::Effect, "note");
        let mut pool = load_pool(&path);
        pool.remove("rain");
        save_pool(&path, PoolKind::Effect, &pool).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        let note = original
            .lines()
            .find(|l| l.contains("\"_note\""))
            .expect("fixture has a note");
        assert!(after.contains(note), "the note did not survive");
        assert_eq!(
            after.lines().next().unwrap(),
            "{",
            "the note must stay the first member"
        );
    }

    /// A registry the scanner cannot read is refused, never replaced. Losing a
    /// pool to a stray bracket would be silent and total.
    #[test]
    fn a_registry_this_writer_cannot_read_is_refused_not_replaced() {
        let dir = std::env::temp_dir().join("bm-pool-write-bad");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("music-pool.json");
        for bad in ["{ nope", "[]", "{\"a\": }", "{\"a\": 1", "{\"a\" 1}"] {
            std::fs::write(&path, bad).unwrap();
            let pool = load_pool(&path);
            let res = save_pool(&path, PoolKind::Music, &pool);
            assert!(res.is_err(), "{bad:?} was accepted");
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                bad,
                "{bad:?} was overwritten"
            );
        }
    }

    /// A readable file whose members are not sounds: the `_`-prefixed ones are
    /// notes and are kept, the rest *are* the pool — a key that will not parse
    /// as a `Sound` is already invisible to `load_pool`, so writing drops it and
    /// the file and the pool agree again. That is the one case where a save
    /// removes something the operator did not name, so it is pinned here.
    #[test]
    fn a_member_that_is_not_a_sound_is_dropped_and_the_notes_are_kept() {
        let dir = std::env::temp_dir().join("bm-pool-write-junk");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("music-pool.json");
        std::fs::write(
            &path,
            "{\n  \"_note\": \"kept\",\n  \"broken\": 7,\n  \"market\": {\"tags\": [\"market\"], \"files\": [\"music/market-bg-1.mp3\"]}\n}\n",
        )
        .unwrap();
        let pool = load_pool(&path);
        assert_eq!(pool.len(), 1, "only `market` is a sound");
        save_pool(&path, PoolKind::Music, &pool).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            after,
            "{\n  \"_note\": \"kept\",\n  \"market\": {\"tags\": [\"market\"], \"files\": [\"music/market-bg-1.mp3\"]}\n}\n"
        );
    }

    /// A registry that does not exist yet is created rather than refused: an
    /// operator adding the first sound to an empty layer is ordinary.
    #[test]
    fn a_missing_registry_is_created() {
        let dir = std::env::temp_dir().join("bm-pool-write-fresh");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("inject-pool.json");
        let mut pool = ClipPool::new();
        pool.insert(
            "coin".into(),
            Sound {
                tags: vec!["coin".into()],
                files: vec!["injects/coin-1.mp3".into()],
                looped: false,
                dur_s: Some(1.0),
                mode: Some("hit".into()),
                hold: None,
                level: None,
            },
        );
        save_pool(&path, PoolKind::Inject, &pool).unwrap();
        assert_eq!(load_pool(&path), pool);
    }

    /// The ladder: a sound's own trim is a plain multiplier, and an absent one
    /// is 1.0 — so a registry written before the field existed mixes exactly as
    /// it did.
    #[test]
    fn an_absent_level_is_one_and_a_level_rides_on_the_pick() {
        let mut p = ClipPool::new();
        let mut s = Sound {
            tags: tags(&["day"]),
            files: vec!["effects/day-1.mp3".into()],
            looped: true,
            dur_s: None,
            mode: None,
            hold: None,
            level: None,
        };
        assert_eq!(sound_level(&s), 1.0);
        p.insert("day".into(), s.clone());
        assert_eq!(pick(&p, &tags(&["day"]), 0).unwrap().level, 1.0);

        s.level = Some(0.25);
        p.insert("day".into(), s.clone());
        assert_eq!(sound_level(&s), 0.25);
        assert_eq!(pick(&p, &tags(&["day"]), 0).unwrap().level, 0.25);

        // Zero and negatives read as "unset", never as a mute: a pool cannot
        // silence a layer by arithmetic accident.
        s.level = Some(0.0);
        assert_eq!(sound_level(&s), 1.0);
        s.level = Some(-2.0);
        assert_eq!(sound_level(&s), 1.0);
    }

    /// Every shipped sound resolves to 1.0 today. If that ever stops being
    /// true the mix has changed, and it should be a decision, not a surprise.
    #[test]
    fn the_shipped_registries_are_all_at_unity_today() {
        for kind in PoolKind::ALL {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../../assets")
                .join(kind.registry());
            for (name, sound) in load_pool(&path) {
                if kind == PoolKind::Inject {
                    continue; // the inject layer has always had per-sound trims
                }
                assert_eq!(
                    sound_level(&sound),
                    1.0,
                    "{}/{name} carries a level; the effect and music layers used to ignore it",
                    kind.registry()
                );
            }
        }
    }

    #[test]
    fn the_registry_paths_are_spelled_once() {
        let l = crate::Layout::new("/repo");
        for kind in PoolKind::ALL {
            assert!(l.pool(kind).ends_with(kind.registry()));
            assert!(l.assets().join(kind.dir()).starts_with(l.assets()));
        }
        assert_eq!(PoolKind::ALL.len(), 3);
    }
}
