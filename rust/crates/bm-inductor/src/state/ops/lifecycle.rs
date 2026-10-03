use super::*;

impl Inner {
    /// Is a launched box still waiting on the account?
    pub fn has_pending_launch(&self) -> bool {
        self.machines.values().any(|m| {
            matches!(
                m.state,
                bm_proto::MachineState::AwaitingIp | bm_proto::MachineState::Initializing
            ) && bm_core::provision::ec2_id_from_note(&m.note).is_some()
        })
    }

    /// Retire boxes that never came up.
    pub fn expire_initializing(&mut self) -> Vec<String> {
        let now = now_secs();
        let mut out = Vec::new();
        for m in self.machines.values_mut() {
            let addressless = m.state == bm_proto::MachineState::AwaitingIp;
            if m.state != bm_proto::MachineState::Initializing && !addressless {
                continue;
            }
            // `0` is "never stamped" — a record written before this field
            if m.state_since == 0 {
                m.state_since = now;
                continue;
            }
            if now.saturating_sub(m.state_since) < BOOT_DEADLINE_SECS {
                continue;
            }
            let note = if addressless {
                format!(
                    "no public address within {} min of launch — terminated, or a subnet with no route out",
                    BOOT_DEADLINE_SECS / 60
                )
            } else {
                format!(
                    "never answered ssh within {} min of launch — terminated, or unreachable from here",
                    BOOT_DEADLINE_SECS / 60
                )
            };
            m.set_state(bm_proto::MachineState::Error);
            m.note = bm_core::provision::preserve_ec2_id(&m.note, &note);
            out.push(format!("[{}] {note}", m.addr));
        }
        for line in &out {
            self.push_event("warn", line.clone());
        }
        if !out.is_empty() {
            self.save();
        }
        out
    }

    /// Ask every beating worker to exit on its next heartbeat (2s). The
    pub fn op_shutdown_workers(&mut self) -> String {
        self.shutdown_requested = true;
        let n = self
            .beats
            .values()
            .filter(|b| now_secs().saturating_sub(b.ts) < 90)
            .count();
        let msg = format!("shutdown asked of {n} live worker(s) — exiting on next beat");
        self.push_event("warn", msg.clone());
        msg
    }

    /// Arm drain-then-exit: workers stop on their own once the queue drains.
    pub fn op_shutdown_when_idle(&mut self) -> String {
        self.shutdown_when_idle = true;
        self.maybe_auto_shutdown();
        if self.shutdown_requested {
            return "queue already drained — workers exiting on next beat".into();
        }
        let msg = "shutdown armed — workers exit once the queue drains".to_string();
        self.push_event("info", msg.clone());
        msg
    }

    /// Fire the drain-then-exit latch when nothing is unfinished.
    pub fn busy(&self) -> bool {
        self.tasks.values().any(|t| {
            matches!(
                t.state,
                TaskState::Pending | TaskState::Assigned | TaskState::Running
            )
        })
    }

    /// Work the cluster could actually start right now.
    pub fn runnable(&self) -> bool {
        self.tasks.values().any(|t| {
            t.state == TaskState::Pending
                && !self.shelved(t.chapter)
                && self.upstream_done(t.chapter, t.stage)
        })
    }

    /// Nothing running and nothing startable — the cluster has nothing to do.
    pub fn idle(&self) -> bool {
        let in_flight = self
            .tasks
            .values()
            .any(|t| matches!(t.state, TaskState::Assigned | TaskState::Running));
        !in_flight && !self.runnable()
    }

    /// Start or stop distribution, and report it. Returns the line to show.
    pub(crate) fn set_dispatch(&mut self, go: bool) -> String {
        self.dispatch_held = !go;
        let line = if go {
            format!("go: distributing {}", self.remaining_line())
        } else {
            format!(
                "hold: no task will be offered — the range stands at {}",
                self.remaining_line()
            )
        };
        self.push_event(if go { "info" } else { "warn" }, line.clone());
        line
    }

    pub(crate) fn maybe_auto_shutdown(&mut self) {
        if !self.shutdown_when_idle {
            return;
        }
        if self.busy() {
            return;
        }
        self.shutdown_when_idle = false;
        self.shutdown_requested = true;
        self.push_event(
            "warn",
            "queue drained — workers exiting on next beat".into(),
        );
    }

    /// Manual trigger for the same orphan logic `reap` runs automatically:
    pub fn op_requeue_orphans(&mut self) -> String {
        let now = now_secs();
        let live: std::collections::HashSet<&str> = self
            .beats
            .values()
            .filter(|b| now.saturating_sub(b.ts) < 90)
            .map(|b| b.worker_id.as_str())
            .collect();
        let mut back = Vec::new();
        for t in self.tasks.values_mut() {
            if !matches!(t.state, TaskState::Assigned | TaskState::Running) {
                continue;
            }
            // Racing rows prune dead holders but stay with the live ones.
            if !t.racers.is_empty() {
                let dead: Vec<String> = t
                    .holders()
                    .into_iter()
                    .filter(|w| !live.contains(w))
                    .map(str::to_string)
                    .collect();
                for w in &dead {
                    t.remove_holder(w);
                }
                if t.assigned_to.is_some() {
                    continue;
                }
            }
            let orphan = match &t.assigned_to {
                None => true,
                Some(w) => !live.contains(w.as_str()),
            };
            if orphan {
                Self::release(t, now, "worker gone");
                back.push(t.id());
            }
        }
        back.sort();
        if back.is_empty() {
            return "no orphaned tasks — every assignment has a live worker".into();
        }
        self.save();
        format!(
            "requeued {} orphaned task(s): {}",
            back.len(),
            back.iter().take(8).cloned().collect::<Vec<_>>().join(", ")
        )
    }
}
