use super::*;

impl Inner {
    /// Apply a worker report. Returns a human-readable line for the event log.
    pub fn complete(&mut self, c: &Complete) -> String {
        // Snapshot what the transition needs, then mutate, the borrow checker
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
            /// The site has no such chapter. Terminal, strike-free, and the
            /// artifact does not exist by definition, so the digest closes at
            /// the same time, or the chapter waits forever on a crawl that will
            /// never produce text.
            Absent {
                chapter: u32,
                reason: String,
            },
            /// A refusal that will not improve on a retry (a login wall, a 404
            /// on a URL the index named). Shelved at once **with the class on
            /// the row**: three attempts at a paywall prove nothing a single
            /// one did not, and they cost a worker and an hour.
            Shelved {
                task_id: String,
                detail: String,
            },
        }
        // **The operator is authoritative for the chapter they digested by hand.**
        //
        // A worker's report is believed only while it still holds the row, that
        // is what stops a stale report from a box that lost its lease from
        // resurrecting work. The operator holds nothing: they are not a worker,
        // the row may well be assigned to a box that is grinding on it right now,
        // and their answer is the better one either way.
        //
        // Accepting it here **is** the release. Marking the row Done clears
        // its holders, so every other box's eventual report finds a row it no
        // longer owns and is dropped as stale, and the dispatcher's drive
        // loop aborts a losing digest racer's POST on its next poll, so the
        // worker reads a dropped connection as a cancel instead of grinding
        // to a stale report. No new instruction on the wire either way.
        let manual = c.worker_id == bm_proto::MANUAL_WORKER;
        // A manual digest may name the next chapter before any worker task
        // exists for it, the operator works ahead of the enqueue, not from
        // it. Create the row so the report lands instead of bouncing as
        // unknown. Digest only: the manual path digests, nothing else.
        if manual && !self.tasks.contains_key(&c.task_id) {
            if let Some(chapter) = Task::chapter_of(&c.task_id) {
                if c.task_id == format!("{}:{chapter}", Stage::Digest.as_str()) {
                    self.ensure_task(chapter, Stage::Digest);
                }
            }
        }
        let outcome = match self.tasks.get(&c.task_id) {
            None => Outcome::Unknown,
            // A digest racer holds the row as well as the primary: the first
            // `ok` wins and every later report, racer or primary, finds a
            // row it no longer owns and is dropped here, strike-free.
            Some(t)
                if !manual
                    && t.assigned_to.as_deref() != Some(c.worker_id.as_str())
                    && !t.racers.iter().any(|r| r == &c.worker_id) =>
            {
                Outcome::Stale
            }
            Some(t) => match (&c.crawl, c.ok) {
                // Before `ok`: an absent chapter is reported successfully (there
                // was nothing to fail at), and either way it is terminal and
                // strike-free.
                (Some(r), _) if r.verdict == bm_proto::CrawlVerdict::Absent => Outcome::Absent {
                    chapter: t.chapter,
                    reason: if r.detail.is_empty() {
                        "the site has no chapter here".into()
                    } else {
                        r.detail.clone()
                    },
                },
                // A refusal the crawler itself classified as terminal, a bot
                // check it knows will not clear, a login wall, a gone page.
                // Retrying is what the class says not to do.
                (Some(r), false) if !r.retryable() => Outcome::Shelved {
                    task_id: c.task_id.clone(),
                    detail: format!(
                        "ch{} blocked [{}]: {}",
                        t.chapter,
                        if r.class.is_empty() {
                            "unknown"
                        } else {
                            &r.class
                        },
                        r.detail
                    ),
                },
                (_, true) => Outcome::Done {
                    chapter: t.chapter,
                    stage: t.stage,
                    delta: c.bible_delta.clone(),
                    script: c.script.clone(),
                    text: c.text.clone(),
                    mp3_b64: c.mp3_b64.clone(),
                },
                _ => Outcome::Failed,
            },
        };
        match outcome {
            Outcome::Unknown => return format!("unknown task {}", c.task_id),
            Outcome::Stale => {
                return format!("{}: stale report for {} ignored", c.worker_id, c.task_id)
            }
            Outcome::Absent { chapter, reason } => {
                // A chapter the site does not have is *finished work*, not
                // missing work: the crawl has no artifact and never will, so
                // leaving the row `Pending` would re-offer it every run and
                // leaving the digest waiting would stall the whole chain (a
                // digest waits for its predecessor). Both close here, with the
                // reason on the row where `Enter` can show it.
                let now = now_secs();
                for (stage, detail) in [
                    (Stage::Crawl, format!("not on the site: {reason}")),
                    (
                        Stage::Digest,
                        "skipped: the site has no such chapter".to_string(),
                    ),
                ] {
                    let t = self.ensure_task(chapter, stage);
                    if t.state != TaskState::Done {
                        t.state = TaskState::Done;
                        t.detail = detail;
                        t.attempts = 0;
                        t.clear_holders();
                        t.lease_until = None;
                        t.updated = now;
                    }
                }
                self.push_event(
                    "info",
                    format!("ch{chapter} is not on the site ({reason}) — crawl and digest closed"),
                );
                self.save();
                // Closing a chapter may have drained the queue, trip the
                // armed latch, exactly as a completion does.
                self.maybe_auto_shutdown();
                // And: did a queued exclusive write just become runnable? Same
                // moment, same question shape — work landed, ask what is left.
                self.run_exclusive();
                return format!("ch{chapter} absent: {reason}");
            }
            Outcome::Shelved { task_id, detail } => {
                return self.shelve_now(&task_id, &detail);
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
                // evidence, the take's file is. Read out of the **plan**, not
                // re-derived from the script and cast: the plan is what named
                // the file and what the mixer will read, so the gate and the
                // merge ask one question with one answer.
                //
                // **Every row the offer assigned**, not just the one it named.
                // A batch is one report covering N takes, and a gate that
                // checked only the primary would pass a report whose other nine
                // takes never landed, the failure would surface at the merge
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
                    // The merge row exists from here on: `ensure_task` creates
                    // it, unpinned like every other row. A merge runs on
                    // whichever box asks first and pulls the pieces it lacks
                    // from the inductor, so completions record nothing about
                    // who rendered what.
                    self.ensure_task(chapter, Stage::Merge);
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
                // A merge that failed never gets here, which is right, it left
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
                // that has already answered, their leases would then expire
                // into a second render of takes that landed. The grouping is
                // cleared as it is consumed, so it can never settle a later,
                // unrelated report.
                let rows = self.covered_rows(&c.task_id);
                for id in &rows {
                    if let Some(t) = self.tasks.get_mut(id) {
                        t.state = TaskState::Done;
                        t.detail = c.detail.clone();
                        t.assigned_to = None;
                        t.racers.clear();
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
        // A completion may have drained the queue, trip the armed latch.
        self.maybe_auto_shutdown();
        // And: did a queued exclusive write just become runnable? Same
        // moment, same question shape — work landed, ask what is left.
        self.run_exclusive();
        format!(
            "{}: {} {} ({})",
            c.worker_id,
            c.task_id,
            if c.ok { "done" } else { "failed" },
            bm_core::util::head_chars(&c.detail, 120)
        )
    }
}
