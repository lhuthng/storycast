use super::{lease_for, Inner};
use bm_proto::{now_secs, Complete, RenderUnitSpec, Stage, Task, TaskOffer, TaskState};
use serde_json::{json, Value};
use std::collections::HashMap;

impl Inner {
    /// The task this worker should do next.
    ///
    /// The box's own **policy** decides which stages it will run and in what
    /// order (most-preferred first): the scheduler walks the enabled stages and
    /// takes the oldest chapter of the first stage that has assignable work.
    /// Merge affinity is an optimization for remote boxes, never a gate for
    /// the local node: it shares the inductor's segment store — units are
    /// collected before a render completion is applied, so a `done` render's
    /// wavs are always on local disk — and takes any pending merge. A surgical
    /// re-render pins the same way to the box that already holds the chapter,
    /// so a cold box never re-speaks the whole chapter for a voice swap; the
    /// local node takes those too. Without
    /// this a render on a dead, ffmpeg-less, or merge-disabled box strands
    /// its merge pending for ever. A machine with no stored policy gets the
    /// default (all four, merge → render
    /// → digest → crawl). Capability gates still apply within a stage — render
    /// needs `render-segments`, merge needs `merge` (absent when the box has no
    /// ffmpeg) — and a merge's affinity still pins it to the box that rendered
    /// the chapter.
    ///
    /// A merge is additionally offered only when its segments are on this
    /// disk (`missing_wavs` empty): the ledger's `render:Done` is a claim
    /// about files, and files deleted or never collected since make it a lie
    /// that every box would fail the same way. A starved merge heals its
    /// render (see `heal_render_for_merge`) and yields to the next stage,
    /// so one bad chapter never idles the worker.
    pub fn offer(&mut self, worker_id: &str) -> Option<TaskOffer> {
        self.reap();
        let machine = self.workers.get(worker_id).cloned().unwrap_or_default();
        // The readiness gate, and it is deliberately the *first* thing after
        // the worker lookup: a task handed to a box that is booting, being
        // pushed to, or known-broken is a task that fails slowly and strikes
        // the chapter for the inductor's mistake.
        //
        // `Unknown` passes, as "no opinion formed": a hand-written ledger, or
        // the legacy pull worker asking before its first beat. Both were
        // offered work before this gate existed, and a live worker asking for
        // work is itself evidence the box is up — second-guessing that is not
        // this gate's job. Every *other* state is one the inductor chose.
        if let Some(m) = self.machines.get(&machine) {
            if !m.state.accepts_work() && m.state != bm_proto::MachineState::Unknown {
                return None;
            }
        }
        // The box's order + enablement. An unknown worker id (a hand-written
        // ledger) runs the default policy, exactly as it always did.
        let policy = self
            .machines
            .get(&machine)
            .map(|m| m.effective_task_policy())
            .unwrap_or_else(bm_proto::TaskPref::default_list);
        let caps = self.caps.get(worker_id).cloned();
        // Pick first, then mutate — the borrow checker wants the scan finished
        // before the assignment begins. Walk the policy in order and take the
        // oldest chapter of the first enabled stage that has assignable work.
        let mut pick: Option<(String, Stage)> = None;
        for pref in policy.iter().filter(|p| p.enabled) {
            let stage = pref.stage;
            // Capability gates. Render uploads units; merge shells out to
            // ffmpeg and a box without it advertises no `merge`. A worker with
            // no recorded capabilities is allowed — failing closed here would
            // strand anything that never registered.
            if let Some(caps) = &caps {
                let can = |cap: &str| caps.iter().any(|c| c == cap);
                if (stage == Stage::Render && !can("render-segments"))
                    || (stage == Stage::Merge && !can("merge"))
                {
                    continue;
                }
            }
            let mut ids: Vec<String> = self
                .tasks
                .iter()
                .filter(|(_, t)| t.stage == stage)
                .filter(|(_, t)| t.state == TaskState::Pending && !self.shelved(t.chapter))
                .filter(|(_, t)| self.upstream_done(t.chapter, t.stage))
                .filter(|(_, t)| match &t.affinity {
                    Some(only) => {
                        *only == machine
                            || ((t.stage == Stage::Merge || t.stage == Stage::Render)
                                && bm_core::is_local_node(&machine))
                    }
                    None => true,
                })
                .map(|(id, _)| id.clone())
                .collect();
            // Numeric chapter order — lexical sort would put ch100 before ch93.
            ids.sort_by_key(|id| {
                id.split_once(':')
                    .and_then(|(_, ch)| ch.parse::<u32>().ok())
                    .unwrap_or(0)
            });
            if stage == Stage::Merge {
                // The data check delivery was missing: offer the oldest merge
                // whose segments are actually here, and heal the render of
                // every starved one skipped along the way. A chapter this
                // disk cannot plan (`None` — no script, uncast speaker) is
                // offered as before: there is nothing local to prove it
                // starved, and its failure names the real cause.
                let mut ready: Option<String> = None;
                for id in &ids {
                    let chapter = id
                        .split_once(':')
                        .and_then(|(_, ch)| ch.parse::<u32>().ok())
                        .unwrap_or(0);
                    match crate::segments::missing_wavs(
                        &self.layout,
                        &self.settings.engine,
                        chapter,
                    ) {
                        Some(missing) if !missing.is_empty() => {
                            self.heal_render_for_merge(chapter);
                        }
                        _ => {
                            ready = Some(id.clone());
                            break;
                        }
                    }
                }
                if let Some(id) = ready {
                    pick = Some((id, stage));
                    break;
                }
                continue;
            }
            if let Some(id) = ids.first() {
                pick = Some((id.clone(), stage));
                break;
            }
        }
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
        // Digest merges the delta into it; merge hands it to `assemble`.
        let bible = if matches!(t.stage, Stage::Digest | Stage::Merge) {
            bm_core::read_json::<Value>(&self.layout.bible()).unwrap_or(json!({"characters": []}))
        } else {
            Value::Null
        };
        // Render/merge need the script, digest needs the text. Small files;
        // shipping them in the offer beats shared storage.
        let script = matches!(t.stage, Stage::Render | Stage::Merge)
            .then(|| bm_core::read_json::<Value>(&self.layout.script(n)).ok())
            .flatten();
        // The cast decides segment *filenames*. A worker that had to recompute
        // it would plan different names than the render wrote — and a
        // provisioned worker cannot recompute it at all, because `data/` is
        // not part of what provisioning copies.
        let cast = matches!(t.stage, Stage::Render | Stage::Merge)
            .then(|| bm_core::read_json::<Value>(&self.layout.cast(&self.settings.engine)).ok())
            .flatten();
        let text = (t.stage == Stage::Digest)
            .then(|| std::fs::read_to_string(self.layout.chapter_txt(n)).ok())
            .flatten();
        let render_units = if t.stage == Stage::Render {
            self.planned_units(n)
        } else {
            None
        };
        // ponytail: offer-time diff against this store, not a persisted set.
        let render_force: Vec<String> = render_units
            .as_deref()
            .map(|units| {
                let seg_dir = self.layout.seg_dir(&self.settings.engine, n);
                units
                    .iter()
                    .filter(|u| {
                        !seg_dir
                            .join(&u.name)
                            .metadata()
                            .map(|m| m.len() > 1000)
                            .unwrap_or(false)
                    })
                    .map(|u| u.name.clone())
                    .collect()
            })
            .unwrap_or_default();
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
            cast,
            text,
            gap_ms: self.settings.gap_ms,
            speed: self.settings.speed,
            ambience: self.settings.ambience,
            music: self.settings.music,
            effect_volume: self.settings.effect_volume,
            music_volume: self.settings.music_volume,
            inject_volume: self.settings.inject_volume,
            // The inductor plans; the worker speaks. The whole chapter's
            // units, so the worker can skip what it already holds — see
            // `planned_units`. `None` when this chapter cannot be planned
            // here — the worker falls back to its own script, exactly as
            // before the migration. `render_force` names the units this
            // store lacks (the surgical set a swap/retag just deleted, or
            // everything after a full invalidation): the worker must speak
            // those even when its own disk holds a same-named file, or it
            // keeps serving stale bytes under the new text.
            render_units,
            render_force,
            // The inductor decides locality; the worker never guesses from
            // paths. Same predicate the provisioner uses for `Ssh.local`.
            local_node: bm_core::is_local_node(machine),
        }
    }

    /// **Every** unit `chapter` needs, for a render offer. `None` when the
    /// chapter cannot be planned here (missing script, unparseable JSON,
    /// uncast speaker).
    ///
    /// The whole set, deliberately — not the difference against this store.
    /// That difference reads as an optimisation and is a bug: the offer goes
    /// to whichever box asks next, that box has a store of its own, and the
    /// inductor cannot see it. So a partial offer lands on a box holding a
    /// strict subset — a voice swap's new units on a box that never had the
    /// chapter's others — and the merge that follows, pinned by `affinity` to
    /// that same box, fails on `N segments missing` for a chapter the cluster
    /// has rendered in full. Sending everything and letting the worker skip
    /// what it holds (`bm-agent`'s `pending_units`) means any box that finishes
    /// a render holds the whole chapter, which is what makes `affinity` true
    /// rather than merely intended.
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
    pub(crate) fn planned_units(&self, chapter: u32) -> Option<Vec<RenderUnitSpec>> {
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
        let planned = bm_core::assemble::Planned::plan(segments);
        let units =
            bm_core::assemble::plan_render(&planned, &cast, &seg_dir, local, title.as_ref())
                .ok()?;
        Some(
            units
                .into_iter()
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
                    // **Merge runs where the segments are**, and after an
                    // inverted render that is the box that just wrote them: a
                    // remote one cannot be handed thirty-odd wavs inside an
                    // offer, and the inductor's own copy arrived over
                    // `GET /unit` for the completion gate to read.
                    //
                    // This used to be the literal `127.0.0.1`. That read as
                    // "the local node" but meant "the only machine whose disk
                    // the inductor can read" — true on one box, and the reason
                    // a cluster of remote workers rendered for ever without
                    // ever merging.
                    //
                    // `assigned_to` is still set here: the block below clears
                    // it, and `workers` is what turns a worker id into a
                    // machine.
                    let rendered_by = self
                        .tasks
                        .get(&c.task_id)
                        .and_then(|t| t.assigned_to.as_deref())
                        .and_then(|w| self.workers.get(w).cloned());
                    let m = self.ensure_task(chapter, Stage::Merge);
                    // No known renderer — a hand-written ledger, a report from
                    // a worker that never registered. The local node shares the
                    // inductor's store, which is what this always was.
                    m.affinity = rendered_by.or_else(|| Some("127.0.0.1".into()));
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
                // The design this mp3 was mixed under, read *before* the borrow
                // below: the stamp needs `self.settings` and the registries,
                // and it has to be written under the same borrow as the state.
                // A merge that failed never gets here, which is right — it left
                // no artifact to make a claim about.
                let design = if stage == Stage::Merge {
                    let d = bm_core::design::MergeDesign::load(&self.layout);
                    self.design_stamp(&d, self.design_knobs(), chapter)
                } else {
                    None
                };
                if let Some(t) = self.tasks.get_mut(&c.task_id) {
                    t.state = TaskState::Done;
                    t.detail = c.detail.clone();
                    t.assigned_to = None;
                    t.lease_until = None;
                    t.updated = now_secs();
                    if let Some(d) = design {
                        t.design = Some(d);
                    }
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
                // Same completion feeds the Stats pane: per-worker counts
                // plus the durations the TUI-side ETA averages.
                self.stats.record(&c.worker_id, stage, c.duration_secs);
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
        // A completion may have drained the queue — trip the armed latch.
        self.maybe_auto_shutdown();
        format!(
            "{}: {} {} ({})",
            c.worker_id,
            c.task_id,
            if c.ok { "done" } else { "failed" },
            bm_core::util::head_chars(&c.detail, 120)
        )
    }

    /// A merge failed on `N segments missing`, or an offer-time check proved
    /// its segments are not on this disk: the ledger's `render:Done` is a
    /// claim about files that is no longer true (a wiped box, a surgically
    /// deleted stale file, units never collected). Flip that `Done` render
    /// back to `Pending` so the chapter re-renders instead of failing the
    /// same merge into shelved.
    ///
    /// Only `Done` moves: anything else is already queued, in flight, or
    /// parked for an operator. A published mp3 vetoes the flip — then the
    /// segments are provenance (TTS does not reproduce), not cache. Nothing
    /// is deleted: the next render fills gaps (`pending_units` skips what
    /// the box holds) rather than starting over.
    fn heal_render_for_merge(&mut self, chapter: u32) -> bool {
        if self.layout.final_mp3(chapter).is_file() {
            return false;
        }
        match self.tasks.get_mut(&format!("{}:{chapter}", Stage::Render)) {
            Some(t) if t.state == TaskState::Done => {
                t.state = TaskState::Pending;
                t.attempts = 0;
                t.assigned_to = None;
                t.lease_until = None;
                t.detail = "requeued: merge found segments missing".into();
                t.updated = now_secs();
                true
            }
            _ => false,
        }
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
        // A merge fails where its segments are — a per-box store the ledger
        // cannot see, so the offer guard above only catches it when *this*
        // disk is the short one. A remote that was wiped (or never rendered
        // the chapter) fails the same way with a healthy disk here: requeue
        // the render alongside the merge retry, or the same merge fails twice
        // more into shelved and waits for a manual force. The merge keeps its
        // strike — a render that cannot close the gap still shelves.
        if let Some(rest) = task_id.strip_prefix("merge:") {
            if detail.contains("segments missing") {
                if let Ok(chapter) = rest.parse::<u32>() {
                    self.heal_render_for_merge(chapter);
                }
            }
        }
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
        // A shelving may have drained the queue — trip the armed latch.
        self.maybe_auto_shutdown();
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
