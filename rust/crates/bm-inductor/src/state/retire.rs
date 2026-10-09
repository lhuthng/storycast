//! Retiring a box: `:drop` forgets one machine all the way down.

use super::Inner;
use bm_proto::{now_secs, TaskState};
use std::collections::BTreeSet;

impl Inner {
    /// Forget one machine: its registry entry, its runtime state, and every row
    /// its workers were holding (those go back to the pool, so another box can
    /// take them). The address is remembered so a worker still running on that
    /// box is refused — its beat ignored, its polls unanswered — instead of
    /// re-registering the box as a nameless stray. Re-binding it (`:add`)
    /// clears that refusal.
    pub fn drop_machine(&mut self, addr: &str) -> String {
        let known = self.machines.remove(addr).is_some();
        let _ = bm_core::provision::remove_box(&self.layout.machines(), addr);

        // The workers that reported this address. Their mapping stays behind on
        // purpose: it is what lets `offer` resolve a dropped box's poll back to
        // this address and refuse it. Their beats and caps go.
        let workers: BTreeSet<String> = self
            .workers
            .iter()
            .filter(|(_, a)| a.as_str() == addr)
            .map(|(w, _)| w.clone())
            .collect();
        for w in &workers {
            self.beats.remove(w);
            self.caps.remove(w);
        }
        let released = self.release_held_rows(&workers);
        self.dropped.insert(addr.to_string());
        self.save();

        let line = if known {
            format!("dropped {addr} from the registry")
        } else {
            format!("{addr} was not registered — the address is now refused")
        };
        let line = if released > 0 {
            format!("{line}; {released} in-flight row(s) returned to the pool")
        } else {
            line
        };
        self.push_event("info", line.clone());
        line
    }

    /// Return every row held by these workers to the pool. Racing rows keep
    /// their other holders, exactly as the reaper's orphan pass does: only the
    /// dropped box's share is taken back.
    fn release_held_rows(&mut self, workers: &BTreeSet<String>) -> usize {
        let now = now_secs();
        let mut released = 0;
        for t in self.tasks.values_mut() {
            if !matches!(t.state, TaskState::Assigned | TaskState::Running) {
                continue;
            }
            let held: Vec<String> = t
                .holders()
                .into_iter()
                .filter(|w| workers.contains(*w))
                .map(str::to_string)
                .collect();
            if held.is_empty() {
                continue;
            }
            for w in &held {
                t.remove_holder(w);
            }
            if t.holders().is_empty() {
                Self::release(t, now, "machine dropped");
                released += 1;
            } else {
                t.detail = "requeued: one racer dropped, still racing".to_string();
                t.updated = now;
            }
        }
        released
    }
}
