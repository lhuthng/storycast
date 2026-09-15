use super::{Inner, lease_for};
use bm_proto::{Complete, Stage, Task, TaskOffer, TaskState, now_secs};
use serde_json::{Value, json};
use std::collections::HashMap;

impl Inner {
    /// Oldest assignable task for this worker. Merge affinity keeps segments
    /// on the machine that rendered them.
    pub fn offer(&mut self, worker_id: &str) -> Option<TaskOffer> {
        self.reap();
        let machine = self.workers.get(worker_id).cloned().unwrap_or_default();
        let mut ids: Vec<String> = self.tasks.keys().cloned().collect();
        // Numeric chapter order — lexical sort would put ch100 before ch93.
        ids.sort_by_key(|id| {
            let (stage, ch) = id.split_once(':').unwrap_or(("", "0"));
            (ch.parse::<u32>().unwrap_or(0), stage.to_string())
        });
        // Pick first, then mutate — the borrow checker wants the scan
        // finished before the assignment begins.
        let pick = ids
            .iter()
            .filter_map(|id| self.tasks.get(id))
            .find(|t| {
                if t.state != TaskState::Pending || self.shelved(t.chapter) {
                    return false;
                }
                if !self.upstream_done(t.chapter, t.stage) {
                    return false;
                }
                if let Some(only) = &t.affinity {
                    return *only == machine;
                }
                true
            })
            .map(|t| (t.id(), t.stage));
        let (id, stage) = pick?;
        {
            let t = self.tasks.get_mut(&id)?;
            t.state = TaskState::Assigned;
            t.assigned_to = Some(worker_id.into());
            t.lease_until = Some(now_secs() + lease_for(stage));
            t.updated = now_secs();
        }
        let t = self.tasks.get(&id)?;
        let offer = self.build_offer(t, &machine);
        self.save();
        Some(offer)
    }

    fn build_offer(&self, t: &Task, machine: &str) -> TaskOffer {
        let n = t.chapter;
        let tts_url = self
            .machines
            .get(machine)
            .and_then(|m| m.tts_url.clone())
            .unwrap_or_else(|| "http://127.0.0.1:8818".into());
        let bible = if t.stage == Stage::Digest {
            bm_core::read_json::<Value>(&self.layout.bible()).unwrap_or(json!({"characters": []}))
        } else {
            Value::Null
        };
        // Render/merge need the script, digest needs the text. Small files;
        // shipping them in the offer beats shared storage.
        let script = matches!(t.stage, Stage::Render | Stage::Merge)
            .then(|| bm_core::read_json::<Value>(&self.layout.script(n)).ok())
            .flatten();
        let text = (t.stage == Stage::Digest)
            .then(|| std::fs::read_to_string(self.layout.chapter_txt(n)).ok())
            .flatten();
        TaskOffer {
            task_id: t.id(),
            chapter: n,
            stage: t.stage,
            root: self.layout.root.display().to_string(),
            url: (t.stage == Stage::Crawl).then(|| self.settings.chapter_url(n)),
            tts_url: t.stage.needs_tts().then_some(tts_url),
            engine: self.settings.engine.clone(),
            model_order: self.settings.model_order.clone(),
            analyzer: self.settings.analyzer.clone(),
            bible: if bible.is_null() { None } else { Some(bible) },
            script,
            text,
            gap_ms: self.settings.gap_ms,
            speed: self.settings.speed,
            ambience: self.settings.ambience,
        }
    }

    /// Apply a worker report. Returns a human-readable line for the event log.
    pub fn complete(&mut self, c: &Complete) -> String {
        // Snapshot what the transition needs, then mutate — the borrow checker
        // wants facts first, decisions after.
        enum Outcome {
            Unknown,
            Stale,
            Done {
                chapter: u32,
                stage: Stage,
                machine: String,
                delta: Option<Value>,
                script: Option<Value>,
                text: Option<String>,
                mp3_b64: Option<String>,
            },
            Failed,
        }
        let outcome = match self.tasks.get(&c.task_id) {
            None => Outcome::Unknown,
            Some(t) if t.assigned_to.as_deref() != Some(c.worker_id.as_str()) => Outcome::Stale,
            Some(t) => {
                let machine = self.workers.get(&c.worker_id).cloned().unwrap_or_default();
                if c.ok {
                    Outcome::Done {
                        chapter: t.chapter,
                        stage: t.stage,
                        machine,
                        delta: c.bible_delta.clone(),
                        script: c.script.clone(),
                        text: c.text.clone(),
                        mp3_b64: c.mp3_b64.clone(),
                    }
                } else {
                    Outcome::Failed
                }
            }
        };
        match outcome {
            Outcome::Unknown => return format!("unknown task {}", c.task_id),
            Outcome::Stale => {
                return format!("{}: stale report for {} ignored", c.worker_id, c.task_id)
            }
            Outcome::Done { chapter, stage, machine, delta, script, text, mp3_b64 } => {
                // Artifacts first: the inductor holds every artifact so any
                // machine can run downstream stages.
                if stage == Stage::Crawl {
                    if let Some(txt) = text {
                        let _ = bm_core::atomic_write(
                            &self.layout.chapter_txt(chapter), &txt,
                        );
                    }
                    self.ensure_task(chapter, Stage::Digest);
                }
                if stage == Stage::Digest {
                    if let Some(d) = delta {
                        let path = self.layout.bible();
                        let mut bible: Value =
                            bm_core::read_json(&path).unwrap_or(json!({"characters": []}));
                        bm_core::digest::merge_bible(&mut bible, &d, &format!("{chapter:02}"));
                        let _ = bm_core::digest::save_bible(&bible, &path);
                    }
                    if let Some(mut s) = script {
                        // Canonicalize against the just-merged bible (which now
                        // includes this chapter's own newcomers): variant
                        // speakers collapse to one name before the script hits
                        // disk, so cast/render/merge never see the fork.
                        let path = self.layout.bible();
                        let bible: Value =
                            bm_core::read_json(&path).unwrap_or(json!({"characters": []}));
                        bm_core::digest::canonicalize_script(&mut s, &bible);
                        let _ = bm_core::atomic_write(
                            &self.layout.script(chapter),
                            &serde_json::to_string_pretty(&s).unwrap_or_default(),
                        );
                    }
                    self.ensure_task(chapter, Stage::Render);
                }
                if stage == Stage::Render {
                    let m = self.ensure_task(chapter, Stage::Merge);
                    m.affinity =
                        Some(if machine.is_empty() { "127.0.0.1".into() } else { machine });
                }
                if stage == Stage::Merge {
                    // A remote merge's product comes home in the report.
                    if let Some(b64) = mp3_b64 {
                        use base64::Engine;
                        if let Ok(raw) =
                            base64::engine::general_purpose::STANDARD.decode(&b64)
                        {
                            let dest = self.layout.final_mp3(chapter);
                            if let Some(parent) = dest.parent() {
                                let _ = std::fs::create_dir_all(parent);
                            }
                            let tmp = dest.with_extension("mp3.incoming");
                            if std::fs::write(&tmp, &raw).is_ok() {
                                let _ = std::fs::rename(&tmp, &dest);
                            }
                        }
                    }
                }
                if let Some(t) = self.tasks.get_mut(&c.task_id) {
                    t.state = TaskState::Done;
                    t.detail = c.detail.clone();
                    t.assigned_to = None;
                    t.lease_until = None;
                    t.updated = now_secs();
                }
                // Throughput ledger: every completion feeds the ETA model.
                // Render units are TTS calls; other stages count 1 per chapter.
                let units = if stage == Stage::Render { c.units.max(1) } else { 1 };
                let _ = bm_core::eta::record(
                    &self.layout.stats(),
                    stage,
                    units,
                    c.duration_secs,
                    &c.worker_id,
                );
                self.push_event("ok", format!(
                    "[{}] {} done in {:.1}s{}",
                    c.worker_id, c.task_id, c.duration_secs,
                    if c.detail.is_empty() { String::new() } else { format!(" — {}", bm_core::util::head_chars(&c.detail, 80)) }
                ));
                self.save();
            }
            Outcome::Failed => {
                let shelved = {
                    if let Some(t) = self.tasks.get_mut(&c.task_id) {
                        t.attempts += 1;
                        t.detail = c.detail.clone();
                        t.state = if t.attempts >= 3 { TaskState::Shelved } else { TaskState::Pending };
                        t.assigned_to = None;
                        t.lease_until = None;
                        t.updated = now_secs();
                        t.state == TaskState::Shelved
                    } else {
                        false
                    }
                };
                let level = if shelved { "error" } else { "warn" };
                let note = if shelved { " (shelved — press u to retry)" } else { " (will retry)" };
                self.push_event(level, format!(
                    "[{}] {} FAILED{}: {}",
                    c.worker_id, c.task_id, note,
                    bm_core::util::head_chars(&c.detail, 200)
                ));
                self.save();
            }
        }
        format!(
            "{}: {} {} ({})",
            c.worker_id,
            c.task_id,
            if c.ok { "done" } else { "failed" },
            bm_core::util::head_chars(&c.detail, 120)
        )
    }

    pub fn counts(&self) -> HashMap<String, HashMap<String, usize>> {
        let mut out: HashMap<String, HashMap<String, usize>> = HashMap::new();
        for t in self.tasks.values() {
            let e = out.entry(t.stage.as_str().into()).or_default();
            let k = format!("{:?}", t.state).to_lowercase();
            *e.entry(k).or_default() += 1;
        }
        out
    }

    /// The cast exactly as the cast file holds it.
    pub fn cast_snapshot(&self) -> std::collections::BTreeMap<String, String> {
        bm_core::cast::read_cast(&self.settings.engine, &self.layout.cast(&self.settings.engine))
    }

    /// Every speaker the inductor can name: the operator's cast, the cast file,
    /// the bible, and every script's roster and segments.
    ///
    /// This is the voice picker's first step — without it the operator has to
    /// recall exact Vietnamese character names from memory. The shipped
    /// catalogue carries no character names, so the seed is the operator's own
    /// roster; a malformed one seeds nothing, which is a missing convenience
    /// rather than a broken gate.
    pub fn known_characters(&self) -> Vec<String> {
        use std::collections::BTreeSet;
        let engine = self.settings.engine.clone();
        let mut set: BTreeSet<String> = BTreeSet::new();
        let (effective, _) =
            bm_core::voices::effective_engine_lenient(&self.layout.roster(), &engine);
        for (name, _) in &effective.to_policy(&engine).default_cast {
            set.insert(name.clone());
        }
        for name in self.cast_snapshot().keys() {
            set.insert(name.clone());
        }
        let bible = bm_core::digest::load_bible(&self.layout.bible());
        if let Some(chars) = bible.get("characters").and_then(|c| c.as_array()) {
            for c in chars {
                if let Some(n) = c.get("name").and_then(|n| n.as_str()) {
                    if !n.is_empty() {
                        set.insert(n.to_string());
                    }
                }
            }
        }
        let mut scripts: Vec<std::path::PathBuf> = std::fs::read_dir(self.layout.data())
            .map(|rd| {
                rd.filter_map(|e| e.ok().map(|x| x.path()))
                    .filter(|p| {
                        p.file_name()
                            .and_then(|n| n.to_str())
                            .map(|n| n.starts_with("script-") && n.ends_with(".json"))
                            .unwrap_or(false)
                    })
                    .collect()
            })
            .unwrap_or_default();
        scripts.sort();
        for sp in scripts {
            let Ok(data) = bm_core::read_json::<Value>(&sp) else {
                continue;
            };
            if let Some(roster) = data.get("roster").and_then(|r| r.as_array()) {
                for n in roster.iter().filter_map(|v| v.as_str()) {
                    if !n.is_empty() {
                        set.insert(n.to_string());
                    }
                }
            }
            if let Some(segs) = data.get("segments").and_then(|s| s.as_array()) {
                for s in segs {
                    if let Some(sp) = s.get("speaker").and_then(|v| v.as_str()) {
                        if !sp.is_empty() {
                            set.insert(sp.to_string());
                        }
                    }
                }
            }
        }
        // `Narrator` is the one speaker that always exists; it leads the list.
        set.remove("Narrator");
        let mut out: Vec<String> = std::iter::once("Narrator".to_string()).collect();
        out.extend(set);
        out
    }
}
