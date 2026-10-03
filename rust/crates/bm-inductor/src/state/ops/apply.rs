use super::*;

impl Inner {
    /// Repoint one character's voice and invalidate only its cached segments.
    /// Other characters keep their cache; affected chapters re-render + merge.
    ///
    /// **The offline path, and tests.** The live API never calls this: it posts
    /// [`bm_proto::ExclusiveOp::SwapVoice`], which parks the swap on exactly the
    /// chapters it invalidates instead of refusing. Here there is no scheduler
    /// to wait on — `offline_swap` runs this against a throwaway Inner with the
    /// API down and no local worker alive — so the cluster-wide guard stands.
    /// Only *fresh* evidence counts (30s) — stale beats and ghost assignments
    /// are the reaper's job, and an empty ledger (offline) always passes.
    /// Repoint one character's voice and invalidate only its cached segments.
    /// Other characters keep their cache; affected chapters re-render + merge.
    ///
    /// **The offline path, and tests.** The live API never calls this: it posts
    /// [`bm_proto::ExclusiveOp::SwapVoice`], which parks the swap on exactly the
    /// chapters it invalidates instead of refusing. Here there is no scheduler
    /// to wait on — `offline_swap` runs this against a throwaway Inner with the
    /// API down and no local worker alive — so the cluster-wide guard stands.
    /// Only *fresh* evidence counts (30s) — stale beats and ghost assignments
    /// are the reaper's job, and an empty ledger (offline) always passes.
    pub fn op_swap_voice(&mut self, character: &str, voice: &str) -> anyhow::Result<String> {
        self.ensure_idle()?;
        self.swap_apply(character, voice)
    }

    /// The swap body, guardless: the exclusive queue's gate has already
    /// cleared exactly the chapters this invalidates, so the cluster-wide
    /// refusal must not re-run here — a crawl finishing somewhere else is
    /// not a reason to error a write that cannot reach it.
    pub(crate) fn swap_apply(&mut self, character: &str, voice: &str) -> anyhow::Result<String> {
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
        let pooled = bm_core::pool::load_pool(&self.layout.voice_pool()).contains_key(&voice);
        let manifested =
            bm_core::pool::load_manifest(&self.layout.voices_manifest()).contains_key(&voice);
        let admitted = declared || in_use || pooled || manifested;
        if !admitted {
            anyhow::bail!("voice {voice:?} is neither a preset nor an enrolled clone");
        }
        // A swap must be speakable everywhere it will be offered: a clone the
        // engine's store lacks is one no freshly-provisioned worker has
        // either, and the invalidation below would queue renders that 500 on
        // every box. So a swap first enrolls what this book declares and the
        // store lacks — the same merge provisioning runs, in the shape the
        // bound engine declares (a preset store takes the python enrollment's
        // preset, a clip store takes this book's own clip) — and refuses only
        // what the store still does not hold. Presets ship with the sidecar, so
        // only clones gate here, and only where a store exists to check
        // against.
        //
        // The check reads the **engine's own store**
        // (`bm_core::pool::installed_voices`), never an assumption about what
        // a store is: the old form looked for a VieNeu preset key and so
        // refused every clone in a pocket workspace, where the store is a
        // `file` per voice and nothing could ever have put one there.
        if !declared && self.layout.tts_voices().is_file() {
            bm_core::pool::bake_missing_voices(&self.layout);
            let want = bm_core::util::fold(&voice);
            let held = bm_core::pool::installed_voices(&self.layout).is_some_and(|names| {
                names
                    .iter()
                    .any(|n| n == &voice || bm_core::util::fold(n) == want)
            });
            if !held {
                // Name the clip the book declared, when there is one: a
                // `refs/…` file that is gone (or undecodable) is the one
                // reason the just-run enrollment could still have left the
                // store without the voice, and it is not visible from here.
                let declared = bm_core::pool::load_manifest(&self.layout.voices_manifest())
                    .get(&voice)
                    .map(|clip| format!(" (this book's clip: {clip})"))
                    .unwrap_or_default();
                anyhow::bail!(
                    "voice {voice:?} is not enrolled in {engine}'s voice store ({}){declared} — :N/:A enrolls a clip from this book's refs/ into that store and :prov pushes it to workers; every render naming it fails on workers until then",
                    self.layout.tts_voices().display()
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
    /// at merge time, so no segment needs re-speaking.
    ///
    /// **The offline path, and tests** — the live API posts
    /// [`bm_proto::ExclusiveOp::Remix`] and parks the mix behind the merges it
    /// rewrites (see `offline_remix`, the one live caller here).
    pub fn op_remix(
        &mut self,
        speed: Option<f64>,
        effect_volume: Option<f64>,
        music_volume: Option<f64>,
        inject_volume: Option<f64>,
    ) -> anyhow::Result<String> {
        self.ensure_idle()?;
        self.remix_apply(speed, effect_volume, music_volume, inject_volume)
    }

    /// The remix body, guardless — see [`Self::swap_apply`].
    pub(crate) fn remix_apply(
        &mut self,
        speed: Option<f64>,
        effect_volume: Option<f64>,
        music_volume: Option<f64>,
        inject_volume: Option<f64>,
    ) -> anyhow::Result<String> {
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
    ///
    /// **Queued rather than refused**, like every other cache surgery — the
    /// `Op::Remerge` API arm routes here through `exclusive_request`, so the
    /// guarded `op_remerge_all` wrapper this replaced had no callers left. What
    /// waits is the *whole cluster*; nothing waits on a chapter.
    pub(crate) fn remerge_apply(&mut self) -> anyhow::Result<String> {
        let n = self.requeue_stage(Stage::Merge, "requeued: remerge", now_secs());
        self.save();
        Ok(format!("remerge: {n} merge(s) requeued, render cache kept"))
    }

    /// Requeue every render task and its merge, deleting cached segments and
    /// finished mp3s: a full re-speak.
    ///
    /// This is the expensive path — mix-only changes (speed, volumes, effect
    /// clips) requeue merges via [`Self::remix_apply`] and keep the render cache
    /// instead. Queued like every other cache surgery: the `Op::Rerender` API
    /// arm goes through `exclusive_request`, so the guarded wrapper that used to
    /// sit in front of this had no callers left.
    pub(crate) fn rerender_apply(&mut self) -> anyhow::Result<String> {
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
}
