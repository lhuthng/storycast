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
                         "machines": self.machines.values().collect::<Vec<_>>(),
                         "workers": self.workers});
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
        // Worker identity survives restarts: without it, completions filed
        // while the map is cold get attributed to the wrong machine (and
        // merge affinity strands tasks on machines that never rendered).
        if let Some(w) = doc.get("workers").and_then(|w| w.as_object()) {
            for (k, v) in w {
                if let Some(addr) = v.as_str() {
                    self.workers.insert(k.clone(), addr.to_string());
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
                    // Locally complete segments merge locally (remote renders
                    // set their own affinity when they report).
                    if stage == Stage::Render && t.affinity.is_none() {
                        t.affinity = Some("127.0.0.1".into());
                    }
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
    pub fn op_swap_voice(&mut self, character: &str, voice: &str) -> anyhow::Result<String> {
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
        let admitted = if policy.allowed.is_empty() {
            true
        } else if declared {
            policy.allowed.iter().any(|a| a == &voice)
        } else {
            in_use
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
        let mut chapters: Vec<u32> = Vec::new();
        let mut files = 0u32;
        let mut scripts: Vec<std::path::PathBuf> = std::fs::read_dir(self.layout.data())?
            .filter_map(|e| e.ok().map(|x| x.path()))
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("script-") && n.ends_with(".json"))
                    .unwrap_or(false)
            })
            .collect();
        scripts.sort();
        for sp in scripts {
            let n: u32 = sp
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.strip_prefix("script-"))
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let data: Value = bm_core::read_json(&sp).unwrap_or(Value::Null);
            let segments = data.get("segments").and_then(|s| s.as_array()).cloned().unwrap_or_default();
            let planned = bm_core::assemble::drop_headline(&segments);
            let seg_dir = self.layout.seg_dir(&engine, n);
            let local = engine == "vieneu";
            let mut touched = false;
            for run in bm_core::assemble::runs(planned) {
                if run.speaker != character {
                    continue;
                }
                // Filenames embed the OLD voice — exactly the stale set:
                // run tags locally, per-line files on the cloud path.
                let names: Vec<String> = if local {
                    let (a, b) = (run.idx[0], run.idx[run.idx.len() - 1]);
                    let tag = if a == b { format!("{a:04}") } else { format!("{a:04}-{b:04}") };
                    vec![format!("{tag}_{old}.wav")]
                } else {
                    run.idx.iter().map(|i| format!("{i:04}_{old}.wav")).collect()
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
            if touched {
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

#[cfg(test)]
mod tests {
    use super::*;
    use bm_core::config::Settings;

    fn fixture() -> (tempfile::TempDir, Inner) {
        let d = tempfile::tempdir().unwrap();
        let layout = Layout::new(d.path());
        std::fs::create_dir_all(layout.data()).unwrap();
        std::fs::create_dir_all(layout.output()).unwrap();
        std::fs::write(layout.bible(), r#"{"characters":[]}"#).unwrap();
        let inner = Inner::new(layout, Settings::default());
        (d, inner)
    }

    #[test]
    fn swap_invalidates_only_the_characters_files() {
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::write(
            layout.script(1),
            r#"{"segments":[{"speaker":"A","text":"x"},{"speaker":"A","text":"y"},{"speaker":"B","text":"z"}]}"#,
        )
        .unwrap();
        std::fs::write(layout.cast("vieneu"), r#"{"A":"Đức Trí","B":"Adam","Narrator":"Đức Trí"}"#).unwrap();
        let seg = layout.seg_dir("vieneu", 1);
        std::fs::create_dir_all(&seg).unwrap();
        std::fs::write(seg.join("0000-0001_Đức Trí.wav"), vec![0u8; 2000]).unwrap();
        std::fs::write(seg.join("0002_Adam.wav"), vec![0u8; 2000]).unwrap();
        std::fs::write(layout.final_mp3(1), vec![0u8; 2000]).unwrap();

        let msg = inner.op_swap_voice("A", "Minh Triết").unwrap();
        assert!(msg.contains("Đức Trí -> Minh Triết"), "{msg}");
        assert!(!seg.join("0000-0001_Đức Trí.wav").exists(), "stale run file must go");
        assert!(seg.join("0002_Adam.wav").exists(), "other voices keep cache");
        assert!(!layout.final_mp3(1).exists(), "stale product goes away");
        assert_eq!(inner.tasks["render:1"].state, TaskState::Pending);
        assert_eq!(inner.tasks["merge:1"].state, TaskState::Pending);
        // The swap's *meaning* is checked through the reader (which resolves
        // keys back to names), and the stored form is checked directly — the
        // file persists keys now, so both assertions matter.
        let cast = bm_core::cast::read_cast("vieneu", &layout.cast("vieneu"));
        assert_eq!(cast["A"], "Minh Triết");
        assert_eq!(cast["B"], "Adam", "untouched speakers survive");
        let disk: std::collections::HashMap<String, String> =
            bm_core::read_json(&layout.cast("vieneu")).unwrap();
        assert_eq!(disk["A"], "minh-triet", "persisted as a key, not a name");
        assert_eq!(disk["B"], "adam");
    }

    #[test]
    fn swap_admits_anything_until_the_operator_narrows_it() {
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::create_dir_all(layout.bm_state()).unwrap();
        std::fs::write(layout.cast("vieneu"), r#"{"A":"Đức Trí"}"#).unwrap();

        // No local roster, so the shipped catalogue applies — and it restricts
        // nothing. A Northern preset that the old hardcoded Central/South policy
        // refused is assignable, which is the whole point of the split.
        assert!(inner.op_swap_voice("A", "Minh Đức").is_ok());

        // Narrow it the way an operator would, and the same voice is refused.
        std::fs::write(
            layout.roster(),
            r#"{"version":1,"engines":{"vieneu":{"policy":{"excluded_accents":["Northern"]}}}}"#,
        )
        .unwrap();
        let err = inner.op_swap_voice("A", "Minh Đức").unwrap_err().to_string();
        assert!(err.contains("neither an admitted preset"), "{err}");

        // An admitted voice still swaps.
        let msg = inner.op_swap_voice("A", "Quang Sơn").unwrap();
        assert!(msg.contains("->"), "{msg}");

        // A malformed roster is refused outright rather than silently ignored:
        // falling back to the catalogue would re-admit every excluded voice.
        std::fs::write(layout.roster(), "{ this is not json").unwrap();
        let err = inner.op_swap_voice("A", "Đức Trí").unwrap_err().to_string();
        assert!(err.contains("parsing"), "{err}");
    }

    #[test]
    fn eta_reports_a_total_even_with_no_measurements() {
        let (_d, inner) = fixture();
        let msg = inner.op_eta(1, 10);
        assert!(msg.contains("total"), "{msg}");
        assert!(msg.contains("(guess)"), "{msg}");
    }
}
