//! Scheduler state: the ledger the orchestrator owns.

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
/// It is not `n ×` the single-take lease either. That value is a "this is
/// definitely stuck" bound rather than an estimate, a take is seconds to a
const LEASE_BATCH_MAX_FACTOR: u64 = 4;

/// The lease for an assignment covering `n` takes of `stage`.
fn lease_for_batch(stage: Stage, n: usize) -> u64 {
    let base = lease_for(stage);
    base.saturating_mul(n.max(1) as u64)
        .min(base.saturating_mul(LEASE_BATCH_MAX_FACTOR))
}

/// Reported failures before a row is shelved (parked for an operator).
fn shelve_after(stage: Stage) -> u32 {
    match stage {
        Stage::Digest => 15,
        _ => 3,
    }
}

/// Maximum number of events kept in memory. Older entries fall off the front.
const EVENT_CAP: usize = 200;

/// An event from the scheduler: task completion/failure, lease expiry, operator
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
    pub ledger_profile: Option<bm_core::profile::Binding>,
    pub machines: HashMap<String, Machine>,
    pub workers: HashMap<String, String>,
    /// Advertised capabilities per worker, refreshed at every registration.
    pub caps: HashMap<String, Vec<String>>,
    pub beats: HashMap<String, bm_proto::Heartbeat>,
    /// Boot time: the orphan pass in `reap` stays quiet for the first 120s
    pub started_at: u64,
    /// Ring buffer of scheduler events surfaced to the TUI.
    pub events: VecDeque<EventRecord>,
    next_event_id: u64,
    /// Graceful-stop latch, set by the shutdown op and read on every
    pub shutdown_requested: bool,
    /// Drain-then-exit arm: when set, the latch above fires on its own the
    pub shutdown_when_idle: bool,
    /// **The dispatcher is held until an operator says go.**
    /// restart always comes up held. Persisting it would turn "the fleet was
    /// running last night" into "start automatically on boot", which is the
    pub dispatch_held: bool,
    /// Completed-task ledger behind the Stats pane: per-worker per-stage
    pub stats: StatsAgg,
    /// The exclusive-write queue: surgeries that wait for the work they
    pub(crate) exclusive: Vec<exclusive::Exclusive>,
    /// Ledger rows this build could not deserialise, kept **verbatim** and
    pub unreadable_tasks: Vec<serde_json::Value>,
}

/// Per-worker per-stage completions, with a capped run of durations.
#[derive(Debug, Default)]
pub struct StatsAgg {
    counts: HashMap<String, HashMap<String, u64>>,
    durations: HashMap<String, Vec<f64>>,
}

/// Recent samples feeding one stage average. Same window as the file
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
