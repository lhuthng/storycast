//! Exclusive writes: the surgeries that wait for the work they would

use super::Inner;
use bm_proto::{now_secs, ExclusiveOp, Stage, TaskState};

/// One queued surgery.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Exclusive {
    /// The write to run, with its chapter scope already computed.
    pub op: ExclusiveOp,
    /// The operator's own route (`swap-voice`), for messages and for
    pub label: String,
    /// When it was asked, for the status line.
    pub queued: u64,
}

impl Inner {
    /// Queue an exclusive write, or run it at once when nothing is in the
    pub fn exclusive_request(&mut self, mut op: ExclusiveOp) -> anyhow::Result<String> {
        let label = op.route().to_string();
        // The chapter scope, computed at ask time from the same predicate
        let mut scope: Vec<u32> = Vec::new();
        match &op {
            ExclusiveOp::SwapVoice {
                character, voice, ..
            } => {
                if character.trim().is_empty() || voice.trim().is_empty() {
                    anyhow::bail!("swap needs a character and a voice");
                }
                self.validate_swap(character, voice)?;
                scope = self.chapters_hearing_speaker(character);
            }
            ExclusiveOp::Merge {
                survivor, absorbed, ..
            } => {
                if survivor.trim().is_empty() || absorbed.is_empty() {
                    anyhow::bail!("merge requires a survivor and at least one absorbed name");
                }
                self.validate_merge(survivor, absorbed)?;
                let mut names: Vec<String> = vec![survivor.clone()];
                names.extend(absorbed.iter().cloned());
                scope = self.chapters_hearing_names(&names);
                // Plus the digest half of the blast radius: a chapter whose
                scope.extend(self.chapters_naming_in_text(absorbed));
                scope.sort_unstable();
                scope.dedup();
            }
            ExclusiveOp::Recast { chapter, .. } | ExclusiveOp::FixSpeaker { chapter, .. } => {
                if *chapter == 0 {
                    anyhow::bail!("recast requires a chapter");
                }
                if let ExclusiveOp::FixSpeaker {
                    chapter,
                    segment,
                    expect,
                    speaker,
                } = &op
                {
                    if *segment == 0 || expect.trim().is_empty() || speaker.trim().is_empty() {
                        anyhow::bail!("fix-speaker requires segment, expect and speaker");
                    }
                    let _ = chapter;
                }
            }
            ExclusiveOp::Remix {
                speed,
                effect_volume,
                music_volume,
                inject_volume,
            } => {
                // Resolved at ask time: the parked entry carries final numbers.
                let inj = *inject_volume;
                for (v, what, lo, hi) in [
                    (*speed, "speed", 0.5, 2.0),
                    (*effect_volume, "fx volume", 0.0, 2.0),
                    (*music_volume, "music volume", 0.0, 2.0),
                    (inj, "inject volume", 0.0, 2.0),
                ] {
                    if !v.is_finite() || v < lo || v > hi {
                        anyhow::bail!("{what} must be {lo}\u{2013}{hi}, got {v}");
                    }
                }
            }
            ExclusiveOp::Reconcile { merges, .. } => {
                // No ask-time name validation: these pairs came from the canon
                if merges.is_empty() {
                    anyhow::bail!("reconcile has nothing to fold");
                }
                // One walk for every name in the plan, not one per pair.
                let mut names: Vec<String> = Vec::new();
                for (survivor, absorbed) in merges {
                    names.push(survivor.clone());
                    names.extend(absorbed.iter().cloned());
                }
                names.sort();
                names.dedup();
                scope = self.chapters_hearing_names(&names);
                let absorbs: Vec<String> =
                    merges.iter().flat_map(|(_, a)| a.iter().cloned()).collect();
                scope.extend(self.chapters_naming_in_text(&absorbs));
                scope.sort_unstable();
                scope.dedup();
            }
            ExclusiveOp::Remerge | ExclusiveOp::Rerender => {}
            ExclusiveOp::Retag { .. } => {
                // A retag rewrites **scripts**, so its scope is the chapters it
                scope = self.script_paths().into_iter().map(|(n, _)| n).collect();
            }
        }
        // Carry the scope on the arms that use one.
        if let Some(chapters) = match &mut op {
            ExclusiveOp::SwapVoice { chapters, .. }
            | ExclusiveOp::Merge { chapters, .. }
            | ExclusiveOp::Reconcile { chapters, .. }
            | ExclusiveOp::Retag { chapters } => Some(chapters),
            _ => None,
        } {
            *chapters = scope;
        }
        // Way clear? Run now, exactly as the old direct path did — the
        if self.exclusive_clear(&op) {
            return self.apply_exclusive(op);
        }
        // Someone is already queued: line up behind them. FIFO, one at a
        let behind = self.exclusive.len();
        let describe = op.describe();
        self.exclusive.push(Exclusive {
            op,
            label,
            queued: now_secs(),
        });
        self.save();
        let blocked = self.exclusive_blocked_summary(&self.exclusive[behind].op);
        let msg = if behind > 0 {
            format!("{describe} queued behind {behind} queued write(s) — {blocked}")
        } else {
            format!("{describe} queued — {blocked}")
        };
        self.push_event(
            "info",
            format!("exclusive write queued: {describe} — {blocked}"),
        );
        Ok(msg)
    }

    /// Whether `op` blocks **every** stage of `chapter` — which is exactly
    fn op_owns_whole_chapter(op: &ExclusiveOp, chapter: u32) -> bool {
        Stage::ALL.iter().all(|s| op.blocks(*s, chapter))
    }

    /// Whether nothing in the ledger would race `op` right now.
    pub(crate) fn exclusive_clear(&self, op: &ExclusiveOp) -> bool {
        let now = now_secs();
        let fresh = |ts: u64| now.saturating_sub(ts) < 30;
        let row_in_the_way = self.tasks.values().any(|t| {
            op.blocks(t.stage, t.chapter)
                && matches!(t.state, TaskState::Assigned | TaskState::Running)
                && (t
                    .holders()
                    .iter()
                    .any(|w| self.beats.get(*w).map(|b| fresh(b.ts)).unwrap_or(false))
                    || (matches!(t.stage, Stage::Digest) && now.saturating_sub(t.updated) < 120))
        });
        let beat_in_the_way = self.beats.values().any(|b| {
            fresh(b.ts)
                && b.chapter
                    .is_some_and(|ch| Self::op_owns_whole_chapter(op, ch))
        });
        !row_in_the_way && !beat_in_the_way
    }

    /// What the gate is waiting on, for the operator's message: the live
    pub(crate) fn exclusive_blocked_summary(&self, op: &ExclusiveOp) -> String {
        let now = now_secs();
        let fresh = |ts: u64| now.saturating_sub(ts) < 30;
        let mut rows: Vec<String> = self
            .tasks
            .values()
            .filter(|t| {
                op.blocks(t.stage, t.chapter)
                    && matches!(t.state, TaskState::Assigned | TaskState::Running)
            })
            .map(|t| {
                let holders = t
                    .holders()
                    .into_iter()
                    .filter(|w| self.beats.get(*w).map(|b| fresh(b.ts)).unwrap_or(false))
                    .collect::<Vec<_>>()
                    .join(",");
                format!("{} ({holders})", t.id())
            })
            .collect();
        rows.sort();
        match rows.len() {
            0 => match self.beats.values().find(|b| {
                fresh(b.ts)
                    && b.chapter
                        .is_some_and(|ch| Self::op_owns_whole_chapter(op, ch))
            }) {
                Some(b) => format!(
                    "a worker is still on ch{} ({} at {})",
                    b.chapter.unwrap_or_default(),
                    b.worker_id,
                    b.activity
                ),
                None => "waiting for a beat".into(),
            },
            n if n > 4 => format!("{} row(s) in the way, e.g. {}", n, rows[..4].join(", ")),
            n => format!("{n} row(s) in the way: {}", rows.join(", ")),
        }
    }

    /// Run the queued writes whose way is now clear.
    pub fn run_exclusive(&mut self) {
        if self.exclusive.is_empty() {
            return;
        }
        while let Some(head) = self.exclusive.first() {
            if !self.exclusive_clear(&head.op) {
                break;
            }
            let Exclusive { op, label, .. } = self.exclusive.remove(0);
            match self.apply_exclusive(op) {
                Ok(msg) => self.push_event("ok", format!("{label}: {msg}")),
                Err(e) => self.push_event("error", format!("{label}: {e:#}")),
            }
        }
        self.save();
    }

    /// The surgery itself: the same code the direct path always ran, minus
    fn apply_exclusive(&mut self, op: ExclusiveOp) -> anyhow::Result<String> {
        match op {
            ExclusiveOp::SwapVoice {
                character, voice, ..
            } => self.swap_apply(&character, &voice),
            ExclusiveOp::Remix {
                speed,
                effect_volume,
                music_volume,
                inject_volume,
            } => self.remix_apply(
                Some(speed),
                Some(effect_volume),
                Some(music_volume),
                Some(inject_volume),
            ),
            ExclusiveOp::Remerge => self.remerge_apply(),
            ExclusiveOp::Rerender => self.rerender_apply(),
            ExclusiveOp::Retag { chapters } => self.op_retag_queued(chapters),
            ExclusiveOp::Recast {
                chapter,
                fixes,
                remove,
            } => self.recast_apply(chapter, &fixes, &remove),
            ExclusiveOp::FixSpeaker {
                chapter,
                segment,
                expect,
                speaker,
            } => self.fix_speaker_apply(chapter, segment, &expect, &speaker),
            ExclusiveOp::Merge {
                survivor, absorbed, ..
            } => self.reconcile_apply(&[(survivor, absorbed)], true),
            ExclusiveOp::Reconcile { merges, .. } => self.reconcile_apply(&merges, false),
        }
    }

    /// Drop the queued writes: everything, or the ones naming `route`.
    pub fn exclusive_cancel(&mut self, route: Option<&str>) -> usize {
        let before = self.exclusive.len();
        match route {
            Some(r) => self.exclusive.retain(|e| e.label != r),
            None => self.exclusive.clear(),
        }
        let dropped = before - self.exclusive.len();
        if dropped > 0 {
            self.save();
            self.push_event("info", format!("dropped {dropped} queued write(s)"));
        }
        dropped
    }

    /// The queued writes as the status API carries them.
    pub fn exclusive_snapshot(&self) -> &[Exclusive] {
        &self.exclusive
    }

    /// The ask-time half of a swap: the trust rule the surgery itself
    fn validate_swap(&self, character: &str, voice: &str) -> anyhow::Result<()> {
        let engine = self.settings.engine.clone();
        let policy = bm_core::voices::effective_policy(&engine);
        let voice = bm_core::voices::resolve_voice_name(&engine, voice);
        let cast = bm_core::cast::read_cast(&engine, &self.layout.cast(&engine));
        let declared = policy
            .male
            .iter()
            .chain(&policy.female)
            .chain(&policy.neutral)
            .any(|n| n == &voice);
        let in_use = cast.values().any(|v| v == &voice);
        let pooled = bm_core::pool::load_pool(&self.layout.voice_pool()).contains_key(&voice);
        let manifested =
            bm_core::pool::load_manifest(&self.layout.voices_manifest()).contains_key(&voice);
        if !(declared || in_use || pooled || manifested) {
            anyhow::bail!("voice {voice:?} is neither a preset nor an enrolled clone");
        }
        if cast.get(character).is_none() && !character.trim().is_empty() {
            // A speaker the scripts have never heard is still allowed (the
        }
        Ok(())
    }

    /// The ask-time half of a merge: the bible checks `apply_reconcile`
    fn validate_merge(&self, survivor: &str, absorbed: &[String]) -> anyhow::Result<()> {
        let bible = bm_core::digest::load_bible(&self.layout.bible());
        let survivor = bm_core::digest::resolve_speaker(&bible, survivor);
        let entry = bible
            .get("characters")
            .and_then(|c| c.as_array())
            .into_iter()
            .flatten()
            .find(|c| {
                c.get("name")
                    .and_then(|n| n.as_str())
                    .map(|n| bm_core::util::fold(n) == bm_core::util::fold(&survivor))
                    .unwrap_or(false)
            });
        if entry.is_none() {
            anyhow::bail!("survivor {survivor:?} is not in the bible");
        }
        for a in absorbed {
            let a = bm_core::digest::resolve_speaker(&bible, a);
            let known = bible
                .get("characters")
                .and_then(|c| c.as_array())
                .into_iter()
                .flatten()
                .any(|c| {
                    let name_matches = c
                        .get("name")
                        .and_then(|n| n.as_str())
                        .map(|n| bm_core::util::fold(n) == bm_core::util::fold(&a))
                        .unwrap_or(false);
                    let alias_matches = c
                        .get("proper_aliases")
                        .and_then(|p| p.as_array())
                        .into_iter()
                        .flatten()
                        .filter_map(|x| x.as_str())
                        .any(|x| bm_core::util::fold(x) == bm_core::util::fold(&a));
                    name_matches || alias_matches
                });
            if !known {
                anyhow::bail!("{a:?} is neither a bible entry nor a cast voice");
            }
        }
        Ok(())
    }
}
