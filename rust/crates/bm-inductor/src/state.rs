//! Scheduler state: the ledger the orchestrator owns.
//!
//! Workers report facts; every transition below is a decision. The ledger is
//! persisted on each mutation, and reconciled from artifacts on startup, so a
//! restart resumes instead of restarting.

use anyhow::Result;
use bm_core::{Layout, config::Settings};
use bm_proto::{now_secs, Complete, Machine, MachineState, Stage, Task, TaskOffer, TaskState};
use serde_json::{json, Value};
use std::collections::HashMap;

const LEASE_SECS: [(Stage, u64); 4] = [
    (Stage::Crawl, 600),
    (Stage::Digest, 1200),
    (Stage::Render, 5400),
    (Stage::Merge, 1800),
];

fn lease_for(stage: Stage) -> u64 {
    LEASE_SECS.iter().find(|(s, _)| *s == stage).map(|(_, l)| *l).unwrap_or(600)
}

pub struct Inner {
    pub layout: Layout,
    pub settings: Settings,
    pub tasks: HashMap<String, Task>,
    pub machines: HashMap<String, Machine>,
    pub workers: HashMap<String, String>,
    pub beats: HashMap<String, bm_proto::Heartbeat>,
}

impl Inner {
    pub fn new(layout: Layout, settings: Settings) -> Self {
        Inner {
            layout,
            settings,
            tasks: HashMap::new(),
            machines: HashMap::new(),
            workers: HashMap::new(),
            beats: HashMap::new(),
        }
    }

    fn ledger_path(&self) -> std::path::PathBuf {
        self.layout.bm_state().join("ledger.json")
    }

    pub fn save(&self) {
        let doc = json!({"tasks": self.tasks.values().collect::<Vec<_>>(),
                         "machines": self.machines.values().collect::<Vec<_>>()});
        let _ = bm_core::write_json(&self.ledger_path(), &doc);
    }

    pub fn load_ledger(&mut self) {
        let Ok(doc): Result<Value, _> =
            bm_core::read_json(&self.ledger_path()).map_err(|_| anyhow::anyhow!("none"))
        else {
            return;
        };
        if let Some(tasks) = doc.get("tasks").and_then(|t| t.as_array()) {
            for t in tasks {
                if let Ok(task) = serde_json::from_value::<Task>(t.clone()) {
                    self.tasks.insert(task.id(), task);
                }
            }
        }
        if let Some(ms) = doc.get("machines").and_then(|m| m.as_array()) {
            for m in ms {
                if let Ok(mac) = serde_json::from_value::<Machine>(m.clone()) {
                    self.machines.insert(mac.addr.clone(), mac);
                }
            }
        }
    }

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
            if segs_done {
                let t = self
                    .tasks
                    .entry(format!("render:{n}"))
                    .or_insert_with(|| Task::new(n, Stage::Render));
                if t.state == TaskState::Pending {
                    t.state = TaskState::Done;
                    t.updated = now_secs();
                    t.affinity = Some("127.0.0.1".into());
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
                            if stage == Stage::Render && t.affinity.is_none() {
                                t.affinity = Some("127.0.0.1".into());
                            }
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

    fn upstream_done(&self, chapter: u32, stage: Stage) -> bool {
        stage.upstream().iter().all(|u| {
            self.tasks
                .get(&format!("{u}:{chapter}"))
                .map(|t| t.state == TaskState::Done)
                .unwrap_or(false)
        })
    }

    fn shelved(&self, chapter: u32) -> bool {
        Stage::ALL.iter().any(|s| {
            self.tasks
                .get(&format!("{s}:{chapter}"))
                .map(|t| t.state == TaskState::Shelved)
                .unwrap_or(false)
        })
    }

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
                    if let Some(s) = script {
                        let _ = bm_core::atomic_write(
                            &self.layout.script(chapter),
                            &serde_json::to_string_pretty(&s).unwrap_or_default(),
                        );
                    }
                    if let Some(d) = delta {
                        let path = self.layout.bible();
                        let mut bible: Value =
                            bm_core::read_json(&path).unwrap_or(json!({"characters": []}));
                        bm_core::digest::merge_bible(&mut bible, &d, &format!("{chapter:02}"));
                        let _ = bm_core::digest::save_bible(&bible, &path);
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
                self.save();
            }
            Outcome::Failed => {
                if let Some(t) = self.tasks.get_mut(&c.task_id) {
                    t.attempts += 1;
                    t.detail = c.detail.clone();
                    t.state = if t.attempts >= 3 { TaskState::Shelved } else { TaskState::Pending };
                    t.assigned_to = None;
                    t.lease_until = None;
                    t.updated = now_secs();
                }
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

    fn ensure_task(&mut self, chapter: u32, stage: Stage) -> &mut Task {
        self.tasks
            .entry(format!("{stage}:{chapter}"))
            .or_insert_with(|| Task::new(chapter, stage))
    }

    /// Expired leases return to the pool with no strike. Returns their ids.
    pub fn reap(&mut self) -> Vec<String> {
        let now = now_secs();
        let mut out = Vec::new();
        for t in self.tasks.values_mut() {
            if matches!(t.state, TaskState::Assigned | TaskState::Running)
                && t.lease_until.map(|l| l < now).unwrap_or(false)
            {
                t.state = TaskState::Pending;
                t.assigned_to = None;
                t.lease_until = None;
                t.updated = now;
                out.push(t.id());
            }
        }
        if !out.is_empty() {
            self.save();
        }
        out
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
                let t = self.ensure_task(n, Stage::Digest);
                if t.state == TaskState::Pending {
                    digests += 1;
                }
            }
        }
        self.save();
        (crawls, digests)
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
}
