use super::*;

impl Inner {
    /// Why no digest or render may be offered, or `None` when the adapter, the
    /// binding's pack and the bound engine agree.
    ///
    /// **The language refusal, and the reason it lives here.** An adapter has
    /// one language and it is both the source's and the target's, so a digest
    /// writes a script in that language and a render bakes audio in it — and
    /// neither is undoable cheaply: the script is content-addressed and the
    /// segments are cached, so a wrong-language chapter is hours of synthesis
    /// carrying a mistake. `crawl` and `prepare` are deliberately not gated:
    /// the crawled text and the quote split are properties of the source, and
    /// the fork line puts them on the adapter-independent side.
    ///
    /// The binding read is the ledger's stamp when it has one — that is the
    /// authority for the tasks that exist, and [`Inner::check_profile`] refuses
    /// a ledger from another binding — falling back to the workspace's own
    /// settings, which is what a run about to be planned is stamped from.
    ///
    /// **This refuses; it does not strike or shelve.** Nothing about the box or
    /// the chapter is wrong, and one keypress (a different engine, or a fixed
    /// adapter) makes every withheld row offerable again — so the rows stay
    /// `Pending` and untouched. The same verdict is raised as a warning once at
    /// load, which is where the operator will actually read it.
    fn voice_gate(&self) -> Option<String> {
        let binding = self.bound();
        // `settings.engine` and not `layout.engine`: the two are different
        // facts (the run config's engine, and the one the load pointer's
        // `engines/<name>/` tree belongs to), and **this gate must judge the
        // engine the offer will actually name** — every offer builds on
        // `settings.engine`, from `seg_dir` to the cast to the sidecar's
        // dictionary. Judging the other one would be a gate that passes a
        // chapter the render lane then bakes in the wrong engine's language.
        let verdict =
            bm_core::adapter::inspect(&self.layout, &binding.pack.name, &self.settings.engine);
        if verdict.agrees() {
            None
        } else {
            Some(verdict.reason())
        }
    }


    /// The binding the tasks in flight were created under, and the one an offer
    /// is made under.
    ///
    /// The ledger's stamp when it has one — that is the authority for the tasks
    /// that exist, and [`Inner::check_profile`] refuses a ledger from another
    /// binding — falling back to the workspace's own settings, which is what a
    /// run about to be planned is stamped from. Both the language gate and the
    /// three fields of the offer's binding read it, so the two cannot disagree
    /// about whose chapter this is.
    fn bound(&self) -> &bm_core::profile::Binding {
        self.ledger_profile
            .as_ref()
            .unwrap_or(&self.settings.profile)
    }


    /// The task this worker should do next.
    ///
    /// The box's own **policy** decides which stages it will run and in what
    /// order (most-preferred first): the scheduler walks the enabled stages and
    /// takes the oldest chapter of the first stage that has assignable work.
    /// A machine with no stored policy gets the default (all four, merge →
    /// render → digest → crawl). Capability gates still apply within a stage:
    /// render needs `render-segments`, merge needs `merge` (absent when the
    /// box has no ffmpeg).
    ///
    /// No row carries affinity: takes are independent, every unit lands on the
    /// inductor before its completion is applied, and a merge pulls the pieces
    /// it lacks from the inductor. So any capable box runs anything whose
    /// inputs are done, and a chapter's audio routinely ends up on several
    /// stores. That is expected, not a fault.
    ///
    /// A merge is additionally offered only when its segments are on this
    /// disk (`missing_wavs` empty): the ledger's `render:Done` is a claim
    /// about files, and files deleted or never collected since make it a lie
    /// that every box would fail the same way. A starved merge heals its
    /// render (see `heal_render_for_merge`) and yields to the next stage,
    /// so one bad chapter never idles the worker.
    pub fn offer(&mut self, worker_id: &str) -> Option<TaskOffer> {
        self.reap();
        // The operator's global answer, and it is deliberately the *first*
        // question asked: every gate below is about *which* box should get a
        // task, and this is whether any task should be handed out at all. A
        // process starts held (see `Inner::dispatch_held`), so a fleet is never
        // set loose by the mere act of a restart.
        //
        // Withheld, not failed: the worker gets the same nothing-to-do it gets
        // from an empty queue, so it keeps beating and its leases stay clean,
        // and the reason is on `/api/state` for whoever is looking at the
        // dashboard wondering why the cluster is quiet.
        //
        // After `reap` on purpose: taking rows back off dead workers is
        // bookkeeping, not distribution, and a held cluster still wants its
        // orphaned rows released so the task table reads true.
        if self.dispatch_held {
            return None;
        }
        let machine = self.workers.get(worker_id).cloned().unwrap_or_default();
        // The readiness gate, and it is deliberately the *first* thing after
        // the worker lookup: a task handed to a box that is booting, being
        // pushed to, or known-broken is a task that fails slowly and strikes
        // the chapter for the inductor's mistake.
        //
        // `Unknown` passes, as "no opinion formed": a hand-written ledger, or
        // the legacy pull worker asking before its first beat. Both were
        // offered work before this gate existed, and a live worker asking for
        // work is itself evidence the box is up, second-guessing that is not
        // this gate's job. Every *other* state is one the inductor chose.
        if let Some(m) = self.machines.get(&machine) {
            if !m.state.accepts_work() && m.state != bm_proto::MachineState::Unknown {
                return None;
            }
            // The operator's pause, and it is second because the two answers are
            // different questions: the state above is *is this box able*, this is
            // *should it be working*. A relaxed box is deliberately idle, so it
            // is withheld work **whatever its state**, including `Unknown`, and
            // including `Online`. That last one is the whole point: a parked box
            // keeps beating (it is alive), so the state gate alone would let the
            // scheduler hand it a task the operator just said not to run.
            if m.relaxed() {
                return None;
            }
        }
        // The memory guardrail, second for the same reason the readiness gate is
        // first: handing work to a box that cannot hold it is a task that fails
        // slowly and strikes the chapter for the scheduler's mistake. It sits
        // here rather than in `build_offer` because a withheld offer must leave
        // the task `Pending` and untouched, `build_offer` runs *after* the
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
        // Pick first, then mutate, the borrow checker wants the scan finished
        // before the assignment begins. Walk the policy in order and take the
        // oldest chapter of the first enabled stage that has assignable work.
        //
        // `pick` is a **batch**: one row for every stage, and up to
        // `Settings::render_batch` of one chapter's takes for a render. The
        // grouping is an assignment detail, each take keeps its own ledger row
        //, and it is recorded on the row the offer names (`Task::batch`) so
        // the completion settles the whole group.
        //
        // **The language gate's answer, read once per offer rather than per
        // stage.** The adapter in force is a property of this checkout, so
        // every stage sees the same verdict; reading it inside the loop would
        // re-read the manifest for each stage of each ask.
        //
        // It is raised here, in the scheduler, and not on a box, because this
        // is the only place that knows all three facts at once: the workspace's
        // binding, the adapter it names, and the engine those bytes would be
        // spoken with.
        let language = self.voice_gate();
        let mut pick: Option<(Vec<String>, Stage)> = None;
        for pref in policy.iter().filter(|p| p.enabled) {
            let stage = pref.stage;
            // **The stage that would cook bytes in a language nothing can
            // speak.** `crawl` and `prepare` are deliberately outside this: the
            // crawled text and the quote split are properties of the *source*,
            // and the fork line puts them on the adapter-independent side. A
            // digest writes a script and a render writes audio, and both are
            // one language that stays for ever.
            if language.is_some() && matches!(stage, Stage::Digest | Stage::Render) {
                continue;
            }
            // Capability gates. Render uploads units; merge shells out to
            // ffmpeg and a box without it advertises no `merge`. A worker with
            // no recorded capabilities is allowed, failing closed here would
            // strand anything that never registered.
            if let Some(caps) = &caps {
                let can = |cap: &str| caps.iter().any(|c| c == cap);
                if (stage == Stage::Render && !can("render-segments"))
                    || (stage == Stage::Merge && !can("merge"))
                {
                    continue;
                }
            }
            // **The exclusive-write gate.** A queued surgery (a swap, a
            // remix, …) holds its stages and chapters: rows inside the scope
            // are not offered while it waits, because a task completing into
            // work the write is about to requeue is the stale-done the old
            // refusal existed to prevent. Outside the scope delivery is
            // untouched — a swap parked on chapter 41 still lets chapter 42
            // render, and crawl never pauses at all. One closure, read by
            // both the pending filter and the digest-racing one below, so a
            // parked retag cannot draw new digests onto the scripts it edits.
            let write_blocks = self.exclusive.first().map(|w| {
                let op = w.op.clone();
                move |ch: u32| op.blocks(stage, ch)
            });
            // **The stage this box was never sent the files for.** The policy
            // is the operator's intent, the reported bundle is the fact, and
            // when they disagree the box is asked to run a stage it cannot:
            // a digest with no `prompts/analyze.txt` fails on every retry and
            // the chapter wears the strikes. Widening a policy is one keypress
            // and provisioning is another, so the two are read together here.
            //
            // An agent that reports nothing (no bundle yet, or one that
            // predates the field) is offered work exactly as before: no
            // opinion is not a verdict, the same rule the capability gate
            // above and the memory ceiling already follow.
            if let Some(beat) = self.beats.get(worker_id) {
                if !beat.sources_stages.is_empty()
                    && !bm_core::provision::sources::holds(
                        &beat.sources_stages,
                        stage,
                        &self.layout.adapter,
                    )
                {
                    continue;
                }
            }
            let mut ids: Vec<String> = self
                .tasks
                .iter()
                .filter(|(_, t)| t.stage == stage)
                .filter(|(_, t)| {
                    if t.state == TaskState::Pending && !self.shelved(t.chapter) {
                        return !write_blocks.as_ref().is_some_and(|f| f(t.chapter))
                            && self.upstream_done(t.chapter, t.stage);
                    }
                    // Digest racing: the head digest is the pipeline's
                    // bottleneck (the N-1→N chain leaves exactly one digest
                    // offerable at a time), so an idle digest worker would
                    // otherwise sit out a 20-minute LLM call. An already
                    // assigned digest row is offerable to a box that does not
                    // hold it yet, same snapshot, same prompt, first report
                    // wins and every later one is dropped as stale. Other
                    // stages stay single-assignee: their rows are cheap and
                    // parallel across chapters already.
                    // ponytail: unbounded racers by design, the operator's
                    // digest-worker count is the cap; each box holds one task.
                    stage == Stage::Digest
                        && matches!(t.state, TaskState::Assigned | TaskState::Running)
                        && !self.shelved(t.chapter)
                        && !t.is_holder(worker_id)
                        && !write_blocks.as_ref().is_some_and(|f| f(t.chapter))
                        && self.upstream_done(t.chapter, t.stage)
                })
                // No affinity gate on any stage: takes are independent and a
                // merge pulls the pieces it lacks from the inductor, so every
                // pending row whose inputs are done is offerable to every
                // capable box. Pins only ever serialised the cluster.
                .map(|(id, _)| id.clone())
                .collect();
            // Numeric chapter order, lexical sort would put ch100 before ch93
            //, and take order within a chapter, so a render speaks front to
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
                // disk cannot plan (`None`, no script, uncast speaker) is
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
                    // A digest row already grinding on another box: join the
                    // race instead of stealing it. The primary keeps its seat;
                    // this box runs the same snapshot and the first `ok`
                    // report settles the row for everyone.
                    if stage == Stage::Digest
                        && matches!(t.state, TaskState::Assigned | TaskState::Running)
                        && !t.is_holder(worker_id)
                    {
                        if !t.racers.iter().any(|r| r == worker_id) {
                            t.racers.push(worker_id.into());
                        }
                        t.lease_until = Some(lease);
                        t.updated = now_secs();
                        continue;
                    }
                    t.state = TaskState::Assigned;
                    t.assigned_to = Some(worker_id.into());
                    t.lease_until = Some(lease);
                    t.updated = now_secs();
                    // The grouping is recorded on the row the offer names
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
        let offer = self.build_offer(t, &batch, &machine);
        // **A chapter may be spoken by several boxes at once, and that is the
        // point.** No row is ever pinned, takes are independent, a merge
        // pulls the pieces it lacks from the inductor, and the next worker
        // to ask deepens this chapter instead of opening another one.
        //
        // What keeps the merge possible is the store, not a pin: every unit
        // lands on the inductor before its completion is applied.
        self.save();
        Some(offer)
    }


    /// The ledger rows one render offer assigns: the oldest chapter's pending
    /// takes, up to [`bm_core::config::Settings::render_batch`], truncated at
    /// the first take this store cannot resolve.
    ///
    /// **One chapter, never two.** The progress line and the inductor's unit
    /// collection are all keyed by chapter, so an offer spanning chapters
    /// would collect one chapter's wavs against another's report. `ids`
    /// arrives sorted by `(chapter, take)`, so "one chapter" is a prefix, and
    /// it is also what makes the batch *deepen*: the next worker to ask gets
    /// the next slice of this chapter, not a new one. Takes are never pinned.
    ///
    /// The truncation keeps the batch and its payload the same length: a take
    /// with no plan entry has no unit to speak, and assigning it would make
    /// the offer claim work it cannot name. The **first** row is taken whether
    /// or not it resolves, so an unplannable take still reaches the
    /// completion gate and fails there by name instead of stranding the
    /// chapter silently.
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
    /// report applies to the whole group or to nothing, a partially settled
    /// batch would leave rows `Assigned` to a worker that has already answered,
    /// and their leases would expire into a second render of takes that landed.
    /// `pub(crate)` because the ledger's strike-free release of a refused
    /// render (`release_render_rows`) settles the same set a report would.
    pub(crate) fn covered_rows(&self, task_id: &str) -> Vec<String> {
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
        // Merge needs the script, it plans the mix from it, and this is the
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
        // it would plan different names than the render wrote, and a
        // provisioned worker cannot recompute it at all, because `data/` is
        // not part of what provisioning copies.
        let cast = (t.stage == Stage::Merge)
            .then(|| bm_core::read_json::<Value>(&self.layout.cast(&self.settings.engine)).ok())
            .flatten();
        let text = (t.stage == Stage::Digest)
            .then(|| std::fs::read_to_string(self.layout.chapter_txt(n)).ok())
            .flatten();
        // **One take per row, `render_batch` rows per offer.** The offer stays
        // self-sufficient, voice, text, parameters, so the worker needs
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
                    // one-row case, which is exactly the `Some([])` below.
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
        // are the plan's, the mixer cannot re-derive a content-addressed take
        // name from the script and the cast, and must not try.
        let merge_takes = if t.stage == Stage::Merge {
            RenderPlan::load(&self.layout.plan(n))
                .map(|p| p.files())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        // The crawl stage's three extra facts: where the chapter lives, which
        // crawler fetches it, and how many times this row has already been
        // tried (so a script can try a mirror on the second go).
        let (crawl_url, crawl_spec) = if t.stage == Stage::Crawl {
            (
                Some(self.crawl_url(n)),
                Some(bm_core::crawl::spec_from_settings(
                    &self.layout,
                    &self.settings,
                )),
            )
        } else {
            (None, None)
        };
        // The LLM source of truth is `.bm/llm.json`, not the workspace
        // settings: keys are machine-global, and the offer below carries the
        // active key+model per task. Unconfigured (or legacy) falls back to
        // the workspace settings, which the `L` screen keeps mirrored.
        let llm = bm_core::config::LlmConfig::load_or_seed(&self.layout.root, &self.settings);
        let (analyzer, analyzer_settings) = llm.offer_analyzer(&self.settings);
        // Narrowed to what this stage reads, so a crawl offer carries no
        // secret and a digest carries only the analyzer's key. The slot (not
        // the provider id) decides the narrowing.
        let backend = analyzer_settings.backend.clone();
        let credentials = llm
            .credentials()
            .for_stage(t.stage, &backend, &self.settings.engine);
        TaskOffer {
            task_id: t.id(),
            chapter: n,
            stage: t.stage,
            root: self.layout.root.display().to_string(),
            url: crawl_url,
            crawl: crawl_spec,
            attempt: t.attempts + 1,
            tts_url: t.stage.needs_tts().then_some(tts_url),
            // **The binding, on the wire.** See `Layout::rebind` for what the
            // box does with it and `TaskOffer::adapter` for why the box cannot
            // work it out for itself. The adapter is the layout's — the one
            // every cache path on this machine is keyed by — not the binding's
            // own field: they are the same name in the ordinary case, and when
            // they differ it is the caches that decide where the bytes land.
            adapter: self.layout.adapter.clone(),
            // The pack is a *claim* rather than a cache key: the registries the
            // prompts read are the pack's, and nothing else reports which pack
            // a box is holding.
            pack: self.bound().pack.name.clone(),
            engine: self.settings.engine.clone(),
            model_order: self.settings.model_order.clone(),
            analyzer,
            // The analyzer's *backend* travels in `analyzer` above; what that
            // backend runs travels here. Both are needed: a provisioned worker
            // has no `.bm/llm.json` to read (provisioning never copies
            // `.bm/`), so without this it digests with the compiled-in
            // default instead of the operator's choice. The key and the model
            // travel per task, so switching with `L` takes effect on the
            // next offer with no other sync.
            analyzer_settings,
            // The inductor is the only machine whose `.bm/llm.json` the
            // operator maintains: a provisioned worker has none, because
            // `.bm/` is never copied by `install_sources`. Shipping the key
            // with the task is what makes a remote digest possible at all —
            // narrowed to what this stage actually reads, so a crawl offer
            // carries no secret.
            credentials,
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
            // per assigned take, see `take_spec` and `render_batch`.
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


    /// Where a chapter lives: the frozen index's answer when there is one, and
    /// the URL template otherwise.
    ///
    /// The index is read from disk rather than rebuilt here, an offer is not
    /// the place for a network walk, so a mapping rebuilt since (by `:crawl`,
    /// by `:translate`, or by hand) takes effect on the next offer, and a
    /// workspace that never built one keeps working off its template.
    fn crawl_url(&self, n: u32) -> String {
        bm_core::crawl::CrawlIndex::load(&self.layout)
            .and_then(|i| i.url(n).map(str::to_string))
            .unwrap_or_else(|| self.settings.chapter_url(n))
    }
}
