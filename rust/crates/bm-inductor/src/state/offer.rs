use super::plan::present;
use super::{lease_for_batch, shelve_after, Inner};
use bm_core::assemble::RenderPlan;
use bm_proto::{now_secs, Complete, Stage, Task, TaskOffer, TaskState};
use serde_json::{json, Value};
use std::collections::HashMap;

/// RAM percent above which a box is offered nothing.
///
/// **A guardrail, not a capacity model.** One sidecar is ~36% of an 8 GiB box,
/// so a healthy busy worker sits near half; a box over this line is one the OOM
/// killer is already circling, a second sidecar, a leak, a co-resident ffmpeg
/// and every task handed to it ends with a struck chapter and no artifact. The
/// threshold only *withholds*: nothing is failed and nothing moves, and a box
/// that settles (the idle reaper returns the model's pages) is offered work
/// again on its next ask. Deliberately high, because the cost of a false
/// positive is a box sitting idle while its peers absorb the queue, and one
/// threshold because the stage with the biggest working set is the merge, and
/// it reaps the model before ffmpeg (see `bm-agent`'s `Sidecar::reap_all`), so
/// the reading it is judged on is already net of the 2.85 GB it frees.
const MEM_PCT_CEILING: f32 = 90.0;

mod build;
mod complete;
mod fail;
impl Inner {


    pub fn counts(&self) -> HashMap<String, HashMap<String, usize>> {
        let mut out: HashMap<String, HashMap<String, usize>> = HashMap::new();
        for t in self.tasks.values() {
            let e = out.entry(t.stage.as_str().into()).or_default();
            let k = format!("{:?}", t.state).to_lowercase();
            *e.entry(k).or_default() += 1;
        }
        out
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
    /// the bible, and every script's roster and segments.
    ///
    /// This is the voice picker's first step, without it the operator has to
    /// recall exact Vietnamese character names from memory. The shipped
    /// catalogue carries no character names, so the seed is the operator's own
    /// roster; a malformed one seeds nothing, which is a missing convenience
    /// rather than a broken gate.
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
            let Ok(data) = bm_core::read_json::<Value>(&sp) else {
                continue;
            };
            if let Some(roster) = data.get("roster").and_then(|r| r.as_array()) {
                for n in roster.iter().filter_map(|v| v.as_str()) {
                    if !n.is_empty() {
                        set.insert(n.to_string());
                    }
                }
            }
            if let Some(segs) = data.get("segments").and_then(|s| s.as_array()) {
                for s in segs {
                    if let Some(sp) = s.get("speaker").and_then(|v| v.as_str()) {
                        if !sp.is_empty() {
                            set.insert(sp.to_string());
                        }
                    }
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


