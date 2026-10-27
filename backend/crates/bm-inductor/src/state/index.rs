//! A disk cache of which speakers a chapter names, so a voice swap answers
//! from the index instead of parsing every script in the book.

use bm_core::assemble::{Planned, RenderPlan};
use bm_core::digest::resolve_speaker;
use bm_core::Layout;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

const VERSION: u32 = 1;

/// A file's freshness key. `len` catches same-timestamp writes that a coarse
/// filesystem clock would hide; a content hash would mean reading the file,
/// which is the cost this cache removes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Stamp {
    mtime_ns: u64,
    len: u64,
}

impl Stamp {
    fn of(path: &Path) -> Stamp {
        let Ok(meta) = std::fs::metadata(path) else {
            return Stamp::default();
        };
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        Stamp {
            mtime_ns,
            len: meta.len(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Chapter {
    #[serde(default)]
    script: Stamp,
    #[serde(default)]
    plan: Stamp,
    /// Speaker labels as the script writes them.
    #[serde(default)]
    surfaces: Vec<String>,
    /// `surfaces` resolved through the bible the index was built against.
    #[serde(default)]
    names: Vec<String>,
    /// Speakers the recorded render plan still holds.
    #[serde(default)]
    takes: Vec<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct Doc {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    bible: Stamp,
    #[serde(default)]
    chapters: BTreeMap<u32, Chapter>,
}

/// Which chapters name which speakers, read from `speaker-index.json` and
/// refreshed only where a script or its plan changed.
pub(crate) struct SpeakerIndex {
    chapters: BTreeMap<u32, Chapter>,
}

impl SpeakerIndex {
    pub(crate) fn load(layout: &Layout) -> SpeakerIndex {
        let path = layout.speaker_index();
        let mut doc: Doc = bm_core::read_json(&path).unwrap_or_default();
        let mut dirty = doc.version != VERSION;
        if dirty {
            doc = Doc {
                version: VERSION,
                ..Doc::default()
            };
        }

        // A bible edit re-resolves the names already collected, so the
        // scripts do not have to be read again.
        let bible_stamp = Stamp::of(&layout.bible());
        let mut bible: Option<Value> = None;
        if doc.bible != bible_stamp {
            let text = bm_core::digest::load_bible(&layout.bible());
            let mut memo: HashMap<String, String> = HashMap::new();
            for chapter in doc.chapters.values_mut() {
                chapter.names = resolve_all(&text, &chapter.surfaces, &mut memo);
            }
            bible = Some(text);
            doc.bible = bible_stamp;
            dirty = true;
        }

        let mut chapters: BTreeMap<u32, Chapter> = BTreeMap::new();
        for script in layout.scripts() {
            let Some(n) = bm_core::paths::chapter_of(&script) else {
                continue;
            };
            let plan = layout.plan(n);
            let script_stamp = Stamp::of(&script);
            let plan_stamp = Stamp::of(&plan);
            let fresh = doc
                .chapters
                .get(&n)
                .is_some_and(|c| c.script == script_stamp && c.plan == plan_stamp);
            if fresh {
                chapters.insert(n, doc.chapters.remove(&n).expect("checked above"));
                continue;
            }
            let text = bible.get_or_insert_with(|| bm_core::digest::load_bible(&layout.bible()));
            chapters.insert(
                n,
                build_chapter(&script, &plan, text, script_stamp, plan_stamp),
            );
            dirty = true;
        }
        // Anything the walk did not visit names a script that is gone.
        if !doc.chapters.is_empty() {
            dirty = true;
        }

        if dirty || !path.is_file() {
            let doc = Doc {
                version: VERSION,
                bible: bible_stamp,
                chapters: chapters.clone(),
            };
            let _ = bm_core::write_json(&path, &doc);
        }
        SpeakerIndex { chapters }
    }

    /// Chapters that would hear any of these names, under the name or a bible
    /// alias. A render plan carrying the name counts even with no script line.
    /// The raw label is compared too: a merge may already have made the
    /// absorbed spelling an alias of the survivor when invalidation runs.
    pub(crate) fn chapters_hearing(&self, names: &[String]) -> Vec<u32> {
        self.chapters
            .iter()
            .filter(|(_, c)| {
                c.surfaces
                    .iter()
                    .chain(&c.names)
                    .any(|name| names.iter().any(|w| w == name))
                    || c.takes.iter().any(|take| names.iter().any(|w| w == take))
            })
            .map(|(n, _)| *n)
            .collect()
    }
}

fn build_chapter(
    script: &Path,
    plan: &Path,
    bible: &Value,
    script_stamp: Stamp,
    plan_stamp: Stamp,
) -> Chapter {
    let mut surfaces: Vec<String> = Vec::new();
    if let Ok(data) = bm_core::read_json::<Value>(script) {
        let segments = data
            .get("segments")
            .and_then(|s| s.as_array())
            .cloned()
            .unwrap_or_default();
        for run in Planned::plan(&segments).runs() {
            push_unique(&mut surfaces, &run.speaker);
        }
    }
    let mut memo: HashMap<String, String> = HashMap::new();
    let names = resolve_all(bible, &surfaces, &mut memo);
    let mut takes: Vec<String> = Vec::new();
    if let Some(plan) = RenderPlan::load(plan) {
        for take in plan.takes {
            push_unique(&mut takes, &take.speaker);
        }
    }
    Chapter {
        script: script_stamp,
        plan: plan_stamp,
        surfaces,
        names,
        takes,
    }
}

fn resolve_all(
    bible: &Value,
    surfaces: &[String],
    memo: &mut HashMap<String, String>,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for surface in surfaces {
        let name = memo
            .entry(surface.clone())
            .or_insert_with(|| resolve_speaker(bible, surface))
            .clone();
        push_unique(&mut out, &name);
    }
    out
}

fn push_unique(out: &mut Vec<String>, name: &str) {
    let name = name.trim();
    if !name.is_empty() && !out.iter().any(|n| n == name) {
        out.push(name.to_string());
    }
}

#[cfg(test)]
mod tests;
