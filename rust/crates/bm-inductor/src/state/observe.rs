//! What the inductor records when a worker speaks to it.

use super::Inner;
use bm_proto::{now_secs, Heartbeat, Machine, MachineState};

impl Inner {
    /// Record that a worker is alive, where it is, and what it is doing.
    pub fn observe(&mut self, h: &Heartbeat) -> bool {
        // A worker the ledger has never seen is registering, whatever route it
        let first_sighting = !self.workers.contains_key(&h.worker_id);
        let mut persist = false;

        // Known machine by address, else the local one for loopback agents,
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
        if !h.capabilities.is_empty() {
            self.caps
                .insert(h.worker_id.clone(), h.capabilities.clone());
            // Staged-rollout visibility: a box without `render-segments` keeps
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
                m.set_state(MachineState::Online);
                persist = true;
            }
            // A beating worker refutes the provision-time verdict: without this
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

        // **A stage the policy wants and the bundle cannot serve.** The gate
        if !h.sources_stages.is_empty() {
            let was = self
                .beats
                .get(&h.worker_id)
                .map(|b| b.sources_stages.clone())
                .unwrap_or_default();
            if was != h.sources_stages {
                let adapter = self.layout.adapter.clone();
                let missing: Vec<String> = self
                    .machines
                    .get(&addr)
                    .map(|m| {
                        m.effective_task_policy()
                            .into_iter()
                            .filter(|p| p.enabled)
                            .filter(|p| {
                                !bm_core::provision::sources::holds(
                                    &h.sources_stages,
                                    p.stage,
                                    &adapter,
                                )
                            })
                            .map(|p| p.stage.as_str().to_string())
                            .collect()
                    })
                    .unwrap_or_default();
                if !missing.is_empty() {
                    self.push_event(
                        "warn",
                        format!(
                            "[{}] policy covers {} but the sources bundle on this box does not \
                             (it reports [{}]) — that work is withheld until the box is \
                             provisioned (p)",
                            h.worker_id,
                            missing.join("+"),
                            h.sources_stages.join("+"),
                        ),
                    );
                }
            }
        }

        self.beats.insert(h.worker_id.clone(), h.clone());

        // The "unknown"-user placeholder carries no configured values, so it
        if first_sighting
            && self.machines.get(&addr).map(|m| m.ssh_user.as_str()) != Some("unknown")
        {
            self.persist_box(&addr, &h.hostname);
        }

        persist || first_sighting
    }

    /// What a failed `/status` poll means for one machine.
    pub fn note_silence(&mut self, addr: &str) {
        if let Some(m) = self.machines.get_mut(addr) {
            if !m.state.coming_up() {
                m.set_state(MachineState::Offline);
            }
        }
    }
}
