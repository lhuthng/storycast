//! Exclusive writes: the surgeries that wait for the work they would
//! disturb, instead of refusing.
//!
//! `op_swap_voice` and friends used to call `ensure_idle`, which refuses
//! whenever **any** row is assigned or running with a live beat — whatever
//! its stage. So a swap was refused because a *crawl* was running, work a
//! swap cannot possibly disturb, and the operator's only move was to keep
//! retrying by hand until the cluster happened to be quiet. `op_recast`
//! hand-rolled its own per-chapter guard, and `sound-changed` had no guard
//! at all.
//!
//! The shape now: each surgery declares the chapters it invalidates, and
//! waits only on those. [`bm_proto::ExclusiveOp`] carries the scope
//! (`stages` / `blocks`); this module holds the queue, the delivery gate
//! the scheduler consults, and the executor that runs the surgery when the
//! way is clear.
//!
//! Three properties the whole design hangs on:
//!
//! * **Validate at ask time, not run time.** A typo'd voice must fail now,
//!   where the operator is looking, not five minutes later when the queue
//!   drains. So enqueueing runs the same checks the surgery will run, and
//!   only the *waiting* is deferred. The chapter scans themselves are
//!   re-run by the surgery at run time — its own invalidation loop — so a
//!   chapter that enters the scope between ask and run is caught by the
//!   apply, never missed by the gate.
//! * **One at a time.** Two swaps interleaved, or a swap under a remix, is
//!   exactly the concurrency the old refusal existed to prevent. A second
//!   entry lines up behind the first.
//! * **The gate and the surgery cannot disagree.** The same `ExclusiveOp`
//!   that pauses delivery decides when it is quiet — there is no second
//!   list to drift.

use super::Inner;
use bm_proto::{now_secs, ExclusiveOp, TaskState};

/// One queued surgery.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Exclusive {
    /// The write to run, with its chapter scope already computed.
    pub op: ExclusiveOp,
    /// The operator's own route (`swap-voice`), for messages and for
    /// `:xdrop swap-voice`.
    pub label: String,
    /// When it was asked, for the status line.
    pub queued: u64,
}

impl Inner {
    /// Queue an exclusive write, or run it at once when nothing is in the
    /// way. Returns the message the operator sees.
    pub fn exclusive_request(&mut self, mut op: ExclusiveOp) -> anyhow::Result<String> {
        let label = op.route().to_string();
        // The chapter scope, computed at ask time from the same predicate
        // the surgery's own invalidation will use at run time — a swap's
        // scope is the chapters that hear the speaker, a merge's the
        // chapters that hear the absorbed names. Nothing blocks on a
        // chapter the write cannot reach.
        let mut scope: Vec<u32> = Vec::new();
        match &op {
            ExclusiveOp::SwapVoice { character, voice, .. } => {
                if character.trim().is_empty() || voice.trim().is_empty() {
                    anyhow::bail!("swap needs a character and a voice");
                }
                self.validate_swap(character, voice)?;
                scope = self.chapters_hearing_speaker(character);
            }
            ExclusiveOp::Merge { survivor, absorbed, .. } => {
                if survivor.trim().is_empty() || absorbed.is_empty() {
                    anyhow::bail!("merge requires a survivor and at least one absorbed name");
                }
                self.validate_merge(survivor, absorbed)?;
                scope = self.chapters_hearing_speaker(survivor);
                for a in absorbed {
                    scope.extend(self.chapters_hearing_speaker(a));
                }
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
            ExclusiveOp::Remerge | ExclusiveOp::Rerender | ExclusiveOp::Retag { .. } => {}
        }
        // Carry the scope on the arms that use one.
        if let Some(chapters) = match &mut op {
            ExclusiveOp::SwapVoice { chapters, .. }
            | ExclusiveOp::Merge { chapters, .. } => Some(chapters),
            _ => None,
        } {
            *chapters = scope;
        }
        // Way clear? Run now, exactly as the old direct path did — the
        // queue is the *waiting* half, never a second code path.
        if self.exclusive_clear(&op) {
            return self.apply_exclusive(op);
        }
        // Someone is already queued: line up behind them. FIFO, one at a
        // time — interleaving two surgeries is the concurrency the old
        // refusal existed to prevent.
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

    /// Whether nothing in the ledger would race `op` right now.
    ///
    /// Waits on **live holders only** — the same 30-second beat rule
    /// `ensure_idle` used — because a wedged box that stopped beating is
    /// not going to finish, and the queue must never be hostage to it.
    /// `x` / `X` / `A` on the ledger are how a stuck row is taken back.
    pub(crate) fn exclusive_clear(&self, op: &ExclusiveOp) -> bool {
        let now = now_secs();
        let fresh = |ts: u64| now.saturating_sub(ts) < 30;
        !self.tasks.values().any(|t| {
            op.blocks(t.stage, t.chapter)
                && matches!(t.state, TaskState::Assigned | TaskState::Running)
                && t.holders()
                    .iter()
                    .any(|w| self.beats.get(*w).map(|b| fresh(b.ts)).unwrap_or(false))
        })
    }

    /// What the gate is waiting on, for the operator's message: the live
    /// rows in the write's way, by id and holder.
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
            0 => "waiting for a beat".into(),
            n if n > 4 => format!("{} row(s) in the way, e.g. {}", n, rows[..4].join(", ")),
            n => format!("{n} row(s) in the way: {}", rows.join(", ")),
        }
    }

    /// Run the queued writes whose way is now clear.
    ///
    /// Called from beside the idle-latch hook — the places a task lands,
    /// where the inductor already asks "is anything left". One at a time:
    /// after the head runs, the next may still be blocked (and a `remix`
    /// queued behind a `swap` will want the chapters the swap just
    /// re-opened to finish), so the loop stops at the first blocked head.
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
    /// the cluster-wide guard — the gate above has already cleared exactly
    /// the chapters and stages this write touches, and re-checking it
    /// cluster-wide here would error the write over a crawl it cannot
    /// reach. Each `*_apply` is the direct op's body; nothing here may
    /// consult the queue.
    fn apply_exclusive(&mut self, op: ExclusiveOp) -> anyhow::Result<String> {
        match op {
            ExclusiveOp::SwapVoice {
                character,
                voice,
                ..
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
            } => self.op_recast(chapter, &fixes, &remove),
            ExclusiveOp::FixSpeaker {
                chapter,
                segment,
                expect,
                speaker,
            } => self.op_fix_speaker(chapter, segment, &expect, &speaker),
            ExclusiveOp::Merge {
                survivor,
                absorbed,
                ..
            } => self.apply_reconcile(&[(survivor, absorbed)], true),
        }
    }

    /// Drop the queued writes: everything, or the ones naming `route`.
    /// Returns how many went, for the operator's message.
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
    /// runs, minus the file surgery (`bake_missing_voices` merges the
    /// store into the bake, which must happen at run time where its
    /// result is used). A voice that is neither preset nor enrolled is
    /// refused now, where the operator is looking.
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
        let pooled = bm_core::pool::load_pool(&self.layout.root.join("voice-pool.json"))
            .contains_key(&voice);
        let manifested = bm_core::pool::load_manifest(&self.layout.root).contains_key(&voice);
        if !(declared || in_use || pooled || manifested) {
            anyhow::bail!("voice {voice:?} is neither a preset nor an enrolled clone");
        }
        if cast.get(character).is_none() && !character.trim().is_empty() {
            // A speaker the scripts have never heard is still allowed (the
            // swap just records them); nothing to check there.
        }
        Ok(())
    }

    /// The ask-time half of a merge: the bible checks `apply_reconcile`
    /// runs, done now so a bad name refuses while asked.
    fn validate_merge(&self, survivor: &str, absorbed: &[String]) -> anyhow::Result<()> {
        let bible = bm_core::digest::load_bible(&self.layout.bible());
        let survivor = bm_core::digest::resolve_speaker(&bible, survivor);
        let entry = bible
            .get("characters")
            .and_then(|c| c.as_array())
            .into_iter()
            .flatten()
            .find(|c| {
                c.get("name").and_then(|n| n.as_str())
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
