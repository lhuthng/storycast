//! Does a published chapter still match the sound design on disk?
//!
//! The fingerprint itself is `bm_core::design`; this is the half that owns a
//! ledger. A merge task records the design it was mixed under, and any merge
//! whose record no longer matches the design in force is requeued — with its
//! mp3 deleted, because a stale mp3 sitting in `output/` is a file the current
//! design would not have produced.
//!
//! Deleting is not optional. `reconcile` promotes a `Pending` merge to `Done`
//! whenever the mp3 is on disk, so a requeue that left the file behind would be
//! undone by the very next pass.

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
    /// no script, unreadable JSON. `None` is *not* staleness: a chapter that
    /// cannot be planned has no mix to be wrong about, and requeueing it here
    /// would be this pass guessing at another stage's job.
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
    /// Returns the chapters touched, sorted.
    ///
    /// `adopt` decides what a merge with **no** stamp means, and it is the whole
    /// reason this takes an argument. Such a merge predates the field, so
    /// calling it stale would re-merge an entire library the first time this
    /// runs — but a caller that has *just changed the design* is looking at a
    /// library where an unstamped merge is by definition one that change
    /// invalidated. So: every routine caller passes `true`, and the two that
    /// write a design (`op_remix`, `Op::SoundChanged`) pass `false`.
    ///
    /// Note what `adopt` does not do: it does not make a *stamped* merge safe.
    /// A stamped merge whose design moved is always requeued, whatever the flag.
    pub fn invalidate_stale_design(&mut self, adopt: bool) -> Vec<u32> {
        // Loaded once for the whole pass: the fingerprint reads four registries,
        // and a library is hundreds of chapters.
        let design = MergeDesign::load(&self.layout);
        let knobs = self.design_knobs();
        let now = now_secs();

        // Plan first, mutate second. The fingerprint reads `self.layout` and
        // `self.settings`; the requeue writes `self.tasks`, and the borrow
        // checker is right to refuse both at once.
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
                // A merge left unstamped would be re-examined by every later
                // pass and stay invisible to a design change for ever.
                None if t.state == TaskState::Done
                    && self.layout.final_mp3(t.chapter).is_file() =>
                {
                    stamp_only.push((t.id(), current));
                }
                _ => {}
            }
        }

        // Read before the loops consume them, and used to decide the single
        // save: an adoption is a write too, so a pass that only adopted still
        // has to reach the disk or the next boot adopts again.
        let touched = !requeue.is_empty() || !stamp_only.is_empty();

        let mut chapters: Vec<u32> = Vec::new();
        for (id, chapter, current) in requeue {
            // Before the state change: a `Pending` merge whose mp3 survives is
            // promoted straight back to `Done` by the next reconcile.
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
