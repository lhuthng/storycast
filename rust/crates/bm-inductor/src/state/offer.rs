use super::{lease_for, Inner};
use bm_proto::{now_secs, Complete, RenderUnitSpec, Stage, Task, TaskOffer, TaskState};
use serde_json::{json, Value};
use std::collections::HashMap;

impl Inner {
    /// Oldest assignable task for this worker. Merge affinity pins merges to
    /// the local node; render goes to any worker that uploads units.
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
                // Render uploads units; a worker that registered without
                // `render-segments` keeps every other stage but never renders
                // (its report would fail the completion gate anyway). Workers
                // with no recorded capabilities are allowed — failing closed
                // here would strand anything that never registered.
                if t.stage == Stage::Render {
                    if let Some(caps) = self.caps.get(worker_id) {
                        if !caps.iter().any(|c| c == "render-segments") {
                            return false;
                        }
                    }
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
            // The analyzer's *backend* travels in `analyzer` above; what that
            // backend runs travels here. Both are needed: a provisioned worker
            // has no `.bm/settings.json` to read (provisioning never copies
            // `.bm/`), so without this it digests with the compiled-in
            // `Settings::default()` — which named a model the operator had
            // stopped using.
            analyzer_settings: self.settings.analyzer_settings(),
            // The inductor is the only machine whose `.env` the operator
            // maintains: a provisioned worker has none, because `.env` is
            // personal and git-ignored and `install_sources` copies only
            // `prompts/`, `python/`, `assets/` and `refs/`. Shipping the key
            // with the task is what makes a remote digest possible at all —
            // narrowed to what this stage actually reads, so a crawl offer
            // carries no secret.
            credentials: bm_proto::Credentials::from_env().for_stage(
                t.stage,
                &self.settings.analyzer,
                &self.settings.engine,
            ),
            bible: if bible.is_null() { None } else { Some(bible) },
            script,
            text,
            gap_ms: self.settings.gap_ms,
            speed: self.settings.speed,
            ambience: self.settings.ambience,
            // The inductor plans; the worker speaks. `None` when this chapter
            // cannot be planned here — the worker falls back to its own
            // script, exactly as before the migration.
            render_units: if t.stage == Stage::Render {
                self.missing_units(n)
            } else {
                None
            },
            // The inductor decides locality; the worker never guesses from
            // paths. Same predicate the provisioner uses for `Ssh.local`.
            local_node: bm_core::is_local_node(machine),
        }
    }

    /// Units of `chapter` the inductor's own store lacks, for a render offer.
    /// `None` when the chapter cannot be planned here (missing script,
    /// unparseable JSON, uncast speaker).
    ///
    /// This **persists** the cast (`save = true`), unlike every read-only
    /// prover. The units it returns are the filenames the worker will write,
    /// and a filename embeds the voice — so a decision that is not written
    /// down is recomputed later by the completion gate and the merger from
    /// whatever the cast file happens to say then. The assignment is
    /// least-used over the whole file, so *any* other chapter's write moves it:
    /// the worker's 32 files land, the gate recomputes 20 different names, and
    /// the chapter reports `incomplete` and `20 segments missing` while the
    /// audio sits on disk. Planning is the moment the voices are decided; this
    /// is where they are frozen.
    pub(crate) fn missing_units(&self, chapter: u32) -> Option<Vec<RenderUnitSpec>> {
        let engine = self.settings.engine.clone();
        let script_path = self.layout.script(chapter);
        let text = std::fs::read_to_string(&script_path).ok()?;
        let data: Value = serde_json::from_str(&text).ok()?;
        let segments = data.get("segments")?.as_array()?;
        let policy = bm_core::cast::policy_for_bible(&engine, &self.layout.bible()).ok()?;
        let cast = bm_core::cast::load_cast(
            &script_path,
            &self.layout.cast(&engine),
            &self.layout.bible(),
            &policy,
            true,
        )
        .ok()?;
        let local = engine == "vieneu";
        let title = bm_core::assemble::title_speech_for_script(&script_path, &cast, segments);
        let seg_dir = self.layout.seg_dir(&engine, chapter);
        let units =
            bm_core::assemble::plan_render(segments, &cast, &seg_dir, local, title.as_ref())
                .ok()?;
        Some(
            units
                .into_iter()
                .filter(|u| {
                    // Same completeness test the agent's resume check uses: a
                    // present, non-trivial file is done.
                    !(u.dest.exists() && u.dest.metadata().map(|m| m.len() > 1000).unwrap_or(false))
                })
                .map(|u| RenderUnitSpec {
                    tag: u.tag,
                    name: u
                        .dest
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_string(),
                    speaker: u.speaker,
                    voice: u.voice,
                    text: u.text,
                    temperature: u.temperature,
                    silence_p: u.silence_p,
                })
                .collect(),
        )
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
                if c.ok {
                    Outcome::Done {
                        chapter: t.chapter,
                        stage: t.stage,
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
            Outcome::Done {
                chapter,
                stage,
                delta,
                script,
                text,
                mp3_b64,
            } => {
                // Completion gate (render only): the worker's word is not
                // evidence — the files are. A report whose units never landed
                // is a failure whose detail names them, so the next
                // missing-only offer repeats exactly those.
                if stage == Stage::Render {
                    let missing =
                        crate::segments::missing_wavs(&self.layout, &self.settings.engine, chapter);
                    let bad = match &missing {
                        None => Some(format!("render ch{chapter} unverifiable here")),
                        Some(m) if !m.is_empty() => Some(format!(
                            "render ch{chapter} incomplete, missing: {}",
                            m.iter().take(8).cloned().collect::<Vec<_>>().join(", ")
                        )),
                        _ => None,
                    };
                    if let Some(detail) = bad {
                        return self.fail_task(&c.task_id, &c.worker_id, detail);
                    }
                }
                // Artifacts first: the inductor holds every artifact so any
                // machine can run downstream stages.
                if stage == Stage::Crawl {
                    if let Some(txt) = text {
                        let _ = bm_core::atomic_write(&self.layout.chapter_txt(chapter), &txt);
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
                        // A changed script invalidates everything downstream:
                        // run boundaries (hence segment filenames) and voices
                        // come from it, so a kept render would speak the old
                        // dramatization under the new one. An identical script
                        // invalidates nothing (duplicate reports are free).
                        let old: Option<Value> =
                            bm_core::read_json(&self.layout.script(chapter)).ok();
                        let changed = old.as_ref() != Some(&s);
                        let _ = bm_core::atomic_write(
                            &self.layout.script(chapter),
                            &serde_json::to_string_pretty(&s).unwrap_or_default(),
                        );
                        if changed {
                            self.invalidate_render(chapter);
                        }
                    }
                    self.ensure_task(chapter, Stage::Render);
                }
                if stage == Stage::Render {
                    // Merge is pinned to the local node: the segments now live
                    // on the inductor, so the merge runs where they are. The
                    // reporting machine no longer matters.
                    let m = self.ensure_task(chapter, Stage::Merge);
                    m.affinity = Some("127.0.0.1".into());
                }
                if stage == Stage::Merge {
                    // A remote merge's product comes home in the report; a
                    // local node's `publish()` already renamed it into place,
                    // so the file itself is the evidence. A report with
                    // neither is a failure, not a silent Done.
                    if let Some(b64) = mp3_b64 {
                        use base64::Engine;
                        if let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(&b64) {
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
                    if !self.layout.final_mp3(chapter).is_file() {
                        return self.fail_task(
                            &c.task_id,
                            &c.worker_id,
                            format!("merge ch{chapter} reported done but no file"),
                        );
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
                let units = if stage == Stage::Render {
                    c.units.max(1)
                } else {
                    1
                };
                let _ = bm_core::eta::record(
                    &self.layout.stats(),
                    stage,
                    units,
                    c.duration_secs,
                    &c.worker_id,
                );
                self.push_event(
                    "ok",
                    format!(
                        "[{}] {} done in {:.1}s{}",
                        c.worker_id,
                        c.task_id,
                        c.duration_secs,
                        if c.detail.is_empty() {
                            String::new()
                        } else {
                            format!(" — {}", bm_core::util::head_chars(&c.detail, 80))
                        }
                    ),
                );
                self.save();
            }
            Outcome::Failed => {
                return self.fail_task(&c.task_id, &c.worker_id, c.detail.clone());
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

    /// Record a failed report: a strike, Pending again (Shelved at 3), and an
    /// event line. Shared by worker-reported failures and the completion
    /// gate, which fails reports whose files never landed.
    fn fail_task(&mut self, task_id: &str, worker_id: &str, detail: String) -> String {
        let shelved = {
            if let Some(t) = self.tasks.get_mut(task_id) {
                t.attempts += 1;
                t.detail = detail.clone();
                t.state = if t.attempts >= 3 {
                    TaskState::Shelved
                } else {
                    TaskState::Pending
                };
                t.assigned_to = None;
                t.lease_until = None;
                t.updated = now_secs();
                t.state == TaskState::Shelved
            } else {
                false
            }
        };
        let level = if shelved { "error" } else { "warn" };
        let note = if shelved {
            " (shelved — press u to retry)"
        } else {
            " (will retry)"
        };
        self.push_event(
            level,
            format!(
                "[{worker_id}] {task_id} FAILED{note}: {}",
                bm_core::util::head_chars(&detail, 200)
            ),
        );
        self.save();
        format!(
            "{worker_id}: {task_id} failed ({})",
            bm_core::util::head_chars(&detail, 120)
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
        bm_core::cast::read_cast(
            &self.settings.engine,
            &self.layout.cast(&self.settings.engine),
        )
        .into_map()
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
