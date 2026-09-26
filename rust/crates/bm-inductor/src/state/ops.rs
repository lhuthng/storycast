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
        let manifested = bm_core::pool::load_manifest(&self.layout.root).contains_key(&voice);
        let admitted = declared || in_use || pooled || manifested;
        if !admitted {
            anyhow::bail!("voice {voice:?} is neither a preset nor an enrolled clone");
        }
        // A swap must be speakable everywhere it will be offered: a clone the
        // local bake lacks is one no freshly-provisioned worker has either,
        // and the invalidation below would queue renders that 500 on every
        // box. So a swap first merges whatever the local store already holds
        // into the bake (drifting the stamp, which is what makes the next
        // :prov push it) and refuses what is enrolled nowhere. Presets ship
        // with the sidecar, so only clones gate here — and only where a bake
        // exists to check against.
        if !declared && self.layout.root.join("models/voices.json").is_file() {
            bm_core::pool::bake_missing_voices(&self.layout.root);
            let want = bm_core::util::fold(&voice);
            let baked = std::fs::read_to_string(self.layout.root.join("models/voices.json"))
                .ok()
                .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                .and_then(|v| {
                    v.get("presets").and_then(|p| p.as_object()).map(|o| {
                        o.keys()
                            .any(|k| k == &voice || bm_core::util::fold(k) == want)
                    })
                })
                .unwrap_or(false);
            if !baked {
                anyhow::bail!(
                    "voice {voice:?} is not enrolled in this machine's voice store — enroll it (:A / roster add-sample) and :prov before swapping, or every render naming it fails on workers"
                );
            }
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
            "{character}: {old} -> {voice}; invalidated {files} segment files across {} chapters ({:?}); re-render queued — :prov workers to push the new voices",
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
            t.clear_holders();
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
                        t.clear_holders();
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

    /// Record what the chapter index says about a range, before anything is
    /// queued from it.
    ///
    /// A chapter the site does not have is **finished work, not missing work**:
    /// the crawl will never produce an artifact, so the row closes (strike-free)
    /// and the digest closes with it — a digest waits for its predecessor, and
    /// leaving one waiting on a chapter that cannot exist stalls the chain behind
    /// it. Returns how many chapters were closed this way.
    pub fn apply_index(&mut self, index: &bm_core::crawl::CrawlIndex) -> usize {
        let mut closed = 0;
        for (n, entry) in index.chapters() {
            if !entry.absent {
                continue;
            }
            // Never overwrite work that exists: a stale index must not erase a
            // chapter somebody imported by hand.
            if self.layout.chapter_txt(*n).is_file() {
                continue;
            }
            let reason = if entry.title.trim().is_empty() {
                "the chapter index has no chapter here".to_string()
            } else {
                entry.title.clone()
            };
            for (stage, detail) in [
                (Stage::Crawl, format!("not on the site: {reason}")),
                (Stage::Digest, "skipped: not on the site".to_string()),
            ] {
                let t = self.ensure_task(*n, stage);
                if t.state != TaskState::Done {
                    t.state = TaskState::Done;
                    t.detail = detail;
                    t.attempts = 0;
                    t.clear_holders();
                    t.lease_until = None;
                    t.updated = now_secs();
                    closed += 1;
                }
            }
        }
        if closed > 0 {
            self.save();
        }
        closed
    }

    /// Close a chapter's crawl because the operator supplied its text.
    ///
    /// This is what makes manual import a *supply path* rather than a mode: the
    /// chapter's crawl is done by definition, so its row is Done and the digest
    /// becomes offerable — and a chapter that was shelved on a broken selector
    /// gets its digest requeued by the same call.
    pub fn mark_imported(&mut self, n: u32, bytes: usize) -> bool {
        let now = now_secs();
        {
            let t = self.ensure_task(n, Stage::Crawl);
            t.state = TaskState::Done;
            t.attempts = 0;
            t.detail = format!("imported ({bytes} bytes)");
            t.clear_holders();
            t.lease_until = None;
            t.batch.clear();
            t.updated = now;
        }
        let requeued = {
            let d = self.ensure_task(n, Stage::Digest);
            let was = d.state;
            if matches!(was, TaskState::Shelved | TaskState::Failed) {
                d.state = TaskState::Pending;
                d.attempts = 0;
                d.detail = "requeued: text imported".into();
                d.clear_holders();
                d.lease_until = None;
                d.updated = now;
            }
            matches!(was, TaskState::Shelved | TaskState::Failed)
        };
        self.push_event(
            "ok",
            format!(
                "ch{n} imported ({bytes} bytes) — crawl done, digest {}",
                if requeued { "requeued" } else { "queued" }
            ),
        );
        self.save();
        true
    }

    /// Enqueue crawl+digest for chapters missing scripts (idempotent).
    ///
    /// **In manual mode nothing is fetched**, so a chapter with no text gets no
    /// crawl row at all: a task no worker can run is a row that fails three
    /// times and shelves. What it gets instead is a line in the event log naming
    /// it, and the digest that would consume it waits on an upstream row that is
    /// not there — which is exactly "blocked, pending an operator", without a
    /// new task state to explain.
    pub fn enqueue_translate(&mut self, start: u32, count: u32) -> (usize, usize) {
        let (mut crawls, mut digests) = (0, 0);
        let manual = self.settings.crawl.is_manual();
        let mut needs_import: Vec<u32> = Vec::new();
        for n in start..start + count {
            if !self.layout.chapter_txt(n).is_file() {
                if manual {
                    needs_import.push(n);
                } else {
                    let t = self.ensure_task(n, Stage::Crawl);
                    if t.state == TaskState::Pending {
                        crawls += 1;
                    }
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
        if !needs_import.is_empty() {
            // Named, not counted: the operator's next move is `:import` with a
            // file, and a bare number would not say which chapters to fetch.
            let shown: Vec<String> = needs_import
                .iter()
                .take(12)
                .map(|n| format!("ch{n}"))
                .collect();
            self.push_event(
                "warn",
                format!(
                    "manual crawl: {} chapter(s) need text — `:import` a file for {}{}",
                    needs_import.len(),
                    shown.join(", "),
                    if needs_import.len() > shown.len() {
                        " …"
                    } else {
                        ""
                    }
                ),
            );
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

    /// Re-attribute speakers on one chapter's script, then requeue exactly
    /// what the edit reached.
    ///
    /// The digest's recurring misattribution, confirmed against chapter text:
    /// third-person narration given to the character it describes ("Nàng lập
    /// tức nhíu mày…" spoken by Lạc Lan Tuyết), and a quote with no dialogue
    /// tag defaulted to Narrator instead of whoever the surrounding action
    /// introduces. The prompt's rule 3 already forbids the first half word
    /// for word — the small model disobeyed it — so re-digesting rolls the
    /// same dice; the correction is surgical.
    ///
    /// Chapter-scoped busy guard rather than the cluster-global `ensure_idle`:
    /// every file this touches belongs to the chapter (its script, its plan,
    /// its segments, its mp3), so unrelated chapters rendering alongside are
    /// unaffected. Refused while the chapter itself has work in flight or a
    /// live beat on it.
    ///
    /// A new speaker must already hold a voice (`Narrator` always does), or
    /// the chapter would requeue into a row no box can speak. The plan's diff
    /// decides the blast radius for free: a re-voiced run has a new
    /// content-addressed name, so its old file is superseded and its take is
    /// work again, while untouched runs keep their audio.
    /// Point one segment at another speaker, having first checked that the
    /// segment says who the caller thought it said.
    ///
    /// `segment` is 1-based, the way a person counts lines in the file, and
    /// `expect` is the guard: a mistyped number lands on a line that is not
    /// the one meant, and re-attributing it would be a silent, permanent edit
    /// to a chapter already rendered. So the mismatch refuses, and the refusal
    /// names what is actually there and where the expected speaker *is*, which
    /// is the answer to "I miscounted".
    ///
    /// The invalidation is [`invalidate_render`]'s, which is the plan's diff:
    /// only the takes whose voice or text moved become work, and the rest of
    /// the chapter keeps the audio it has. One caveat worth knowing, because it
    /// is the difference between one take and several: the local engine groups
    /// consecutive same-speaker segments into a single take, so re-pointing the
    /// middle of a run splits that run and re-speaks the two halves.
    pub fn op_fix_speaker(
        &mut self,
        chapter: u32,
        segment: usize,
        expect: &str,
        speaker: &str,
    ) -> anyhow::Result<String> {
        let index = segment.saturating_sub(1);
        let to = speaker.trim();
        if to.is_empty() {
            anyhow::bail!("segment {segment}: empty speaker");
        }
        if to == expect.trim() {
            anyhow::bail!(
                "segment {segment} already speaks as {to:?} — nothing to change"
            );
        }
        for t in self.tasks.values() {
            if t.chapter == chapter && matches!(t.state, TaskState::Assigned | TaskState::Running) {
                anyhow::bail!(
                    "ch{chapter} has {} in flight — wait for it to settle, then fix the speaker",
                    t.id()
                );
            }
        }
        let now = now_secs();
        for b in self.beats.values() {
            if now.saturating_sub(b.ts) < 30 && b.chapter == Some(chapter) {
                anyhow::bail!(
                    "a worker is on ch{chapter} right now ({} at {}) — wait a beat, then fix the speaker",
                    b.worker_id, b.activity,
                );
            }
        }
        let engine = self.settings.engine.clone();
        let cast = bm_core::cast::read_cast(&engine, &self.layout.cast(&engine));
        // Checked before the edit, not after: a speaker with no voice is a hard
        // planning error, so writing the script first would leave the chapter
        // unplannable and the requeue with nowhere to go.
        if to != "Narrator" && cast.get(to).is_none() {
            anyhow::bail!(
                "{to:?} holds no voice in the {engine} cast — enrol it (:voices, or roster add-sample) and :prov, or this chapter requeues into a row no box can speak"
            );
        }
        let path = self.layout.script(chapter);
        let mut data: serde_json::Value = bm_core::read_json(&path)
            .map_err(|_| anyhow::anyhow!("ch{chapter} has no script yet — digest it first"))?;
        let segments = data
            .get_mut("segments")
            .and_then(|s| s.as_array_mut())
            .ok_or_else(|| anyhow::anyhow!("ch{chapter} script has no segments array"))?;
        let len = segments.len();
        if index >= len {
            anyhow::bail!(
                "segment {segment} is past the end — ch{chapter} has {len} segment(s), numbered 1..{len}"
            );
        }
        let item = &mut segments[index];
        let here = item
            .get("speaker")
            .and_then(|s| s.as_str())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "segment {segment} is a sound, not a line — it has no speaker to change"
                )
            })?
            .to_string();
        if here != expect.trim() {
            // Name the neighbours, because "wrong number" is the likeliest
            // cause and the fix is one of the numbers printed here.
            let mut where_: Vec<String> = segments
                .iter()
                .enumerate()
                .filter(|(_, s)| {
                    s.get("speaker").and_then(|v| v.as_str()) == Some(expect.trim())
                })
                .map(|(i, _)| (i + 1).to_string())
                .collect();
            where_.truncate(8);
            let near: Vec<String> = (index.saturating_sub(1)..(index + 2).min(len))
                .map(|i| {
                    let s = segments[i].get("speaker").and_then(|v| v.as_str()).unwrap_or("?");
                    let text: String = segments[i]
                        .get("text")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .chars()
                        .take(40)
                        .collect();
                    format!("{}: {s:?} “{text}”", i + 1)
                })
                .collect();
            anyhow::bail!(
                "segment {segment} is spoken by {here:?}, not {expect:?} — nothing changed.\n  \
                 nearby: {}\n  \
                 {expect:?} is at: {}",
                near.join(" | "),
                if where_.is_empty() {
                    "nowhere in this chapter".to_string()
                } else {
                    where_.join(", ")
                }
            );
        }
        item["speaker"] = serde_json::Value::String(to.to_string());
        let _ = bm_core::atomic_write(
            &path,
            &serde_json::to_string_pretty(&data).unwrap_or_default(),
        );
        // The count is the plan's diff, taken around the invalidation, because
        // that is the number the operator watches drain. The chapter's take
        // count is not it: a three-take chapter with one segment re-pointed has
        // one take to speak, and reporting three would be a promise the
        // scheduler does not keep.
        let before = self.take_keys(chapter);
        self.invalidate_render(chapter);
        let after = self.take_keys(chapter);
        let fresh = after.iter().filter(|k| !before.contains(k)).count();
        self.save();
        let msg = format!(
            "ch{chapter} segment {segment}: {here} -> {to}; {fresh} take(s) to re-speak, merge requeued"
        );
        self.push_event("ok", msg.clone());
        Ok(msg)
    }

    /// The chapter's take keys, which is what a re-plan diffs. Empty when the
    /// chapter has no plan yet, which makes every take after an edit look new,
    /// and is the honest answer: nothing was recorded to compare against.
    fn take_keys(&self, chapter: u32) -> Vec<String> {
        bm_core::assemble::RenderPlan::load(&self.layout.plan(chapter))
            .map(|p| p.takes.into_iter().map(|t| t.take_key).collect())
            .unwrap_or_default()
    }

    pub fn op_recast(
        &mut self,
        chapter: u32,
        fixes: &[bm_proto::SpeakerFix],
        remove: &[usize],
    ) -> anyhow::Result<String> {
        for t in self.tasks.values() {
            if t.chapter == chapter && matches!(t.state, TaskState::Assigned | TaskState::Running) {
                anyhow::bail!(
                    "ch{chapter} has {} in flight — wait for it to settle, then recast",
                    t.id()
                );
            }
        }
        let now = now_secs();
        for b in self.beats.values() {
            if now.saturating_sub(b.ts) < 30 && b.chapter == Some(chapter) {
                anyhow::bail!(
                    "a worker is on ch{chapter} right now ({} at {}) — wait a beat, then recast",
                    b.worker_id,
                    b.activity,
                );
            }
        }
        if fixes.is_empty() && remove.is_empty() {
            anyhow::bail!(
                "nothing to fix — pass segment indexes with their speakers, or indexes to delete"
            );
        }
        let engine = self.settings.engine.clone();
        let cast = bm_core::cast::read_cast(&engine, &self.layout.cast(&engine));
        let path = self.layout.script(chapter);
        let mut data: serde_json::Value = bm_core::read_json(&path)
            .map_err(|_| anyhow::anyhow!("ch{chapter} has no script yet — digest it first"))?;
        let segments = data
            .get_mut("segments")
            .and_then(|s| s.as_array_mut())
            .ok_or_else(|| anyhow::anyhow!("ch{chapter} script has no segments array"))?;
        let mut done: Vec<String> = Vec::new();
        let len = segments.len();
        for f in fixes {
            let item = match segments.get_mut(f.index) {
                Some(item) => item,
                None => anyhow::bail!("segment {} is out of range (0..{})", f.index, len),
            };
            let old = item
                .get("speaker")
                .and_then(|s| s.as_str())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "segment {} is a sound, not a line — nothing to re-attribute",
                        f.index
                    )
                })?
                .to_string();
            let new = f.speaker.trim().to_string();
            if new.is_empty() {
                anyhow::bail!("segment {}: empty speaker", f.index);
            }
            if new != "Narrator" && cast.get(&new).is_none() {
                anyhow::bail!(
                    "{new:?} holds no voice — cast it first (:voices fills gaps), or the chapter requeues into a row no box can speak"
                );
            }
            if old == new {
                continue;
            }
            item["speaker"] = serde_json::Value::String(new.clone());
            done.push(format!("#{} {old}→{new}", f.index));
        }
        // Deletions run after re-attribution and in descending index order,
        // so earlier indexes stay valid while later items leave. Only lines
        // go: sounds hold the mix together and are never the duplication.
        let mut removed: Vec<usize> = Vec::new();
        if !remove.is_empty() {
            let mut order: Vec<usize> = remove.to_vec();
            order.sort_unstable();
            order.dedup();
            for idx in order.iter().rev() {
                let is_line = segments
                    .get(*idx)
                    .and_then(|s| s.get("speaker").and_then(|v| v.as_str()))
                    .is_some();
                if !is_line {
                    anyhow::bail!(
                        "segment {idx} is not a line — only duplicated lines are removed"
                    );
                }
                segments.remove(*idx);
                removed.push(*idx);
            }
            if segments.is_empty() {
                anyhow::bail!("refusing to empty ch{chapter}: at least one segment must remain");
            }
        }
        if done.is_empty() && removed.is_empty() {
            return Ok(format!(
                "recast ch{chapter}: every named speaker already matched — nothing changed"
            ));
        }
        let _ = bm_core::atomic_write(
            &path,
            &serde_json::to_string_pretty(&data).unwrap_or_default(),
        );
        self.invalidate_render(chapter);
        let mut parts = done;
        if !removed.is_empty() {
            parts.push(format!("removed {} duplicated segments", removed.len()));
        }
        Ok(format!(
            "recast ch{chapter}: {}; re-render queued",
            parts.join(", ")
        ))
    }
}
