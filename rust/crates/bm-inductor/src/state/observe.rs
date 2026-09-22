//! What the inductor records when a worker speaks to it.
//!
//! **One implementation for both directions.** The pull protocol reaches this
//! through `POST /api/register` and `POST /api/heartbeat`; the inverted one
//! through the dispatcher's `GET /status` poll. Two copies would let the
//! machine state, the worker map and the capability list disagree depending on
//! which way the report travelled — and the panes would show whichever arrived
//! last, which is exactly the class of bug the inversion is meant to remove.

use super::Inner;
use bm_proto::{now_secs, Heartbeat, Machine, MachineState};

impl Inner {
    /// Record that a worker is alive, where it is, and what it is doing.
    ///
    /// Returns whether anything worth *persisting* changed — a machine turning
    /// Online, a name being learned, a stale verdict being refuted. The caller
    /// decides whether to write: `register` always does, because it happens
    /// once, but the dispatcher polls every couple of seconds per worker and
    /// `save()` serialises every task in the book, so writing on each poll
    /// would turn a status check into a 200 kB file write.
    pub fn observe(&mut self, h: &Heartbeat) -> bool {
        // A worker the ledger has never seen is registering, whatever route it
        // came in by. This is the one moment the *connection* config is worth
        // writing; every later beat is runtime only.
        let first_sighting = !self.workers.contains_key(&h.worker_id);
        let mut persist = false;

        // Known machine by address, else the local one for loopback agents,
        // else a box learned from its own beat.
        let addr = if self.machines.contains_key(&h.addr) {
            h.addr.clone()
        } else if h.addr == "127.0.0.1" || h.addr == "localhost" {
            "127.0.0.1".into()
        } else {
            let m = Machine::new(&h.addr, "unknown", 22, None, "worker");
            self.machines.insert(h.addr.clone(), m);
            persist = true;
            h.addr.clone()
        };

        // Carry the registry handle (not the OS hostname) to the panes: the
        // provision log says `hawk`, so the machines pane must say it too.
        // Kept when set — a beat never renames a box.
        if self
            .machines
            .get(&addr)
            .map(|m| m.name.is_empty())
            .unwrap_or(false)
        {
            let name = self.box_name(&addr, &h.hostname);
            if let Some(m) = self.machines.get_mut(&addr) {
                m.name = name;
            }
            persist = true;
        }

        self.workers.insert(h.worker_id.clone(), addr.clone());

        // An empty list means "not reported", not "can do nothing". An agent
        // that predates the field sends none, and must not wipe capabilities
        // an earlier registration established — the render gate would then
        // refuse a box that can render perfectly well.
        if !h.capabilities.is_empty() {
            self.caps
                .insert(h.worker_id.clone(), h.capabilities.clone());
            // Staged-rollout visibility: a box without `render-segments` keeps
            // taking every other stage but never sees a render offer. Say so on
            // the machine, or the idle box looks broken.
            if !h.capabilities.iter().any(|c| c == "render-segments") {
                if let Some(m) = self.machines.get_mut(&addr) {
                    m.note = bm_core::provision::preserve_ec2_id(
                        &m.note,
                        "agent predates render-segments: crawl/digest/merge only",
                    );
                    persist = true;
                }
            }
        }

        if let Some(m) = self.machines.get_mut(&addr) {
            m.last_seen = now_secs();
            if m.state != MachineState::Online {
                // Through `set_state` so the stamp moves with it: a box that
                // was `Initializing` a second ago is now online *since* now,
                // which is what the pane and the boot deadline both read.
                m.set_state(MachineState::Online);
                persist = true;
            }
            // A beating worker refutes the provision-time verdict: without this
            // the pane keeps saying "would not start" under a live worker row.
            // One-shot — the match is on the stale wording.
            if m.note.contains("would not start")
                || m.note.contains("worker start failed")
                || m.note.contains("worker start crashed")
            {
                m.note = bm_core::provision::preserve_ec2_id(
                    &m.note,
                    "worker is beating — earlier start verdict was stale",
                );
                persist = true;
            }
        }

        // **The duplicate-sidecar alarm.** Two `bm-tts` on one box is the OOM
        // this cluster kept taking, and the count is the one fact it could not
        // see: the model is ~2.85 GB resident the moment it loads, so a second
        // one on an 8 GiB box is the kernel choosing a victim. Edge-triggered
        // against the previous beat — the dispatcher polls every couple of
        // seconds per worker, and an event per poll is a log nobody reads — and
        // `None` from an older agent is not a zero, so it is never read as
        // "was fine" and never reads as "is fine".
        if let Some(n) = h.sidecars {
            let was = self.beats.get(&h.worker_id).and_then(|b| b.sidecars);
            if n > 1 && was.unwrap_or(0) <= 1 {
                self.push_event(
                    "error",
                    format!(
                        "[{}] {n} bm-tts sidecars resident on {} ({:.1} GB) — one box, one model; a duplicate is the OOM race, press X to sweep it",
                        h.worker_id,
                        h.addr,
                        h.sidecar_gb.unwrap_or(0.0)
                    ),
                );
            }
        }

        self.beats.insert(h.worker_id.clone(), h.clone());

        // The "unknown"-user placeholder carries no configured values, so it
        // stays memory-only — the same rule `register` has always had.
        if first_sighting
            && self.machines.get(&addr).map(|m| m.ssh_user.as_str()) != Some("unknown")
        {
            self.persist_box(&addr, &h.hostname);
        }

        persist || first_sighting
    }

    /// What a failed `/status` poll means for one machine.
    ///
    /// The counterpart to [`Inner::observe`]: that records a worker speaking,
    /// this records nobody answering. Named and separate because the decision
    /// has a case that is easy to get wrong — a box that is *coming up* cannot
    /// answer, and silence about it is not news.
    ///
    /// Stamping `Offline` on a box that is booting, being pushed to, or
    /// provisioned with no worker started yet replaces the one state that says
    /// "this is expected, give it time" with "it is gone". That is the
    /// fresh-pool-looks-broken bug, and it is why this is not a bare
    /// assignment.
    pub fn note_silence(&mut self, addr: &str) {
        if let Some(m) = self.machines.get_mut(addr) {
            if !m.state.coming_up() {
                m.set_state(MachineState::Offline);
            }
        }
    }
}
