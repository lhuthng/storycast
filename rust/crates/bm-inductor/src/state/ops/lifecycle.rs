use super::*;

impl Inner {
    /// Is a launched box still waiting on the account?
    ///
    /// True while some machine sits in `AwaitingIp` or `Initializing` *and*
    /// carries an EC2 instance id — that is, while a launch is in flight and an
    /// account read could move something forward. It gates the periodic relink,
    /// which is the whole point of asking: the account is read while boxes are
    /// coming up and never otherwise, so a settled cluster costs no API calls at
    /// all and the watch stops itself when the last box answers.
    ///
    /// The EC2 requirement matters: a hand-added machine is `Unknown`, never
    /// `Initializing`, but a stuck `Initializing` row from a ledger written by
    /// hand would otherwise keep the account being read for ever.
    pub fn has_pending_launch(&self) -> bool {
        self.machines.values().any(|m| {
            matches!(
                m.state,
                bm_proto::MachineState::AwaitingIp | bm_proto::MachineState::Initializing
            ) && bm_core::provision::ec2_id_from_note(&m.note).is_some()
        })
    }

    /// Retire boxes that never came up.
    ///
    /// `Initializing` and `AwaitingIp` are the two machine states with a
    /// deadline, because they are the only ones where the inductor is *waiting*
    /// rather than acting. A state with no exit condition is a lie: a box
    /// terminated before it booted, or launched into a subnet this machine
    /// cannot dial, would sit in "initializing" for ever with the pane implying
    /// it is about to work.
    ///
    /// `AwaitingIp` needs it for a sharper reason: the account assigns a
    /// public address within seconds of a launch, so one that has not arrived in
    /// five minutes is never arriving — a terminated instance, or a subnet with
    /// no route to an internet gateway. Without a deadline that box is a
    /// permanent row in the pane that no account read will ever repair.
    ///
    /// Returns one line per box retired, for the caller to log.
    pub fn expire_initializing(&mut self) -> Vec<String> {
        let now = now_secs();
        let mut out = Vec::new();
        for m in self.machines.values_mut() {
            let addressless = m.state == bm_proto::MachineState::AwaitingIp;
            if m.state != bm_proto::MachineState::Initializing && !addressless {
                continue;
            }
            // `0` is "never stamped" — a record written before this field
            // existed. Adopt it now and give it the whole deadline rather than
            // expiring a box on the strength of a missing timestamp.
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
    /// graceful half of the cluster stop: workers die on their own, no ssh.
    /// In-flight tasks are stranded, not failed — the stop flow requeues
    /// them (attempts kept), and lease expiry reaps them if it doesn't.
    /// Whatever stays behind (a dead box, the detached TTS sidecar) is
    /// still swept over ssh afterwards.
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
    /// Fires at once when nothing is unfinished (all done, or an idle
    /// backend) — otherwise the arm would sit forever with no completion
    /// left to trip it.
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
    /// One-shot: the arm disarms as it fires, so work enqueued afterwards
    /// waits for the next backend start instead of murdering fresh workers.
    /// Shelved tasks don't block — they're parked for an operator, not work.
    /// Is there work outstanding? Shelved tasks don't count — they are parked
    /// for an operator, not work in flight.
    pub fn busy(&self) -> bool {
        self.tasks.values().any(|t| {
            matches!(
                t.state,
                TaskState::Pending | TaskState::Assigned | TaskState::Running
            )
        })
    }

    /// Work the cluster could actually start right now.
    ///
    /// Deliberately **not** `busy()`. When a chapter's crawl shelves, its
    /// digest/render/merge stay `Pending` for good — queued behind a stage
    /// that will not run again without an operator. Counting those as work
    /// means the idle timer never fires in precisely the case it exists for:
    /// a cluster holding a queue it cannot move.
    pub fn runnable(&self) -> bool {
        self.tasks.values().any(|t| {
            t.state == TaskState::Pending
                && !self.shelved(t.chapter)
                && self.upstream_done(t.chapter, t.stage)
        })
    }

    /// Nothing running and nothing startable — the cluster has nothing to do.
    ///
    /// This is the idle timer's predicate, and the distinction from `busy()`
    /// is the whole reason it is a separate method: a stalled queue is idle
    /// even though the ledger is full.
    pub fn idle(&self) -> bool {
        let in_flight = self
            .tasks
            .values()
            .any(|t| matches!(t.state, TaskState::Assigned | TaskState::Running));
        !in_flight && !self.runnable()
    }

    /// Start or stop distribution, and report it. Returns the line to show.
    ///
    /// The whole of the gate's state is one bool the `offer` path reads first,
    /// so this is the entire control surface: no rows are touched, no lease is
    /// released, nothing is deleted. Holding a live cluster leaves whatever is
    /// in flight to finish — the boxes are told nothing — and only stops the
    /// *next* offer, which is the shape an operator means by "hold": quiet, not
    /// killed.
    ///
    /// Going does not enqueue anything by itself; the caller does that with the
    /// remainder this reports (see `remaining_line`), because the enqueue needs
    /// the chapter index and a network round trip, and a control that sometimes
    /// goes to the network is a control that sometimes stalls the dashboard.
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
    /// requeue assignments with no live beat. Live workers' tasks are
    /// untouched. Attempts are kept.
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
