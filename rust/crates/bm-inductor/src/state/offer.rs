use super::plan::present;
use super::{lease_for_batch, Inner};
use bm_core::assemble::RenderPlan;
use bm_proto::{now_secs, Complete, Stage, Task, TaskOffer, TaskState};
use serde_json::{json, Value};
use std::collections::HashMap;

/// RAM percent above which a box is offered nothing.
///
/// **A guardrail, not a capacity model.** One sidecar is ~36% of an 8 GiB box,
/// so a healthy busy worker sits near half; a box over this line is one the OOM
/// killer is already circling — a second sidecar, a leak, a co-resident ffmpeg —
/// and every task handed to it ends with a struck chapter and no artifact. The
/// threshold only *withholds*: nothing is failed and nothing moves, and a box
/// that settles (the idle reaper returns the model's pages) is offered work
/// again on its next ask. Deliberately high, because the cost of a false
/// positive is a box sitting idle while its peers absorb the queue, and one
/// threshold because the stage with the biggest working set is the merge — and
/// it reaps the model before ffmpeg (see `bm-agent`'s `Sidecar::reap_all`), so
/// the reading it is judged on is already net of the 2.85 GB it frees.
const MEM_PCT_CEILING: f32 = 90.0;

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
    /// local node takes those too. Without this a render on a dead,
    /// ffmpeg-less, or merge-disabled box strands its merge pending for ever.
    /// A machine with no stored policy gets the default (all four, merge →
    /// render → digest → crawl). Capability gates still apply within a stage —
    /// render needs `render-segments`, merge needs `merge` (absent when the box
    /// has no ffmpeg) — and a merge's affinity still pins it to the box that
    /// rendered the chapter.
    ///
    /// **The local-node escape makes a pin advisory, and that is the one way a
    /// chapter splits.** A row pinned to a remote box is still offerable here,
    /// so the local node can take part of a chapter the remote is rendering —
    /// and from that moment the remote's store can never hold the whole set.
    /// The pin is therefore re-homed to this box at the moment of the split
    /// (see the assignment below) rather than left to be discovered at the
    /// merge, where the only evidence is `N segments missing` and the only
    /// repair used to be three strikes and a shelving.
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
        // The memory guardrail, second for the same reason the readiness gate is
        // first: handing work to a box that cannot hold it is a task that fails
        // slowly and strikes the chapter for the scheduler's mistake. It sits
        // here rather than in `build_offer` because a withheld offer must leave
        // the task `Pending` and untouched — `build_offer` runs *after* the
        // task has been marked `Assigned`.
        //
        // `None` (an older agent, or a registration that has not measured yet)
        // passes: no opinion is not a verdict, exactly as `Unknown` passes the
        // readiness gate above.
        if let Some(pct) = self.beats.get(worker_id).and_then(|b| b.mem_pct) {
            if pct >= MEM_PCT_CEILING {
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
        //
        // `pick` is a **batch**: one row for every stage, and up to
        // `Settings::render_batch` of one chapter's takes for a render. The
        // grouping is an assignment detail — each take keeps its own ledger row
        // — and it is recorded on the row the offer names (`Task::batch`) so
        // the completion settles the whole group.
        let mut pick: Option<(Vec<String>, Stage)> = None;
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
            // Numeric chapter order — lexical sort would put ch100 before ch93
            // — and take order within a chapter, so a render speaks front to
            // back instead of in whatever order the map iterates. The render
            // batch relies on this: "one chapter" is a prefix of this order.
            ids.sort_by_key(|id| {
                (
                    Task::chapter_of(id).unwrap_or(0),
                    Task::take_of(id).unwrap_or(0),
                )
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
                    pick = Some((vec![id], stage));
                    break;
                }
                continue;
            }
            // A render takes a slice of one chapter's takes; every other stage
            // is one row per chapter and has nothing to batch.
            let batch = if stage == Stage::Render {
                self.render_batch(&ids)
            } else {
                ids.first().cloned().into_iter().collect()
            };
            if !batch.is_empty() {
                pick = Some((batch, stage));
                break;
            }
        }
        let (batch, stage) = pick?;
        let primary = batch[0].clone();
        {
            // The whole batch is assigned together, with one lease: the box is
            // being given this work as a unit, and a partial assignment would
            // leave rows claimable while the same worker is speaking them.
            let lease = now_secs() + lease_for_batch(stage, batch.len());
            for id in &batch {
                if let Some(t) = self.tasks.get_mut(id) {
                    t.state = TaskState::Assigned;
                    t.assigned_to = Some(worker_id.into());
                    t.lease_until = Some(lease);
                    t.updated = now_secs();
                    // The grouping is recorded on the row the offer names —
                    // and only there. Repeating it on every member would be the
                    // same fact stored N times, which is the shape that goes
                    // inconsistent.
                    t.batch = if id == &primary {
                        batch.iter().skip(1).cloned().collect()
                    } else {
                        Vec::new()
                    };
                }
            }
        }
        let t = self.tasks.get(&primary)?;
        let (chapter, stage) = (t.chapter, t.stage);
        let offer = self.build_offer(t, &batch, &machine);
        // A chapter's takes belong on one box. The merge runs where the
        // segments are, so takes spread across boxes leave no single store
        // holding the whole chapter — the merge would then fail on `N segments
        // missing` for a chapter the cluster rendered in full. The merge's
        // affinity already reads this way; here it is established at the first
        // assignment instead of discovered at the merge, when it is too late.
        // A re-render keeps the pin its invalidation set, so a swap still
        // re-speaks on the warm box rather than a cold one.
        if stage == Stage::Render {
            // Except that the pin is not a lock: the filter above lets the
            // local node take a row pinned to any box, so "one box" holds
            // against other remotes and not against this one. When the local
            // node takes a row of a chapter pinned elsewhere the chapter is
            // **split** — this box speaks part of it, the pinned box the rest,
            // and the pinned box's store can never hold the whole set again.
            //
            // The merge has to follow the store, and the store it can follow is
            // this one: `collect_units` brings every completion home, so once
            // the chapter's takes are all `Done` this disk holds all of them
            // while the pinned box holds a strict subset. Left alone, the merge
            // stays pinned to the short box, and the offer's readiness check —
            // which reads *this* disk, the complete one — keeps offering it
            // there until it shelves. Re-home it at the moment the split is
            // made, so the first merge attempt already runs where the audio is.
            if bm_core::is_local_node(&machine) && self.merge_pinned_elsewhere(chapter) {
                self.point_merge_here(
                    chapter,
                    &machine,
                    "requeued: chapter split across boxes — merge moved to the store that holds it",
                );
            }
            self.pin_chapter_takes(chapter, &machine);
        }
        self.save();
        Some(offer)
    }

    /// The ledger rows one render offer assigns: the oldest chapter's pending
    /// takes, up to [`bm_core::config::Settings::render_batch`], truncated at
    /// the first take this store cannot resolve.
    ///
    /// **One chapter, never two.** The pin, the merge's affinity, the progress
    /// line and the inductor's unit collection are all keyed by chapter, so an
    /// offer spanning chapters would collect one chapter's wavs against
    /// another's report. `ids` arrives sorted by `(chapter, take)`, so "one
    /// chapter" is a prefix.
    ///
    /// The truncation keeps the batch and its payload the same length: a take
    /// with no plan entry has no unit to speak, and assigning it would make the
    /// offer claim work it cannot name. The **first** row is taken whether or
    /// not it resolves, so an unplannable take still reaches the completion gate
    /// and fails there by name — which is what it did before batching existed,
    /// and skipping it would strand the chapter silently instead.
    fn render_batch(&self, ids: &[String]) -> Vec<String> {
        let Some(chapter) = ids.first().and_then(|id| Task::chapter_of(id)) else {
            return Vec::new();
        };
        let mut out: Vec<String> = Vec::new();
        for id in ids.iter().take(self.settings.render_batch()) {
            if Task::chapter_of(id) != Some(chapter) {
                break;
            }
            if out.is_empty() {
                out.push(id.clone());
                continue;
            }
            if self.take_spec(chapter, Task::take_of(id)).is_none() {
                break;
            }
            out.push(id.clone());
        }
        out
    }

    /// Every ledger row one report settles: the row its `task_id` names, plus
    /// the rest of the batch that offer assigned.
    ///
    /// A batch is recorded on the row the offer named (see `Task::batch`), so a
    /// report applies to the whole group or to nothing — a partially settled
    /// batch would leave rows `Assigned` to a worker that has already answered,
    /// and their leases would expire into a second render of takes that landed.
    fn covered_rows(&self, task_id: &str) -> Vec<String> {
        let mut out = vec![task_id.to_string()];
        if let Some(t) = self.tasks.get(task_id) {
            out.extend(t.batch.iter().cloned());
        }
        out
    }

    fn build_offer(&self, t: &Task, batch: &[String], machine: &str) -> TaskOffer {
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
        // Merge needs the script — it plans the mix from it, and this is the
        // one stage whose input is a *chapter* of segments. Digest needs the
        // text.
        //
        // **Render needs neither any more.** A take arrives fully specified
        // (voice, text, parameters, and the content-addressed file to write),
        // so there is nothing on this box the worker has to look up: shipping
        // the script and the cast alongside a single segment was the "send the
        // whole chapter and hope" half of the old contract, and it is gone
        // with it.
        let script = (t.stage == Stage::Merge)
            .then(|| bm_core::read_json::<Value>(&self.layout.script(n)).ok())
            .flatten();
        // The cast decides segment *filenames*. A worker that had to recompute
        // it would plan different names than the render wrote — and a
        // provisioned worker cannot recompute it at all, because `data/` is
        // not part of what provisioning copies.
        let cast = (t.stage == Stage::Merge)
            .then(|| bm_core::read_json::<Value>(&self.layout.cast(&self.settings.engine)).ok())
            .flatten();
        let text = (t.stage == Stage::Digest)
            .then(|| std::fs::read_to_string(self.layout.chapter_txt(n)).ok())
            .flatten();
        // **One take per row, `render_batch` rows per offer.** The offer stays
        // self-sufficient — voice, text, parameters — so the worker needs
        // neither the script nor the cast, and a local edit still costs one
        // segment instead of a chapter. What batching changes is only how many
        // of those self-sufficient takes travel together: a worker pays a round
        // trip, a heartbeat and a completion report per offer, and a chapter is
        // dozens of takes.
        //
        // Each unit is named by its own row's position in the chapter's
        // recorded plan; the plan names the file.
        //
        // `Some([])` (a take this store cannot resolve) stays distinguishable
        // from `None` (an old inductor): the worker reports `units: 0` and the
        // completion gate then fails the task with the missing file named,
        // rather than the worker inventing a name of its own.
        let (render_units, cast_hash, render_force) = match t.stage {
            Stage::Render => {
                let mut units = Vec::with_capacity(batch.len());
                let mut force: Vec<String> = Vec::new();
                let mut hash = String::new();
                for id in batch {
                    // `batch` was built from `take_spec` succeeding for every
                    // row but the first, so this only fails on the degenerate
                    // one-row case — which is exactly the `Some([])` below.
                    match self.take_spec(n, Task::take_of(id)) {
                        Some((unit, h, f)) => {
                            if hash.is_empty() {
                                hash = h;
                            }
                            force.extend(f);
                            units.push(unit);
                        }
                        None => break,
                    }
                }
                (Some(units), hash, force)
            }
            _ => (None, String::new(), Vec::new()),
        };
        // A merge needs the whole chapter's files, in mix order, and the names
        // are the plan's — the mixer cannot re-derive a content-addressed take
        // name from the script and the cast, and must not try.
        let merge_takes = if t.stage == Stage::Merge {
            RenderPlan::load(&self.layout.plan(n))
                .map(|p| p.files())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
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
            // The inductor plans; the worker speaks. One fully specified unit
            // per assigned take — see `take_spec` and `render_batch`.
            // `render_force` carries only adopted takes this store lacks, the
            // one case where a same-named file on a warm box is not proof of
            // the right bytes.
            render_units,
            render_force,
            cast_hash,
            merge_takes,
            // The inductor decides locality; the worker never guesses from
            // paths. Same predicate the provisioner uses for `Ssh.local`.
            local_node: bm_core::is_local_node(machine),
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
                delta: Option<Value>,
                script: Option<Value>,
                text: Option<String>,
                mp3_b64: Option<String>,
            },
            Failed,
        }
        // **The operator is authoritative for the chapter they digested by hand.**
        //
        // A worker's report is believed only while it still holds the row — that
        // is what stops a stale report from a box that lost its lease from
        // resurrecting work. The operator holds nothing: they are not a worker,
        // the row may well be assigned to a box that is grinding on it right now,
        // and their answer is the better one either way.
        //
        // Accepting it here **is** the release. Marking the row Done clears
        // `assigned_to`, so the other worker's eventual report finds a row it no
        // longer owns and is dropped as stale — which is the whole of "tell him
        // to give that up", with no new instruction on the wire. The box keeps
        // working until it notices, bounded by the stage's own deadline (120 s
        // for the `opencode` fallback, and the lease above that).
        let manual = c.worker_id == bm_proto::MANUAL_WORKER;
        let outcome = match self.tasks.get(&c.task_id) {
            None => Outcome::Unknown,
            Some(t) if !manual && t.assigned_to.as_deref() != Some(c.worker_id.as_str()) => {
                Outcome::Stale
            }
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
                // evidence — the take's file is. Read out of the **plan**, not
                // re-derived from the script and cast: the plan is what named
                // the file and what the mixer will read, so the gate and the
                // merge ask one question with one answer.
                //
                // **Every row the offer assigned**, not just the one it named.
                // A batch is one report covering N takes, and a gate that
                // checked only the primary would pass a report whose other nine
                // takes never landed — the failure would surface at the merge
                // instead, as "N segments missing", with nothing pointing at
                // the render.
                if stage == Stage::Render {
                    let plan = RenderPlan::load(&self.layout.plan(chapter));
                    let seg_dir = self.layout.seg_dir(&self.settings.engine, chapter);
                    // Asked once, not per row: it re-plans the chapter, and a
                    // batch of sixty-four would pay for that sixty-four times
                    // to print the same sentence.
                    let unplannable = plan.is_none().then(|| self.why_unplannable(chapter));
                    let mut missing: Vec<String> = Vec::new();
                    for id in self.covered_rows(&c.task_id) {
                        let take = self.tasks.get(&id).and_then(|t| t.take);
                        match (&plan, take) {
                            (Some(p), Some(pos)) => match p.takes.get(pos) {
                                Some(tk) if present(&seg_dir, &tk.file) => {}
                                Some(tk) => missing.push(tk.file.clone()),
                                None => missing.push(format!("{id} (no plan entry for this take)")),
                            },
                            // The cause, not a shrug: "cannot be planned here"
                            // cost a whole run to a generic message when the
                            // real answer was on the next line of the planner.
                            (None, _) => missing.push(format!(
                                "{id} (chapter cannot be planned here: {})",
                                unplannable.as_deref().unwrap_or("unknown")
                            )),
                            (_, None) => {
                                missing.push(format!("{id} (no plan entry for this take)"))
                            }
                        }
                    }
                    if !missing.is_empty() {
                        let detail = if missing.len() == 1 {
                            format!("render ch{chapter} incomplete, missing: {}", missing[0])
                        } else {
                            format!(
                                "render ch{chapter} incomplete, {} of {} takes missing: {}",
                                missing.len(),
                                self.covered_rows(&c.task_id).len(),
                                missing.join(", ")
                            )
                        };
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
                    // The render ledger is per take: the plan is built and
                    // materialised the moment the script exists, so the offer,
                    // the completion gate and the merge gate all read one set
                    // of names. A script this build cannot plan keeps a
                    // chapter-granular row, so the unplannable case is named by
                    // the stage that could not read it instead of vanishing.
                    if self.materialize_render_takes(chapter).is_none() {
                        self.ensure_task(chapter, Stage::Render);
                    }
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
                    //
                    // **A pin to this box outranks the completion**, though. A
                    // local completion is already the local node, so the only
                    // case this changes is the split: the local node took part
                    // of the chapter (the offer re-homed the merge here), and a
                    // remote box is now completing another take. That box
                    // cannot hold the whole set and never will, so its
                    // completion must not claim the merge back — otherwise the
                    // pin flips remote, the merge is offered to a short store,
                    // and the chapter is re-homed again one wasted strike at a
                    // time.
                    let held_here = m
                        .affinity
                        .as_deref()
                        .map(bm_core::is_local_node)
                        .unwrap_or(false);
                    if !held_here {
                        m.affinity = rendered_by.or_else(|| Some("127.0.0.1".into()));
                    }
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
                // **Every row the report covers**, not just the one it names: a
                // batched offer is one report for N takes, and settling only
                // the primary would leave its siblings `Assigned` to a worker
                // that has already answered — their leases would then expire
                // into a second render of takes that landed. The grouping is
                // cleared as it is consumed, so it can never settle a later,
                // unrelated report.
                let rows = self.covered_rows(&c.task_id);
                for id in &rows {
                    if let Some(t) = self.tasks.get_mut(id) {
                        t.state = TaskState::Done;
                        t.detail = c.detail.clone();
                        t.assigned_to = None;
                        t.lease_until = None;
                        t.updated = now_secs();
                        t.batch.clear();
                        // The assignment resolved, so the silent-expiry streak is
                        // over: a row that once hung must not make every later,
                        // legitimate expiry read as a repeat.
                        t.expiries = 0;
                        if let Some(d) = design.clone() {
                            t.design = Some(d);
                        }
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
    ///
    /// This is the branch where **this** disk is the short one. When it is not,
    /// the merge is the thing that is wrong, and [`Self::rehome_starved_merge`]
    /// is the one that answers — a merge that failed with a complete store here
    /// has nothing for this function to do, which is why the two are separate
    /// rather than one function guessing.
    fn heal_render_for_merge(&mut self, chapter: u32) -> bool {
        if self.layout.final_mp3(chapter).is_file() {
            return false;
        }
        // The plan's diff *is* the heal: a take whose file this store lacks is
        // work again, and one it holds stays `Done`. Nothing is deleted — the
        // next render fills the gap rather than starting the chapter over.
        if self.materialize_render_takes(chapter).is_none() {
            // Unplannable here (no script, an uncast speaker): the
            // chapter-granular row is all the ledger has to offer, so fall back
            // to it rather than inventing takes.
            let key = format!("{}:{chapter}", Stage::Render);
            return match self.tasks.get_mut(&key) {
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
            };
        }
        let mut healed = false;
        for t in self.tasks.values_mut() {
            if t.stage == Stage::Render && t.chapter == chapter && t.state == TaskState::Pending {
                t.detail = "requeued: merge found segments missing".into();
                healed = true;
            }
        }
        healed
    }

    /// A merge failed on `N segments missing` and **this** store holds every
    /// take of the chapter: the chapter is not the starved input, the pin is.
    /// Re-home the merge here, and report that nothing should be struck.
    ///
    /// The companion to [`Self::heal_render_for_merge`], and the half that was
    /// missing. A merge reads its segments from the disk it runs on and is
    /// offered wherever its affinity names, so when that box's store is short
    /// the merge fails on a chapter the cluster rendered in full — and the
    /// offer's readiness check cannot see it, because the only disk it can read
    /// is this one, the complete one. `heal_render_for_merge` then looked for
    /// local gaps, found none, and did nothing: three identical failures five
    /// seconds apart, and a chapter shelved for a fault that was never its own.
    ///
    /// Completeness here is not a guess. `collect_units` pulls every unit home
    /// before a render completion is applied, so a chapter whose takes are all
    /// `Done` is complete on this disk — which makes the local node the one
    /// merge target *known* to work. The box that just failed is known not to.
    /// So the merge moves, and the strike does not count: handing a task to a
    /// box that cannot run it is the scheduler's mistake, the same rule the
    /// memory guardrail follows.
    ///
    /// `false` when the merge is already pinned here — then the failure is real
    /// and the strike stands — or when this disk is short too, in which case the
    /// render is the starved input and [`Self::heal_render_for_merge`] has it.
    fn rehome_starved_merge(&mut self, task_id: &str, detail: &str) -> bool {
        if !detail.contains("segments missing") {
            return false;
        }
        let Some(chapter) = task_id
            .strip_prefix("merge:")
            .and_then(|c| c.parse::<u32>().ok())
        else {
            return false;
        };
        if !self.merge_pinned_elsewhere(chapter) {
            return false;
        }
        // Empty means this disk holds the chapter. `None` means it cannot be
        // planned here at all, which is a different failure with its own name.
        match crate::segments::missing_wavs(&self.layout, &self.settings.engine, chapter) {
            Some(missing) if missing.is_empty() => {}
            _ => return false,
        }
        self.point_merge_here(
            chapter,
            "127.0.0.1",
            "requeued: segments missing on the pinned box, this store holds them all",
        )
    }

    /// Record a failed report: a strike, Pending again (Shelved at 3), and an
    /// event line. Shared by worker-reported failures and the completion
    /// gate, which fails reports whose files never landed.
    ///
    /// **The whole batch takes the strike, not the row the report named.** One
    /// report is one answer about one offer, and the offer covered N takes: a
    /// worker that died mid-batch, or whose units never landed, did not fail
    /// only the first of them. Per-take strikes would also let a batch of sixty
    /// shelter sixty rows from the three-strikes rule while never once
    /// finishing — the chapter would be retried for ever instead of shelving
    /// for an operator to look at.
    ///
    /// The cost is honest and small: a retry is cheap, because `pending_units`
    /// skips the takes whose files did land, so a batch that got nine of ten
    /// re-speaks one.
    fn fail_task(&mut self, task_id: &str, worker_id: &str, detail: String) -> String {
        // **Which input is short?** A merge fails on `N segments missing` for
        // exactly two reasons, and they need opposite answers: the render never
        // produced the audio, or the merge was offered to a box that does not
        // hold it. The second is the scheduler's mistake, so it is settled
        // before the strike is counted — see `rehome_starved_merge`. A chapter
        // is not struck for being handed to a box that cannot run it, the same
        // rule the memory guardrail follows.
        if self.rehome_starved_merge(task_id, &detail) {
            let note = " (will retry — merge moved to the store that holds it)";
            self.push_event(
                "warn",
                format!(
                    "[{worker_id}] {task_id} FAILED{note}: {}",
                    bm_core::util::head_chars(&detail, 200)
                ),
            );
            self.save();
            return format!(
                "{worker_id}: {task_id} failed ({}){note}",
                bm_core::util::head_chars(&detail, 120)
            );
        }
        let rows = self.covered_rows(task_id);
        let mut shelved = false;
        for id in &rows {
            if let Some(t) = self.tasks.get_mut(id) {
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
                t.batch.clear();
                // A reported failure is an answer: the worker got far enough to
                // say something, so it was not stuck, and the silent-expiry
                // streak it may have had before is not this assignment's story.
                t.expiries = 0;
                shelved |= t.state == TaskState::Shelved;
            }
        }
        // The other branch of the same failure: the merge is pinned here, or
        // this disk is short too — so the render really is the starved input.
        // Requeue it alongside the merge retry, or the same merge fails twice
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
        // Capitalised and counted, because a shelving is the one outcome that
        // needs a human: the chapter stops being retried and will sit there
        // until somebody presses `u`. It has to be distinguishable at a glance
        // from the ordinary "will retry" failure, which needs nobody.
        let note = if shelved {
            " (SHELVED — 3 strikes, no further retries; press u to requeue)"
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
        // The log line carries the same distinction as the event. It used to say
        // only "failed", so a shelving — the outcome that needs an operator —
        // was indistinguishable from a failure that retries itself, in the one
        // place a person actually greps.
        let outcome = if shelved { "SHELVED" } else { "failed" };
        format!(
            "{worker_id}: {task_id} {outcome} ({})",
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
