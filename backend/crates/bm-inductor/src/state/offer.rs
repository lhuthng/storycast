use super::plan::present;
use super::{lease_for_batch, shelve_after, Inner};
use bm_core::assemble::RenderPlan;
use bm_proto::{now_secs, Complete, Stage, Task, TaskOffer, TaskState};
use serde_json::{json, Value};
use std::collections::HashMap;

/// RAM percent above which a box is offered nothing.
const MEM_PCT_CEILING: f32 = 90.0;

/// The two places a script names a speaker, and nothing else: the roster walks
/// every script in the book on each fetch, so the read is typed rather than a
/// `Value` tree it then picks apart.
#[derive(serde::Deserialize)]
struct ScriptNames {
    #[serde(default)]
    roster: Vec<String>,
    #[serde(default)]
    segments: Vec<ScriptSpeaker>,
}

#[derive(serde::Deserialize)]
struct ScriptSpeaker {
    #[serde(default)]
    speaker: String,
}

mod build;
mod complete;
mod fail;
impl Inner {
    pub fn counts(&self) -> HashMap<String, HashMap<String, usize>> {
        // Borrowed keys inside the loop: a book is tens of thousands of rows and
        // this runs on every `/api/state`, so the only allocations are the few
        // stage and state names the ledger actually uses.
        let mut out: HashMap<&'static str, HashMap<&'static str, usize>> = HashMap::new();
        for t in self.tasks.values() {
            *out.entry(t.stage.as_str())
                .or_default()
                .entry(t.state.as_str())
                .or_default() += 1;
        }
        out.into_iter()
            .map(|(stage, states)| {
                (
                    stage.to_string(),
                    states
                        .into_iter()
                        .map(|(state, n)| (state.to_string(), n))
                        .collect(),
                )
            })
            .collect()
    }

    /// The cast exactly as the cast file holds it.
    pub fn cast_snapshot(&self) -> std::collections::BTreeMap<String, String> {
        bm_core::cast::read_cast(
            &self.settings.engine,
            &self.layout.cast(&self.settings.engine),
        )
        .into_map()
    }

    /// Every speaker the inductor can name: the operator's cast, the cast file,
    pub fn known_characters(&self) -> Vec<String> {
        use std::collections::BTreeSet;
        let engine = self.settings.engine.clone();
        let mut set: BTreeSet<String> = BTreeSet::new();
        let (effective, _) = bm_core::voices::effective_engine_lenient(&engine);
        for (name, _) in &effective.to_policy(&engine).default_cast {
            set.insert(name.clone());
        }
        for name in self.cast_snapshot().keys() {
            set.insert(name.clone());
        }
        let bible = bm_core::digest::load_bible(&self.layout.bible());
        if let Some(chars) = bible.get("characters").and_then(|c| c.as_array()) {
            for c in chars {
                if let Some(n) = c.get("name").and_then(|n| n.as_str()) {
                    if !n.is_empty() {
                        set.insert(n.to_string());
                    }
                }
            }
        }
        let scripts = self.layout.scripts();
        for sp in scripts {
            let Ok(doc) = bm_core::read_json::<ScriptNames>(&sp) else {
                continue;
            };
            for n in doc.roster {
                if !n.is_empty() {
                    set.insert(n);
                }
            }
            for seg in doc.segments {
                if !seg.speaker.is_empty() {
                    set.insert(seg.speaker);
                }
            }
        }
        // `Narrator` is the one speaker that always exists; it leads the list.
        set.remove("Narrator");
        let mut out: Vec<String> = std::iter::once("Narrator".to_string()).collect();
        out.extend(set);
        out
    }
}
