use super::{lease_for, Inner};
use bm_proto::{now_secs, Machine, MachineState, Stage, Task, TaskState};
use serde_json::{json, Value};

impl Inner {
    /// Derive artifact truth from disk, then reconcile assignments a dead run
    /// left behind. Idempotent: safe to run on every startup.
    pub fn reconcile(&mut self, start: u32, count: u32) {
        self.machines.entry("127.0.0.1".into()).or_insert_with(|| {
            let mut m = Machine::new("127.0.0.1", "local", 22, None, "both");
            m.state = MachineState::Online;
            m.tts_url = Some("http://127.0.0.1:8818".into());
            m
        });
        for n in start..start + count {
            let script = self.layout.script(n);
            let txt = self.layout.chapter_txt(n);
            let mp3 = self.layout.final_mp3(n);
            let has_txt = txt.is_file();
            let has_script = script.is_file();
            let engine = self.settings.engine.clone();
            let segs_done = has_script
                && bm_core::assemble::segments_complete(
                    &script,
                    &self.layout.cast(&engine),
                    &self.layout.bible(),
                    &self.layout.seg_dir(&engine, n),
                    &engine,
                );
            let has_mp3 = mp3.is_file();
            // Ground truth upgrades pending stages; assignments are verified below.
            // Every stage gets a task object whether or not its artifacts exist
            // yet — a missing object is indistinguishable from "no work".
            for (stage, done) in [
                (Stage::Crawl, has_txt),
                (Stage::Digest, has_script),
                (Stage::Render, segs_done),
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
            // Assignments from a dead run: verify artifacts; unverified keeps
            // its assignee with a fresh lease (a live worker's report still
            // counts; a dead one's lease expires and the reaper requeues).
            for stage in Stage::ALL {
                let key = format!("{stage}:{n}");
                let done_now = match stage {
                    Stage::Crawl => has_txt,
                    Stage::Digest => has_script,
                    Stage::Render => segs_done,
                    Stage::Merge => has_mp3,
                };
                if let Some(t) = self.tasks.get_mut(&key) {
                    if matches!(t.state, TaskState::Assigned | TaskState::Running) {
                        if done_now {
                            t.state = TaskState::Done;
                            t.assigned_to = None;
                            t.lease_until = None;
                        } else {
                            t.lease_until = Some(now_secs() + lease_for(stage));
                        }
                        t.updated = now_secs();
                    }
                }
            }
        }
        self.save();
    }

    pub(crate) fn upstream_done(&self, chapter: u32, stage: Stage) -> bool {
        stage.upstream().iter().all(|u| {
            self.tasks
                .get(&format!("{u}:{chapter}"))
                .map(|t| t.state == TaskState::Done)
                .unwrap_or(false)
        })
    }

    pub(crate) fn shelved(&self, chapter: u32) -> bool {
        Stage::ALL.iter().any(|s| {
            self.tasks
                .get(&format!("{s}:{chapter}"))
                .map(|t| t.state == TaskState::Shelved)
                .unwrap_or(false)
        })
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

    /// Drop a chapter's rendered output and requeue render+merge: the script
    /// changed underneath them, so every segment filename and voice resolve
    /// is suspect. Attempts reset — this is new work, not a retry — and the
    /// stale mp3 goes so nothing serves the old dramatization meanwhile.
    pub(crate) fn invalidate_render(&mut self, chapter: u32) {
        let engine = self.settings.engine.clone();
        let _ = std::fs::remove_dir_all(self.layout.seg_dir(&engine, chapter));
        let _ = std::fs::remove_file(self.layout.final_mp3(chapter));
        for stage in [Stage::Render, Stage::Merge] {
            let key = format!("{stage}:{chapter}");
            match self.tasks.get_mut(&key) {
                Some(t) => {
                    t.state = TaskState::Pending;
                    t.attempts = 0;
                    t.assigned_to = None;
                    t.lease_until = None;
                    t.detail = "requeued: script changed".into();
                    t.updated = now_secs();
                }
                None => {
                    let mut t = Task::new(chapter, stage);
                    t.updated = now_secs();
                    self.tasks.insert(key, t);
                }
            }
        }
        self.save();
    }

    /// Surgical invalidation for one speaker: delete only their local run
    /// files (filenames embed the OLD voice — exactly the stale set), drop
    /// the finished mp3s, requeue render+merge. Returns touched chapters + files.
    ///
    /// A chapter counts when the speaker is heard in it, not when a stale
    /// file happened to be deleted: renders run on workers whose segment
    /// cache never comes home, so gating on local files silently skips every
    /// remotely-rendered chapter (its mp3 keeps the old voice forever).
    /// Requeueing is safe regardless — segment filenames embed the voice, so
    /// the worker only re-synthesizes the new voice's files and the merger
    /// (`expected_wavs`) resolves against the current cast.
    ///
    /// Narrowing: a chapter whose local store is already complete and holds
    /// no stale files is left alone — there is nothing to re-speak.
    pub(crate) fn invalidate_character(
        &mut self,
        engine: &str,
        character: &str,
        old: &str,
    ) -> (Vec<u32>, u32) {
        let mut chapters: Vec<u32> = Vec::new();
        let mut files = 0u32;
        let store = bm_core::segments::LocalStore::new(self.layout.clone());
        for (n, sp) in self.script_paths() {
            let data: Value = bm_core::read_json(&sp).unwrap_or(Value::Null);
            let segments = data
                .get("segments")
                .and_then(|s| s.as_array())
                .cloned()
                .unwrap_or_default();
            let planned = bm_core::assemble::drop_headline(&segments);
            let seg_dir = bm_core::segments::SegmentStore::dir(&store, engine, n);
            let local = engine == "vieneu";
            let speaks = bm_core::assemble::runs(planned)
                .iter()
                .any(|run| run.speaker == character);
            let mut touched = false;
            for run in bm_core::assemble::runs(planned) {
                if run.speaker != character {
                    continue;
                }
                let names: Vec<String> = if local {
                    let (a, b) = (run.idx[0], run.idx[run.idx.len() - 1]);
                    let tag = if a == b {
                        format!("{a:04}")
                    } else {
                        format!("{a:04}-{b:04}")
                    };
                    vec![format!("{tag}_{old}.wav")]
                } else {
                    run.idx
                        .iter()
                        .map(|i| format!("{i:04}_{old}.wav"))
                        .collect()
                };
                for name in names {
                    let stale = seg_dir.join(&name);
                    if stale.is_file() {
                        let _ = std::fs::remove_file(&stale);
                        files += 1;
                        touched = true;
                    }
                }
            }
            if character == "Narrator" {
                let stale = seg_dir.join(format!("title_{old}.wav"));
                if stale.is_file() {
                    let _ = std::fs::remove_file(&stale);
                    files += 1;
                    touched = true;
                }
            }
            // Requeue when stale files were found, or the speaker is heard but
            // the local store is incomplete (pre-migration remote chapters,
            // never-rendered chapters). A complete store with no stale files
            // needs nothing — the gate `touched || speaks` used to requeue
            // those too, because no local file meant "unknown origin".
            let complete = speaks
                && !touched
                && bm_core::assemble::segments_complete(
                    &sp,
                    &self.layout.cast(engine),
                    &self.layout.bible(),
                    &seg_dir,
                    engine,
                );
            if touched || (speaks && !complete) {
                chapters.push(n);
                // Stale product goes away; render+merge requeue fresh.
                let _ = std::fs::remove_file(self.layout.final_mp3(n));
                for stage in [Stage::Render, Stage::Merge] {
                    let key = format!("{stage}:{n}");
                    if let Some(t) = self.tasks.get_mut(&key) {
                        t.state = TaskState::Pending;
                        t.attempts = 0;
                        t.assigned_to = None;
                        t.lease_until = None;
                        t.updated = now_secs();
                    } else {
                        let mut t = Task::new(n, stage);
                        t.updated = now_secs();
                        self.tasks.insert(key, t);
                    }
                }
            }
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
