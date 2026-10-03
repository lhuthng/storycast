//! Does a published chapter still match the sound design on disk?

use super::Inner;
use bm_core::design::{Knobs, MergeDesign};
use bm_proto::{now_secs, Stage, TaskState};

impl Inner {
    /// The merge knobs as `bm_core::design` spells them.
    pub(crate) fn design_knobs(&self) -> Knobs {
        Knobs {
            gap_ms: self.settings.gap_ms,
            speed: self.settings.speed,
            on: bm_core::ambience::LayerSwitch::new(
                self.settings.ambience,
                self.settings.music,
                self.settings.effect_volume,
                self.settings.music_volume,
                self.settings.inject_volume,
            ),
        }
    }

    /// The design stamp for one chapter, or `None` when it cannot be computed —
    pub(crate) fn design_stamp(
        &self,
        design: &MergeDesign,
        knobs: Knobs,
        chapter: u32,
    ) -> Option<String> {
        let text = std::fs::read_to_string(self.layout.script(chapter)).ok()?;
        let script: serde_json::Value = serde_json::from_str(&text).ok()?;
        Some(design.fingerprint(&script, knobs))
    }

    /// Requeue every merge whose sound design has moved on, deleting its mp3.
    pub fn invalidate_stale_design(&mut self, adopt: bool) -> Vec<u32> {
        // Loaded once for the whole pass: the fingerprint reads four registries,
        let design = MergeDesign::load(&self.layout);
        let knobs = self.design_knobs();
        let now = now_secs();

        // Plan first, mutate second. The fingerprint reads `self.layout` and
        let mut requeue: Vec<(String, u32, String)> = Vec::new();
        let mut stamp_only: Vec<(String, String)> = Vec::new();
        for t in self.tasks.values() {
            if t.stage != Stage::Merge {
                continue;
            }
            let Some(current) = self.design_stamp(&design, knobs, t.chapter) else {
                continue;
            };
            match &t.design {
                Some(was) if *was != current => {
                    requeue.push((t.id(), t.chapter, current));
                }
                // Unstamped: adopt, or treat as invalidated — see the doc above.
                None if t.state == TaskState::Done && !adopt => {
                    requeue.push((t.id(), t.chapter, current));
                }
                // Adoption is by *writing* the stamp, not by leaving it absent.
                None if t.state == TaskState::Done
                    && self.layout.final_mp3(t.chapter).is_file() =>
                {
                    stamp_only.push((t.id(), current));
                }
                _ => {}
            }
        }

        // Read before the loops consume them, and used to decide the single
        let touched = !requeue.is_empty() || !stamp_only.is_empty();

        let mut chapters: Vec<u32> = Vec::new();
        for (id, chapter, current) in requeue {
            // Before the state change: a `Pending` merge whose mp3 survives is
            let _ = std::fs::remove_file(self.layout.final_mp3(chapter));
            if let Some(t) = self.tasks.get_mut(&id) {
                t.state = TaskState::Pending;
                t.attempts = 0;
                t.clear_holders();
                t.lease_until = None;
                t.detail = "requeued: sound design changed".into();
                t.updated = now;
                t.design = Some(current);
            }
            chapters.push(chapter);
        }
        for (id, current) in stamp_only {
            if let Some(t) = self.tasks.get_mut(&id) {
                t.design = Some(current);
            }
        }

        if touched {
            self.save();
        }
        chapters.sort_unstable();
        chapters.dedup();
        chapters
    }
}
