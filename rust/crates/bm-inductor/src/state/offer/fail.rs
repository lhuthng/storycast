use super::*;

impl Inner {
    /// A merge failed on `N segments missing`, or an offer-time check proved
    /// its segments are not on this disk: the ledger's `render:Done` is a
    /// claim about files that is no longer true (a wiped box, a surgically
    /// deleted stale file, units never collected). Flip that `Done` render
    /// back to `Pending` so the chapter re-renders instead of failing the
    /// same merge into shelved.
    ///
    /// Only `Done` moves: anything else is already queued, in flight, or
    /// parked for an operator. A published mp3 vetoes the flip: then the
    /// segments are provenance (TTS does not reproduce), not cache. Nothing
    /// is deleted: the next render fills gaps rather than starting over. A
    /// merge pulls the pieces it lacks from the inductor, so a worker-side
    /// `segments missing` means the inductor itself is short, and this heal
    /// is what refills it.
    pub(crate) fn heal_render_for_merge(&mut self, chapter: u32) -> bool {
        if self.layout.final_mp3(chapter).is_file() {
            return false;
        }
        // The plan's diff *is* the heal: a take whose file this store lacks is
        // work again, and one it holds stays `Done`. Nothing is deleted, the
        // next render fills the gap rather than starting the chapter over.
        if self.materialize_render_takes(chapter).is_none() {
            // Unplannable here (no script, an uncast speaker): the
            // chapter-granular row is all the ledger has to offer, so fall back
            // to it rather than inventing takes.
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
    /// stage's threshold, 3 everywhere but digest, which gets 15), and an
    /// event line. Shared by worker-reported failures and the completion
    /// gate, which fails reports whose files never landed.
    ///
    /// Digest racing changes one thing: a failing racer while other boxes
    /// are still grinding the same row costs no strike. Otherwise N racers
    /// failing one bad prompt would shelve the chapter in a single wave.
    ///
    /// **The whole batch takes the strike, not the row the report named.** One
    /// report is one answer about one offer, and the offer covered N takes: a
    /// worker that died mid-batch did not fail only the first of them.
    /// Per-take strikes would also let a batch of sixty shelter sixty rows
    /// from the strikes rule while never once finishing. The cost is honest
    /// and small: a retry is cheap, because `pending_units` skips the takes
    /// whose files did land, so a batch that got nine of ten re-speaks one.
    /// Park a row immediately, without spending the three strikes a retry
    /// ladder exists to spend on *uncertain* failures.
    ///
    /// Same end state as [`Inner::fail_task`] reaching its cap, `Shelved`,
    /// holders cleared, the reason on the row, reached in one step because the
    /// crawler already classified the refusal as terminal.
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
        // moment, same question shape — work landed, ask what is left.
        self.run_exclusive();
        format!(
            "{n} row(s) shelved: {}",
            bm_core::util::head_chars(detail, 120)
        )
    }

    pub(crate) fn fail_task(&mut self, task_id: &str, worker_id: &str, detail: String) -> String {
        // A digest racer failing while others still hold the row: drop just
        // this holder, strike nothing. The race continues; the row is
        // struck only when its last holder fails below.
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
        // when the render never produced the audio or the inductor lost it
        // the merge itself pulls what it lacks, so its box is never the
        // problem. The render is requeued alongside the merge retry below.
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
                // say something, so it was not stuck, and the silent-expiry
                // streak it may have had before is not this assignment's story.
                t.expiries = 0;
                shelved |= t.state == TaskState::Shelved;
            }
        }
        // The other half of a `segments missing` merge failure: the render
        // really is the starved input. Requeue it alongside the merge retry,
        // or the same merge fails twice more into shelved and waits for a
        // manual force. The merge keeps its strike, a render that cannot
        // close the gap still shelves.
        if let Some(rest) = task_id.strip_prefix("merge:") {
            if detail.contains("segments missing") {
                if let Ok(chapter) = rest.parse::<u32>() {
                    self.heal_render_for_merge(chapter);
                }
            }
            // The plan predates the script: a digest landed after the render
            // plan was built, so the recorded take list no longer matches the
            // timeline. Rebuild the plan from the current script, the diff
            // requeues exactly the changed takes, or the same merge fails
            // twice more into shelved and waits for a manual force. The merge
            // keeps its strike, like the starved-input heal above.
            if detail.contains("turns for ") && detail.contains(" rendered segments") {
                if let Ok(chapter) = rest.parse::<u32>() {
                    self.replan_render_takes(chapter);
                }
            }
        }
        let level = if shelved { "error" } else { "warn" };
        // Capitalised and counted, because a shelving is the one outcome that
        // needs a human: the chapter stops being retried and will sit there
        // until somebody presses `u`. It has to be distinguishable at a glance
        // from the ordinary "will retry" failure, which needs nobody.
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
        // moment, same question shape — work landed, ask what is left.
        self.run_exclusive();
        // The log line carries the same distinction as the event. It used to say
        // only "failed", so a shelving, the outcome that needs an operator
        // was indistinguishable from a failure that retries itself, in the one
        // place a person actually greps.
        let outcome = if shelved { "SHELVED" } else { "failed" };
        format!(
            "{worker_id}: {task_id} {outcome} ({})",
            bm_core::util::head_chars(&detail, 120)
        )
    }
}
