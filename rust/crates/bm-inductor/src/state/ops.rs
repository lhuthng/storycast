use super::Inner;
use bm_proto::{now_secs, Stage, Task, TaskState};

/// How long a box may sit in `Initializing` before the inductor stops
/// believing it is booting.
///
/// A stock Ubuntu AMI answers ssh roughly 30–60 s after `RunInstances` returns.
/// Five minutes is not a wait — it is the point past which "still booting"
/// stops being a believable explanation, so the state becomes a verdict instead
/// of a placeholder that never resolves.
pub const BOOT_DEADLINE_SECS: u64 = 300;

impl Inner {
    /// Retire boxes that never came up.
    ///
    /// `Initializing` is the one machine state with a deadline, because it is
    /// the only one where the inductor is *waiting* rather than acting. A state
    /// with no exit condition is a lie: a box terminated before it booted, or
    /// launched into a subnet this machine cannot dial, would sit in
    /// "initializing" for ever with the pane implying it is about to work.
    ///
    /// Returns one line per box retired, for the caller to log.
    pub fn expire_initializing(&mut self) -> Vec<String> {
        let now = now_secs();
        let mut out = Vec::new();
        for m in self.machines.values_mut() {
            if m.state != bm_proto::MachineState::Initializing {
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
            let note = format!(
                "never answered ssh within {} min of launch — terminated, or unreachable from here",
                BOOT_DEADLINE_SECS / 60
            );
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

    /// Manual retry for every shelved task (3 strikes) after fixing the cause.
    ///
    /// Strikes reset — unlike `release`, which keeps them — so the next failure
    /// gets a full 3 attempts again. The operator asserts the cause is fixed by
    /// pressing the key, so forgiveness is the point. It deletes nothing, so
    /// against a failure whose cause is unmet *input* it is a loop rather than a
    /// repair; reaching the producer is what `op_retry_task`'s `force` is for.
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
            t.assigned_to = None;
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
        task.assigned_to = None;
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
        // Render's unit is already one take — a pending render task *is* one
        // TTS call — so it needs no calls-per-chapter scaling any more. That
        // estimate existed only while a render task was a whole chapter, and
        // leaving it in place would multiply the render ETA by forty.
        let remaining = [
            (Stage::Crawl, pending(Stage::Crawl)),
            (Stage::Digest, pending(Stage::Digest)),
            (Stage::Render, pending(Stage::Render)),
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
                    if e.estimated_from_fallback {
                        " (guess)"
                    } else {
                        ""
                    }
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
        // The shipped catalogue is the gate that decides what may be assigned
        // on this machine.
        let policy = bm_core::voices::effective_policy(&engine);
        let cast_path = self.layout.cast(&engine);
        // Accept either form: the picker sends display names today, but a key is
        // the stable identifier and both have to work.
        let voice = bm_core::voices::resolve_voice_name(&engine, voice);
        let mut cast = bm_core::cast::read_cast(&engine, &cast_path);
        let old = cast.get(character).cloned().unwrap_or_default();
        // Trust rule:
        //   * a declared preset, or
        //   * an enrolled clone — in the `voices.json` manifest or the
        //     sample pool (both vetted at adding) — or already assigned
        //     somewhere.
        //
        // Anything else is refused at swap time rather than failing thirty
        // renders later on the sidecar's unknown-voice gate.
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
        let manifested = bm_core::pool::load_manifest(&self.layout.root)
            .contains_key(&voice);
        let admitted = declared || in_use || pooled || manifested;
        if !admitted {
            anyhow::bail!("voice {voice:?} is neither a preset nor an enrolled clone");
        }
        if old == voice {
            return Ok(format!(
                "{character} already speaks as {voice} — nothing to do"
            ));
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

    /// Save a new mix and requeue every merge: the finished mp3s were mixed
    /// with the old one. Render cache is kept — tempo and layer volumes apply
    /// at merge time, so no segment needs re-speaking. Refused while workers
    /// are mid-play, like every other cache surgery.
    pub fn op_remix(
        &mut self,
        speed: Option<f64>,
        effect_volume: Option<f64>,
        music_volume: Option<f64>,
        inject_volume: Option<f64>,
    ) -> anyhow::Result<String> {
        self.ensure_idle()?;
        let (speed, fx, music) = match (speed, effect_volume, music_volume) {
            (Some(s), Some(f), Some(m)) => (s, f, m),
            _ => anyhow::bail!("remix needs speed + fx + music volumes"),
        };
        let inj = inject_volume.unwrap_or(self.settings.inject_volume);
        for (v, what, lo, hi) in [
            (speed, "speed", 0.5, 2.0),
            (fx, "fx volume", 0.0, 2.0),
            (music, "music volume", 0.0, 2.0),
            (inj, "inject volume", 0.0, 2.0),
        ] {
            if !v.is_finite() || v < lo || v > hi {
                anyhow::bail!("{what} must be {lo}–{hi}, got {v}");
            }
        }
        self.settings.speed = speed;
        self.settings.effect_volume = fx;
        self.settings.music_volume = music;
        self.settings.inject_volume = inj;
        self.settings.save(&self.layout.settings())?;
        // `adopt = false`: this call *is* a design change, so a merge with no
        // stamp is one this change invalidated rather than one to bless. The
        // stamp decides the scope for free — every chapter reaches the knobs,
        // so every merge whose mp3 the new mix would not reproduce comes back,
        // and a merge that was never produced stays where it is.
        let n = self.invalidate_stale_design(false).len();
        Ok(format!(
            "mix saved: speed {speed}, fx {fx}, music {music}, inject {inj}; {n} merge(s) requeued"
        ))
    }

    /// A sound-design write happened outside the scheduler — `:sound` writes the
    /// pool registries itself, from the TUI, so this is the inductor being told
    /// to look rather than the inductor having done it. Same scope rule as
    /// [`Self::op_remix`]: the fingerprint decides which chapters the edit
    /// actually reached, so retuning a clip nobody uses requeues nothing.
    pub fn op_sound_changed(&mut self) -> String {
        let n = self.invalidate_stale_design(false).len();
        if n == 0 {
            // Not "everything matches": a chapter with no script cannot be
            // stamped at all, so the honest claim is that this pass found
            // nothing to requeue.
            return "sound design rechecked: nothing to requeue".into();
        }
        format!("sound design changed: {n} merge(s) requeued")
    }

    /// Reset every task of one stage to pending, dropping finished mp3s for
    /// merges so the stage re-runs end-to-end. Returns tasks touched.
    fn requeue_stage(&mut self, stage: Stage, detail: &str, now: u64) -> u32 {
        let mut n = 0u32;
        for t in self.tasks.values_mut() {
            if t.stage != stage {
                continue;
            }
            if stage == Stage::Merge {
                let _ = std::fs::remove_file(self.layout.final_mp3(t.chapter));
            }
            t.state = TaskState::Pending;
            t.attempts = 0;
            t.assigned_to = None;
            t.lease_until = None;
            t.detail = detail.into();
            t.updated = now;
            n += 1;
        }
        n
    }

    /// Requeue every merge without touching the mix or the render cache:
    /// effect clips, the scene map and the pools all apply at merge time.
    /// Refused while workers are mid-play, like every other cache surgery.
    pub fn op_remerge_all(&mut self) -> anyhow::Result<String> {
        self.ensure_idle()?;
        let n = self.requeue_stage(Stage::Merge, "requeued: remerge", now_secs());
        self.save();
        Ok(format!("remerge: {n} merge(s) requeued, render cache kept"))
    }

    /// Requeue every render task and its merge, deleting cached segments and
    /// finished mp3s: a full re-speak. Refused while workers are mid-play,
    /// like every other cache surgery. This is the expensive path — mix-only
    /// changes (speed, volumes, effect clips) requeue merges via `op_remix`
    /// and keep the render cache instead.
    pub fn op_rerender_all(&mut self) -> anyhow::Result<String> {
        self.ensure_idle()?;
        let engine = self.settings.engine.clone();
        let store = bm_core::segments::LocalStore::new(self.layout.clone());
        let mut chapters: Vec<u32> = self
            .tasks
            .values()
            .filter(|t| t.stage == Stage::Render)
            .map(|t| t.chapter)
            .collect();
        chapters.sort_unstable();
        chapters.dedup();
        let now = now_secs();
        for n in &chapters {
            let _ =
                std::fs::remove_dir_all(bm_core::segments::SegmentStore::dir(&store, &engine, *n));
            let _ = std::fs::remove_file(self.layout.final_mp3(*n));
            for stage in [Stage::Render, Stage::Merge] {
                let key = format!("{stage}:{n}");
                match self.tasks.get_mut(&key) {
                    Some(t) => {
                        t.state = TaskState::Pending;
                        t.attempts = 0;
                        t.assigned_to = None;
                        t.lease_until = None;
                        t.detail = "requeued: rerender".into();
                        t.updated = now;
                    }
                    None => {
                        let mut t = Task::new(*n, stage);
                        t.updated = now;
                        self.tasks.insert(key, t);
                    }
                }
            }
        }
        self.save();
        Ok(format!(
            "rerender: {} render(s) requeued with their merges",
            chapters.len()
        ))
    }

    /// Enqueue crawl+digest for chapters missing scripts (idempotent).
    pub fn enqueue_translate(&mut self, start: u32, count: u32) -> (usize, usize) {
        let (mut crawls, mut digests) = (0, 0);
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

    /// Rewrite written-out non-verbal sounds into engine tags across every
    /// script (`Ha ha ha!` → `[cười]`), and requeue the chapters it touches.
    /// Deterministic — no LLM, same mapping as the prompt's rule 9 — so a
    /// re-run is a no-op once every script is clean.
    ///
    /// Refused while workers are mid-play (same hazard as a swap): text edits
    /// plus file deletes under a running render mix voices. `dry_run` reports
    /// every `(chapter#index: before → after)` and writes nothing.
    pub fn op_retag(&mut self, dry_run: bool) -> anyhow::Result<String> {
        if !dry_run {
            self.ensure_idle()?;
        }
        let mut chapters: Vec<u32> = Vec::new();
        let mut edits = 0u32;
        let mut files = 0u32;
        let mut detail: Vec<String> = Vec::new();
        for (n, sp) in self.script_paths() {
            let mut data: serde_json::Value =
                bm_core::read_json(&sp).unwrap_or(serde_json::Value::Null);
            let owned: Vec<serde_json::Value> = data
                .get("segments")
                .and_then(|s| s.as_array())
                .cloned()
                .unwrap_or_default();
            if owned.is_empty() {
                continue;
            }
            // Headline segments never render (the title file speaks instead),
            // so editing them is churn: skip exactly what `drop_headline` drops.
            let skip = owned.len() - bm_core::assemble::drop_headline(&owned).len();
            let mut touched: Vec<usize> = Vec::new();
            if let Some(segments) = data.get_mut("segments").and_then(|s| s.as_array_mut()) {
                for (i, s) in segments.iter_mut().enumerate() {
                    if i < skip {
                        continue;
                    }
                    let old = s
                        .get("text")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string();
                    if let Some(new) = bm_core::digest::retag_text(&old) {
                        if !dry_run {
                            s["text"] = serde_json::Value::String(new.clone());
                        }
                        touched.push(i);
                        edits += 1;
                        // Capped so one pathological chapter cannot flood the op
                        // message; 200 entries is the whole book in practice.
                        if detail.len() < 200 {
                            detail.push(format!(
                                "ch{n}#{i}: {} → {}",
                                bm_core::util::head_chars(&old, 40),
                                bm_core::util::head_chars(&new, 40)
                            ));
                        }
                    }
                }
            }
            if touched.is_empty() {
                continue;
            }
            chapters.push(n);
            if dry_run {
                continue;
            }
            // Write the edited script, then let the plan's diff say what the
            // edit reached: a retagged run has a new content-addressed name, so
            // its old file is superseded and its take is work again, while
            // every run the edit did not touch keeps its audio. This replaced a
            // hand-rolled "delete the runs holding edited segments" that had to
            // reconstruct `expected_wavs` positions and the title offset to
            // find them — the plan already knows, exactly.
            let _ = bm_core::atomic_write(
                &sp,
                &serde_json::to_string_pretty(&data).unwrap_or_default(),
            );
            files += self.resume_render_after_edit(n, "requeued: retag");
        }
        if !dry_run {
            self.save();
        }
        if chapters.is_empty() {
            return Ok("retag: no written-out sounds found — every script already tags".into());
        }
        Ok(format!(
            "retag: {edits} segments in {} chapters ({:?}){detail_str}; invalidated {files} run files, re-render queued",
            chapters.len(),
            chapters.iter().take(12).collect::<Vec<_>>(),
            detail_str = if detail.is_empty() {
                String::new()
            } else {
                format!(" — e.g. {}", detail.join("; "))
            },
        ))
    }
}
