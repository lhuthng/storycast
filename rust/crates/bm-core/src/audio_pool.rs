//! Sound-design clip pools: the effect layer's beds, the music layer's tracks
//! The voice pool (`pool.rs`) answers "which of these clips may stand in for
//! this character". This is the same shape one level down: a JSON registry

use anyhow::{anyhow, Context, Result};
use std::collections::BTreeMap;
use std::path::Path;

/// Filename tags are shared with the voice pool: one parser, three pools.
pub use crate::pool::parse_sample_tags;

/// Which registry a pool is.
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
    #[serde(default)]
    pub files: Vec<String>,
    /// A loop is stretched to fill its window; a one-shot plays once and its
    #[serde(default = "loops")]
    pub looped: bool,
    /// Longest take in seconds, post-trim. Only the inject registry sets it:
    #[serde(default)]
    pub dur_s: Option<f64>,
    /// `hit` | `overlap` | `trail`. Only the inject registry sets it, and it is
    #[serde(default)]
    pub mode: Option<String>,
    /// Solo seconds before a `trail`'s tail ducks under the speech. `None`
    #[serde(default)]
    pub hold: Option<f64>,
    /// Optional trim for this sound alone. `None` is 1.0.
    #[serde(default)]
    pub level: Option<f64>,
}

fn loops() -> bool {
    true
}

/// `sound -> sound`. Ordered so the file on disk diffs cleanly — and so a pick
pub type ClipPool = BTreeMap<String, Sound>;

/// A resolved pick: which sound answered, and which of its takes.
#[derive(Debug, Clone, PartialEq)]
pub struct Picked {
    /// The sound's name — `day`. This is what the log reports, because it is
    pub sound: String,
    /// The take, relative to `assets/` (`effects/day-2.mp3`) — the same
    pub file: String,
    pub looped: bool,
    /// The sound's own trim, `Sound::level` with `None` already resolved to
    pub level: f64,
}

/// Read a registry. A missing or broken file is "no pool", not an error: a
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

/// Longest a field may be and still stay on one line, in columns. Not an
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
fn parses_as(raw: &str, sound: &Sound) -> bool {
    serde_json::from_str::<Sound>(raw)
        .map(|s| s == *sound)
        .unwrap_or(false)
}

/// One entry, in the field order its layer's file uses.
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
/// question the mix asks, and a second implementation of "which clip answers
/// this tag" would be free to disagree with the mix about a chapter being
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
pub fn pick(pool: &ClipPool, tags: &[String], seed: u64) -> Option<Picked> {
    let cands = candidates(pool, tags);
    // The guard has to come *before* the index, not inside it: `seed % 0` panics,
    if cands.is_empty() {
        return None;
    }
    let (sound, entry) = cands[(seed % cands.len() as u64) as usize];
    // A second, decorrelated roll picks the take: two chapters that land on the
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
pub fn sound_level(sound: &Sound) -> f64 {
    sound.level.filter(|l| *l > 0.0).unwrap_or(1.0)
}

/// Seed for one pick: FNV-1a over the chapter and the tag set.
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
