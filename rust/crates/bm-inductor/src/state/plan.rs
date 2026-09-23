//! The render plan in the ledger: **one task per take**.
//!
//! A render used to be one task for a whole chapter. The offer then had to ship
//! every unit and a `render_force` list, because the worker's store is invisible
//! from here and a filename like `0007_Đức Trí.wav` does not prove *what text*
//! is inside it. That is what "send the whole chapter and hope" meant: a local
//! edit re-offered forty units to re-speak one, and the force-list existed
//! because the only way to make a warm box re-speak was to tell it to.
//!
//! A take's name is now **content-addressed** (`t-<take_key>.wav`, where the
//! key hashes voice, text, parameters and engine), so the file *is* the proof:
//! a box that holds it holds the right bytes, and a changed input has a
//! different name. The plan (`data/render-NN.json`) records the current key and
//! file for every take, and every question — what is missing, what is stale,
//! what the mix is — is read from it rather than re-derived.
//!
//! Consequences, all of them the point:
//!
//! * An offer carries **one take**: its voice, text and parameters are
//!   sufficient, so the worker needs neither the script nor the cast, and a
//!   local edit costs one segment instead of a chapter.
//! * `render_force` becomes unnecessary — the take's file is either there (and
//!   correct) or it is not.
//! * The merge gate is the plan's coverage — every take's task `Done`, which is
//!   the same question the mixer asks with the same plan, so the two cannot
//!   disagree.
//!
//! ## Adoption, not invalidation
//!
//! A chapter that has no stored plan predates this module, and writing one must
//! not re-speak the library: the first plan **adopts** every wav already on disk
//! and marks only the genuinely absent takes dirty. From the first edit onward
//! the diff is exact. See `bm_core::assemble::reconcile`.

use super::Inner;
use bm_core::assemble::{reconcile_with, RenderPlan, RenderUnit};
use bm_proto::{now_secs, RenderUnitSpec, Stage, Task, TaskState};
use serde_json::Value;
use std::collections::BTreeSet;

/// The size below which a wav is a half-write, not a take. The same threshold
/// the renderer, the plan and the mixer use.
const MIN_TAKE_BYTES: u64 = 1000;

impl Inner {
    /// The chapter's planned units — the same `plan_render` the renderer runs,
    /// from the same script, cast and bible, so the plan and the renderer can
    /// never disagree about order, grouping or voices.
    ///
    /// **Persists the cast** (`save = true`), unlike every read-only prover.
    /// The units it returns name the files the worker will write, and a
    /// filename embeds the voice — so a decision that is not written down is
    /// recomputed later by the completion gate and the merger from whatever the
    /// cast file happens to say then. The assignment is least-used over the
    /// whole file, so *any* other chapter's write moves it: the worker's 32
    /// files land, the gate recomputes 20 different names, and the chapter
    /// reports `incomplete` while the audio sits on disk. Planning is the
    /// moment the voices are decided; this is where they are frozen.
    ///
    /// `None` when the chapter cannot be planned here (missing script,
    /// unparseable JSON, uncast speaker) — see [`Inner::why_unplannable`] for
    /// the cause, which the ledger records instead of a shrug.
    pub(crate) fn plan_units(&self, chapter: u32) -> Option<Vec<RenderUnit>> {
        self.plan_units_with(chapter, true).ok()
    }

    /// The same, with every failure named.
    ///
    /// A generic `None` is what made "render ch136 cannot be planned here"
    /// unactionable for a whole run: the chapter might be missing its script,
    /// have an uncast speaker, or hit a read error — three different repairs,
    /// one message. Callers on a failure path take the sentence;
    /// `save = false` is the read-only form a diagnostic sweep uses, because
    /// assigning voices writes the cast.
    pub(crate) fn plan_units_with(
        &self,
        chapter: u32,
        save: bool,
    ) -> anyhow::Result<Vec<RenderUnit>> {
        use anyhow::Context as _;
        let engine = self.settings.engine.clone();
        let script_path = self.layout.script(chapter);
        let text = std::fs::read_to_string(&script_path)
            .with_context(|| format!("reading {}", script_path.display()))?;
        let data: Value = serde_json::from_str(&text)
            .with_context(|| format!("parsing {}", script_path.display()))?;
        let segments = data
            .get("segments")
            .and_then(|s| s.as_array())
            .with_context(|| format!("{} has no `segments` array", script_path.display()))?;
        let policy = bm_core::cast::policy_for_bible(&engine);
        let cast = bm_core::cast::load_cast(
            &script_path,
            &self.layout.cast(&engine),
            &self.layout.bible(),
            &policy,
            save,
        )
        .with_context(|| format!("loading the {engine:?} cast"))?;
        let local = engine == "vieneu";
        let title = bm_core::assemble::title_speech_for_script(&script_path, &cast, segments);
        let seg_dir = self.layout.seg_dir(&engine, chapter);
        let planned = bm_core::assemble::Planned::plan(segments);
        bm_core::assemble::plan_render(&planned, &cast, &seg_dir, local, title.as_ref())
            .with_context(|| format!("planning chapter {chapter}"))
    }

    /// One line naming why [`Inner::plan_units`] refused, for the ledger.
    ///
    /// The read-only form: it must not assign voices as a side effect of
    /// reporting that this chapter cannot be planned.
    pub(crate) fn why_unplannable(&self, chapter: u32) -> String {
        match self.plan_units_with(chapter, false) {
            Ok(units) => format!("{chapter} plans to {} unit(s)", units.len()),
            Err(e) => format!("{e:#}"),
        }
    }

    /// Build the chapter's canonical plan, diff it against the stored one,
    /// delete what the diff superseded, and write the plan back.
    ///
    /// The deletion is the whole of "invalidate": a take whose inputs changed
    /// has a new content-addressed name, so the old file is named by the diff
    /// rather than guessed from a voice string. A take whose inputs did *not*
    /// change keeps its file — which is why a one-line retag now costs one
    /// segment instead of a chapter.
    ///
    /// `adopt` is false on an **invalidation**, where the caller knows an input
    /// changed: a legacy file matching the new legacy name there is a
    /// coincidence rather than evidence, so the take is work and the old bytes
    /// are superseded. It is true on the routine pass (startup reconcile),
    /// which must not re-speak a library that predates the plan.
    pub(crate) fn refresh_render_plan(&mut self, chapter: u32, adopt: bool) -> Option<RenderPlan> {
        let engine = self.settings.engine.clone();
        let units = self.plan_units(chapter)?;
        let new = RenderPlan::build(chapter, &engine, &units);
        let path = self.layout.plan(chapter);
        let stored = RenderPlan::load(&path);
        let seg_dir = self.layout.seg_dir(&engine, chapter);
        let up = reconcile_with(stored.as_ref(), new, &seg_dir, adopt);
        for f in &up.stale {
            let _ = std::fs::remove_file(seg_dir.join(f));
        }
        // The plan names this chapter's audio now, so anything in the store it
        // does not name is not part of the mix: a file from before the plan
        // existed, or from a previous naming scheme. The diff alone cannot
        // reach those — with no stored plan there is no record to diff against
        // — which is how a first plan after a voice swap would leave the old
        // voice's bytes sitting next to the new ones. In-flight transfers write
        // `*.wav.incoming`, so a half-arrived take is never swept.
        if let Ok(rd) = std::fs::read_dir(&seg_dir) {
            let named: std::collections::HashSet<&str> =
                up.plan.takes.iter().map(|t| t.file.as_str()).collect();
            for name in rd.filter_map(|e| e.ok()).filter_map(|e| {
                let n = e.file_name();
                let n = n.to_str()?.to_string();
                n.ends_with(".wav").then_some(n)
            }) {
                if !named.contains(name.as_str()) {
                    let _ = std::fs::remove_file(seg_dir.join(&name));
                }
            }
        }
        let _ = up.plan.save(&path);
        Some(up.plan)
    }

    /// The chapter's plan, materialised into the ledger as one `render:ch:pos`
    /// row per take. A take whose file is on disk is `Done`; the rest are work.
    ///
    /// This is also the migration: a pre-per-take `render:7` row is replaced by
    /// its takes, and a take the new plan no longer contains loses its row.
    pub(crate) fn materialize_render_takes(&mut self, chapter: u32) -> Option<RenderPlan> {
        self.materialize_render_takes_with(chapter, true)
    }

    /// The invalidation flavour: the plan is rebuilt **without adoption**, so a
    /// chapter that has no stored plan — or one whose stored plan the change
    /// supersedes — has every take it cannot prove current marked as work.
    pub(crate) fn replan_render_takes(&mut self, chapter: u32) -> Option<RenderPlan> {
        self.materialize_render_takes_with(chapter, false)
    }

    fn materialize_render_takes_with(&mut self, chapter: u32, adopt: bool) -> Option<RenderPlan> {
        let engine = self.settings.engine.clone();
        let plan = self.refresh_render_plan(chapter, adopt)?;
        let seg_dir = self.layout.seg_dir(&engine, chapter);
        let now = now_secs();
        // The chapter-granular row is superseded by its takes.
        self.tasks.remove(&format!("{}:{chapter}", Stage::Render));
        let mut live: Vec<usize> = Vec::with_capacity(plan.takes.len());
        for take in &plan.takes {
            live.push(take.pos);
            let here = present(&seg_dir, &take.file);
            let id = format!("{}:{chapter}:{}", Stage::Render, take.pos);
            match self.tasks.get_mut(&id) {
                Some(t) => {
                    // Ground truth in both directions, and the assignment check
                    // reconcile used to run per stage: a file that arrived is
                    // `Done` (a live worker's report still counts, an in-flight
                    // take is verified by its artifact), one that was deleted —
                    // or superseded by a changed key — is work again, and an
                    // unverified assignment keeps its owner with a fresh lease.
                    if here
                        && matches!(
                            t.state,
                            TaskState::Pending | TaskState::Assigned | TaskState::Running
                        )
                    {
                        t.state = TaskState::Done;
                        t.clear_holders();
                        t.lease_until = None;
                        t.updated = now;
                    } else if !here && t.state == TaskState::Done {
                        t.state = TaskState::Pending;
                        t.updated = now;
                    } else if !here
                        && matches!(t.state, TaskState::Assigned | TaskState::Running)
                    {
                        t.lease_until = Some(now + super::lease_for(Stage::Render));
                        t.updated = now;
                    }
                }
                None => {
                    let mut t = Task::new_take(chapter, take.pos);
                    if here {
                        t.state = TaskState::Done;
                    }
                    t.updated = now;
                    self.tasks.insert(id, t);
                }
            }
        }
        let stale: Vec<String> = self
            .tasks
            .iter()
            .filter(|(_, t)| t.stage == Stage::Render && t.chapter == chapter)
            .filter(|(_, t)| t.take.map(|p| !live.contains(&p)).unwrap_or(true))
            .map(|(k, _)| k.clone())
            .collect();
        for k in stale {
            self.tasks.remove(&k);
        }
        Some(plan)
    }

    /// The offer's payload for one take: everything needed to speak exactly
    /// that file, and nothing else. Returns the unit, the chapter's voice
    /// collection hash, and the filenames the worker must speak **even if it
    /// already holds them of that name**.
    ///
    /// That force list is empty in the normal case, and that is the point:
    /// with a content-addressed name, holding the file is proof of holding the
    /// right bytes, so "has it" and "has the right audio" are one question.
    /// The exception is an **adopted** take — a pre-plan cache file still under
    /// its legacy `{tag}_{voice}.wav` name — which carries no such proof and
    /// whose text may have changed before the plan existed. Only those, and
    /// only when this store lacks them, need forcing; forcing every adopted
    /// take would re-speak the whole library on the first run.
    ///
    /// `None` when the plan cannot be read or the take is not in it — the
    /// caller says so instead of guessing a name.
    pub(crate) fn take_spec(
        &self,
        chapter: u32,
        pos: Option<usize>,
    ) -> Option<(RenderUnitSpec, String, Vec<String>)> {
        let plan = RenderPlan::load(&self.layout.plan(chapter))?;
        let take = plan.takes.get(pos?)?;
        let engine = self.settings.engine.clone();
        let here = present(&self.layout.seg_dir(&engine, chapter), &take.file);
        let force = if take.adopted && !here {
            vec![take.file.clone()]
        } else {
            Vec::new()
        };
        Some((
            RenderUnitSpec {
                tag: take.tag.clone(),
                name: take.file.clone(),
                speaker: take.speaker.clone(),
                voice: take.voice.clone(),
                text: take.text.clone(),
                temperature: take.temperature,
                silence_p: take.silence_p,
                take_key: take.take_key.clone(),
            },
            plan.cast_hash.clone(),
            force,
        ))
    }

    /// Materialise the takes of every chapter the ledger already knows about.
    ///
    /// `reconcile`'s `--start/--count` bounds which chapters get their *first*
    /// row; it must not bound *repair*. A render row left by an earlier run
    /// lives outside a narrow range, and a take is only offerable when its plan
    /// names its file — so without this pass `make serve` (the `COUNT=100`
    /// default) against a 150-chapter ledger leaves the tail un-runnable, and
    /// each of those takes fails on the box for a reason that has nothing to do
    /// with the box.
    ///
    /// The stale rows are the tell: a take row with no plan beside it, or the
    /// chapter-granular `render:n` row of an older build. Both are repaired
    /// here. `skip` is the range `reconcile` just walked, so in-range chapters
    /// are not planned twice.
    pub(crate) fn materialize_known_render_takes(&mut self, skip: std::ops::Range<u32>) {
        let mut chapters: Vec<u32> = self
            .tasks
            .values()
            .filter(|t| matches!(t.stage, Stage::Render | Stage::Merge))
            .map(|t| t.chapter)
            .filter(|n| !skip.contains(n))
            .collect();
        chapters.sort_unstable();
        chapters.dedup();
        for n in chapters {
            self.materialize_render_takes(n);
        }
    }

    /// Establish the render plans the routine pass would have written.
    ///
    /// The live inductor does this at every startup (`reconcile`), and an
    /// invalidation is **diffed against it**: with no plan there is nothing to
    /// diff against, so a swap would have to re-speak the whole chapter rather
    /// than the voice that moved. A throwaway `Inner` — the inductor-down
    /// paths in `api` — must therefore take the same step first, or it would
    /// invalidate differently from the live one. Runs before the mutation, so
    /// the adoption records the *old* inputs, which is what makes the diff
    /// afterwards real.
    pub(crate) fn adopt_render_plans(&mut self) {
        for (n, _) in self.script_paths() {
            self.materialize_render_takes(n);
        }
    }

    /// Every take of a chapter is `Done` — the merge gate, and the same
    /// question `RenderPlan::covered` answers for the mixer. A chapter with no
    /// takes is vacuously done, exactly as an empty expected set was before.
    pub(crate) fn render_takes_done(&self, chapter: u32) -> bool {
        self.tasks
            .values()
            .filter(|t| t.stage == Stage::Render && t.chapter == chapter)
            .all(|t| t.state == TaskState::Done)
    }

    /// `render:ch:pos` for a chapter's tasks, oldest take first.
    pub(crate) fn render_take_ids(&self, chapter: u32) -> Vec<String> {
        let mut ids: Vec<(usize, String)> = self
            .tasks
            .values()
            .filter(|t| t.stage == Stage::Render && t.chapter == chapter)
            .map(|t| (t.take.unwrap_or(0), t.id()))
            .collect();
        ids.sort();
        ids.into_iter().map(|(_, id)| id).collect()
    }

    /// Requeue every take of a chapter. Attempts reset — an invalidation is new
    /// work, not a retry.
    pub(crate) fn reset_render_takes(&mut self, chapter: u32, why: &str) {
        let now = now_secs();
        for t in self.tasks.values_mut() {
            if t.stage == Stage::Render && t.chapter == chapter {
                t.state = TaskState::Pending;
                t.attempts = 0;
                t.clear_holders();
                t.lease_until = None;
                t.detail = why.to_string();
                t.updated = now;
                // Requeued work is offerable to any box — never re-pinned.
                t.affinity = None;
            }
        }
    }

    /// Apply a local edit's plan diff and requeue what it changed, unpinned so
    /// any box speaks the changed takes. Returns the number of files the diff
    /// superseded.
    ///
    /// `None` from the planner means the chapter cannot be planned here; the
    /// caller's edit is left alone rather than acted on from a guess.
    pub(crate) fn resume_render_after_edit(&mut self, chapter: u32, why: &str) -> u32 {
        let path = self.layout.plan(chapter);
        let before: BTreeSet<String> = RenderPlan::load(&path)
            .map(|p| p.files().into_iter().collect())
            .unwrap_or_default();
        let Some(plan) = self.replan_render_takes(chapter) else {
            return 0;
        };
        let after: BTreeSet<String> = plan.files().into_iter().collect();
        let files = before.difference(&after).count() as u32;
        // Every take still on disk under its current key: this edit did not
        // reach the chapter, so there is nothing to speak and nothing to mix
        // again. Requeueing here would be the churn the plan exists to remove.
        if self.render_takes_done(chapter) {
            return files;
        }
        let _ = std::fs::remove_file(self.layout.final_mp3(chapter));
        let now = now_secs();
        // **Only the takes the diff made work.** Resetting the whole chapter
        // here would re-offer every unchanged segment; the plan already knows
        // which ones moved, so a retag costs the retagged runs and nothing
        // else. No pin: any box speaks them, and the units land on the
        // inductor before their completions are applied.
        for t in self.tasks.values_mut() {
            if t.stage != Stage::Render || t.chapter != chapter || t.state != TaskState::Pending {
                continue;
            }
            t.attempts = 0;
            t.clear_holders();
            t.lease_until = None;
            t.detail = why.to_string();
            t.updated = now;
            // And any pin from the batch-pinning era goes: takes are
            // independent, so a stale pin would only hide them from idle
            // boxes again.
            t.affinity = None;
        }
        // And the mix comes back: its inputs moved, so any published mp3 is
        // stale. Unpinned like everything else — whichever box asks first
        // merges it, pulling the pieces it lacks.
        let key = format!("{}:{chapter}", Stage::Merge);
        match self.tasks.get_mut(&key) {
            Some(t) => {
                t.state = TaskState::Pending;
                t.attempts = 0;
                t.clear_holders();
                t.lease_until = None;
                t.detail = why.to_string();
                t.updated = now;
                t.affinity = None;
            }
            None => {
                let mut t = Task::new(chapter, Stage::Merge);
                t.detail = why.to_string();
                t.updated = now;
                self.tasks.insert(key, t);
            }
        }
        files
    }
}

/// A take on disk, as opposed to a half-write the sweeper must not adopt.
///
/// `pub(super)` because the completion gate asks the same question of the same
/// file, and a second copy of the threshold is how the planner and the gate
/// come to disagree about what "the take landed" means.
pub(super) fn present(seg_dir: &std::path::Path, name: &str) -> bool {
    seg_dir
        .join(name)
        .metadata()
        .map(|m| m.len() > MIN_TAKE_BYTES)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Local-only diagnostic (not CI): point at a real workspace and sweep
    /// every chapter the renderer would plan, naming the cause for the ones it
    /// cannot. Read-only — `save = false` — so auditing a live workspace
    /// cannot reassign voices and invalidate the audio it is inspecting.
    ///
    ///     BM_DEBUG_ROOT=/path/to/repo ~/.cargo/bin/cargo test -p bm-inductor \
    ///         --bin bm-inductor sweep -- --nocapture
    #[test]
    fn sweep_planning_against_a_real_workspace() {
        let Ok(root) = std::env::var("BM_DEBUG_ROOT") else {
            return;
        };
        let layout = bm_core::Layout::resolve(std::path::PathBuf::from(root))
            .expect("resolve the active workspace");
        let settings = bm_core::config::Settings::load(&layout.settings());
        eprintln!(
            "root={} work={} engine={}",
            layout.root.display(),
            layout.work.display(),
            settings.engine
        );
        let inner = Inner::new(layout.clone(), settings);
        let only: Option<u32> = std::env::var("BM_DEBUG_CH")
            .ok()
            .and_then(|v| v.parse().ok());
        let chapters: Vec<u32> = inner
            .script_paths()
            .into_iter()
            .map(|(n, _)| n)
            .filter(|n| only.map(|c| c == *n).unwrap_or(true))
            .collect();
        let (mut ok, mut units) = (0u32, 0usize);
        let mut bad: Vec<(u32, String)> = Vec::new();
        for n in &chapters {
            match inner.plan_units_with(*n, false) {
                Ok(u) => {
                    ok += 1;
                    units += u.len();
                }
                Err(e) => bad.push((*n, format!("{e:#}"))),
            }
        }
        eprintln!(
            "planned {ok}/{} chapter(s), {units} take(s); {} unplannable",
            chapters.len(),
            bad.len()
        );
        for (n, why) in &bad {
            eprintln!("ch{n}: {why}");
        }
        assert!(chapters.is_empty() || ok > 0, "nothing plans: {bad:?}");
    }
}
