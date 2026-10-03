use super::{lease_for, Inner};
use bm_proto::{now_secs, Machine, MachineState, Stage, Task, TaskState};
use serde_json::{json, Value};

impl Inner {
    /// Derive artifact truth from disk, then reconcile assignments a dead run
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
            if has_script {
                self.materialize_render_takes(n);
            }
            // Assignments from a dead run: verify artifacts; unverified keeps
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
        let end = start.saturating_add(count);
        self.materialize_known_render_takes(start..end);
        // Tasks reconciled above are bound to the workspace profile from here
        self.ledger_profile = Some(self.settings.profile.clone());
        self.save();
        // **After** the promotion loop above, never before: that loop marks a
        // and it must not read "this merge predates the stamp field" as "this
        // merge is stale".
        self.invalidate_stale_design(true);
    }

    /// Every upstream stage is `Done`. Render is the one stage whose upstream
    /// which is the same set the mixer will read out of the plan — so "the
    /// render is finished" and "the merge can run" are one question with one
    pub(crate) fn upstream_done(&self, chapter: u32, stage: Stage) -> bool {
        // Digests chain: chapter N reads the bible chapter N-1 wrote, so N is
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
    pub(crate) fn script_paths(&self) -> Vec<(u32, std::path::PathBuf)> {
        self.layout
            .scripts()
            .into_iter()
            .filter_map(|sp| bm_core::paths::chapter_of(&sp).map(|n| (n, sp)))
            .collect()
    }

    /// The script changed underneath the chapter, so every unit's inputs are
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

    /// Chapters that would hear one speaker, under the name or any alias
    pub(crate) fn chapters_hearing_speaker(&self, character: &str) -> Vec<u32> {
        self.chapters_hearing_names(&[character.to_string()])
    }

    /// The same predicate for a whole plan at once. `chapters_hearing` walks
    pub(crate) fn chapters_hearing_names(&self, names: &[String]) -> Vec<u32> {
        let bible = bm_core::digest::load_bible(&self.layout.bible());
        self.chapters_hearing(&bible, names)
    }

    /// Chapters that would hear these names: a script speaking them
    fn chapters_hearing(&self, bible: &Value, names: &[String]) -> Vec<u32> {
        let mut out = Vec::new();
        for (n, sp) in self.script_paths() {
            let data: Value = bm_core::read_json(&sp).unwrap_or(Value::Null);
            let segments = data
                .get("segments")
                .and_then(|s| s.as_array())
                .cloned()
                .unwrap_or_default();
            let planned = bm_core::assemble::Planned::plan(&segments);
            let heard = names.iter().any(|name| {
                planned.runs().iter().any(|run| {
                    run.speaker == *name
                        || bm_core::digest::resolve_speaker(bible, &run.speaker) == *name
                })
            });
            let in_plan = bm_core::assemble::RenderPlan::load(&self.layout.plan(n))
                .map(|p| p.takes.iter().any(|t| names.contains(&t.speaker)))
                .unwrap_or(false);
            if (heard || in_plan) && !out.contains(&n) {
                out.push(n);
            }
        }
        out.sort();
        out
    }

    /// Chapters whose raw text still names any of these (fold-insensitive):
    pub(crate) fn chapters_naming_in_text(&self, names: &[String]) -> Vec<u32> {
        let folds: Vec<String> = names
            .iter()
            .map(|n| bm_core::util::fold(n))
            .filter(|f| !f.is_empty())
            .collect();
        if folds.is_empty() {
            return Vec::new();
        }
        let mut out: Vec<u32> = Vec::new();
        for t in self.tasks.values() {
            if out.contains(&t.chapter) {
                continue;
            }
            let named = std::fs::read_to_string(self.layout.chapter_txt(t.chapter))
                .map(|text| {
                    let f = bm_core::util::fold(&text);
                    folds.iter().any(|n| f.contains(n.as_str()))
                })
                .unwrap_or(false);
            if named {
                out.push(t.chapter);
            }
        }
        out.sort_unstable();
        out
    }

    /// Refuse the merge only where it collides — never cluster-wide.
    #[cfg(test)]
    fn ensure_mergeable(&self, absorbs: &[String], affected: &[u32]) -> anyhow::Result<()> {
        let now = now_secs();
        let fresh = |ts: u64| now.saturating_sub(ts) < 30;
        // The digest half, from the one predicate the gate reads too.
        let naming = self.chapters_naming_in_text(absorbs);
        let live_holders = |t: &Task| -> Vec<String> {
            t.holders()
                .into_iter()
                .filter(|w| self.beats.get(*w).map(|b| fresh(b.ts)).unwrap_or(false))
                .map(|w| w.to_string())
                .collect::<Vec<_>>()
        };
        let mut busy: Vec<String> = Vec::new();
        for t in self.tasks.values() {
            if !matches!(t.state, TaskState::Assigned | TaskState::Running) {
                continue;
            }
            // Renders and merges only collide on chapters being rewritten.
            if matches!(t.stage, Stage::Render | Stage::Merge) {
                if !affected.contains(&t.chapter) {
                    continue;
                }
                let holders = live_holders(t);
                if !holders.is_empty() {
                    busy.push(format!("{} on {}", t.id(), holders.join(",")));
                }
                continue;
            }
            // Digests collide through the bible, but only when their chapter
            if t.stage != Stage::Digest {
                continue;
            }
            let holders: Vec<&str> = t.holders().into_iter().collect();
            let live = holders
                .iter()
                .any(|w| self.beats.get(*w).map(|b| fresh(b.ts)).unwrap_or(false));
            if !live && now.saturating_sub(t.updated) >= 120 {
                continue;
            }
            if naming.contains(&t.chapter) {
                let who = if holders.is_empty() {
                    "unstarted".to_string()
                } else {
                    holders.join(",")
                };
                busy.push(format!("{} on {who}", t.id()));
            }
        }
        // Fresh beats naming affected chapters directly, for the row this
        for (w, b) in &self.beats {
            if !fresh(b.ts) {
                continue;
            }
            if let Some(tid) = &b.task_id {
                if let Some(ch) = Task::chapter_of(tid) {
                    if affected.contains(&ch) && !busy.iter().any(|s| s.starts_with(tid.as_str())) {
                        busy.push(format!("{tid} on {w}"));
                    }
                }
            }
        }
        if busy.is_empty() {
            return Ok(());
        }
        busy.sort();
        anyhow::bail!(
            "merge waits on {} — its chapters are mid-play (or a digest names the absorbed)",
            busy.iter().take(4).cloned().collect::<Vec<_>>().join(", "),
        )
    }

    /// Fold duplicate characters into one: bible entries, cast keys, every
    #[cfg(test)]
    pub fn apply_reconcile(
        &mut self,
        merges: &[bm_core::digest::BibleMerge],
        manual: bool,
    ) -> anyhow::Result<String> {
        // No cluster-wide quiet: only the chapters this merge rewrites (plus
        let scan: Value =
            bm_core::read_json(&self.layout.bible()).unwrap_or(json!({"characters": []}));
        let absorbs: Vec<String> = merges.iter().flat_map(|(_, a)| a.iter().cloned()).collect();
        let affected = self.chapters_hearing(&scan, &absorbs);
        self.ensure_mergeable(&absorbs, &affected)?;
        self.reconcile_apply(merges, manual)
    }

    /// The fold body, guardless — see [`Self::apply_reconcile`].
    pub(crate) fn reconcile_apply(
        &mut self,
        merges: &[bm_core::digest::BibleMerge],
        manual: bool,
    ) -> anyhow::Result<String> {
        let engine = self.settings.engine.clone();
        let path = self.layout.bible();
        if manual {
            // A named pair that folds nothing must refuse, not silently pass:
            let bible: Value = bm_core::read_json(&path).unwrap_or(json!({"characters": []}));
            let names: Vec<&str> = bible
                .get("characters")
                .and_then(|c| c.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|c| c.get("name").and_then(|x| x.as_str()))
                        .collect()
                })
                .unwrap_or_default();
            let cast = bm_core::cast::read_cast(&engine, &self.layout.cast(&engine));
            for (canonical, absorbs) in merges {
                if canonical == "Narrator" || absorbs.iter().any(|a| a == "Narrator") {
                    anyhow::bail!("merge refuses the Narrator — it is a voice, not a character");
                }
                if absorbs.is_empty() {
                    anyhow::bail!("merge names nobody to absorb into {canonical:?}");
                }
                if !names.contains(&canonical.as_str()) {
                    anyhow::bail!("merge refuses: survivor {canonical:?} is not in the bible");
                }
                for name in absorbs {
                    if name == canonical {
                        anyhow::bail!("merge refuses: {name:?} cannot absorb itself");
                    }
                    if !names.contains(&name.as_str()) && !cast.contains_key(name) {
                        anyhow::bail!(
                            "merge refuses: {name:?} is in neither the bible nor the cast"
                        );
                    }
                }
            }
        }
        // Pre-mutation snapshot: one reconcile rewrites bible, cast and
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
                        // Automatic folds only trust spelling variants; a
                        || (!manual
                            && bm_core::digest::canon_key(name)
                                != bm_core::digest::canon_key(canonical))
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
