//! The render plan in the ledger: **one task per take**.

use super::Inner;
use bm_core::assemble::{reconcile_with, RenderPlan, RenderUnit};
use bm_proto::{now_secs, RenderUnitSpec, Stage, Task, TaskState};
use serde_json::Value;
use std::collections::BTreeSet;

/// The size below which a wav is a half-write, not a take. The same threshold
const MIN_TAKE_BYTES: u64 = 1000;

impl Inner {
    /// The chapter's planned units — the same `plan_render` the renderer runs,
    pub(crate) fn plan_units(&self, chapter: u32) -> Option<Vec<RenderUnit>> {
        self.plan_units_with(chapter, true).ok()
    }

    /// The same, with every failure named.
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
        let policy = bm_core::cast::policy_for_bible(&engine, &self.layout);
        let installed = bm_core::pool::installed_voices(&self.layout);
        let cast = bm_core::cast::load_cast(
            &script_path,
            &self.layout.cast(&engine),
            &self.layout.bible(),
            &policy,
            installed.as_ref(),
            save,
        )
        .with_context(|| format!("loading the {engine:?} cast"))?;
        let local = engine == "vieneu";
        let title = bm_core::assemble::title_speech_for_script(&script_path, &cast, segments);
        let seg_dir = self.layout.seg_dir(&engine, chapter);
        let planned = bm_core::assemble::Planned::plan(segments);
        let mut units =
            bm_core::assemble::plan_render(&planned, &cast, &seg_dir, local, title.as_ref())
                .with_context(|| format!("planning chapter {chapter}"))?;
        // **The engine's ceiling, applied here and not inside the planner.**
        let cap = bm_core::voices::max_temperature(&engine);
        for u in &mut units {
            u.temperature = u.temperature.min(cap);
        }
        Ok(units)
    }

    /// One line naming why [`Inner::plan_units`] refused, for the ledger.
    pub(crate) fn why_unplannable(&self, chapter: u32) -> String {
        match self.plan_units_with(chapter, false) {
            Ok(units) => format!("{chapter} plans to {} unit(s)", units.len()),
            Err(e) => format!("{e:#}"),
        }
    }

    /// Build the chapter's canonical plan, diff it against the stored one,
    pub(crate) fn refresh_render_plan(&mut self, chapter: u32, adopt: bool) -> Option<RenderPlan> {
        let engine = self.settings.engine.clone();
        let units = self.plan_units(chapter)?;
        // The tier is the plan's business because the extension is part of
        let quality = bm_core::assemble::TakeQuality::parse(&self.settings.take_quality);
        let new = RenderPlan::build(chapter, &engine, &units, quality);
        let path = self.layout.plan(chapter);
        let stored = RenderPlan::load(&path);
        let seg_dir = self.layout.seg_dir(&engine, chapter);
        let up = reconcile_with(stored.as_ref(), new, &seg_dir, adopt);
        for f in &up.stale {
            let _ = std::fs::remove_file(seg_dir.join(f));
        }
        // The plan names this chapter's audio now, so anything in the store it
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
    pub(crate) fn materialize_render_takes(&mut self, chapter: u32) -> Option<RenderPlan> {
        self.materialize_render_takes_with(chapter, true)
    }

    /// The invalidation flavour: the plan is rebuilt **without adoption**, so a
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
                    } else if !here && matches!(t.state, TaskState::Assigned | TaskState::Running) {
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
        // The bitrate rides the offer: the sidecar speaks wav, the box that
        let quality = bm_core::assemble::TakeQuality::parse(&self.settings.take_quality);
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
                mp3_kbps: quality.mp3_kbps().unwrap_or(0),
            },
            plan.cast_hash.clone(),
            force,
        ))
    }

    /// Materialise the takes of every chapter the ledger already knows about.
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
    pub(crate) fn adopt_render_plans(&mut self) {
        for (n, _) in self.script_paths() {
            self.materialize_render_takes(n);
        }
    }

    /// Every take of a chapter is `Done` — the merge gate, and the same
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
        if self.render_takes_done(chapter) {
            return files;
        }
        let _ = std::fs::remove_file(self.layout.final_mp3(chapter));
        let now = now_secs();
        // **Only the takes the diff made work.** Resetting the whole chapter
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
            t.affinity = None;
        }
        // And the mix comes back: its inputs moved, so any published mp3 is
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
