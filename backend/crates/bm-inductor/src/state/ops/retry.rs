use super::*;

impl Inner {
    /// The worker ids still answering: a beat inside the 90s window.
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
    pub fn op_release_task(&mut self, stage: Stage, chapter: u32, force: bool) -> String {
        // No `now` here: this scope decides *what* to release, and
        let live = Self::live_workers(&self.beats);
        // Both facts are read before anything is mutated: the refusal has to be
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
    pub fn op_retry_shelved(&mut self) -> String {
        self.requeue_shelved(None)
    }

    /// The same, narrowed to one chapter — every stage of it that is shelved.
    pub fn op_retry_chapter(&mut self, chapter: u32) -> String {
        self.requeue_shelved(Some(chapter))
    }

    /// The one implementation behind both scopes: `chapter` of `None` is the
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
    pub fn op_retry_task(&mut self, stage: Stage, chapter: u32, force: bool) -> String {
        // A forced upstream run invalidates every downstream row for this
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
        let published = self.layout.final_mp3(chapter).exists();
        let mut cascaded = false;

        // When forcing, remove the output artifact so the stage re-runs fully
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
