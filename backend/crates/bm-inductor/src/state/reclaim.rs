//! Taking a box out of service for good: its work back into the pool, its row
//! out of the registry, its cloud-side delete queued until the provider agrees.

use super::{DeleteIntent, Inner};
use bm_proto::{now_secs, MachineState};
use std::collections::BTreeSet;

/// Attempts before the sweep stops trying and leaves the intent for an
/// operator: a delete that keeps failing is a permission or a hand-edited
/// configuration, and retrying it every minute forever hides that.
pub const MAX_DELETE_ATTEMPTS: u32 = 5;

impl Inner {
    /// The workers that reported this address, if any.
    fn workers_on(&self, addr: &str) -> BTreeSet<String> {
        self.workers
            .iter()
            .filter(|(_, a)| a.as_str() == addr)
            .map(|(w, _)| w.clone())
            .collect()
    }

    /// Retire a box the provider has lost.
    ///
    /// Only a row with both halves — the provider and its id — is deleted on
    /// the cloud: a hand-linked LAN machine is the operator's to remove, and it
    /// is marked killed rather than forgotten so the pane still shows it.
    ///
    /// Returns the line to log, or `None` when there was nothing to do.
    pub fn reclaim(&mut self, addr: &str, why: &str) -> Option<String> {
        let (provider, provider_id) = {
            let m = self.machines.get(addr)?;
            (m.provider.clone(), m.provider_id.clone())
        };

        // The work comes back first: a row must never stay leased to a box
        // that is being deleted. Racing rows keep their other holders.
        let workers = self.workers_on(addr);
        for w in &workers {
            self.beats.remove(w);
            self.caps.remove(w);
        }
        let released = self.release_held_rows(&workers);
        // Whatever still polls this address is refused from here on, exactly as
        // `:drop` does, so a worker surviving on a paused box gets no work.
        self.dropped.insert(addr.to_string());
        let back = if released > 0 {
            format!(" — {released} row(s) back in the pool")
        } else {
            String::new()
        };

        let (Some(provider), Some(provider_id)) = (provider, provider_id) else {
            // No cloud half: keep the row, say it is finished, and stop there.
            if let Some(m) = self.machines.get_mut(addr) {
                m.set_state(MachineState::Killed);
                m.note = format!("killed: {why} — not deleted (no provider id){back}");
            }
            let line = format!("{addr} killed: {why} — kept, no provider id to delete{back}");
            self.push_event("warn", line.clone());
            self.save();
            return Some(line);
        };

        self.machines.remove(addr);
        let _ = bm_core::provision::remove_box(&self.layout.machines(), addr);
        // One intent per provider id: a deadline verdict after a spot verdict is
        // the same box, and booking the delete twice ends in a 404 as a failure.
        let queued = !self.intents.iter().any(|i| i.provider_id == provider_id);
        if queued {
            self.intents.push(DeleteIntent {
                provider: provider.clone(),
                provider_id: provider_id.clone(),
                addr: addr.to_string(),
                why: why.to_string(),
                attempts: 0,
                last_error: None,
                created: now_secs(),
            });
        }
        let line = format!("[{addr}] killed: {why} — {provider} {provider_id}{back}");
        self.push_event("warn", line.clone());
        self.save();
        Some(line)
    }

    /// The delete the sweep should attempt now, oldest first. An intent past its
    /// attempt cap is skipped: it waits for `retry_intents`.
    pub fn next_intent(&self) -> Option<DeleteIntent> {
        self.intents
            .iter()
            .filter(|i| i.attempts < MAX_DELETE_ATTEMPTS)
            .min_by_key(|i| (i.created, i.provider_id.as_str()))
            .cloned()
    }

    /// Record one delete attempt: gone is gone, and anything else is kept with
    /// the provider's own words.
    pub fn settle_intent(
        &mut self,
        provider_id: &str,
        outcome: Result<(), String>,
    ) -> Option<String> {
        let at = self
            .intents
            .iter()
            .position(|i| i.provider_id == provider_id)?;
        match outcome {
            Ok(()) => {
                let i = self.intents.remove(at);
                let line = format!(
                    "[{0}] deleted — {1} {2} is gone",
                    i.addr, i.provider, i.provider_id
                );
                self.push_event("info", line.clone());
                self.save();
                Some(line)
            }
            Err(e) => {
                let i = &mut self.intents[at];
                i.attempts += 1;
                i.last_error = Some(e.clone());
                let line = if i.attempts >= MAX_DELETE_ATTEMPTS {
                    format!(
                        "[{}] delete failed ({}): {e} — not retrying; `:cloud reap` tries again",
                        i.addr, i.attempts
                    )
                } else {
                    format!(
                        "[{}] delete failed ({}): {e} — will retry",
                        i.addr, i.attempts
                    )
                };
                self.push_event("error", line.clone());
                self.save();
                Some(line)
            }
        }
    }

    /// How many deletes are still owed to the cloud.
    pub fn intents_pending(&self) -> usize {
        self.intents.len()
    }

    /// Give every stuck delete a fresh set of attempts, as `:cloud reap` does.
    pub fn retry_intents(&mut self) -> String {
        let n = self.intents.len();
        for i in &mut self.intents {
            i.attempts = 0;
        }
        self.save();
        if n == 0 {
            "no cloud deletes owed".to_string()
        } else {
            format!("{n} delete(s) will be retried on the next sweep")
        }
    }
}
