use super::*;

impl Inner {
    /// Why no digest or render may be offered, or `None` when the adapter, the
    fn voice_gate(&self) -> Option<String> {
        let binding = self.bound();
        // `settings.engine` and not `layout.engine`: the two are different
        let verdict =
            bm_core::adapter::inspect(&self.layout, &binding.pack.name, &self.settings.engine);
        if verdict.agrees() {
            None
        } else {
            Some(verdict.reason())
        }
    }

    /// The binding the tasks in flight were created under, and the one an offer
    fn bound(&self) -> &bm_core::profile::Binding {
        self.ledger_profile
            .as_ref()
            .unwrap_or(&self.settings.profile)
    }

    /// The task this worker should do next.
    pub fn offer(&mut self, worker_id: &str) -> Option<TaskOffer> {
        self.reap();
        // The operator's global answer, and it is deliberately the *first*
        if self.dispatch_held {
            return None;
        }
        let machine = self.workers.get(worker_id).cloned().unwrap_or_default();
        // The readiness gate, and it is deliberately the *first* thing after
        if let Some(m) = self.machines.get(&machine) {
            if !m.state.accepts_work() && m.state != bm_proto::MachineState::Unknown {
                return None;
            }
            // The operator's pause, and it is second because the two answers are
            if m.relaxed() {
                return None;
            }
        }
        // The memory guardrail, second for the same reason the readiness gate is
        if let Some(pct) = self.beats.get(worker_id).and_then(|b| b.mem_pct) {
            if pct >= MEM_PCT_CEILING {
                return None;
            }
        }
        // The box's order + enablement. An unknown worker id (a hand-written
        let policy = self
            .machines
            .get(&machine)
            .map(|m| m.effective_task_policy())
            .unwrap_or_else(bm_proto::TaskPref::default_list);
        let caps = self.caps.get(worker_id).cloned();
        // Pick first, then mutate, the borrow checker wants the scan finished
        let language = self.voice_gate();
        let mut pick: Option<(Vec<String>, Stage)> = None;
        for pref in policy.iter().filter(|p| p.enabled) {
            let stage = pref.stage;
            // **The stage that would cook bytes in a language nothing can
            if language.is_some() && matches!(stage, Stage::Digest | Stage::Render) {
                continue;
            }
            // Capability gates. Render uploads units; merge shells out to
            if let Some(caps) = &caps {
                let can = |cap: &str| caps.iter().any(|c| c == cap);
                if (stage == Stage::Render && !can("render-segments"))
                    || (stage == Stage::Merge && !can("merge"))
                {
                    continue;
                }
            }
            // **The exclusive-write gate.** A queued surgery (a swap, a
            let write_blocks = self.exclusive.first().map(|w| {
                let op = w.op.clone();
                move |ch: u32| op.blocks(stage, ch)
            });
            // **The stage this box was never sent the files for.** The policy
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
                    stage == Stage::Digest
                        && matches!(t.state, TaskState::Assigned | TaskState::Running)
                        && !self.shelved(t.chapter)
                        && !t.is_holder(worker_id)
                        && !write_blocks.as_ref().is_some_and(|f| f(t.chapter))
                        && self.upstream_done(t.chapter, t.stage)
                })
                // No affinity gate on any stage: takes are independent and a
                .map(|(id, _)| id.clone())
                .collect();
            // Numeric chapter order, lexical sort would put ch100 before ch93
            ids.sort_by_key(|id| {
                (
                    Task::chapter_of(id).unwrap_or(0),
                    Task::take_of(id).unwrap_or(0),
                )
            });
            if stage == Stage::Merge {
                // The data check delivery was missing: offer the oldest merge
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
            let lease = now_secs() + lease_for_batch(stage, batch.len());
            for id in &batch {
                if let Some(t) = self.tasks.get_mut(id) {
                    // A digest row already grinding on another box: join the
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
        self.save();
        Some(offer)
    }

    /// The ledger rows one render offer assigns: the oldest chapter's pending
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
        // the script and the cast alongside a single segment was the "send the
        // whole chapter and hope" half of the old contract, and it is gone
        let script = (t.stage == Stage::Merge)
            .then(|| bm_core::read_json::<Value>(&self.layout.script(n)).ok())
            .flatten();
        // The cast decides segment *filenames*. A worker that had to recompute
        let cast = (t.stage == Stage::Merge)
            .then(|| bm_core::read_json::<Value>(&self.layout.cast(&self.settings.engine)).ok())
            .flatten();
        let text = (t.stage == Stage::Digest)
            .then(|| std::fs::read_to_string(self.layout.chapter_txt(n)).ok())
            .flatten();
        // **One take per row, `render_batch` rows per offer.** The offer stays
        let (render_units, cast_hash, render_force) = match t.stage {
            Stage::Render => {
                let mut units = Vec::with_capacity(batch.len());
                let mut force: Vec<String> = Vec::new();
                let mut hash = String::new();
                for id in batch {
                    // `batch` was built from `take_spec` succeeding for every
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
        let merge_takes = if t.stage == Stage::Merge {
            RenderPlan::load(&self.layout.plan(n))
                .map(|p| p.files())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        // The crawl stage's three extra facts: where the chapter lives, which
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
        let llm = bm_core::config::LlmConfig::load_or_seed(&self.layout.root, &self.settings);
        let (analyzer, analyzer_settings) = llm.offer_analyzer(&self.settings);
        // Narrowed to what this stage reads, so a crawl offer carries no
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
            adapter: self.layout.adapter.clone(),
            // The pack is a *claim* rather than a cache key: the registries the
            pack: self.bound().pack.name.clone(),
            engine: self.settings.engine.clone(),
            model_order: self.settings.model_order.clone(),
            analyzer,
            // The analyzer's *backend* travels in `analyzer` above; what that
            analyzer_settings,
            // The inductor is the only machine whose `.bm/llm.json` the
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
            render_units,
            render_force,
            cast_hash,
            merge_takes,
            // The inductor decides locality; the worker never guesses from
            local_node: bm_core::is_local_node(machine),
        }
    }

    /// Where a chapter lives: the frozen index's answer when there is one, and
    fn crawl_url(&self, n: u32) -> String {
        bm_core::crawl::CrawlIndex::load(&self.layout)
            .and_then(|i| i.url(n).map(str::to_string))
            .unwrap_or_else(|| self.settings.chapter_url(n))
    }
}
