use super::*;

impl Inner {
    /// A merge failed on `N segments missing`, or an offer-time check proved
    pub(crate) fn heal_render_for_merge(&mut self, chapter: u32) -> bool {
        if self.layout.final_mp3(chapter).is_file() {
            return false;
        }
        // The plan's diff *is* the heal: a take whose file this store lacks is
        if self.materialize_render_takes(chapter).is_none() {
            // Unplannable here (no script, an uncast speaker): the
            let key = format!("{}:{chapter}", Stage::Render);
            return match self.tasks.get_mut(&key) {
                Some(t) if t.state == TaskState::Done => {
                    t.state = TaskState::Pending;
                    t.attempts = 0;
                    t.assigned_to = None;
                    t.lease_until = None;
                    t.detail = "requeued: merge found segments missing".into();
                    t.updated = now_secs();
                    true
                }
                _ => false,
            };
        }
        let mut healed = false;
        for t in self.tasks.values_mut() {
            if t.stage == Stage::Render && t.chapter == chapter && t.state == TaskState::Pending {
                t.detail = "requeued: merge found segments missing".into();
                healed = true;
            }
        }
        healed
    }

    /// Record a failed report: a strike, Pending again (Shelved at the
    pub(crate) fn shelve_now(&mut self, task_id: &str, detail: &str) -> String {
        let rows = self.covered_rows(task_id);
        let mut n = 0;
        for id in &rows {
            if let Some(t) = self.tasks.get_mut(id) {
                t.state = TaskState::Shelved;
                t.detail = detail.to_string();
                t.assigned_to = None;
                t.racers.clear();
                t.lease_until = None;
                t.batch.clear();
                t.updated = now_secs();
                n += 1;
            }
        }
        self.push_event(
            "error",
            format!(
                "{task_id} SHELVED without retry: {} (press u to requeue)",
                bm_core::util::head_chars(detail, 200)
            ),
        );
        self.save();
        self.maybe_auto_shutdown();
        // And: did a queued exclusive write just become runnable? Same
        self.run_exclusive();
        format!(
            "{n} row(s) shelved: {}",
            bm_core::util::head_chars(detail, 120)
        )
    }

    pub(crate) fn fail_task(&mut self, task_id: &str, worker_id: &str, detail: String) -> String {
        // A digest racer failing while others still hold the row: drop just
        if task_id.starts_with("digest:") {
            let mut racing_on = false;
            if let Some(t) = self.tasks.get_mut(task_id) {
                if t.is_holder(worker_id)
                    && matches!(t.state, TaskState::Assigned | TaskState::Running)
                {
                    let holders_before = t.holders().len();
                    if holders_before > 1 {
                        t.remove_holder(worker_id);
                        t.detail = "requeued: racer failed, still racing".to_string();
                        t.updated = now_secs();
                        racing_on = true;
                    }
                }
            }
            if racing_on {
                let remaining = self
                    .tasks
                    .get(task_id)
                    .map(|t| t.holders().join(", "))
                    .unwrap_or_default();
                self.push_event(
                    "warn",
                    format!(
                        "[{worker_id}] {task_id} FAILED (racer out, still racing: {remaining}): {}",
                        bm_core::util::head_chars(&detail, 200)
                    ),
                );
                self.save();
                return format!(
                    "{worker_id}: {task_id} failed (racer out, still racing) ({})",
                    bm_core::util::head_chars(&detail, 120)
                );
            }
        }
        // **Which input is short?** A merge fails on `N segments missing`
        let rows = self.covered_rows(task_id);
        let shelve_limit = self
            .tasks
            .get(task_id)
            .map(|t| shelve_after(t.stage))
            .unwrap_or(3);
        let mut shelved = false;
        for id in &rows {
            if let Some(t) = self.tasks.get_mut(id) {
                t.attempts += 1;
                t.detail = detail.clone();
                let after = shelve_after(t.stage);
                t.state = if t.attempts >= after {
                    TaskState::Shelved
                } else {
                    TaskState::Pending
                };
                t.assigned_to = None;
                t.racers.clear();
                t.lease_until = None;
                t.updated = now_secs();
                t.batch.clear();
                // A reported failure is an answer: the worker got far enough to
                t.expiries = 0;
                shelved |= t.state == TaskState::Shelved;
            }
        }
        // The other half of a `segments missing` merge failure: the render
        if let Some(rest) = task_id.strip_prefix("merge:") {
            if detail.contains("segments missing") {
                if let Ok(chapter) = rest.parse::<u32>() {
                    self.heal_render_for_merge(chapter);
                }
            }
            // The plan predates the script: a digest landed after the render
            if detail.contains("turns for ") && detail.contains(" rendered segments") {
                if let Ok(chapter) = rest.parse::<u32>() {
                    self.replan_render_takes(chapter);
                }
            }
        }
        let level = if shelved { "error" } else { "warn" };
        // Capitalised and counted, because a shelving is the one outcome that
        let note = if shelved {
            format!(" (SHELVED — {shelve_limit} strikes, no further retries; press u to requeue)")
        } else {
            " (will retry)".to_string()
        };
        self.push_event(
            level,
            format!(
                "[{worker_id}] {task_id} FAILED{note}: {}",
                bm_core::util::head_chars(&detail, 200)
            ),
        );
        self.save();
        // A shelving may have drained the queue, trip the armed latch.
        self.maybe_auto_shutdown();
        // And: did a queued exclusive write just become runnable? Same
        self.run_exclusive();
        // The log line carries the same distinction as the event. It used to say
        let outcome = if shelved { "SHELVED" } else { "failed" };
        format!(
            "{worker_id}: {task_id} {outcome} ({})",
            bm_core::util::head_chars(&detail, 120)
        )
    }
}
