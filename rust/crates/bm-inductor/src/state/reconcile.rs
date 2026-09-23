use super::{lease_for, Inner};
use bm_proto::{now_secs, Machine, MachineState, Stage, Task, TaskState};
use serde_json::{json, Value};

impl Inner {
    /// Derive artifact truth from disk, then reconcile assignments a dead run
    /// left behind. Idempotent: safe to run on every startup.
    pub fn reconcile(&mut self, start: u32, count: u32) {
        self.machines.entry("127.0.0.1".into()).or_insert_with(|| {
            let mut m = Machine::new("127.0.0.1", "local", 22, None, "both");
            m.set_state(MachineState::Online);
            m.tts_url = Some("http://127.0.0.1:8818".into());
            m
        });
        for n in start..start + count {
            let script = self.layout.script(n);
            let txt = self.layout.chapter_txt(n);
            let mp3 = self.layout.final_mp3(n);
            let has_txt = txt.is_file();
            let has_script = script.is_file();
            let has_mp3 = mp3.is_file();
            // Ground truth upgrades pending stages; assignments are verified below.
            // Every stage gets a task object whether or not its artifacts exist
            // yet — a missing object is indistinguishable from "no work".
            //
            // Render is **not** among them: it is materialised per take below,
            // because a render's unit of work is one segment now. A
            // chapter-granular `render:n` row from an older build is replaced
            // by its takes there.
            for (stage, done) in [
                (Stage::Crawl, has_txt),
                (Stage::Digest, has_script),
                (Stage::Merge, has_mp3),
            ] {
                let t = self
                    .tasks
                    .entry(format!("{stage}:{n}"))
                    .or_insert_with(|| Task::new(n, stage));
                if done && t.state == TaskState::Pending {
                    t.state = TaskState::Done;
                    t.updated = now_secs();
                }
            }
            // The render ledger: one row per take, `Done` exactly where the
            // take's file is on disk. This is also where a changed input is
            // noticed — a take whose key moved has a new content-addressed
            // name, so its old file is superseded by the plan's diff and its
            // row is work again.
            if has_script {
                self.materialize_render_takes(n);
            }
            // Assignments from a dead run: verify artifacts; unverified keeps
            // its assignee with a fresh lease (a live worker's report still
            // counts; a dead one's lease expires and the reaper requeues).
            // Render takes were verified by the materialisation above.
            for stage in [Stage::Crawl, Stage::Digest, Stage::Merge] {
                let key = format!("{stage}:{n}");
                let done_now = match stage {
                    Stage::Crawl => has_txt,
                    Stage::Digest => has_script,
                    _ => has_mp3,
                };
                if let Some(t) = self.tasks.get_mut(&key) {
                    if matches!(t.state, TaskState::Assigned | TaskState::Running) {
                        if done_now {
                            t.state = TaskState::Done;
                            t.clear_holders();
                            t.lease_until = None;
                        } else {
                            t.lease_until = Some(now_secs() + lease_for(stage));
                        }
                        t.updated = now_secs();
                    }
                }
            }
        }
        // Then the chapters outside the range: the range decides what this run
        // *discovers*, not what it is allowed to repair. A render row with no
        // plan behind it cannot be offered — the take's file is named by the
        // plan — so leaving it is leaving work that fails on the box for a
        // bookkeeping reason.
        let end = start.saturating_add(count);
        self.materialize_known_render_takes(start..end);
        // Tasks reconciled above are bound to the workspace profile from here
        // on; the serve gate refuses to run them anywhere else.
        self.ledger_profile = Some(self.settings.profile.clone());
        self.save();
        // **After** the promotion loop above, never before: that loop marks a
        // merge `Done` on `has_mp3` alone, so an invalidation that ran first
        // would have its deletion undone by the very promotion it was trying to
        // prevent. `adopt` is true because reconcile is routine — it is the
        // pass that also catches a hand-edited `settings.json` or scene map,
        // and it must not read "this merge predates the stamp field" as "this
        // merge is stale".
        self.invalidate_stale_design(true);
    }

    /// Every upstream stage is `Done`. Render is the one stage whose upstream
    /// is not a single row: a merge waits for **every take** of the chapter,
    /// which is the same set the mixer will read out of the plan — so "the
    /// render is finished" and "the merge can run" are one question with one
    /// answer instead of two derivations that can drift.
    pub(crate) fn upstream_done(&self, chapter: u32, stage: Stage) -> bool {
        // Digests chain: chapter N reads the bible chapter N-1 wrote, so N is
        // offerable only after N-1's digest is Done. A missing previous row
        // (a range that starts here, a hand-written ledger) counts as
        // satisfied — otherwise work that was never enqueued would block work
        // that was. Chapter 1 (and 0) have no predecessor.
        if stage == Stage::Digest && chapter > 1 {
            let prev = format!("{}:{}", Stage::Digest.as_str(), chapter - 1);
            let ready = self
                .tasks
                .get(&prev)
                .map(|t| t.state == TaskState::Done)
                .unwrap_or(true);
            if !ready {
                return false;
            }
        }
        stage.upstream().iter().all(|u| {
            if *u == Stage::Render {
                return self.render_takes_done(chapter);
            }
            self.tasks
                .get(&format!("{u}:{chapter}"))
                .map(|t| t.state == TaskState::Done)
                .unwrap_or(false)
        })
    }

    /// A chapter is parked when any of its tasks is shelved — including a
    /// single take that struck out three times, because the merge cannot run
    /// without it either.
    pub(crate) fn shelved(&self, chapter: u32) -> bool {
        self.tasks
            .values()
            .any(|t| t.chapter == chapter && t.state == TaskState::Shelved)
    }

    pub(crate) fn ensure_task(&mut self, chapter: u32, stage: Stage) -> &mut Task {
        self.tasks
            .entry(format!("{stage}:{chapter}"))
            .or_insert_with(|| Task::new(chapter, stage))
    }

    /// Every persisted chapter script, sorted: the unit every bulk pass
    /// (swap invalidation, reconcile rewrite) walks.
    pub(crate) fn script_paths(&self) -> Vec<(u32, std::path::PathBuf)> {
        let mut scripts: Vec<std::path::PathBuf> = std::fs::read_dir(self.layout.data())
            .map(|rd| rd.filter_map(|e| e.ok().map(|x| x.path())).collect())
            .unwrap_or_default();
        scripts.retain(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("script-") && n.ends_with(".json"))
                .unwrap_or(false)
        });
        let mut out: Vec<(u32, std::path::PathBuf)> = scripts
            .into_iter()
            .map(|sp| {
                let n: u32 = sp
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .and_then(|s| s.strip_prefix("script-"))
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                (n, sp)
            })
            .collect();
        out.sort();
        out
    }

    /// The script changed underneath the chapter, so every unit's inputs are
    /// suspect. **The plan's diff is the invalidation**: a changed input has a
    /// new content-addressed name, so its old file is superseded and the take is
    /// work again — while a take whose inputs did not change keeps its audio.
    /// That is strictly better than deleting the directory, which re-spoke the
    /// whole chapter for a one-line edit.
    ///
    /// Attempts reset (this is new work, not a retry) and the stale mp3 goes, so
    /// nothing serves the old dramatization meanwhile. A chapter that cannot be
    /// planned here gets no rows — its failure is named by the stage that could
    /// not read it, rather than invented here.
    pub(crate) fn invalidate_render(&mut self, chapter: u32) {
        let _ = std::fs::remove_file(self.layout.final_mp3(chapter));
        self.replan_render_takes(chapter);
        let key = format!("{}:{chapter}", Stage::Merge);
        match self.tasks.get_mut(&key) {
            Some(t) => {
                t.state = TaskState::Pending;
                t.attempts = 0;
                t.clear_holders();
                t.lease_until = None;
                t.detail = "requeued: script changed".into();
                t.updated = now_secs();
            }
            None => {
                let mut t = Task::new(chapter, Stage::Merge);
                t.detail = "requeued: script changed".into();
                t.updated = now_secs();
                self.tasks.insert(key, t);
            }
        }
        self.save();
    }

    /// Surgical invalidation for one speaker: the chapters that hear them
    /// rebuild their plan, the diff names exactly the files that speaker's
    /// takes superseded, and only those takes re-speak. Returns touched
    /// chapters + superseded files.
    ///
    /// This used to reconstruct the stale filenames from the OLD voice string
    /// (`{tag}_{old}.wav`) and delete them by hand. A filename is not an
    /// identity — with content-addressed takes the diff *is* the stale set, so
    /// one path serves a rename, a fold, a retag and a script rewrite without
    /// knowing which of them it is looking at.
    ///
    /// A chapter counts when the speaker is heard in it, not when a stale file
    /// happened to be deleted: renders run on workers whose cache never comes
    /// home, so gating on local files silently skips every remotely-rendered
    /// chapter (its mp3 keeps the old voice forever). Narrowing is now the
    /// plan's: a chapter whose takes are all still on disk under their current
    /// keys is left alone.
    pub(crate) fn invalidate_character(
        &mut self,
        engine: &str,
        character: &str,
        _old: &str,
    ) -> (Vec<u32>, u32) {
        let mut chapters: Vec<u32> = Vec::new();
        let mut files = 0u32;
        let _ = engine;
        // Speakers are matched literally *or* through the bible. Literally,
        // because a fold calls this with the absorbed name while the bible has
        // already been rewritten to hold that name as the winner's alias — the
        // scripts still say it, so the files it produced are the stale ones.
        // Through the bible, because the picker offers canonical names while a
        // script may spell the same speaker as a variant, and a swap that
        // misses those leaves exactly the files it was invoked to remove.
        let bible = bm_core::digest::load_bible(&self.layout.bible());
        for (n, sp) in self.script_paths() {
            let data: Value = bm_core::read_json(&sp).unwrap_or(Value::Null);
            let segments = data
                .get("segments")
                .and_then(|s| s.as_array())
                .cloned()
                .unwrap_or_default();
            let planned = bm_core::assemble::Planned::plan(&segments);
            let hears = planned.runs().iter().any(|run| {
                run.speaker == character
                    || bm_core::digest::resolve_speaker(&bible, &run.speaker) == character
            });
            // A chapter can carry this speaker's voice without hearing them:
            // the headline/published title speaks as the Narrator even when
            // nobody else in the chapter does. The stored plan is what says so
            // — its takes record the speaker each voice came from.
            let in_plan = bm_core::assemble::RenderPlan::load(&self.layout.plan(n))
                .map(|p| p.takes.iter().any(|t| t.speaker == character))
                .unwrap_or(false);
            if !hears && !in_plan {
                continue;
            }
            let superseded = self.resume_render_after_edit(n, "requeued: voice changed");
            if superseded == 0 && self.render_takes_done(n) {
                continue;
            }
            files += superseded;
            chapters.push(n);
        }
        self.save();
        (chapters, files)
    }

    /// Fold duplicate characters into one: bible entries, cast keys, every
    /// persisted script, then the losers' cached audio. Same mid-play refusal
    /// as a voice swap — it performs the same surgery, once per absorbed name.
    ///
    /// Invalidation runs BEFORE the script rewrite: the stale-file scan
    /// matches variant speakers, which the rewrite then erases.
    pub fn apply_reconcile(
        &mut self,
        merges: &[bm_core::digest::BibleMerge],
    ) -> anyhow::Result<String> {
        self.ensure_idle()?;
        let engine = self.settings.engine.clone();
        let path = self.layout.bible();
        // Pre-mutation snapshot: one reconcile rewrites bible, cast and
        // dozens of scripts at once — a bad merge must be restorable.
        {
            let snap = self
                .layout
                .scratch()
                .join(format!("reconcile-bak-{}", now_secs()));
            let _ = std::fs::create_dir_all(&snap);
            let _ = std::fs::copy(&path, snap.join("bible.json"));
            let _ = std::fs::copy(self.layout.cast(&engine), snap.join("cast.json"));
        }
        let mut bible: Value = bm_core::read_json(&path).unwrap_or(json!({"characters": []}));
        let (mut applied, log) = bm_core::digest::apply_merges(&mut bible, merges);
        // Cast-only variants never entered the bible, so the merger skips
        // them — yet they fork voices. Same canon-key + present in the cast
        // folds here, so the cast + script rewrite below still runs.
        {
            let cast_now = bm_core::cast::read_cast(&engine, &self.layout.cast(&engine));
            fn in_bible(bible: &Value, n: &str) -> bool {
                bible
                    .get("characters")
                    .and_then(|c| c.as_array())
                    .map(|a| {
                        a.iter()
                            .any(|c| c.get("name").and_then(|x| x.as_str()) == Some(n))
                    })
                    .unwrap_or(false)
            }
            for (canonical, absorbs) in merges {
                if !in_bible(&bible, canonical) {
                    continue;
                }
                let mut extra = Vec::new();
                for name in absorbs {
                    if name == canonical
                        || in_bible(&bible, name)
                        || !cast_now.contains_key(name)
                        || applied.iter().any(|(_, d)| d.contains(name))
                        || bm_core::digest::canon_key(name) != bm_core::digest::canon_key(canonical)
                    {
                        continue;
                    }
                    extra.push(name.clone());
                }
                if extra.is_empty() {
                    continue;
                }
                if let Some(chars) = bible.get_mut("characters").and_then(|c| c.as_array_mut()) {
                    if let Some(target) = chars.iter_mut().find(|c| {
                        c.get("name").and_then(|x| x.as_str()) == Some(canonical.as_str())
                    }) {
                        let mut aliases: Vec<String> = target
                            .get("proper_aliases")
                            .and_then(|a| a.as_array())
                            .map(|a| {
                                a.iter()
                                    .filter_map(|x| x.as_str().map(String::from))
                                    .collect()
                            })
                            .unwrap_or_default();
                        for a in &extra {
                            if !aliases.contains(a) {
                                aliases.push(a.clone());
                            }
                        }
                        target["proper_aliases"] = json!(aliases);
                        applied.push((canonical.clone(), extra));
                    }
                }
            }
        }
        if applied.is_empty() {
            return Ok("reconcile: nothing to fold".into());
        }
        bm_core::digest::save_bible(&bible, &path)?;

        // Cast keys: the canonical entry keeps its voice and adopts the
        // absorbed one only when unassigned; absorbed keys disappear.
        let cast_path = self.layout.cast(&engine);
        let mut cast = bm_core::cast::read_cast(&engine, &cast_path);
        let mut invalidations: Vec<(String, String)> = Vec::new();
        for (canonical, absorb) in &applied {
            for name in absorb {
                if let Some(v) = cast.remove(name) {
                    if !cast.contains_key(canonical) {
                        cast.insert(canonical.clone(), v.clone());
                    }
                    invalidations.push((name.clone(), v));
                }
            }
        }
        bm_core::cast::write_cast(&engine, &cast_path, &cast)?;

        let mut chapters: Vec<u32> = Vec::new();
        let mut files = 0u32;
        for (name, old) in &invalidations {
            let (ch, f) = self.invalidate_character(&engine, name, old);
            files += f;
            for n in ch {
                if !chapters.contains(&n) {
                    chapters.push(n);
                }
            }
        }

        // Every script through the folded bible: roster + speakers go canonical.
        let mut scripts = 0u32;
        for (_, sp) in self.script_paths() {
            let mut data: Value = bm_core::read_json(&sp).unwrap_or(Value::Null);
            if bm_core::digest::canonicalize_script(&mut data, &bible) > 0
                && bm_core::atomic_write(
                    &sp,
                    &serde_json::to_string_pretty(&data).unwrap_or_default(),
                )
                .is_ok()
            {
                scripts += 1;
            }
        }

        chapters.sort();
        for line in &log {
            self.push_event("info", format!("reconcile {line}"));
        }
        self.save();
        let who: Vec<String> = applied
            .iter()
            .map(|(c, a)| format!("{c} <= {}", a.join(", ")))
            .collect();
        Ok(format!(
            "reconcile: {}; {scripts} scripts rewritten, {files} stale segment files across {} chapters re-render queued",
            who.join("; "),
            chapters.len(),
        ))
    }
}
