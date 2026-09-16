use super::Inner;
use bm_proto::{Stage, Task, TaskState, now_secs};

impl Inner {
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

    /// Manual retry for shelved tasks (3 strikes) after fixing the cause.
    /// Strikes reset — unlike `release`, which keeps them — so the next
    /// failure gets a full 3 attempts again. The operator asserts the cause
    /// is fixed by pressing the key, so forgiveness is the point.
    pub fn op_retry_shelved(&mut self) -> String {
        let now = now_secs();
        let mut back = Vec::new();
        for t in self.tasks.values_mut() {
            if t.state != TaskState::Shelved {
                continue;
            }
            t.state = TaskState::Pending;
            t.attempts = 0;
            t.assigned_to = None;
            t.lease_until = None;
            t.detail = "requeued: manual retry".into();
            t.updated = now;
            back.push(t.id());
        }
        back.sort();
        if back.is_empty() {
            return "no shelved tasks — nothing to retry".into();
        }
        self.save();
        let msg = format!(
            "retried {} shelved task(s): {}",
            back.len(),
            back.iter().take(8).cloned().collect::<Vec<_>>().join(", ")
        );
        self.push_event("ok", msg.clone());
        msg
    }

    /// Retry an individual task by stage and chapter.
    ///
    /// Resets attempts to 0 so the next failure gets a full 3 tries again.
    /// When `force` is true, deletes the on-disk artifact that would otherwise
    /// cause reconcile to mark it Done, so the task re-runs end-to-end.
    pub fn op_retry_task(&mut self, stage: Stage, chapter: u32, force: bool) -> String {
        let key = format!("{stage}:{chapter}");
        let now = now_secs();
        let task = match self.tasks.get_mut(&key) {
            Some(t) => t,
            None => return format!("task {key} not found"),
        };
        let prev_state = format!("{:?}", task.state).to_lowercase();
        task.state = TaskState::Pending;
        task.attempts = 0;
        task.assigned_to = None;
        task.lease_until = None;
        task.detail = format!("requeued: manual retry (was {prev_state})");
        task.updated = now;

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
        }
        self.save();
        let msg = format!(
            "{}:{} requeued (was {prev_state}{})",
            stage, chapter,
            if force { ", forced re-run" } else { "" }
        );
        self.push_event("ok", msg.clone());
        msg
    }

    /// ETA for the remaining range, from measured throughput divided by
    /// live workers (heartbeat within the last 90s).
    pub fn op_eta(&self, start: u32, count: u32) -> String {
        let in_range = |t: &Task| t.chapter >= start && t.chapter < start + count;
        let pending = |stage: Stage| {
            self.tasks
                .values()
                .filter(|t| t.stage == stage && in_range(t) && !t.state.is_terminal())
                .count() as u64
        };
        let workers = self
            .beats
            .values()
            .filter(|b| now_secs().saturating_sub(b.ts) < 90)
            .count()
            .max(1) as u64;
        // Render is estimated in TTS calls, not chapters: scale by the median
        // calls-per-render seen so far (40 before anything measured).
        let mut units_per_render: Vec<u64> = bm_core::eta::read_stats(&self.layout.stats())
            .into_iter()
            .filter(|r| r.stage == "render")
            .map(|r| r.units.max(1))
            .collect();
        units_per_render.sort_unstable();
        let med_units = units_per_render
            .get(units_per_render.len() / 2)
            .copied()
            .unwrap_or(40);
        let remaining = [
            (Stage::Crawl, pending(Stage::Crawl)),
            (Stage::Digest, pending(Stage::Digest)),
            (Stage::Render, pending(Stage::Render) * med_units),
            (Stage::Merge, pending(Stage::Merge)),
        ];
        let etas = bm_core::eta::estimate_job(&self.layout.stats(), &remaining, workers);
        let total: u64 = etas.iter().map(|e| e.secs).sum();
        let mut parts: Vec<String> = etas
            .iter()
            .map(|e| {
                format!(
                    "{} {}{}",
                    e.stage,
                    bm_core::eta::human(e.secs),
                    if e.estimated_from_fallback { " (guess)" } else { "" }
                )
            })
            .collect();
        parts.push(format!("total {}", bm_core::eta::human(total)));
        format!(
            "ch{start}..{} over {workers} worker{}: {}",
            start + count - 1,
            if workers == 1 { "" } else { "s" },
            parts.join(" · ")
        )
    }

    /// Repoint one character's voice and invalidate only its cached segments.
    /// Other characters keep their cache; affected chapters re-render + merge.
    /// Refused while workers are mid-play: swapping then mixes voices and
    /// marks stale mp3s done. Only *fresh* evidence counts (30s) — stale
    /// beats and ghost assignments are the reaper's job, and an offline Inner
    /// (empty beats, e.g. swapping while the inductor is down) always passes.
    pub fn op_swap_voice(&mut self, character: &str, voice: &str) -> anyhow::Result<String> {
        self.ensure_idle()?;
        let engine = self.settings.engine.clone();
        // The operator's own roster, not the shipped default: this is the gate
        // that decides what may be assigned on this machine.
        let policy = bm_core::voices::effective_policy(&self.layout.roster(), &engine)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let cast_path = self.layout.cast(&engine);
        // Accept either form: the picker sends display names today, but a key is
        // the stable identifier and both have to work.
        let voice = bm_core::voices::resolve_voice_name(&engine, voice);
        let mut cast = bm_core::cast::read_cast(&engine, &cast_path);
        let old = cast.get(character).cloned().unwrap_or_default();
        // Trust rule (mirrors the agent gate):
        //   * an admitted preset, or
        //   * a voice the catalogue does not declare at all — an enrolled clone —
        //     that is already assigned somewhere.
        //
        // An empty `allowed` is "no restriction", not "nothing allowed": the same
        // reading `violations()` and `voice_from_label` use. Treating it as
        // "nothing is allowed" would make the picker refuse every voice on a
        // default install, which has no local roster.
        //
        // The clone escape hatch is keyed on "not declared" rather than "not in
        // the allow-list". Keyed on the allow-list it would also let a *declared*
        // preset that the operator excluded back in, simply because it was
        // already assigned — which is how an exclusion quietly stops applying.
        let declared = policy
            .male
            .iter()
            .chain(&policy.female)
            .chain(&policy.neutral)
            .any(|n| n == &voice);
        let in_use = cast.values().any(|v| v == &voice);
        // Pool samples are vetted at adding (`roster add-sample` / TUI `A`),
        // the same trust clones get at enrolment — so a fresh sample is
        // assignable before anything speaks with it. Without this a new sample
        // could be rolled automatically but never picked by hand.
        let pooled = bm_core::pool::load_pool(&self.layout.root.join("voice-pool.json"))
            .contains_key(&voice);
        let admitted = if policy.allowed.is_empty() {
            true
        } else if declared {
            policy.allowed.iter().any(|a| a == &voice)
        } else {
            in_use || pooled
        };
        if !admitted {
            anyhow::bail!(
                "voice {voice:?} is neither an admitted preset nor currently assigned"
            );
        }
        if old == voice {
            return Ok(format!("{character} already speaks as {voice} — nothing to do"));
        }
        cast.insert(character.to_string(), voice.clone());
        bm_core::cast::write_cast(&engine, &cast_path, &cast)?;
        // Surgical invalidation: only this speaker's run files (+ the headline
        // file when the Narrator itself moves), only where scripts exist.
        let (chapters, files) = self.invalidate_character(&engine, character, &old);
        Ok(format!(
            "{character}: {old} -> {voice}; invalidated {files} segment files across {} chapters ({:?}); re-render queued",
            chapters.len(),
            chapters.iter().take(8).collect::<Vec<_>>(),
        ))
    }

    /// Enqueue crawl+digest for chapters missing scripts (idempotent).
    pub fn enqueue_translate(&mut self, start: u32, count: u32) -> (usize, usize) {        let (mut crawls, mut digests) = (0, 0);
        for n in start..start + count {
            if !self.layout.chapter_txt(n).is_file() {
                let t = self.ensure_task(n, Stage::Crawl);
                if t.state == TaskState::Pending {
                    crawls += 1;
                }
            }
            if !self.layout.script(n).is_file() {
                // A digest is only offered once its crawl reads Done — and a
                // backend booted empty never created that task at all. Seed it
                // Done when the text is already on disk, or the digest waits
                // forever while workers idle (exactly this bug).
                if self.layout.chapter_txt(n).is_file() {
                    let c = self.ensure_task(n, Stage::Crawl);
                    if c.state == TaskState::Pending {
                        c.state = TaskState::Done;
                        c.updated = now_secs();
                    }
                }
                let t = self.ensure_task(n, Stage::Digest);
                if t.state == TaskState::Pending {
                    digests += 1;
                }
            }
        }
        self.save();
        (crawls, digests)
    }
}
