//! Scheduler state: the ledger the orchestrator owns.
//!
//! Workers report facts; every transition below is a decision. The ledger is
//! persisted on each mutation, and reconciled from artifacts on startup, so a
//! restart resumes instead of restarting.

use bm_core::{config::Settings, Layout};
use bm_proto::{Machine, Stage, Task};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};

mod design;
mod exclusive;
mod ledger;
mod observe;
mod offer;
mod ops;
mod plan;
mod reconcile;
mod relink;

const LEASE_SECS: [(Stage, u64); 4] = [
    (Stage::Crawl, 600),
    (Stage::Digest, 1200),
    (Stage::Render, 5400),
    (Stage::Merge, 1800),
];

fn lease_for(stage: Stage) -> u64 {
    LEASE_SECS
        .iter()
        .find(|(s, _)| *s == stage)
        .map(|(_, l)| *l)
        .unwrap_or(600)
}

/// How much longer than the stage's own lease one **batched** assignment may
/// run before it counts as stuck.
///
/// A batch is `n` takes of one chapter on one box, so a fixed deadline would
/// expire under a worker that is simply working through a long batch, and the
/// reaper would then hand the same takes to a second box, which re-speaks them
/// (TTS is stochastic, so the two do not even agree on the bytes) for nothing.
///
/// It is not `n ×` the single-take lease either. That value is a "this is
/// definitely stuck" bound rather than an estimate, a take is seconds to a
/// minute, and the render lease is 5400 s, so multiplying it by the largest
/// batch would leave a genuinely dead worker's chapter stranded for most of a
/// day. Growth stops here.
const LEASE_BATCH_MAX_FACTOR: u64 = 4;

/// The lease for an assignment covering `n` takes of `stage`.
fn lease_for_batch(stage: Stage, n: usize) -> u64 {
    let base = lease_for(stage);
    base.saturating_mul(n.max(1) as u64)
        .min(base.saturating_mul(LEASE_BATCH_MAX_FACTOR))
}

/// Reported failures before a row is shelved (parked for an operator).
///
/// Digest gets 15, everything else 3. A digest is two LLM calls plus repairs
/// against a rate-limited tier, flaky in a way a stuck ffmpeg is not, and
/// with racing, each wave of racers re-proves the prompt is bad before the
/// chapter is parked. A low cap there would shelve chapters for tier hiccups.
fn shelve_after(stage: Stage) -> u32 {
    match stage {
        Stage::Digest => 15,
        _ => 3,
    }
}

/// Maximum number of events kept in memory. Older entries fall off the front.
const EVENT_CAP: usize = 200;

/// An event from the scheduler: task completion/failure, lease expiry, operator
/// actions, etc. Surfaced in `/api/state` so the TUI can show them live.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRecord {
    pub id: u64,
    pub ts: u64,
    /// `"info"` | `"ok"` | `"warn"` | `"error"`
    pub level: String,
    pub text: String,
}

pub struct Inner {
    pub layout: Layout,
    pub settings: Settings,
    pub tasks: HashMap<String, Task>,
    /// The binding the ledger's tasks were created under, from the last save.
    /// `None` means unstamped (empty or pre-profile ledger), the next reconcile
    /// adopts the workspace binding. A pre-split stamp reads as a pack-only
    /// binding, so an existing ledger is not treated as a foreign one.
    pub ledger_profile: Option<bm_core::profile::Binding>,
    pub machines: HashMap<String, Machine>,
    pub workers: HashMap<String, String>,
    /// Advertised capabilities per worker, refreshed at every registration.
    /// Drives the render gate: only workers with `render-segments` are
    /// offered render tasks (they upload units; older agents keep files
    /// locally, which the completion gate would fail anyway).
    pub caps: HashMap<String, Vec<String>>,
    pub beats: HashMap<String, bm_proto::Heartbeat>,
    /// Boot time: the orphan pass in `reap` stays quiet for the first 120s
    /// so a reboot never mistakes still-grinding workers (whose beats arrive
    /// within seconds) for dead ones.
    pub started_at: u64,
    /// Ring buffer of scheduler events surfaced to the TUI.
    pub events: VecDeque<EventRecord>,
    next_event_id: u64,
    /// Graceful-stop latch, set by the shutdown op and read on every
    /// heartbeat answer. In-memory only (never in the ledger): a reboot
    /// clears it, so a fresh backend never murders its own workers.
    pub shutdown_requested: bool,
    /// Drain-then-exit arm: when set, the latch above fires on its own the
    /// moment no unfinished task remains. Same memory-only rule.
    pub shutdown_when_idle: bool,
    /// **The dispatcher is held until an operator says go.**
    ///
    /// The broadest gate there is: `offer` returns nothing while held, so a
    /// process that has just loaded a ledger full of pending rows sits and
    /// waits instead of immediately spreading a fleet across them. See
    /// [`Op::Dispatch`](bm_proto::Op::Dispatch) for why that is the default.
    ///
    /// In memory only, like the two shutdown latches above and for the same
    /// reason: `save()` writes an explicit document and this is not in it, so a
    /// restart always comes up held. Persisting it would turn "the fleet was
    /// running last night" into "start automatically on boot", which is the
    /// wrong default for something that spends money and hours.
    pub dispatch_held: bool,
    /// Completed-task ledger behind the Stats pane: per-worker per-stage
    /// counts plus recent per-stage durations for the TUI-side ETA.
    /// In-memory like the beats, a fresh window beats stale history,
    /// the same reason the file estimator only reads the last 20.
    pub stats: StatsAgg,
    /// The exclusive-write queue: surgeries that wait for the work they
    /// would disturb instead of refusing. See [`exclusive`].
    pub(crate) exclusive: Vec<exclusive::Exclusive>,
    /// Ledger rows this build could not deserialise, kept **verbatim** and
    /// re-emitted on every save.
    ///
    /// A row that fails to parse produces no error and no count, and the
    /// `save()` that follows would write the shortened ledger back, turning a
    /// read problem into permanent loss. Carrying the raw `Value`s through
    /// makes that impossible: the row is preserved exactly as written, the
    /// event makes it visible, and a build that *can* read it picks it up
    /// again on the next load. Re-derived from the file on each load, so the
    /// file stays the one source of truth.
    pub unreadable_tasks: Vec<serde_json::Value>,
}

/// Per-worker per-stage completions, with a capped run of durations.
///
/// **`durations` is scoped to one *offer*, not one unit of work**, and that is
/// the number easiest to misuse. For crawl, digest and merge an offer is a
/// chapter; for render an offer is `Settings::render_batch` takes, so the same
/// field holds a *batch's* cost. Its one consumer agrees with it: the Stats
/// pane's ETA column is `median(offer) × (1 - progress)`, both offer-scoped.
/// Feeding this number into anything counted in takes (`:eta` normalises by
/// `units` itself) would be off by the batch size.
#[derive(Debug, Default)]
pub struct StatsAgg {
    counts: HashMap<String, HashMap<String, u64>>,
    durations: HashMap<String, Vec<f64>>,
}

/// Recent samples feeding one stage average. Same window as the file
/// estimator, but truly recent (insertion order, not sorted).
const STATS_WINDOW: usize = 20;

impl StatsAgg {
    pub fn record(&mut self, worker: &str, stage: Stage, secs: f64) {
        *self
            .counts
            .entry(worker.to_string())
            .or_default()
            .entry(stage.as_str().to_string())
            .or_insert(0) += 1;
        if secs > 0.0 {
            let d = self
                .durations
                .entry(stage.as_str().to_string())
                .or_default();
            d.push(secs);
            if d.len() > STATS_WINDOW {
                d.remove(0);
            }
        }
    }

    pub fn summary(&self) -> serde_json::Value {
        let avg: HashMap<_, _> = Stage::ALL
            .iter()
            .filter_map(|st| {
                let d = self.durations.get(st.as_str())?;
                bm_core::eta::median(d).map(|m| (st.as_str().to_string(), m))
            })
            .collect();
        serde_json::json!({ "counts": self.counts, "avg_task_secs": avg })
    }
}

#[cfg(test)]
mod tests;
