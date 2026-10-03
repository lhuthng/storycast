use super::*;

impl Inner {
    /// The worker ids still answering: a beat inside the 90s window.
    ///
    /// Takes the beat map rather than `&self` **on purpose**: the caller holds
    /// this answer across a mutation of `tasks`, and a method borrowing all of
    /// `self` would forbid that. Same window the reaper uses, so the ledger's
    /// idea of a live holder and the screen's cannot drift apart.
    fn live_workers(
        beats: &std::collections::HashMap<String, bm_proto::Heartbeat>,
    ) -> std::collections::HashSet<&str> {
        let now = now_secs();
        beats
            .values()
            .filter(|b| now.saturating_sub(b.ts) < 90)
            .map(|b| b.worker_id.as_str())
            .collect()
    }


    /// Take one row back off whoever holds it, by hand.
    ///
    /// `stage` + `chapter` names a row, and for `render` that is every take of
    /// the chapter: a render ledger row is one take, an offer carries a batch
    /// of them, and a box that died holds the batch. Attempts are kept and
    /// nothing on disk is touched, so a released row is offered again exactly
    /// as it was — only its owner changes.
    ///
    /// Refused against a *live* holder unless `force`, because re-offering work
    /// that is still happening is how one chapter gets spoken twice. `force` is
    /// the whole reason this exists as an op rather than as `requeue`: a box can
    /// be beating and wedged, and `op_requeue_orphans` has no opinion about
    /// that, while the person looking at the row does.
    pub fn op_release_task(&mut self, stage: Stage, chapter: u32, force: bool) -> String {
        // No `now` here: this scope decides *what* to release, and
        // `finish_release` is the one place that stamps and saves it.
        let live = Self::live_workers(&self.beats);
        // Both facts are read before anything is mutated: the refusal has to be
        // decided on the whole set, not on however much of it a previous
        // iteration already released.
        let mut ids: Vec<String> = Vec::new();
        let mut beating: Option<String> = None;
        for t in self.tasks.values() {
            if t.stage != stage || t.chapter != chapter {
                continue;
            }
            if !matches!(t.state, TaskState::Assigned | TaskState::Running) {
                continue;
            }
            ids.push(t.id());
            if let Some(w) = t.holders().into_iter().find(|w| live.contains(w)) {
                beating.get_or_insert_with(|| w.to_string());
            }
        }
        ids.sort();
        if ids.is_empty() {
            return format!("{stage}:{chapter} holds nothing — no assignment to release");
        }
        let Some(beating) = beating else {
            return self.finish_release(&ids, &format!("{stage}:{chapter}"), None);
        };
        if !force {
            return format!(
                "{stage}:{chapter} is held by {beating}, which is still beating — X releases it anyway"
            );
        }
        self.finish_release(&ids, &format!("{stage}:{chapter}"), Some(&beating))
    }


    /// The wider question the same primitive answers: not "this row", but
    /// "what is this box sitting on".
    ///
    /// A racing digest row with other holders keeps them — the box leaves the
    /// row, the row does not leave the fleet. Everything else it holds goes back
    /// to the pool in one pass, which is the point: a dead box's work is a set,
    /// and releasing it row by row is how an operator misses one.
    pub fn op_release_worker(&mut self, worker: &str, force: bool) -> String {
        let now = now_secs();
        let live = Self::live_workers(&self.beats);
        let beating = live.contains(worker);
        if beating && !force {
            return format!("{worker} is still beating — X releases its work anyway");
        }
        let mut ids: Vec<String> = self
            .tasks
            .values()
            .filter(|t| matches!(t.state, TaskState::Assigned | TaskState::Running))
            .filter(|t| t.is_holder(worker))
            .map(|t| t.id())
            .collect();
        ids.sort();
        if ids.is_empty() {
            return format!("{worker} holds nothing — no assignment to release");
        }
        for id in &ids {
            if let Some(t) = self.tasks.get_mut(id) {
                // Still held by a racer: this box leaves, the row stays.
                if t.remove_holder(worker) {
                    t.updated = now;
                    continue;
                }
                Self::release(t, now, "released by hand");
            }
        }
        self.save();
        let msg = format!(
            "released {} row(s) from {worker}{}",
            ids.len(),
            if beating {
                ", which is still beating"
            } else {
                ""
            }
        );
        self.push_event("ok", msg.clone());
        msg
    }


    /// The shared tail of both scopes: release the rows, save, say so.
    fn finish_release(&mut self, ids: &[String], what: &str, beating: Option<&str>) -> String {
        let now = now_secs();
        for id in ids {
            if let Some(t) = self.tasks.get_mut(id) {
                Self::release(t, now, "released by hand");
            }
        }
        self.save();
        let msg = match beating {
            Some(w) => format!(
                "released {} row(s) of {what} from {w}, which is still beating",
                ids.len()
            ),
            None => format!(
                "released {} row(s) of {what} — their worker is gone",
                ids.len()
            ),
        };
        self.push_event("ok", msg.clone());
        msg
    }


    /// Manual retry for every shelved task after fixing the cause.
    ///
    /// Strikes reset — unlike `release`, which keeps them — so the next failure
    /// gets a full threshold of attempts again (3 everywhere, 15 for digest).
    /// The operator asserts the cause is fixed by pressing the key, so
    /// forgiveness is the point. It deletes nothing, so against a failure
    /// whose cause is unmet *input* it is a loop rather than a repair;
    /// reaching the producer is what `op_retry_task`'s `force` is for.
    pub fn op_retry_shelved(&mut self) -> String {
        self.requeue_shelved(None)
    }


    /// The same, narrowed to one chapter — every stage of it that is shelved.
    /// This is what `:retry 24` means, and it is the useful scope after a
    /// failure that named a chapter without naming a stage.
    pub fn op_retry_chapter(&mut self, chapter: u32) -> String {
        self.requeue_shelved(Some(chapter))
    }


    /// The one implementation behind both scopes: `chapter` of `None` is the
    /// whole ledger.
    fn requeue_shelved(&mut self, chapter: Option<u32>) -> String {
        let now = now_secs();
        let mut back = Vec::new();
        for t in self.tasks.values_mut() {
            if t.state != TaskState::Shelved || chapter.is_some_and(|n| t.chapter != n) {
                continue;
            }
            t.state = TaskState::Pending;
            t.attempts = 0;
            t.clear_holders();
            t.lease_until = None;
            t.detail = "requeued: manual retry".into();
            t.updated = now;
            back.push(t.id());
        }
        back.sort();
        let scope = chapter.map(|n| format!(" on ch{n}")).unwrap_or_default();
        if back.is_empty() {
            return format!("no shelved tasks{scope} — nothing to retry");
        }
        self.save();
        let msg = format!(
            "retried {} shelved task(s){scope}: {}",
            back.len(),
            back.iter().take(8).cloned().collect::<Vec<_>>().join(", ")
        );
        self.push_event("ok", msg.clone());
        msg
    }


    /// Remove every ledger row for one downstream stage and chapter.
    ///
    /// A forced upstream rerun invalidates the work, not just the current row.
    /// Keeping a completed render or merge row would let reconcile promote the
    /// old artifact straight back to `Done` before the replacement can run.
    fn remove_stage_tasks(&mut self, stage: Stage, chapter: u32) -> usize {
        let ids: Vec<String> = self
            .tasks
            .iter()
            .filter(|(_, task)| task.stage == stage && task.chapter == chapter)
            .map(|(id, _)| id.clone())
            .collect();
        let count = ids.len();
        for id in ids {
            self.tasks.remove(&id);
        }
        count
    }


    /// Put a chapter's merge back in the queue, creating the row if the chapter
    /// has not reached merge yet. Its old output is the caller's responsibility
    /// to remove; this method only owns the ledger transition.
    fn invalidate_merge_task(&mut self, chapter: u32, detail: &str) {
        let key = format!("{}:{chapter}", Stage::Merge);
        let now = now_secs();
        match self.tasks.get_mut(&key) {
            Some(task) => {
                task.state = TaskState::Pending;
                task.attempts = 0;
                task.clear_holders();
                task.lease_until = None;
                task.detail = detail.to_string();
                task.updated = now;
            }
            None => {
                let mut task = Task::new(chapter, Stage::Merge);
                task.detail = detail.to_string();
                task.updated = now;
                self.tasks.insert(key, task);
            }
        }
    }


    /// Retry an individual task by stage and chapter.
    ///
    /// Resets attempts to 0 so the next failure gets a full 3 tries again.
    /// When `force` is true, deletes the on-disk artifact that would otherwise
    /// cause reconcile to mark it Done, so the task re-runs end-to-end.
    ///
    /// A forced merge also forces its render, unless the chapter already has a
    /// published mp3. A merge reads its segments from the box that runs it and
    /// produces none of its own, so re-offering a merge that failed on
    /// `N segments missing` fails again, on the same box, for the same reason:
    /// the retry has to reach the stage that can actually make them.
    pub fn op_retry_task(&mut self, stage: Stage, chapter: u32, force: bool) -> String {
        // A forced upstream run invalidates every downstream row for this
        // chapter before the current row is requeued. Otherwise a completed
        // merge can be promoted back to Done by the next reconcile, or stale
        // per-take render rows can survive a forced digest and speak the old
        // script when the new one lands.
        if force {
            match stage {
                Stage::Render => {
                    let _ = std::fs::remove_file(self.layout.final_mp3(chapter));
                    self.invalidate_merge_task(chapter, "requeued: render forced");
                }
                Stage::Digest => {
                    self.remove_stage_tasks(Stage::Render, chapter);
                    self.remove_stage_tasks(Stage::Merge, chapter);
                    let engine = self.settings.engine.clone();
                    let _ = std::fs::remove_dir_all(self.layout.seg_dir(&engine, chapter));
                    let _ = std::fs::remove_file(self.layout.plan(chapter));
                    let _ = std::fs::remove_file(self.layout.final_mp3(chapter));
                }
                _ => {}
            }
        }

        // A render is one row per take now, so "retry the render" means every
        // take of the chapter — and `force` deletes the cached takes first so
        // the plan's diff makes each of them work again.
        if stage == Stage::Render && !self.render_take_ids(chapter).is_empty() {
            let takes = self.render_take_ids(chapter).len();
            if force {
                let engine = self.settings.engine.clone();
                let _ = std::fs::remove_dir_all(self.layout.seg_dir(&engine, chapter));
            }
            self.reset_render_takes(chapter, "requeued: manual retry");
            if force {
                self.materialize_render_takes(chapter);
            }
            self.save();
            let msg = format!(
                "render:{chapter} requeued ({takes} take(s){})",
                if force { ", forced re-run" } else { "" }
            );
            self.push_event("ok", msg.clone());
            return msg;
        }
        let key = format!("{stage}:{chapter}");
        let now = now_secs();
        let task = match self.tasks.get_mut(&key) {
            Some(t) => t,
            None => return format!("task {key} not found"),
        };
        let prev_state = format!("{:?}", task.state).to_lowercase();
        task.state = TaskState::Pending;
        task.attempts = 0;
        task.clear_holders();
        task.lease_until = None;
        task.detail = format!("requeued: manual retry (was {prev_state})");
        task.updated = now;

        // Read before the match below: the merge arm deletes this file, so a
        // check afterwards could never see it.
        let published = self.layout.final_mp3(chapter).exists();
        let mut cascaded = false;

        // When forcing, remove the output artifact so the stage re-runs fully
        // rather than reconcile marking it Done immediately.
        if force {
            match stage {
                Stage::Crawl => {
                    let _ = std::fs::remove_file(self.layout.chapter_txt(chapter));
                }
                Stage::Digest => {
                    let _ = std::fs::remove_file(self.layout.script(chapter));
                }
                Stage::Render => {
                    let engine = self.settings.engine.clone();
                    let store = bm_core::segments::LocalStore::new(self.layout.clone());
                    let _ = std::fs::remove_dir_all(bm_core::segments::SegmentStore::dir(
                        &store, &engine, chapter,
                    ));
                }
                Stage::Merge => {
                    if let Some(p) = self.layout.final_mp3(chapter).to_str() {
                        let _ = std::fs::remove_file(p);
                    }
                }
            }
            // A merge's input is a per-box store and this stage makes none of
            // it, so `force` here has to reach the producer or it is the same
            // re-offer under a different label. This is the keypress an
            // operator was otherwise making by hand, on the render row, after
            // working out that the failure was never the merge's to fix.
            //
            // Only when nothing was published: a finished mp3 makes its
            // segments provenance rather than a cache (TTS is stochastic, so
            // they do not reproduce), and deleting them destroys the record of
            // how that file was spoken. A merge that failed has no mp3, so the
            // guarded case is the ordinary one rather than an exception.
            if stage == Stage::Merge && !published && !self.render_take_ids(chapter).is_empty() {
                self.op_retry_task(Stage::Render, chapter, true);
                cascaded = true;
            }
        }
        self.save();
        let msg = format!(
            "{}:{} requeued (was {prev_state}{})",
            stage,
            chapter,
            match (force, cascaded) {
                (_, true) => ", forced re-run + render",
                (true, false) => ", forced re-run",
                _ => "",
            }
        );
        self.push_event("ok", msg.clone());
        msg
    }
}
