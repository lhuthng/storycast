use super::*;

impl Inner {
    /// Rewrite written-out non-verbal sounds into engine tags across every
    /// script (`Ha ha ha!` → `[cười]`), and requeue the chapters it touches.
    /// Deterministic — no LLM, same mapping as the prompt's rule 9 — so a
    /// re-run is a no-op once every script is clean.
    ///
    /// The live API only ever calls this with `dry_run = true` (a read-only
    /// report of what would change), and queues
    /// [`bm_proto::ExclusiveOp::Retag`] for the write, so the cluster-wide
    /// guard stands only for a direct caller: a test, or the offline path.
    pub fn op_retag(&mut self, dry_run: bool) -> anyhow::Result<String> {
        if !dry_run {
            self.ensure_idle()?;
        }
        self.retag_chapters(None, dry_run)
    }


    /// The queued retag: the same rewrite, gated by the queue instead of
    /// `ensure_idle` — it runs only once the digests (and renders) it could
    /// disturb have gone quiet. `chapters` is the ask-time scope; the
    /// per-chapter plan diff at write time still decides what actually moves.
    pub(crate) fn op_retag_queued(&mut self, chapters: Vec<u32>) -> anyhow::Result<String> {
        let scope = if chapters.is_empty() {
            None
        } else {
            Some(chapters)
        };
        self.retag_chapters(scope, true)
    }


    /// The retag body, shared by the direct op (which gates on
    /// `ensure_idle`) and the queued one (which the exclusive gate has
    /// already cleared). `scope` = `None` for every script, or exactly the
    /// ask-time chapters.
    fn retag_chapters(&mut self, scope: Option<Vec<u32>>, dry_run: bool) -> anyhow::Result<String> {
        let mut chapters: Vec<u32> = Vec::new();
        let mut edits = 0u32;
        let mut files = 0u32;
        let mut detail: Vec<String> = Vec::new();
        for (n, sp) in self
            .script_paths()
            .into_iter()
            .filter(|(n, _)| scope.as_ref().map(|s| s.contains(n)).unwrap_or(true))
        {
            let mut data: serde_json::Value =
                bm_core::read_json(&sp).unwrap_or(serde_json::Value::Null);
            let owned: Vec<serde_json::Value> = data
                .get("segments")
                .and_then(|s| s.as_array())
                .cloned()
                .unwrap_or_default();
            if owned.is_empty() {
                continue;
            }
            // Headline segments never render (the title file speaks instead),
            // so editing them is churn: skip exactly what `drop_headline` drops.
            let skip = owned.len() - bm_core::assemble::drop_headline(&owned).len();
            let mut touched: Vec<usize> = Vec::new();
            if let Some(segments) = data.get_mut("segments").and_then(|s| s.as_array_mut()) {
                for (i, s) in segments.iter_mut().enumerate() {
                    if i < skip {
                        continue;
                    }
                    let old = s
                        .get("text")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string();
                    if let Some(new) = bm_core::digest::retag_text(&old) {
                        if !dry_run {
                            s["text"] = serde_json::Value::String(new.clone());
                        }
                        touched.push(i);
                        edits += 1;
                        // Capped so one pathological chapter cannot flood the op
                        // message; 200 entries is the whole book in practice.
                        if detail.len() < 200 {
                            detail.push(format!(
                                "ch{n}#{i}: {} → {}",
                                bm_core::util::head_chars(&old, 40),
                                bm_core::util::head_chars(&new, 40)
                            ));
                        }
                    }
                }
            }
            if touched.is_empty() {
                continue;
            }
            chapters.push(n);
            if dry_run {
                continue;
            }
            // Write the edited script, then let the plan's diff say what the
            // edit reached: a retagged run has a new content-addressed name, so
            // its old file is superseded and its take is work again, while
            // every run the edit did not touch keeps its audio. This replaced a
            // hand-rolled "delete the runs holding edited segments" that had to
            // reconstruct `expected_wavs` positions and the title offset to
            // find them — the plan already knows, exactly.
            let _ = bm_core::atomic_write(
                &sp,
                &serde_json::to_string_pretty(&data).unwrap_or_default(),
            );
            files += self.resume_render_after_edit(n, "requeued: retag");
        }
        if !dry_run {
            self.save();
        }
        if chapters.is_empty() {
            return Ok("retag: no written-out sounds found — every script already tags".into());
        }
        Ok(format!(
            "retag: {edits} segments in {} chapters ({:?}){detail_str}; invalidated {files} run files, re-render queued",
            chapters.len(),
            chapters.iter().take(12).collect::<Vec<_>>(),
            detail_str = if detail.is_empty() {
                String::new()
            } else {
                format!(" — e.g. {}", detail.join("; "))
            },
        ))
    }

    pub fn op_fix_speaker(
        &mut self,
        chapter: u32,
        segment: usize,
        expect: &str,
        speaker: &str,
    ) -> anyhow::Result<String> {
        self.ensure_chapter_idle(chapter, "fix the speaker")?;
        self.fix_speaker_apply(chapter, segment, expect, speaker)
    }


    /// The fix-speaker body, guardless — see [`Self::swap_apply`]. The
    /// exclusive gate already held this chapter still, and its scope is every
    /// stage of it, so the chapter guard must not re-run here and refuse a
    /// write the gate picked the moment for.
    pub(crate) fn fix_speaker_apply(
        &mut self,
        chapter: u32,
        segment: usize,
        expect: &str,
        speaker: &str,
    ) -> anyhow::Result<String> {
        let index = segment.saturating_sub(1);
        let to = speaker.trim();
        if to.is_empty() {
            anyhow::bail!("segment {segment}: empty speaker");
        }
        if to == expect.trim() {
            anyhow::bail!("segment {segment} already speaks as {to:?} — nothing to change");
        }
        let engine = self.settings.engine.clone();
        let cast = bm_core::cast::read_cast(&engine, &self.layout.cast(&engine));
        // Checked before the edit, not after: a speaker with no voice is a hard
        // planning error, so writing the script first would leave the chapter
        // unplannable and the requeue with nowhere to go.
        if to != "Narrator" && cast.get(to).is_none() {
            anyhow::bail!(
                "{to:?} holds no voice in the {engine} cast — enrol it (:voices, or roster add-sample) and :prov, or this chapter requeues into a row no box can speak"
            );
        }
        let path = self.layout.script(chapter);
        let mut data: serde_json::Value = bm_core::read_json(&path)
            .map_err(|_| anyhow::anyhow!("ch{chapter} has no script yet — digest it first"))?;
        let segments = data
            .get_mut("segments")
            .and_then(|s| s.as_array_mut())
            .ok_or_else(|| anyhow::anyhow!("ch{chapter} script has no segments array"))?;
        let len = segments.len();
        if index >= len {
            anyhow::bail!(
                "segment {segment} is past the end — ch{chapter} has {len} segment(s), numbered 1..{len}"
            );
        }
        let item = &mut segments[index];
        let here = item
            .get("speaker")
            .and_then(|s| s.as_str())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "segment {segment} is a sound, not a line — it has no speaker to change"
                )
            })?
            .to_string();
        if here != expect.trim() {
            // Name the neighbours, because "wrong number" is the likeliest
            // cause and the fix is one of the numbers printed here.
            let mut where_: Vec<String> = segments
                .iter()
                .enumerate()
                .filter(|(_, s)| s.get("speaker").and_then(|v| v.as_str()) == Some(expect.trim()))
                .map(|(i, _)| (i + 1).to_string())
                .collect();
            where_.truncate(8);
            let near: Vec<String> = (index.saturating_sub(1)..(index + 2).min(len))
                .map(|i| {
                    let s = segments[i]
                        .get("speaker")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?");
                    let text: String = segments[i]
                        .get("text")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .chars()
                        .take(40)
                        .collect();
                    format!("{}: {s:?} “{text}”", i + 1)
                })
                .collect();
            anyhow::bail!(
                "segment {segment} is spoken by {here:?}, not {expect:?} — nothing changed.\n  \
                 nearby: {}\n  \
                 {expect:?} is at: {}",
                near.join(" | "),
                if where_.is_empty() {
                    "nowhere in this chapter".to_string()
                } else {
                    where_.join(", ")
                }
            );
        }
        item["speaker"] = serde_json::Value::String(to.to_string());
        // The roster names who's in the chapter: drop speakers no segment
        // uses anymore, append the new one in segment order. Render and cast
        // assignment read segments too, so a stale roster never broke
        // anything — but the file should not lie about its own contents.
        // Order is preserved: only membership changes.
        {
            let speakers: Vec<String> = segments
                .iter()
                .filter_map(|s| {
                    s.get("speaker")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                })
                .collect();
            if let Some(roster) = data.get_mut("roster").and_then(|r| r.as_array_mut()) {
                roster.retain(|r| r.as_str().is_some_and(|n| speakers.iter().any(|s| s == n)));
                for s in &speakers {
                    if !roster.iter().any(|r| r.as_str() == Some(s.as_str())) {
                        roster.push(serde_json::Value::String(s.clone()));
                    }
                }
            }
        }
        let _ = bm_core::atomic_write(
            &path,
            &serde_json::to_string_pretty(&data).unwrap_or_default(),
        );
        // The count is the plan's diff, taken around the invalidation, because
        // that is the number the operator watches drain. The chapter's take
        // count is not it: a three-take chapter with one segment re-pointed has
        // one take to speak, and reporting three would be a promise the
        // scheduler does not keep.
        let before = self.take_keys(chapter);
        self.invalidate_render(chapter);
        let after = self.take_keys(chapter);
        let fresh = after.iter().filter(|k| !before.contains(k)).count();
        self.save();
        let msg = format!(
            "ch{chapter} segment {segment}: {here} -> {to}; {fresh} take(s) to re-speak, merge requeued"
        );
        self.push_event("ok", msg.clone());
        Ok(msg)
    }


    /// The chapter's take keys, which is what a re-plan diffs. Empty when the
    /// chapter has no plan yet, which makes every take after an edit look new,
    /// and is the honest answer: nothing was recorded to compare against.
    fn take_keys(&self, chapter: u32) -> Vec<String> {
        bm_core::assemble::RenderPlan::load(&self.layout.plan(chapter))
            .map(|p| p.takes.into_iter().map(|t| t.take_key).collect())
            .unwrap_or_default()
    }

    fn ensure_chapter_idle(&self, chapter: u32, verb: &str) -> anyhow::Result<()> {
        for t in self.tasks.values() {
            if t.chapter == chapter && matches!(t.state, TaskState::Assigned | TaskState::Running) {
                anyhow::bail!(
                    "ch{chapter} has {} in flight — wait for it to settle, then {verb}",
                    t.id()
                );
            }
        }
        let now = now_secs();
        for b in self.beats.values() {
            if now.saturating_sub(b.ts) < 30 && b.chapter == Some(chapter) {
                anyhow::bail!(
                    "a worker is on ch{chapter} right now ({} at {}) — wait a beat, then {verb}",
                    b.worker_id,
                    b.activity,
                );
            }
        }
        Ok(())
    }

    /// Re-attribute speakers on one chapter's script, then requeue exactly
    /// what the edit reached.
    ///
    /// The digest's recurring misattribution, confirmed against chapter text:
    /// third-person narration given to the character it describes, and a quote
    /// with no dialogue tag defaulted to Narrator instead of whoever the
    /// surrounding action introduces. The prompt's rule 3 already forbids the
    /// first half word for word — the small model disobeyed it — so
    /// re-digesting rolls the same dice; the correction is surgical.
    ///
    /// The live API queues this ([`bm_proto::ExclusiveOp::Recast`]) and runs
    /// [`Self::recast_apply`], so this guarded entry is compiled for the tests
    /// that cover the refusal itself.
    #[cfg(test)]
    pub fn op_recast(
        &mut self,
        chapter: u32,
        fixes: &[bm_proto::SpeakerFix],
        remove: &[usize],
    ) -> anyhow::Result<String> {
        self.ensure_chapter_idle(chapter, "recast")?;
        self.recast_apply(chapter, fixes, remove)
    }


    /// The recast body, guardless — see [`Self::swap_apply`].
    pub(crate) fn recast_apply(
        &mut self,
        chapter: u32,
        fixes: &[bm_proto::SpeakerFix],
        remove: &[usize],
    ) -> anyhow::Result<String> {
        if fixes.is_empty() && remove.is_empty() {
            anyhow::bail!(
                "nothing to fix — pass segment indexes with their speakers, or indexes to delete"
            );
        }
        let engine = self.settings.engine.clone();
        let cast = bm_core::cast::read_cast(&engine, &self.layout.cast(&engine));
        let path = self.layout.script(chapter);
        let mut data: serde_json::Value = bm_core::read_json(&path)
            .map_err(|_| anyhow::anyhow!("ch{chapter} has no script yet — digest it first"))?;
        let segments = data
            .get_mut("segments")
            .and_then(|s| s.as_array_mut())
            .ok_or_else(|| anyhow::anyhow!("ch{chapter} script has no segments array"))?;
        let mut done: Vec<String> = Vec::new();
        let len = segments.len();
        for f in fixes {
            let item = match segments.get_mut(f.index) {
                Some(item) => item,
                None => anyhow::bail!("segment {} is out of range (0..{})", f.index, len),
            };
            let old = item
                .get("speaker")
                .and_then(|s| s.as_str())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "segment {} is a sound, not a line — nothing to re-attribute",
                        f.index
                    )
                })?
                .to_string();
            let new = f.speaker.trim().to_string();
            if new.is_empty() {
                anyhow::bail!("segment {}: empty speaker", f.index);
            }
            if new != "Narrator" && cast.get(&new).is_none() {
                anyhow::bail!(
                    "{new:?} holds no voice — cast it first (:voices fills gaps), or the chapter requeues into a row no box can speak"
                );
            }
            if old == new {
                continue;
            }
            item["speaker"] = serde_json::Value::String(new.clone());
            done.push(format!("#{} {old}→{new}", f.index));
        }
        // Deletions run after re-attribution and in descending index order,
        // so earlier indexes stay valid while later items leave. Only lines
        // go: sounds hold the mix together and are never the duplication.
        let mut removed: Vec<usize> = Vec::new();
        if !remove.is_empty() {
            let mut order: Vec<usize> = remove.to_vec();
            order.sort_unstable();
            order.dedup();
            for idx in order.iter().rev() {
                let is_line = segments
                    .get(*idx)
                    .and_then(|s| s.get("speaker").and_then(|v| v.as_str()))
                    .is_some();
                if !is_line {
                    anyhow::bail!(
                        "segment {idx} is not a line — only duplicated lines are removed"
                    );
                }
                segments.remove(*idx);
                removed.push(*idx);
            }
            if segments.is_empty() {
                anyhow::bail!("refusing to empty ch{chapter}: at least one segment must remain");
            }
        }
        if done.is_empty() && removed.is_empty() {
            return Ok(format!(
                "recast ch{chapter}: every named speaker already matched — nothing changed"
            ));
        }
        let _ = bm_core::atomic_write(
            &path,
            &serde_json::to_string_pretty(&data).unwrap_or_default(),
        );
        self.invalidate_render(chapter);
        let mut parts = done;
        if !removed.is_empty() {
            parts.push(format!("removed {} duplicated segments", removed.len()));
        }
        Ok(format!(
            "recast ch{chapter}: {}; re-render queued",
            parts.join(", ")
        ))
    }
}


