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
/// expire under a worker that is simply working through a long batch — and the
/// reaper would then hand the same takes to a second box, which re-speaks them
/// (TTS is stochastic, so the two do not even agree on the bytes) for nothing.
///
/// It is not `n ×` the single-take lease either. That value is a "this is
/// definitely stuck" bound rather than an estimate — a take is seconds to a
/// minute, and the render lease is 5400 s — so multiplying it by the largest
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
/// against a rate-limited tier — flaky in a way a stuck ffmpeg is not — and
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
    /// The profile the ledger's tasks were created under, from the last
    /// save. `None` means unstamped (empty or pre-profile ledger) — the next
    /// reconcile adopts the workspace profile.
    pub ledger_profile: Option<bm_core::profile::Pointer>,
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
    /// Completed-task ledger behind the Stats pane: per-worker per-stage
    /// counts plus recent per-stage durations for the TUI-side ETA.
    /// In-memory like the beats — a fresh window beats stale history,
    /// the same reason the file estimator only reads the last 20.
    pub stats: StatsAgg,
    /// Ledger rows this build could not deserialise, kept **verbatim** and
    /// re-emitted on every save.
    ///
    /// `load_new_shape` reads each row with
    /// `if let Ok(task) = serde_json::from_value::<Task>(..)` — a row that fails
    /// produces no error, no event and no count. That is survivable on its own;
    /// what is not survivable is the `save()` that follows, which wrote the
    /// shortened ledger back and turned a read problem into permanent loss. The
    /// live library is thousands of rows, so any future field added to `Task`
    /// without `#[serde(default)]` would delete it on the next start, quietly.
    ///
    /// Carrying the raw `Value`s through makes that impossible and needs no
    /// operator step: the row is preserved exactly as written, the event below
    /// makes it visible, and a build that *can* read it will pick it up again on
    /// the next load. Re-derived from the file on each load rather than stored
    /// separately, so the file stays the one source of truth.
    pub unreadable_tasks: Vec<serde_json::Value>,
}

/// Per-worker per-stage completions, with a capped run of durations.
///
/// **`durations` is scoped to one *offer*, not one unit of work**, and that is
/// deliberate — but it is also the number easiest to misuse. For crawl, digest
/// and merge an offer is a chapter, so a duration is a chapter's cost. For
/// render an offer is `Settings::render_batch` takes, so the same field holds a
/// *batch's* cost and its magnitude moves with the batch size.
///
/// Its one consumer agrees with it: the Stats pane's ETA column is
/// `median(offer) × (1 - progress)` where `progress` is the fraction of the
/// current offer, so both halves of that product are offer-scoped. Feeding this
/// number into anything counted in takes — `:eta` does exactly that, and gets
/// its seconds from `bm_core::eta::secs_per_unit`, which normalises by `units`
/// — would be off by the batch size.
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
mod tests {
    use super::*;
    use crate::state::ops::BOOT_DEADLINE_SECS;
    use bm_core::config::Settings;
    use bm_proto::{now_secs, Complete, Machine, MachineState, Stage, Task, TaskState};
    use serde_json::Value;

    fn fixture() -> (tempfile::TempDir, Inner) {
        let d = tempfile::tempdir().unwrap();
        let layout = Layout::new(d.path());
        std::fs::create_dir_all(layout.data()).unwrap();
        std::fs::create_dir_all(layout.output()).unwrap();
        std::fs::write(layout.bible(), r#"{"characters":[]}"#).unwrap();
        let inner = Inner::new(layout, Settings::default());
        (d, inner)
    }

    fn old_shape_doc() -> Value {
        let mut m1 = bm_proto::Machine::new(
            "192.168.2.2",
            "thang",
            22,
            Some("~/.ssh/ssh-key-my-wsl".into()),
            "worker",
        );
        m1.state = bm_proto::MachineState::Online;
        m1.last_seen = 123;
        m1.note = "provisioned".into();
        m1.capabilities = vec!["gpu".into()];
        m1.tts_url = Some("http://127.0.0.1:8818".into());
        let m2 = bm_proto::Machine::new("127.0.0.1", "thang", 22, None, "worker");
        serde_json::json!({
            "tasks": [],
            "machines": [m1, m2],
            "workers": {"w1": "192.168.2.2"},
        })
    }

    #[test]
    fn ledger_splits_config_from_runtime_and_migrates_old_shape() {
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        let ledger = layout.bm_state().join("ledger.json");
        bm_core::write_json(&ledger, &old_shape_doc()).unwrap();

        inner.load_ledger();
        // Joined view: config and runtime both survive the split.
        assert_eq!(inner.machines.len(), 2);
        let m1 = &inner.machines["192.168.2.2"];
        assert_eq!(m1.ssh_key.as_deref(), Some("~/.ssh/ssh-key-my-wsl"));
        assert_eq!(m1.state, bm_proto::MachineState::Online);
        assert_eq!((m1.last_seen, m1.note.as_str()), (123, "provisioned"));
        assert_eq!(m1.capabilities, vec!["gpu".to_string()]);
        assert_eq!(inner.machines["127.0.0.1"].ssh_key, None);
        assert_eq!(
            inner.workers.get("w1").map(String::as_str),
            Some("192.168.2.2")
        );

        // Config file: both boxes, keys byte-for-byte, names default to addr.
        let boxes = bm_core::provision::load_boxes(&layout.machines());
        assert_eq!(boxes.len(), 2);
        let b1 = boxes.iter().find(|b| b.addr == "192.168.2.2").unwrap();
        assert_eq!(
            (b1.name.as_str(), b1.key.as_deref()),
            ("192.168.2.2", Some("~/.ssh/ssh-key-my-wsl"))
        );

        // Ledger file: new shape, and the pre-migration snapshot is kept.
        let disk: Value = bm_core::read_json(&ledger).unwrap();
        assert!(disk.get("machines").is_none(), "old array is gone");
        let st = disk["machine_state"].as_object().unwrap();
        assert_eq!(st.len(), 2);
        assert_eq!(st["192.168.2.2"]["state"], serde_json::json!("online"));
        assert!(
            ledger.with_extension("json.bak").exists(),
            "pre-migration snapshot"
        );

        // Idempotent: a second load over the migrated file changes nothing.
        let mut again = Inner::new(layout.clone(), Settings::default());
        again.load_ledger();
        assert_eq!(again.machines.len(), 2);
        assert_eq!(
            again.machines["192.168.2.2"].ssh_key.as_deref(),
            Some("~/.ssh/ssh-key-my-wsl")
        );
        let disk2: Value = bm_core::read_json(&ledger).unwrap();
        assert_eq!(disk, disk2, "reload must not rewrite");
    }

    #[test]
    fn a_ledger_written_before_batching_loads_every_row() {
        // **The load path where a new field can destroy the library.** The live
        // ledger is thousands of rows and none of them carries a `batch` key —
        // the field arrived with batching. `load_new_shape` deserialises with
        // `if let Ok(task) = from_value::<Task>(..)`, so a field that did *not*
        // default would not raise anything: it would drop every row, and the
        // next `save()` would write the empty result back over them. The rows
        // below are copied verbatim out of `workspaces/beyond-myriads/ledger.json`
        // — a render row at the current shape, and a crawl row at the oldest
        // shape (no `take`, no `design`) — so this is anchored to the real file
        // rather than to an imagined one.
        let (_d, mut inner) = fixture();
        let ledger = inner.layout.ledger();
        let rows = serde_json::json!([
            {
                "affinity": null, "assigned_to": null, "attempts": 0, "chapter": 53,
                "design": null, "detail": "", "lease_until": null, "stage": "render",
                "state": "done", "take": 5, "updated": 1790069024
            },
            {
                "chapter": 1, "stage": "crawl", "state": "done", "attempts": 0,
                "assigned_to": null, "lease_until": null, "detail": "ok",
                "updated": 1790069000
            }
        ]);
        bm_core::write_json(
            &ledger,
            &serde_json::json!({
                "tasks": rows, "machine_state": {}, "workers": {}, "caps": {},
                "profile": null
            }),
        )
        .unwrap();

        inner.load_ledger();
        assert_eq!(
            inner.tasks.len(),
            2,
            "a row without `batch` must still load — a dropped row says nothing"
        );
        let take = &inner.tasks["render:53:5"];
        assert!(
            take.batch.is_empty(),
            "the new field defaults to no grouping"
        );
        assert_eq!(take.take, Some(5), "and the fields around it are untouched");
        assert_eq!(take.state, TaskState::Done);
        let crawl = &inner.tasks["crawl:1"];
        assert_eq!(
            (crawl.take, crawl.design.is_none(), crawl.batch.is_empty()),
            (None, true, true),
            "the same defaulting that let `take` and `design` arrive"
        );

        // The write side carries it, so a fresh ledger round-trips instead of
        // holding the field only in memory.
        inner.save();
        let back: Value = bm_core::read_json(&ledger).unwrap();
        let written = back["tasks"].as_array().unwrap();
        assert_eq!(written.len(), 2, "and saving does not drop them either");
        assert!(
            written
                .iter()
                .all(|r| r.get("batch").and_then(|b| b.as_array()).is_some()),
            "every row is written with the field: {written:?}"
        );
    }

    #[test]
    fn a_ledger_row_this_build_cannot_read_is_kept_not_dropped() {
        // **The failure mode this closes.** `load_new_shape` reads each row with
        // `if let Ok(task) = from_value::<Task>(..)`, so an unreadable row
        // disappeared with no error, no event and no count — and then `save()`
        // wrote the shortened ledger back over the full one. With thousands of
        // rows in the live library, any future field added to `Task` without
        // `#[serde(default)]` would delete it on the next start, quietly.
        let (_d, mut inner) = fixture();
        let ledger = inner.layout.ledger();
        let good = serde_json::json!({
            "chapter": 1, "stage": "crawl", "state": "done", "attempts": 0,
            "assigned_to": null, "lease_until": null, "detail": "ok",
            "updated": 1790069000
        });
        // A state this build does not know — exactly what a newer inductor's row
        // looks like to an older one. `stage`/`chapter` stay readable, which is
        // also what lets the event name the row.
        let unknown = serde_json::json!({
            "chapter": 7, "stage": "render", "state": "quarantined", "attempts": 2,
            "assigned_to": "w1", "lease_until": 1790000000, "detail": "from the future",
            "updated": 1790069500, "take": 3, "batch": ["render:7:4"]
        });
        bm_core::write_json(
            &ledger,
            &serde_json::json!({
                "tasks": [good.clone(), unknown.clone()], "machine_state": {},
                "workers": {}, "caps": {}, "profile": null
            }),
        )
        .unwrap();

        inner.load_ledger();
        assert_eq!(inner.tasks.len(), 1, "the row this build understands");
        assert!(inner.tasks.contains_key("crawl:1"));
        assert_eq!(
            inner.unreadable_tasks.len(),
            1,
            "and the one it does not is held"
        );
        assert_eq!(
            inner.unreadable_tasks[0], unknown,
            "preserved, not reinterpreted"
        );
        assert!(
            inner
                .recent_events(10)
                .iter()
                .any(|e| e.text.contains("render:7")),
            "and named in the events, so it is not a silent surprise: {:?}",
            inner
                .recent_events(10)
                .iter()
                .map(|e| &e.text)
                .collect::<Vec<_>>()
        );

        // **The write is the moment the library used to shrink.**
        inner.save();
        let back: Value = bm_core::read_json(&ledger).unwrap();
        let rows = back["tasks"].as_array().unwrap();
        assert_eq!(rows.len(), 2, "both rows survive the round trip");
        // The two rows are treated differently on purpose, and that is the whole
        // design: a row this build *understands* is rewritten in the current
        // schema (it gains `affinity`/`design`/`take`/`batch` if it lacked them),
        // while a row it does not is preserved untouched. So the assertion is
        // structural equality only for the one that was never parsed.
        assert!(
            rows.contains(&unknown),
            "the unreadable one, intact: {rows:?}"
        );
        assert!(
            rows.iter()
                .any(|r| r["stage"] == "crawl" && r["chapter"] == 1),
            "and the readable one is still there, re-serialised in full: {rows:?}"
        );

        // Reloading must not duplicate it: the preserved set is re-derived from
        // the file on each load, not accumulated across them.
        let mut again = Inner::new(inner.layout.clone(), Settings::default());
        again.load_ledger();
        assert_eq!(
            again.unreadable_tasks.len(),
            1,
            "no accumulation across loads"
        );
        assert_eq!(again.tasks.len(), 1);
        again.save();
        let back: Value = bm_core::read_json(&ledger).unwrap();
        assert_eq!(
            back["tasks"].as_array().unwrap().len(),
            2,
            "still two after a second load/save cycle"
        );
    }

    #[test]
    fn a_ledger_of_only_unreadable_rows_is_not_an_empty_ledger() {
        // `check_profile` passes an *empty* ledger and lets reconcile adopt the
        // workspace's profile. Reading a ledger we could not fully parse as
        // empty would stamp this book's profile over rows that may belong to
        // another one — the mixing that gate exists to prevent.
        let (_d, mut inner) = fixture();
        inner.settings.profile = ptr("xianxia", "aaa");
        let ledger = inner.layout.ledger();
        let unknown = serde_json::json!({
            "chapter": 3, "stage": "render", "state": "quarantined", "attempts": 0,
            "assigned_to": null, "lease_until": null, "detail": "", "updated": 1
        });
        bm_core::write_json(
            &ledger,
            &serde_json::json!({
                "tasks": [unknown], "machine_state": {}, "workers": {}, "caps": {},
                "profile": {"name": "khac", "hash": "bbb"}
            }),
        )
        .unwrap();

        inner.load_ledger();
        assert!(inner.tasks.is_empty(), "nothing readable");
        assert_eq!(
            inner.unreadable_tasks.len(),
            1,
            "but the ledger is not empty"
        );
        let err = inner
            .check_profile()
            .expect_err("a foreign ledger is refused, unreadable rows included")
            .to_string();
        assert!(
            err.contains("khac"),
            "and the foreign profile is named: {err}"
        );
    }

    /// A **real** ledger round-trips through the real load/save path.
    ///
    /// The one check that protects the library itself. `load_new_shape` + `save`
    /// is the pair that can shrink a ledger, and a synthetic fixture cannot prove
    /// it on a file with thousands of rows in every shape the project has ever
    /// written — including rows from builds that predate fields this one has.
    ///
    /// Opt-in and pointed at a *file*, so it never depends on a checkout's live
    /// state. **Point it at a copy**: it writes through `save()` into the same
    /// directory it read from, which is the point — that is the real path.
    ///
    /// ```text
    /// cp workspaces/<name>/ledger.json /tmp/ledger.json
    /// BM_LEDGER_ROUNDTRIP=/tmp/ledger.json cargo test -p bm-inductor \
    ///   --bin bm-inductor a_real_ledger_round_trips -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "writes through save(); point BM_LEDGER_ROUNDTRIP at a COPY of a real ledger"]
    fn a_real_ledger_round_trips() {
        let Ok(src) = std::env::var("BM_LEDGER_ROUNDTRIP") else {
            panic!("set BM_LEDGER_ROUNDTRIP to a copy of a real ledger.json");
        };
        let src = std::path::PathBuf::from(src);
        let before: Value = serde_json::from_str(&std::fs::read_to_string(&src).unwrap())
            .expect("parse the ledger");
        let rows_before = before["tasks"].as_array().map(|a| a.len()).unwrap_or(0);
        assert!(rows_before > 0, "{} has no tasks to check", src.display());

        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path());
        std::fs::create_dir_all(layout.bm_state()).unwrap();
        std::fs::create_dir_all(layout.data()).unwrap();
        std::fs::write(layout.bible(), r#"{"characters":[]}"#).unwrap();
        std::fs::copy(&src, layout.ledger()).unwrap();

        let mut inner = Inner::new(layout.clone(), Settings::default());
        inner.load_ledger();
        let read = inner.tasks.len();
        let held = inner.unreadable_tasks.len();
        inner.save();

        let after: Value = bm_core::read_json(&layout.ledger()).unwrap();
        let rows_after = after["tasks"].as_array().unwrap().len();
        println!("rows: {rows_before} in · {read} read · {held} preserved · {rows_after} written");
        assert_eq!(
            rows_after, rows_before,
            "the write must not change the row count — this is the moment a ledger shrinks"
        );
        assert_eq!(
            held, 0,
            "every row of a real ledger should be readable by the current build; \
             a non-zero here is the unreadable-row path doing its job, and is worth reading"
        );

        // A second cycle must be stable: no growth, no loss.
        let mut again = Inner::new(layout.clone(), Settings::default());
        again.load_ledger();
        again.save();
        let after2: Value = bm_core::read_json(&layout.ledger()).unwrap();
        assert_eq!(
            after2["tasks"].as_array().unwrap().len(),
            rows_before,
            "the second cycle is stable too"
        );
    }

    fn ptr(name: &str, hash: &str) -> bm_core::profile::Pointer {
        bm_core::profile::Pointer {
            name: name.into(),
            hash: hash.into(),
        }
    }

    #[test]
    fn the_serve_gate_refuses_a_foreign_ledger_and_reconcile_stamps() {
        let (_d, mut inner) = fixture();
        // Empty ledger passes and adopts the workspace profile at reconcile.
        inner.settings.profile = ptr("xianxia", "h1");
        inner.check_profile().unwrap();
        inner.reconcile(1, 0);
        assert_eq!(inner.ledger_profile, Some(ptr("xianxia", "h1")));
        // Tasks bound to xianxia refuse to run under noir.
        inner
            .tasks
            .insert("crawl:1".into(), Task::new(1, Stage::Crawl));
        inner.settings.profile = ptr("noir", "h2");
        let err = inner.check_profile().unwrap_err();
        assert!(err.to_string().contains("xianxia"), "{err}");
        assert!(err.to_string().contains("noir"), "{err}");
        // ...and pass again under the matching profile.
        inner.settings.profile = ptr("xianxia", "h1");
        inner.check_profile().unwrap();
    }

    #[test]
    fn save_writes_runtime_only() {
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        let (bxo, rt) = bm_core::provision::split_machine(
            &bm_proto::Machine::new("10.0.0.9", "thang", 22, Some("/k".into()), "worker"),
            "box-9",
        );
        bm_core::provision::save_box(&layout.machines(), &bxo).unwrap();
        inner.machines.insert(
            "10.0.0.9".into(),
            bm_core::provision::join_machine(&bxo, Some(&rt)),
        );
        inner.save();

        let disk: Value = bm_core::read_json(&layout.bm_state().join("ledger.json")).unwrap();
        assert!(
            disk.get("machines").is_none(),
            "config never lands in the ledger"
        );
        assert_eq!(
            disk["machine_state"]["10.0.0.9"]["note"],
            serde_json::json!("")
        );
    }

    #[test]
    fn migration_holds_for_the_live_ledger() {
        // Local-only gate (not CI): point at a copy of the real ledger and
        // prove the migration loses nothing at real scale.
        let src = std::env::var("BM_REAL_LEDGER").unwrap_or_default();
        if src.is_empty() {
            return;
        }
        let raw = std::fs::read_to_string(&src).expect("BM_REAL_LEDGER readable");
        let doc: Value = serde_json::from_str(&raw).unwrap();
        let tasks_n = doc["tasks"].as_array().map(|a| a.len()).unwrap_or(0);
        assert!(
            tasks_n > 100,
            "this gate wants the real ledger, got {tasks_n} tasks"
        );

        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path());
        std::fs::create_dir_all(layout.bm_state()).unwrap();
        std::fs::write(layout.bm_state().join("ledger.json"), &raw).unwrap();
        let mut inner = Inner::new(layout, Settings::default());
        inner.load_ledger();

        assert_eq!(inner.tasks.len(), tasks_n, "no task lost");
        assert_eq!(
            inner.machines["192.168.2.2"].ssh_key.as_deref(),
            Some("~/.ssh/ssh-key-my-wsl"),
            "the live key survives"
        );
        assert_eq!(inner.machines["127.0.0.1"].ssh_key, None);

        let snap: Vec<(String, String)> = {
            let mut v: Vec<_> = inner
                .machines
                .values()
                .map(|m| (m.addr.clone(), serde_json::to_string(m).unwrap()))
                .collect();
            v.sort();
            v
        };
        inner.load_ledger();
        let snap2: Vec<(String, String)> = {
            let mut v: Vec<_> = inner
                .machines
                .values()
                .map(|m| (m.addr.clone(), serde_json::to_string(m).unwrap()))
                .collect();
            v.sort();
            v
        };
        assert_eq!(snap, snap2, "migration is idempotent at real scale");
    }

    #[test]
    fn swap_invalidates_only_the_characters_files() {
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::write(
            layout.script(1),
            r#"{"segments":[{"speaker":"A","text":"x"},{"speaker":"A","text":"y"},{"speaker":"B","text":"z"}]}"#,
        )
        .unwrap();
        std::fs::write(
            layout.cast("vieneu"),
            r#"{"A":"Đức Trí","B":"Adam","Narrator":"Đức Trí"}"#,
        )
        .unwrap();
        let seg = layout.seg_dir("vieneu", 1);
        std::fs::create_dir_all(&seg).unwrap();
        std::fs::write(seg.join("0000-0001_Đức Trí.wav"), vec![0u8; 2000]).unwrap();
        std::fs::write(seg.join("0002_Adam.wav"), vec![0u8; 2000]).unwrap();
        std::fs::write(layout.final_mp3(1), vec![0u8; 2000]).unwrap();
        // The routine pass has already recorded what these files are — which
        // is what lets the swap say "only A's take changed" instead of "I
        // cannot prove any of this".
        inner.materialize_render_takes(1);

        let msg = inner.op_swap_voice("A", "Minh Triết").unwrap();
        assert!(msg.contains("Đức Trí -> Minh Triết"), "{msg}");
        assert!(
            !seg.join("0000-0001_Đức Trí.wav").exists(),
            "stale run file must go"
        );
        assert!(
            seg.join("0002_Adam.wav").exists(),
            "other voices keep cache"
        );
        assert!(!layout.final_mp3(1).exists(), "stale product goes away");
        // One row per take: A's run is work again, B's is untouched.
        assert_eq!(inner.tasks["render:1:0"].state, TaskState::Pending);
        assert_eq!(inner.tasks["render:1:1"].state, TaskState::Done);
        assert!(!inner.render_takes_done(1));
        assert_eq!(inner.tasks["merge:1"].state, TaskState::Pending);
        // The swap's *meaning* is checked through the reader (which resolves
        // keys back to names), and the stored form is checked directly — the
        // file persists keys now, so both assertions matter.
        let cast = bm_core::cast::read_cast("vieneu", &layout.cast("vieneu"));
        assert_eq!(cast["A"], "Minh Triết");
        assert_eq!(cast["B"], "Adam", "untouched speakers survive");
        let disk: std::collections::HashMap<String, String> =
            bm_core::read_json(&layout.cast("vieneu")).unwrap();
        assert_eq!(disk["A"], "minh-triet", "persisted as a key, not a name");
        assert_eq!(disk["B"], "adam");
    }

    #[test]
    fn surgical_swap_rerenders_unpinned_and_forces_only_the_stale_set() {
        // The flaw this pins: a swap deleted stale files only locally, the
        // offer carried the whole chapter, and whichever cold box asked next
        // re-spoke everything from scratch. Now the re-render is unpinned and
        // the offer forces only what this store lacks — the swapped voice —
        // so whoever asks speaks one file while untouched voices keep cache.
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::write(
            layout.script(1),
            r#"{"segments":[{"speaker":"A","text":"x"},{"speaker":"B","text":"z"}]}"#,
        )
        .unwrap();
        std::fs::write(layout.cast("vieneu"), r#"{"A":"Đức Trí","B":"Adam"}"#).unwrap();
        let seg = layout.seg_dir("vieneu", 1);
        std::fs::create_dir_all(&seg).unwrap();
        std::fs::write(seg.join("0000_Đức Trí.wav"), vec![0u8; 2000]).unwrap();
        std::fs::write(seg.join("0001_Adam.wav"), vec![0u8; 2000]).unwrap();
        inner.materialize_render_takes(1);
        for (stage, state) in [
            (Stage::Crawl, TaskState::Done),
            (Stage::Digest, TaskState::Done),
        ] {
            let mut t = Task::new(1, stage);
            t.state = state;
            inner.tasks.insert(format!("{stage}:1"), t);
        }
        let mut m = Task::new(1, Stage::Merge);
        m.state = TaskState::Done;
        m.affinity = Some("192.168.2.2".into());
        inner.tasks.insert("merge:1".into(), m);
        for (w, addr) in [
            ("warm-a", "192.168.2.2"),
            ("cold-b", "192.168.2.3"),
            ("lo-w", "127.0.0.1"),
        ] {
            inner.workers.insert(w.into(), addr.into());
            inner.caps.insert(
                w.into(),
                vec![
                    "crawl".into(),
                    "digest".into(),
                    "render".into(),
                    "render-segments".into(),
                    "merge".into(),
                ],
            );
        }

        inner.op_swap_voice("A", "Minh Triết").unwrap();
        assert_eq!(
            inner.tasks["render:1:0"].affinity, None,
            "re-render is offerable to any box — no warm-box pin"
        );
        assert_eq!(
            inner.tasks["render:1:1"].state,
            TaskState::Done,
            "B's take is not work: the swap only reached A"
        );
        assert_eq!(
            inner.tasks["merge:1"].affinity, None,
            "the requeue clears the merge pin too"
        );

        // Any box takes the re-render — no warm-box pin, no cold box.
        let offer = inner
            .offer("cold-b")
            .expect("cold box takes the re-render too");
        assert_eq!(offer.task_id, "render:1:0", "one take, A's");
        let units = offer.render_units.as_ref().expect("planned, not legacy");
        assert_eq!(units.len(), 1, "a single segment travels: {units:?}");
        assert_eq!(units[0].speaker, "A", "and it is the swapped speaker's");
        assert_eq!(units[0].voice, "Minh Triết", "speaking the new voice");
        assert!(
            units[0].name.starts_with("t-"),
            "content-addressed, so the name is the proof: {}",
            units[0].name
        );
        assert_eq!(units[0].take_key.len(), 16);
        assert!(
            offer.render_force.is_empty(),
            "a content-addressed take needs no forcing: {:?}",
            offer.render_force
        );
        assert!(
            !offer.cast_hash.is_empty(),
            "the voice collection travels too"
        );
        assert!(
            seg.join("0001_Adam.wav").is_file(),
            "untouched voices keep their cache"
        );

        // The local node takes merges pinned to it, like any box takes its
        // own pin.
        let t = inner.tasks.get_mut("render:1:0").unwrap();
        t.state = TaskState::Pending;
        t.assigned_to = None;
        t.lease_until = None;
        let offer = inner.offer("lo-w").expect("local takes unpinned renders");
        assert_eq!(offer.task_id, "render:1:0");
        assert!(offer.local_node);
    }

    #[test]
    fn merge_runs_on_whichever_box_asks_first() {
        // No row is ever pinned: a merge pulls the pieces it lacks from the
        // inductor, so completions record nothing about who rendered what —
        // and the merge row exists unpinned for whoever asks first.
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::write(
            layout.script(7),
            r#"{"segments":[{"speaker":"A","text":"x"},{"speaker":"B","text":"z"}]}"#,
        )
        .unwrap();
        std::fs::write(layout.cast("vieneu"), r#"{"A":"Đức Trí","B":"Adam"}"#).unwrap();
        let seg = layout.seg_dir("vieneu", 7);
        std::fs::create_dir_all(&seg).unwrap();
        std::fs::write(seg.join("0000_Đức Trí.wav"), vec![0u8; 2000]).unwrap();
        std::fs::write(seg.join("0001_Adam.wav"), vec![0u8; 2000]).unwrap();
        // The worker map is what turns a worker id into a machine.
        inner
            .workers
            .insert("remote-w".into(), "192.168.2.2".into());
        // The plan adopts the cache that is already here, so the takes are
        // `Done`; put the first one back in flight to exercise the report.
        inner.materialize_render_takes(7);
        let mut t = Task::new_take(7, 0);
        t.state = TaskState::Running;
        t.assigned_to = Some("remote-w".into());
        inner.tasks.insert(t.id(), t);

        inner.complete(&completion(
            "remote-w",
            "render:7:0",
            true,
            "render ch7 (1 calls)",
        ));
        assert_eq!(
            inner.tasks["render:7:0"].state,
            TaskState::Done,
            "gate passes: the take's file is home"
        );
        assert_eq!(
            inner.tasks["merge:7"].affinity, None,
            "no pin recorded: whoever asks first merges"
        );

        // A renderer with no known machine — a hand-written ledger, or a
        // report from a worker that never registered. The local node shares
        // the inductor's store, which is what this always was.
        std::fs::write(
            layout.script(8),
            r#"{"segments":[{"speaker":"A","text":"x"},{"speaker":"B","text":"z"}]}"#,
        )
        .unwrap();
        let seg8 = layout.seg_dir("vieneu", 8);
        std::fs::create_dir_all(&seg8).unwrap();
        std::fs::write(seg8.join("0000_Đức Trí.wav"), vec![0u8; 2000]).unwrap();
        std::fs::write(seg8.join("0001_Adam.wav"), vec![0u8; 2000]).unwrap();
        inner.materialize_render_takes(8);
        let mut t = Task::new_take(8, 0);
        t.state = TaskState::Running;
        t.assigned_to = Some("ghost".into());
        inner.tasks.insert(t.id(), t);
        inner.complete(&completion(
            "ghost",
            "render:8:0",
            true,
            "render ch8 (1 calls)",
        ));
        assert_eq!(
            inner.tasks["merge:8"].affinity, None,
            "no known renderer: still no pin"
        );
    }

    #[test]
    fn merge_without_a_payload_needs_the_file_on_disk() {
        // Local nodes ship no mp3_b64; the file itself is the evidence. A
        // report with neither is a failure, not a silent Done.
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        for ch in [8u32, 9] {
            let mut t = Task::new(ch, Stage::Merge);
            t.state = TaskState::Running;
            t.assigned_to = Some("w1".into());
            inner.tasks.insert(format!("merge:{ch}"), t);
        }
        std::fs::create_dir_all(layout.final_mp3(8).parent().unwrap()).unwrap();
        std::fs::write(layout.final_mp3(8), vec![0u8; 2000]).unwrap();

        inner.complete(&completion("w1", "merge:8", true, "merge ch8 -> out.mp3"));
        assert_eq!(
            inner.tasks["merge:8"].state,
            TaskState::Done,
            "file present: done"
        );
        let msg = inner.complete(&completion("w1", "merge:9", true, "merge ch9 -> out.mp3"));
        assert!(
            msg.contains("failed"),
            "no payload and no file fails: {msg}"
        );
        assert!(msg.contains("no file"), "the absence is named: {msg}");
        assert_eq!(inner.tasks["merge:9"].state, TaskState::Pending);
    }

    #[test]
    fn render_offer_requires_the_upload_capability() {
        // Staged rollout: an agent without `render-segments` keeps every
        // other stage but never renders (its report would fail the gate
        // anyway). Unknown workers are allowed — failing closed would strand
        // anything that never registered.
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::write(
            layout.script(5),
            r#"{"segments":[{"speaker":"A","text":"a full sentence for synthesis here"}]}"#,
        )
        .unwrap();
        std::fs::write(layout.cast("vieneu"), r#"{"A":"Adam"}"#).unwrap();
        for (stage, state) in [
            (Stage::Crawl, TaskState::Done),
            (Stage::Digest, TaskState::Done),
        ] {
            let mut t = Task::new(5, stage);
            t.state = state;
            inner.tasks.insert(format!("{stage}:5"), t);
        }
        let mut t = Task::new(5, Stage::Render);
        t.state = TaskState::Pending;
        inner.tasks.insert("render:5".into(), t);
        inner.workers.insert("old-w".into(), "192.168.2.2".into());
        inner.caps.insert(
            "old-w".into(),
            vec![
                "crawl".into(),
                "digest".into(),
                "render".into(),
                "merge".into(),
            ],
        );
        inner.workers.insert("new-w".into(), "192.168.2.2".into());
        inner.caps.insert(
            "new-w".into(),
            vec![
                "crawl".into(),
                "digest".into(),
                "render".into(),
                "merge".into(),
                "render-segments".into(),
            ],
        );

        assert!(inner.offer("old-w").is_none(), "old agent never renders");
        let offer = inner.offer("new-w").expect("capable worker renders");
        assert_eq!(offer.task_id, "render:5");
        assert!(!offer.local_node, "192.168.2.2 is not the local node");
        assert!(offer.render_units.is_some(), "planned, not legacy");
    }

    /// A beat with the load fields this scheduler reads, and nothing else.
    fn beat_with_load(worker: &str, addr: &str, mem_pct: Option<f32>) -> bm_proto::Heartbeat {
        let mut h: bm_proto::Heartbeat = serde_json::from_value(serde_json::json!({
            "worker_id": worker,
            "addr": addr,
            "progress": 0.0,
            "activity": "idle",
            "ts": now_secs(),
        }))
        .expect("a beat needs only the required fields");
        h.mem_pct = mem_pct;
        h
    }

    #[test]
    fn offer_withholds_a_box_the_oom_killer_is_circling() {
        // The guardrail for the boxes this repo actually runs: one TTS sidecar
        // is ~2.85 GB resident, so a box over the ceiling is one that will not
        // finish what it is handed. Withheld, not failed — nothing moves and
        // the same task is offered to the next box that asks.
        let (_d, mut inner) = fixture();
        inner.workers.insert("w1".into(), "192.168.2.2".into());
        inner.caps.insert("w1".into(), vec!["crawl".into()]);
        inner
            .tasks
            .insert("crawl:5".into(), Task::new(5, Stage::Crawl));
        // The offer marks it `Assigned`; put it back so the next box can take it.
        let rearm = |inner: &mut Inner| {
            let t = inner.tasks.get_mut("crawl:5").unwrap();
            t.state = TaskState::Pending;
            t.assigned_to = None;
            t.lease_until = None;
        };

        // No opinion is not a verdict: an agent that never measured (older
        // agents, a registration) is offered work exactly as before.
        assert!(inner.offer("w1").is_some(), "an unmeasured box still works");
        rearm(&mut inner);

        inner
            .beats
            .insert("w1".into(), beat_with_load("w1", "192.168.2.2", Some(50.0)));
        assert!(inner.offer("w1").is_some(), "a working box is fed");
        rearm(&mut inner);

        inner
            .beats
            .insert("w1".into(), beat_with_load("w1", "192.168.2.2", Some(94.0)));
        assert!(
            inner.offer("w1").is_none(),
            "a box at 94% gets nothing — it would fail the task and strike the chapter"
        );
        // Untouched, not stranded: still Pending with no assignee, so the next
        // box that asks can have it, and this one keeps it after it settles.
        let t = inner.tasks.get("crawl:5").unwrap();
        assert_eq!(t.state, TaskState::Pending);
        assert!(t.assigned_to.is_none());
    }

    #[test]
    fn a_duplicate_sidecar_is_an_event_on_the_edge_not_on_every_beat() {
        // Two `bm-tts` on one box is the OOM this cluster kept taking, and the
        // count is the one fact it could not see. The dispatcher polls every
        // couple of seconds, so this must fire on the transition — an event per
        // poll is a log nobody reads, and one that never fires is why the bug
        // survived.
        let (_d, mut inner) = fixture();
        let errors = |inner: &Inner| {
            inner
                .recent_events(50)
                .into_iter()
                .filter(|e| e.level == "error" && e.text.contains("bm-tts"))
                .count()
        };

        let mut h = beat_with_load("w1", "192.168.2.2", Some(70.0));
        h.sidecars = Some(2);
        h.sidecar_gb = Some(5.7);
        inner.observe(&h);
        assert_eq!(errors(&inner), 1, "a duplicate must be reported");
        assert!(
            inner
                .recent_events(50)
                .iter()
                .any(|e| e.text.contains("5.7 GB")),
            "the cost is named, not just the count"
        );

        // Same wrong count again: polling must not become a log flood.
        inner.observe(&h);
        inner.observe(&h);
        assert_eq!(errors(&inner), 1, "one event per occurrence, not per beat");

        // It settles, then recurs: a new edge, so it is reported again.
        h.sidecars = Some(1);
        inner.observe(&h);
        h.sidecars = Some(2);
        inner.observe(&h);
        assert_eq!(errors(&inner), 2, "a recurrence is news again");

        // An older agent reports nothing; that is not "was fine", so a box
        // whose first sighting is already wrong still fires.
        let mut fresh = fixture().1;
        let mut old = beat_with_load("w2", "192.168.2.3", None);
        old.sidecars = Some(3);
        fresh.observe(&old);
        assert_eq!(
            fresh
                .recent_events(50)
                .into_iter()
                .filter(|e| e.level == "error" && e.text.contains("bm-tts"))
                .count(),
            1,
            "a first sighting of a duplicate is still a duplicate"
        );
    }

    #[test]
    fn merge_affinity_never_gates_the_local_node() {
        // Affinity is an optimization for remote boxes, not a gate for the
        // local one: it shares the inductor's segment store, so it takes any
        // pending merge — pinned to a live box, a ffmpeg-less one, or a dead
        // one. Otherwise a render on a merge-disabled box strands its merge
        // pending for ever while merge-enabled machines stand idle.
        let (_d, mut inner) = fixture();
        for stage in [Stage::Crawl, Stage::Digest, Stage::Render] {
            let mut t = Task::new(5, stage);
            t.state = TaskState::Done;
            inner.tasks.insert(format!("{stage}:5"), t);
        }
        let mut merge = Task::new(5, Stage::Merge);
        merge.affinity = Some("192.0.2.1".into());
        inner.tasks.insert("merge:5".into(), merge);
        inner.workers.insert("lo-w".into(), "127.0.0.1".into());
        inner.workers.insert("rmt-w".into(), "192.0.2.1".into());
        inner.caps.insert(
            "rmt-w".into(),
            vec![
                "crawl".into(),
                "digest".into(),
                "render".into(),
                "merge".into(),
            ],
        );
        // Pinned to a live, capable box — the local node still takes it.
        let offer = inner.offer("lo-w").expect("local merges anything");
        assert_eq!(offer.task_id, "merge:5");
        assert!(offer.local_node, "127.0.0.1 is the local node");
        // The pinned box itself keeps working when it asks first.
        let m = inner.tasks.get_mut("merge:5").unwrap();
        m.state = TaskState::Pending;
        m.assigned_to = None;
        m.lease_until = None;
        let offer = inner.offer("rmt-w").expect("affinity still matches");
        assert_eq!(offer.task_id, "merge:5");
        // Dead box (no workers at all): same sink.
        let m = inner.tasks.get_mut("merge:5").unwrap();
        m.state = TaskState::Pending;
        m.assigned_to = None;
        m.lease_until = None;
        m.affinity = Some("192.0.2.2".into());
        let offer = inner.offer("lo-w").expect("dead box merges locally");
        assert_eq!(offer.task_id, "merge:5");
    }

    #[test]
    fn offer_marks_the_local_node() {
        // The provisioner's `Ssh.local` and the offer's `local_node` run on
        // the same predicate — one fact, checked on both sides of the wire.
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::write(
            layout.script(5),
            r#"{"segments":[{"speaker":"A","text":"a full sentence for synthesis here"}]}"#,
        )
        .unwrap();
        std::fs::write(layout.cast("vieneu"), r#"{"A":"Adam"}"#).unwrap();
        for (stage, state) in [
            (Stage::Crawl, TaskState::Done),
            (Stage::Digest, TaskState::Done),
        ] {
            let mut t = Task::new(5, stage);
            t.state = state;
            inner.tasks.insert(format!("{stage}:5"), t);
        }
        let mut t = Task::new(5, Stage::Render);
        t.state = TaskState::Pending;
        inner.tasks.insert("render:5".into(), t);
        inner.workers.insert("w-local".into(), "127.0.0.1".into());

        inner.settings.inject_volume = 0.25;
        let offer = inner.offer("w-local").expect("local worker renders");
        assert!(offer.local_node);
        assert_eq!(offer.inject_volume, 0.25);
        for addr in ["127.0.0.1", "localhost", "::1"] {
            assert!(bm_core::is_local_node(addr), "{addr}");
        }
        assert!(!bm_core::is_local_node("192.168.2.2"));
    }

    /// The two credential tests below mutate the process environment, which is
    /// global while cargo runs tests in threads. Without this they can read
    /// each other's value and flake; with it, each is deterministic.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn a_digest_offer_carries_the_key_its_analyzer_needs() {
        // The outage: `192.168.2.2` (alias `marmot`) has no `.env` — it is
        // personal and git-ignored, so provisioning never copies it — and every
        // digest offered there died on `GEMINI_API_KEY missing` however
        // carefully this inductor was set up. The key now rides the offer.
        let _g = env_lock();
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::create_dir_all(layout.chapters()).unwrap();
        std::fs::write(layout.chapter_txt(1), "Chương 1: X\n\nbody\n").unwrap();
        inner.settings.analyzer = "gemini".into();
        inner.settings.engine = "vieneu".into();
        inner.enqueue_translate(1, 1);
        inner
            .workers
            .insert("remote-w".into(), "192.168.2.2".into());

        std::env::set_var("GEMINI_API_KEY", "from-the-inductors-env");
        std::env::set_var("OPENROUTER_API_KEY", "a-different-provider");
        let offer = inner.offer("remote-w").expect("digest:1 is offerable");
        std::env::remove_var("GEMINI_API_KEY");
        std::env::remove_var("OPENROUTER_API_KEY");

        assert_eq!(offer.task_id, "digest:1");
        assert_eq!(
            offer.credentials.gemini_api_key, "from-the-inductors-env",
            "the analyzer's key must reach the worker that runs the analyzer"
        );
        assert!(
            offer.credentials.openrouter_api_key.is_empty(),
            "only what this stage reads: {:?}",
            offer.credentials
        );
    }

    #[test]
    fn remix_saves_the_mix_and_requeues_only_merges() {
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        // A script is what makes a chapter a chapter: the fingerprint is
        // computed from it, so without one there is no mix to invalidate and
        // the merge is left where it is (pinned in
        // `a_merge_with_no_script_is_left_alone`).
        std::fs::write(
            layout.script(1),
            r#"{"segments":[{"speaker":"A","text":"Chương 1"}]}"#,
        )
        .unwrap();
        for (n, stage, state) in [
            (1, Stage::Merge, TaskState::Done),
            (2, Stage::Render, TaskState::Done),
            (3, Stage::Merge, TaskState::Shelved),
        ] {
            let mut t = Task::new(n, stage);
            t.state = state;
            inner.tasks.insert(t.id(), t);
        }
        std::fs::create_dir_all(layout.output()).unwrap();
        std::fs::write(layout.final_mp3(1), b"old mix").unwrap();

        let msg = inner
            .op_remix(Some(1.5), Some(0.5), Some(0.0), Some(0.25))
            .expect("valid mix");
        assert!(msg.contains("1.5"), "{msg}");
        assert!(msg.contains("1 merge(s) requeued"), "{msg}");
        assert_eq!(
            (
                inner.settings.speed,
                inner.settings.effect_volume,
                inner.settings.music_volume,
                inner.settings.inject_volume
            ),
            (1.5, 0.5, 0.0, 0.25)
        );
        // The published merge comes back and its mp3 goes with it. The render
        // keeps its cache — tempo and the layer trims apply at merge time, so
        // no segment is re-spoken. The shelved merge never published anything,
        // so it has no mix to be wrong about and a knob change does not
        // un-shelve it.
        let m = &inner.tasks["merge:1"];
        assert_eq!(m.state, TaskState::Pending);
        assert_eq!(m.attempts, 0);
        assert!(!layout.final_mp3(1).is_file(), "the old mix is not kept");
        assert_eq!(inner.tasks["render:2"].state, TaskState::Done);
        assert_eq!(inner.tasks["merge:3"].state, TaskState::Shelved);
        assert!(inner
            .op_remix(Some(9.0), Some(1.0), Some(1.0), None)
            .is_err());
        assert!(inner.op_remix(None, Some(1.0), Some(1.0), None).is_err());
        for inject in [-0.1, 2.1, f64::NAN, f64::INFINITY] {
            assert!(inner
                .op_remix(Some(1.0), Some(1.0), Some(1.0), Some(inject))
                .is_err());
            assert_eq!(inner.settings.inject_volume, 0.25);
            assert_eq!(inner.settings.speed, 1.5);
        }
        inner
            .op_remix(Some(1.0), Some(1.0), Some(1.0), None)
            .unwrap();
        assert_eq!(inner.settings.inject_volume, 0.25);
        let saved = Settings::load(&inner.layout.settings());
        assert_eq!(saved.inject_volume, 0.25);
    }

    /// Two chapters, each in a different scene, and the registries that give
    /// each one a clip of its own — so the fingerprint has something to be
    /// per-chapter *about*.
    fn design_fixture() -> (tempfile::TempDir, Inner) {
        let (d, inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::create_dir_all(layout.assets()).unwrap();
        std::fs::write(
            layout.scene_map(),
            r#"{
              "rules": [
                {"match": ["mountain"], "effect": ["wind"], "level": 0.3},
                {"match": ["kitchen"], "effect": ["fire"], "level": 0.2}
              ],
              "default": {"effect": [], "level": 0.0}
            }"#,
        )
        .unwrap();
        std::fs::write(
            layout.pool(bm_core::audio_pool::PoolKind::Effect),
            r#"{
              "wind": {"tags": ["wind"], "files": ["effects/wind-1.mp3"], "level": 0.5},
              "fire": {"tags": ["fire"], "files": ["effects/fire-1.mp3"], "level": 0.5}
            }"#,
        )
        .unwrap();
        for (n, scene) in [(1u32, "mountain"), (2, "kitchen")] {
            let script = serde_json::json!({
                "segments": [
                    {"speaker": "A", "text": format!("Chương {n}")},
                    {"speaker": "A", "text": "ở đó", "scene": scene}
                ]
            });
            std::fs::write(layout.script(n), script.to_string()).unwrap();
            let seg = layout.seg_dir("vieneu", n);
            std::fs::create_dir_all(&seg).unwrap();
            std::fs::write(seg.join("0000_Adam.wav"), vec![0u8; 2000]).unwrap();
        }
        (d, inner)
    }

    /// A merge that has published: Done, its mp3 on disk, and stamped under the
    /// design in force — the state `offer` leaves a completed merge in.
    fn published(inner: &mut Inner, chapter: u32) {
        let design = bm_core::design::MergeDesign::load(&inner.layout);
        let stamp = inner
            .design_stamp(&design, inner.design_knobs(), chapter)
            .expect("the fixture gives every chapter a script");
        let mut t = Task::new(chapter, Stage::Merge);
        t.state = TaskState::Done;
        t.design = Some(stamp);
        inner.tasks.insert(t.id(), t);
        let mp3 = inner.layout.final_mp3(chapter);
        std::fs::create_dir_all(mp3.parent().unwrap()).unwrap();
        std::fs::write(&mp3, b"old mix").unwrap();
    }

    #[test]
    fn a_sound_edit_reaches_only_the_chapters_that_use_it() {
        // The requirement, at the ledger. Retuning one clip requeues the
        // chapters whose mix can land on it and leaves the rest published —
        // and the fingerprint is what decides the scope, so there is no second
        // rule to keep in step with the mixer.
        let (_d, mut inner) = design_fixture();
        let layout = inner.layout.clone();
        for ch in [1u32, 2] {
            published(&mut inner, ch);
        }
        // Stamped under the design as it stands, so nothing is stale yet.
        assert!(inner.op_sound_changed().contains("nothing to requeue"));

        // Retune the clip the mountain chapter can land on.
        std::fs::write(
            layout.pool(bm_core::audio_pool::PoolKind::Effect),
            r#"{
              "wind": {"tags": ["wind"], "files": ["effects/wind-1.mp3"], "level": 0.9},
              "fire": {"tags": ["fire"], "files": ["effects/fire-1.mp3"], "level": 0.5}
            }"#,
        )
        .unwrap();
        let msg = inner.op_sound_changed();
        assert!(msg.contains("1 merge(s) requeued"), "{msg}");
        let m = &inner.tasks["merge:1"];
        assert_eq!(m.state, TaskState::Pending);
        assert_eq!(m.attempts, 0);
        assert_eq!(m.detail, "requeued: sound design changed");
        assert_eq!(m.assigned_to, None);
        assert!(
            !layout.final_mp3(1).is_file(),
            "the stale mp3 goes now, not on the next reconcile"
        );
        // The kitchen chapter cannot reach `wind`, so it keeps its mix.
        assert_eq!(inner.tasks["merge:2"].state, TaskState::Done);
        assert!(layout.final_mp3(2).is_file());

        // Idempotent: the requeue wrote the new stamp, so a second look finds
        // nothing. Without that write every pass would requeue for ever.
        assert!(inner.op_sound_changed().contains("nothing to requeue"));
    }

    #[test]
    fn a_master_knob_reaches_every_published_chapter() {
        // A gain is applied to every mix, so it is in every chapter's stamp and
        // every published merge comes back. The count in the message is the
        // check that the scope is the library and not one chapter.
        let (_d, mut inner) = design_fixture();
        let layout = inner.layout.clone();
        for ch in [1u32, 2] {
            published(&mut inner, ch);
        }
        let mut render = Task::new(1, Stage::Render);
        render.state = TaskState::Done;
        inner.tasks.insert(render.id(), render);

        let msg = inner
            .op_remix(Some(1.0), Some(1.5), Some(1.0), Some(1.0))
            .expect("valid mix");
        assert!(msg.contains("2 merge(s) requeued"), "{msg}");
        for ch in [1u32, 2] {
            assert_eq!(
                inner.tasks[&format!("merge:{ch}")].state,
                TaskState::Pending,
                "ch{ch}"
            );
            assert!(!layout.final_mp3(ch).is_file(), "ch{ch}");
        }
        // The render is not this pass's business: tempo and the trims apply at
        // merge time, so no segment is re-spoken and no cache is dropped.
        assert_eq!(inner.tasks["render:1"].state, TaskState::Done);
        assert!(layout.seg_dir("vieneu", 1).join("0000_Adam.wav").is_file());
    }

    #[test]
    fn an_unstamped_merge_is_adopted_not_invalidated() {
        // Every merge published before the stamp existed has no stamp. Reading
        // that as "stale" would re-merge the whole library on the first boot
        // after the upgrade, and those mp3s are not reproducible — TTS is
        // stochastic, so re-merging is a re-recording, not a cache miss. So a
        // routine pass adopts: it writes the stamp and leaves the artifact.
        let (_d, mut inner) = design_fixture();
        let layout = inner.layout.clone();
        published(&mut inner, 1);
        inner.tasks.get_mut("merge:1").unwrap().design = None;

        assert!(inner.invalidate_stale_design(true).is_empty());
        let m = &inner.tasks["merge:1"];
        assert_eq!(m.state, TaskState::Done, "an adopted merge is still done");
        assert!(m.design.is_some(), "adoption is a write, not a shrug");
        assert!(layout.final_mp3(1).is_file(), "and the artifact stays");
        // Adopted once is adopted for good.
        assert!(inner.invalidate_stale_design(true).is_empty());

        // A caller that has *just* changed the design is the other case: an
        // unstamped merge is then by definition one that change invalidated.
        inner.tasks.get_mut("merge:1").unwrap().design = None;
        let msg = inner.op_sound_changed();
        assert!(msg.contains("1 merge(s) requeued"), "{msg}");
        assert!(!layout.final_mp3(1).is_file());
    }

    #[test]
    fn reconcile_adopts_the_stamp_without_undoing_a_promotion() {
        // The order in `reconcile` is load-bearing: the promotion loop marks a
        // merge Done on `has_mp3` alone, so an invalidation that ran before it
        // would have its deletion undone by the very promotion it was trying to
        // prevent. Here the merge is Pending with its mp3 already home — the
        // promotion runs, then the adoption, and the file survives both.
        let (_d, mut inner) = design_fixture();
        let layout = inner.layout.clone();
        let mut t = Task::new(1, Stage::Merge);
        t.state = TaskState::Pending;
        inner.tasks.insert(t.id(), t);
        std::fs::create_dir_all(layout.output()).unwrap();
        std::fs::write(layout.final_mp3(1), b"old mix").unwrap();

        inner.reconcile(1, 2);
        let m = &inner.tasks["merge:1"];
        assert_eq!(m.state, TaskState::Done, "ground truth promotes");
        assert!(m.design.is_some(), "and the pass that follows adopts");
        assert!(layout.final_mp3(1).is_file());
    }

    #[test]
    fn a_merge_with_no_script_is_left_alone() {
        // No script means the chapter cannot be planned, so there is no mix to
        // be wrong about — and this pass will not delete a published mp3 on the
        // strength of a read that failed. An unstampable merge is `None`, and
        // `None` is a skip rather than a verdict.
        let (_d, mut inner) = design_fixture();
        let layout = inner.layout.clone();
        published(&mut inner, 1);
        std::fs::remove_file(layout.script(1)).unwrap();

        assert!(inner.op_sound_changed().contains("nothing to requeue"));
        assert_eq!(inner.tasks["merge:1"].state, TaskState::Done);
        assert!(
            layout.final_mp3(1).is_file(),
            "the artifact is not a read error's to delete"
        );
    }

    #[test]
    fn a_completed_merge_carries_the_design_it_was_mixed_under() {
        // The stamp is written where the merge is marked done, not where it is
        // offered. A task that never finished has no artifact to make a claim
        // about, and a stamp taken at offer time would describe a design the
        // worker may not have mixed with.
        let (_d, mut inner) = design_fixture();
        let layout = inner.layout.clone();
        let mut t = Task::new(1, Stage::Merge);
        t.state = TaskState::Running;
        t.assigned_to = Some("w1".into());
        inner.tasks.insert(t.id(), t);
        std::fs::create_dir_all(layout.output()).unwrap();
        std::fs::write(layout.final_mp3(1), vec![0u8; 2000]).unwrap();

        inner.complete(&completion("w1", "merge:1", true, "merge ch1 -> out.mp3"));
        assert_eq!(inner.tasks["merge:1"].state, TaskState::Done);
        let stamped = inner.tasks["merge:1"]
            .design
            .clone()
            .expect("a finished merge says which design it was mixed under");

        // And it is the design in force, not a placeholder: the next sound edit
        // has to disagree with it, which is the whole reason to carry it.
        std::fs::write(
            layout.pool(bm_core::audio_pool::PoolKind::Effect),
            r#"{
              "wind": {"tags": ["wind"], "files": ["effects/wind-1.mp3"], "level": 0.9},
              "fire": {"tags": ["fire"], "files": ["effects/fire-1.mp3"], "level": 0.5}
            }"#,
        )
        .unwrap();
        let design = bm_core::design::MergeDesign::load(&layout);
        assert_ne!(
            inner
                .design_stamp(&design, inner.design_knobs(), 1)
                .unwrap(),
            stamped
        );
        assert!(inner.op_sound_changed().contains("1 merge(s) requeued"));
    }

    #[test]
    fn rerender_all_requeues_every_render_with_its_merge() {
        let (_d, mut inner) = fixture();
        inner.settings.engine = "vieneu".into();
        for n in [1u32, 2] {
            for stage in [Stage::Render, Stage::Merge] {
                let mut t = Task::new(n, stage);
                t.state = TaskState::Done;
                inner.tasks.insert(t.id(), t);
            }
            let seg = inner.layout.seg_dir("vieneu", n);
            std::fs::create_dir_all(&seg).unwrap();
            std::fs::write(seg.join("0000_Adam.wav"), vec![0u8; 2000]).unwrap();
            std::fs::write(inner.layout.final_mp3(n), vec![0u8; 2000]).unwrap();
        }
        let mut d = Task::new(1, Stage::Digest);
        d.state = TaskState::Done;
        inner.tasks.insert(d.id(), d);

        let msg = inner.op_rerender_all().expect("idle rerender");
        assert!(msg.contains("2 render(s)"), "{msg}");
        for n in [1u32, 2] {
            for stage in [Stage::Render, Stage::Merge] {
                let t = &inner.tasks[&format!("{stage}:{n}")];
                assert_eq!(t.state, TaskState::Pending, "{}", t.id());
                assert_eq!(t.attempts, 0);
                assert_eq!(t.detail, "requeued: rerender");
            }
            assert!(
                !inner.layout.seg_dir("vieneu", n).exists(),
                "segment cache goes, or reconcile marks it done"
            );
            assert!(!inner.layout.final_mp3(n).exists(), "stale product goes");
        }
        assert_eq!(inner.tasks["digest:1"].state, TaskState::Done);
    }

    #[test]
    fn remerge_all_requeues_only_merges_and_keeps_renders() {
        let (_d, mut inner) = fixture();
        for n in [1u32, 2] {
            for stage in [Stage::Render, Stage::Merge] {
                let mut t = Task::new(n, stage);
                t.state = TaskState::Done;
                inner.tasks.insert(t.id(), t);
            }
            std::fs::write(inner.layout.final_mp3(n), vec![0u8; 2000]).unwrap();
        }

        let msg = inner.op_remerge_all().expect("idle remerge");
        assert!(msg.contains("2 merge(s)"), "{msg}");
        for n in [1u32, 2] {
            let m = &inner.tasks[&format!("merge:{n}")];
            assert_eq!(m.state, TaskState::Pending, "{}", m.id());
            assert_eq!(m.attempts, 0);
            assert_eq!(m.detail, "requeued: remerge");
            assert!(!inner.layout.final_mp3(n).exists(), "stale product goes");
            assert_eq!(
                inner.tasks[&format!("render:{n}")].state,
                TaskState::Done,
                "render cache kept"
            );
        }
    }

    #[test]
    fn a_digest_offer_carries_the_inductors_analyzer_chain() {
        // The other half of the same defect. The backend *name* already
        // travelled in `analyzer`, but the model chain did not, so a
        // provisioned box — which has no `.bm/settings.json` to read, because
        // provisioning copies `prompts/`, `python/`, `assets/` and `refs/` and
        // never `.bm/` — digested with the compiled-in `Settings::default()`
        // and called `gemini-3.5-flash` long after the operator had switched
        // to `-lite`. No env involved: settings live on `Inner`.
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::create_dir_all(layout.chapters()).unwrap();
        std::fs::write(layout.chapter_txt(1), "Chương 1: X\n\nbody\n").unwrap();
        inner.settings.analyzer = "gemini".into();
        inner.settings.analyze_models = vec!["gemini-3.5-flash-lite".into()];
        inner.enqueue_translate(1, 1);
        inner
            .workers
            .insert("remote-w".into(), "192.168.2.2".into());

        let offer = inner.offer("remote-w").expect("digest:1 is offerable");
        assert_eq!(offer.task_id, "digest:1");
        assert_eq!(offer.analyzer, "gemini");
        assert_eq!(
            offer.analyzer_settings.analyze_models,
            Some(vec!["gemini-3.5-flash-lite".to_string()]),
            "the chain the worker must run"
        );
    }

    #[test]
    fn a_crawl_offer_carries_no_credentials_at_all() {
        // A crawl reads no provider, so the narrowing is what makes this empty.
        // The keys are set here on purpose: without them the assertion would
        // hold for the wrong reason — an environment that happened to be bare —
        // and would keep passing if the narrowing were deleted.
        let _g = env_lock();
        let (_d, mut inner) = fixture();
        inner.settings.analyzer = "gemini".into();
        inner.settings.engine = "gemini".into();
        inner.enqueue_translate(1, 1);
        inner
            .workers
            .insert("remote-w".into(), "192.168.2.2".into());

        std::env::set_var("GEMINI_API_KEY", "a-real-key");
        std::env::set_var("OPENROUTER_API_KEY", "another-real-key");
        let offer = inner.offer("remote-w").expect("crawl:1 is offerable");
        std::env::remove_var("GEMINI_API_KEY");
        std::env::remove_var("OPENROUTER_API_KEY");

        assert_eq!(offer.task_id, "crawl:1");
        assert!(
            offer.credentials.is_empty(),
            "a crawl fetches a URL and must ship no secret: {:?}",
            offer.credentials
        );
    }

    #[test]
    fn swap_leaves_a_chapter_the_character_is_not_in_alone() {
        // The narrowing that survives content addressing: **relevance**, not
        // "the local store looks complete". The old rule read the disk, and the
        // disk cannot prove that a legacy-named file holds the current text —
        // that is the whole reason takes are content-addressed now. What a swap
        // can still say for certain is that a chapter this character never
        // speaks in did not change.
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::write(
            layout.script(11),
            r#"{"segments":[{"speaker":"A","text":"x"},{"speaker":"A","text":"y"}]}"#,
        )
        .unwrap();
        std::fs::write(layout.cast("vieneu"), r#"{"A":"Đức Trí","B":"Adam"}"#).unwrap();
        let seg = layout.seg_dir("vieneu", 11);
        std::fs::create_dir_all(&seg).unwrap();
        inner.materialize_render_takes(11);
        let plan = bm_core::assemble::RenderPlan::load(&layout.plan(11)).unwrap();
        for t in &plan.takes {
            std::fs::write(seg.join(&t.file), vec![0u8; 2000]).unwrap();
        }
        inner.materialize_render_takes(11);
        assert!(inner.render_takes_done(11));
        std::fs::write(layout.final_mp3(11), vec![0u8; 2000]).unwrap();
        inner.ensure_task(11, Stage::Merge).state = TaskState::Done;

        // B speaks nowhere in this chapter: its voices cannot have moved.
        let msg = inner.op_swap_voice("B", "Minh Triết").unwrap();
        assert!(
            !msg.contains("[11]"),
            "a chapter that does not hear B is untouched: {msg}"
        );
        assert!(layout.final_mp3(11).exists(), "product stays");
        assert!(inner.render_takes_done(11), "the takes stay done");
        assert_eq!(inner.tasks["merge:11"].state, TaskState::Done);
        for t in &plan.takes {
            assert!(seg.join(&t.file).is_file(), "audio stays: {}", t.file);
        }
    }

    #[test]
    fn digest_completion_with_a_changed_script_invalidates_render() {
        // A re-digest rewrites run boundaries and voices: the kept render
        // would speak the old dramatization under the new one. Segments, mp3
        // and both tasks go; attempts reset because this is new work.
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::write(
            layout.script(12),
            r#"{"segments":[{"speaker":"A","text":"old line here"}]}"#,
        )
        .unwrap();
        std::fs::write(layout.cast("vieneu"), r#"{"A":"Adam"}"#).unwrap();
        let seg = layout.seg_dir("vieneu", 12);
        std::fs::create_dir_all(&seg).unwrap();
        std::fs::write(seg.join("0000_Adam.wav"), vec![0u8; 2000]).unwrap();
        std::fs::write(layout.final_mp3(12), vec![0u8; 2000]).unwrap();
        for stage in [Stage::Render, Stage::Merge] {
            let mut t = Task::new(12, stage);
            t.state = TaskState::Done;
            inner.tasks.insert(format!("{stage}:12"), t);
        }
        let mut t = Task::new(12, Stage::Digest);
        t.state = TaskState::Running;
        t.assigned_to = Some("w1".into());
        inner.tasks.insert("digest:12".into(), t);

        let mut c = completion("w1", "digest:12", true, "digest ch12");
        c.script =
            Some(serde_json::json!({"segments":[{"speaker":"A","text":"a rewritten line here"}]}));
        inner.complete(&c);

        assert_eq!(inner.tasks["render:12:0"].state, TaskState::Pending);
        assert_eq!(
            inner.tasks["render:12:0"].attempts, 0,
            "new work, not a retry"
        );
        assert_eq!(inner.tasks["merge:12"].state, TaskState::Pending);
        // A known invalidation never adopts: the old bytes are superseded, not
        // mistaken for the new take because the legacy name happened to match.
        assert!(!seg.join("0000_Adam.wav").exists(), "stale segments go");
        let plan = bm_core::assemble::RenderPlan::load(&layout.plan(12)).unwrap();
        assert!(
            plan.takes[0].file.starts_with("t-"),
            "{}",
            plan.takes[0].file
        );
        assert!(!layout.final_mp3(12).exists(), "stale product goes");
    }

    #[test]
    fn digest_completion_with_an_identical_script_invalidates_nothing() {
        // Duplicate reports are free: same bytes, no surgery.
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        let script = r#"{"segments":[{"speaker":"A","text":"old line here"}]}"#;
        std::fs::write(layout.script(12), script).unwrap();
        std::fs::write(layout.cast("vieneu"), r#"{"A":"Adam"}"#).unwrap();
        let seg = layout.seg_dir("vieneu", 12);
        std::fs::create_dir_all(&seg).unwrap();
        std::fs::write(seg.join("0000_Adam.wav"), vec![0u8; 2000]).unwrap();
        std::fs::write(layout.final_mp3(12), vec![0u8; 2000]).unwrap();
        for stage in [Stage::Render, Stage::Merge] {
            let mut t = Task::new(12, stage);
            t.state = TaskState::Done;
            inner.tasks.insert(format!("{stage}:12"), t);
        }
        let mut t = Task::new(12, Stage::Digest);
        t.state = TaskState::Running;
        t.assigned_to = Some("w1".into());
        inner.tasks.insert("digest:12".into(), t);

        let mut c = completion("w1", "digest:12", true, "digest ch12");
        c.script = Some(serde_json::from_str(script).unwrap());
        inner.complete(&c);

        assert_eq!(inner.tasks["render:12:0"].state, TaskState::Done);
        assert_eq!(inner.tasks["merge:12"].state, TaskState::Done);
        assert!(seg.join("0000_Adam.wav").exists(), "nothing touched");
        assert!(layout.final_mp3(12).exists(), "product stays");
    }

    #[test]
    fn retag_rewrites_laughs_and_requeues_only_edited_runs() {
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::write(
            layout.script(13),
            r#"{"segments":[{"speaker":"A","text":"Ha ha ha!"},{"speaker":"B","text":"z"}]}"#,
        )
        .unwrap();
        std::fs::write(layout.cast("vieneu"), r#"{"A":"Adam","B":"Đức Trí"}"#).unwrap();
        let seg = layout.seg_dir("vieneu", 13);
        std::fs::create_dir_all(&seg).unwrap();
        std::fs::write(seg.join("0000_Adam.wav"), vec![0u8; 2000]).unwrap();
        std::fs::write(seg.join("0001_Đức Trí.wav"), vec![0u8; 2000]).unwrap();
        std::fs::write(layout.final_mp3(13), vec![0u8; 2000]).unwrap();
        inner.materialize_render_takes(13);
        for stage in [Stage::Render, Stage::Merge] {
            let mut t = Task::new(13, stage);
            t.state = TaskState::Done;
            inner.tasks.insert(format!("{stage}:13"), t);
        }

        let msg = inner.op_retag(false).unwrap();
        assert!(msg.contains("13"), "touched chapter listed: {msg}");
        let back: serde_json::Value = bm_core::read_json(&layout.script(13)).unwrap();
        assert_eq!(back["segments"][0]["text"], "[cười]");
        assert_eq!(back["segments"][1]["text"], "z", "untouched text kept");
        assert!(
            !seg.join("0000_Adam.wav").exists(),
            "edited run's file goes"
        );
        assert!(
            seg.join("0001_Đức Trí.wav").exists(),
            "other runs keep cache"
        );
        assert!(!layout.final_mp3(13).exists(), "stale product goes");
        assert_eq!(
            inner.tasks["render:13:0"].state,
            TaskState::Pending,
            "the retagged run re-speaks"
        );
        assert_eq!(
            inner.tasks["render:13:1"].state,
            TaskState::Done,
            "and only it"
        );
        assert_eq!(inner.tasks["merge:13"].state, TaskState::Pending);

        // Second run is a no-op: deterministic convergence.
        let msg2 = inner.op_retag(false).unwrap();
        assert!(msg2.contains("already tags"), "{msg2}");
    }

    #[test]
    fn retag_dry_run_reports_without_writing() {
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::write(
            layout.script(15),
            r#"{"segments":[{"speaker":"A","text":"Haizz, x"}]}"#,
        )
        .unwrap();
        std::fs::write(layout.cast("vieneu"), r#"{"A":"Adam"}"#).unwrap();
        let seg = layout.seg_dir("vieneu", 15);
        std::fs::create_dir_all(&seg).unwrap();
        std::fs::write(seg.join("0000_Adam.wav"), vec![0u8; 2000]).unwrap();

        let msg = inner.op_retag(true).unwrap();
        assert!(msg.contains("15"), "report names the chapter: {msg}");
        let back: serde_json::Value = bm_core::read_json(&layout.script(15)).unwrap();
        assert!(back["segments"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("Haizz"));
        assert!(seg.join("0000_Adam.wav").exists(), "nothing deleted");
        assert!(!inner.tasks.contains_key("render:15"), "nothing requeued");
    }

    #[test]
    fn a_chapter_becomes_one_task_per_take_and_adopts_its_cache() {
        // The unit of render work is a **take**, not a chapter. A local edit
        // therefore re-speaks one segment instead of shipping the chapter and
        // hoping the worker re-derives the same names — and the *first* plan
        // adopts the cache already on disk, so writing one does not re-speak
        // the library.
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        let mut segs = Vec::new();
        for i in 0..32u32 {
            let sp = if i % 2 == 0 { "A" } else { "B" };
            segs.push(
                serde_json::json!({"speaker": sp, "text": format!("line {i} spoken aloud here")}),
            );
        }
        std::fs::write(
            layout.script(9),
            serde_json::json!({"segments": segs}).to_string(),
        )
        .unwrap();
        std::fs::write(layout.cast("vieneu"), r#"{"A":"Đức Trí","B":"Adam"}"#).unwrap();
        let seg = layout.seg_dir("vieneu", 9);
        std::fs::create_dir_all(&seg).unwrap();
        // Alternating speakers = 32 single-line runs, named the legacy way:
        // 0000..0031. Three of them are missing.
        for i in 0..32u32 {
            if [5, 17, 30].contains(&i) {
                continue;
            }
            let voice = if i % 2 == 0 { "Đức Trí" } else { "Adam" };
            std::fs::write(seg.join(format!("{i:04}_{voice}.wav")), vec![0u8; 2000]).unwrap();
        }

        let plan = inner
            .materialize_render_takes(9)
            .expect("plannable chapter");
        assert_eq!(plan.takes.len(), 32, "one take per spoken run");
        // Adopted: the pre-plan cache is carried, not re-spoken.
        assert_eq!(plan.takes.iter().filter(|t| t.adopted).count(), 29);
        let ids = inner.render_take_ids(9);
        assert_eq!(ids.len(), 32, "one ledger row per take");
        assert_eq!(ids[0], "render:9:0");
        assert_eq!(ids[31], "render:9:31");
        let work: Vec<&String> = ids
            .iter()
            .filter(|id| inner.tasks[*id].state == TaskState::Pending)
            .collect();
        assert_eq!(
            work.len(),
            3,
            "only the absent takes are work, not the whole chapter: {work:?}"
        );
        assert!(!inner.render_takes_done(9), "the merge gate holds");
        assert_eq!(inner.tasks["render:9:5"].state, TaskState::Pending);
        assert_eq!(inner.tasks["render:9:4"].state, TaskState::Done);

        // The take's offer is self-sufficient: text and voice travel with it,
        // so the worker needs neither the script nor the cast.
        let (unit, hash, force) = inner.take_spec(9, Some(5)).expect("a spec per take");
        assert_eq!(unit.speaker, "B");
        assert!(!unit.text.is_empty(), "the worker gets text, not a key");
        assert_eq!(unit.take_key.len(), 16);
        assert_eq!(hash.len(), 16, "the chapter's voice collection hash");
        assert!(
            unit.name.starts_with("t-"),
            "content-addressed: {}",
            unit.name
        );
        assert!(force.is_empty(), "a new take needs no forcing");

        // Fill the three gaps: nothing is work any more, and the plan covers.
        for id in &work {
            let pos: usize = id.rsplit(':').next().unwrap().parse().unwrap();
            let f = plan.takes[pos].file.clone();
            std::fs::write(seg.join(f), vec![0u8; 2000]).unwrap();
        }
        inner.materialize_render_takes(9);
        assert!(inner.render_takes_done(9), "coverage is the merge gate");

        // No script → no plan → no takes: the failure is named where it can be.
        assert!(inner.materialize_render_takes(77).is_none());
        assert!(inner.render_take_ids(77).is_empty());
    }

    #[test]
    fn a_render_row_outside_the_reconciled_range_is_materialised_at_startup() {
        // `--start/--count` decides what a run *discovers*; it must not decide
        // what it is willing to *repair*. A `render:n` row from an earlier run
        // survives in the ledger, and a take with no plan behind it can only
        // fail on the box (the plan is what names the take's file). The last
        // run of a `COUNT=100` default against a 150-chapter ledger offered 15
        // such chapters and failed all 45 attempts with "cannot be planned
        // here" — a bookkeeping gap reported as a worker fault.
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::write(
            layout.script(9),
            r#"{"segments":[{"speaker":"A","text":"một"}]}"#,
        )
        .unwrap();
        std::fs::write(layout.cast("vieneu"), r#"{"A":"Đức Trí"}"#).unwrap();
        // The ledger an older build left behind: chapter-granular, no takes.
        inner
            .tasks
            .insert("render:9".into(), Task::new(9, Stage::Render));
        assert!(inner.tasks["render:9"].take.is_none());

        // A range that does not contain chapter 9.
        inner.reconcile(50, 10);

        assert!(
            !inner.tasks.contains_key("render:9"),
            "the superseded chapter-granular row is gone"
        );
        assert_eq!(
            inner.render_take_ids(9),
            vec!["render:9:0".to_string()],
            "its takes are the rows now"
        );
        assert!(
            inner.layout.plan(9).is_file(),
            "and the plan names their files"
        );
    }

    #[test]
    fn an_unplannable_chapter_names_its_cause() {
        // The generic failure cost this run a diagnosis: 15 chapters reported
        // "cannot be planned here" and the message said nothing about why.
        let (_d, inner) = fixture();
        let layout = inner.layout.clone();
        let why = inner.why_unplannable(7);
        assert!(
            why.contains("script-07.json"),
            "the missing file is named: {why}"
        );

        // A chapter that plans says so, with the count the ledger would hold.
        std::fs::write(
            layout.script(8),
            r#"{"segments":[{"speaker":"A","text":"một"}]}"#,
        )
        .unwrap();
        std::fs::write(layout.cast("vieneu"), r#"{"A":"Đức Trí"}"#).unwrap();
        assert_eq!(inner.why_unplannable(8), "8 plans to 1 unit(s)");

        // A malformed script is a different repair, so it reads differently.
        std::fs::write(layout.script(8), r#"{"chapter":8}"#).unwrap();
        let why = inner.why_unplannable(8);
        assert!(
            why.contains("no `segments` array") && why.contains("script-08.json"),
            "the shape and the file are both named: {why}"
        );

        // A speaker the cast has never seen is *not* a failure: voices are
        // assigned on demand, which is what freezes them at plan time.
        std::fs::write(
            layout.script(8),
            r#"{"segments":[{"speaker":"Người lạ","text":"một"}]}"#,
        )
        .unwrap();
        assert_eq!(inner.why_unplannable(8), "8 plans to 1 unit(s)");
    }

    #[test]
    fn a_local_edit_re_speaks_only_the_takes_it_changed() {
        // The whole point of per-take tasks: a one-line retag costs one
        // segment. The plan's diff names the changed take; every other take
        // keeps its audio and its `Done` row.
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::write(
            layout.script(3),
            r#"{"segments":[{"speaker":"A","text":"một"},{"speaker":"B","text":"hai"}]}"#,
        )
        .unwrap();
        std::fs::write(layout.cast("vieneu"), r#"{"A":"Đức Trí","B":"Adam"}"#).unwrap();
        let seg = layout.seg_dir("vieneu", 3);
        std::fs::create_dir_all(&seg).unwrap();
        // Render both takes under their content-addressed names.
        inner.materialize_render_takes(3);
        let before = bm_core::assemble::RenderPlan::load(&layout.plan(3)).unwrap();
        for t in &before.takes {
            std::fs::write(seg.join(&t.file), vec![0u8; 2000]).unwrap();
        }
        inner.materialize_render_takes(3);
        assert!(inner.render_takes_done(3));

        // Retag B's line only.
        std::fs::write(
            layout.script(3),
            r#"{"segments":[{"speaker":"A","text":"một"},{"speaker":"B","text":"hai [thở dài]"}]}"#,
        )
        .unwrap();
        let files = inner.resume_render_after_edit(3, "requeued: retag");
        let after = bm_core::assemble::RenderPlan::load(&layout.plan(3)).unwrap();
        assert_eq!(
            after.takes[0].take_key, before.takes[0].take_key,
            "A is carried"
        );
        assert_ne!(
            after.takes[1].take_key, before.takes[1].take_key,
            "B changed"
        );
        assert_eq!(files, 1, "one superseded file, not the chapter");
        assert!(
            seg.join(&before.takes[0].file).is_file(),
            "A's audio survives"
        );
        assert!(
            !seg.join(&before.takes[1].file).is_file(),
            "B's stale file is gone"
        );
        assert_eq!(inner.tasks["render:3:0"].state, TaskState::Done);
        assert_eq!(inner.tasks["render:3:1"].state, TaskState::Pending);
        assert!(
            !inner.render_takes_done(3),
            "the merge waits for the new take"
        );
    }

    #[test]
    fn swap_requeues_a_chapter_with_no_local_segments() {
        // The remote-render case: a worker's segment cache never comes home,
        // so no stale file exists locally — but the chapter still speaks with
        // the old voice and must re-render. Gating on deleted files skipped
        // it silently and its mp3 kept the old voice forever.
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::write(
            layout.script(1),
            r#"{"segments":[{"speaker":"A","text":"x"},{"speaker":"B","text":"z"}]}"#,
        )
        .unwrap();
        std::fs::write(
            layout.cast("vieneu"),
            r#"{"A":"Đức Trí","B":"Adam","Narrator":"Đức Trí"}"#,
        )
        .unwrap();
        std::fs::write(layout.final_mp3(1), vec![0u8; 2000]).unwrap();
        for stage in [Stage::Render, Stage::Merge] {
            let mut t = Task::new(1, stage);
            t.state = TaskState::Done;
            inner.tasks.insert(format!("{stage}:1"), t);
        }

        let msg = inner.op_swap_voice("A", "Minh Triết").unwrap();
        assert!(msg.contains("[1]"), "chapter 1 must be listed: {msg}");
        assert!(!layout.final_mp3(1).exists(), "stale product goes away");
        assert!(
            !inner.render_take_ids(1).is_empty(),
            "the chapter re-renders per take"
        );
        assert!(!inner.render_takes_done(1), "the takes are work again");
        assert_eq!(inner.tasks["merge:1"].state, TaskState::Pending);
    }

    #[test]
    fn swap_leaves_a_chapter_the_speaker_never_enters() {
        // Narrowness in the other direction: chapters without the speaker
        // keep their product and their Done tasks.
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::write(
            layout.script(2),
            r#"{"segments":[{"speaker":"B","text":"z"}]}"#,
        )
        .unwrap();
        std::fs::write(
            layout.cast("vieneu"),
            r#"{"A":"Đức Trí","B":"Adam","Narrator":"Đức Trí"}"#,
        )
        .unwrap();
        std::fs::write(layout.final_mp3(2), vec![0u8; 2000]).unwrap();
        for stage in [Stage::Render, Stage::Merge] {
            let mut t = Task::new(2, stage);
            t.state = TaskState::Done;
            inner.tasks.insert(format!("{stage}:2"), t);
        }

        let msg = inner.op_swap_voice("A", "Minh Triết").unwrap();
        assert!(!msg.contains('2'), "chapter 2 speaks nothing of A: {msg}");
        assert!(layout.final_mp3(2).exists(), "untouched product stays");
        assert_eq!(inner.tasks["render:2"].state, TaskState::Done);
        assert_eq!(inner.tasks["merge:2"].state, TaskState::Done);
    }

    #[test]
    fn reconcile_folds_bible_cast_scripts_and_requeues_renders() {
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::write(
            layout.bible(),
            serde_json::to_string(&serde_json::json!({"characters": [
                {"name": "Huyền Vũ", "personality": "cold", "voice_hint": "adult male",
                 "proper_aliases": ["Huyền Vũ"], "first_seen": "10", "chapters_seen": ["10"]},
                {"name": "Huyền Vũ lão tổ", "personality": "", "voice_hint": "",
                 "proper_aliases": ["Huyền Vũ lão tổ"], "first_seen": "25", "chapters_seen": ["25"]}
            ]}))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            layout.cast("vieneu"),
            r#"{"Huyền Vũ":"Đức Trí","Huyền Vũ lão tổ":"Adam","Narrator":"Đức Trí"}"#,
        )
        .unwrap();
        std::fs::write(
            layout.script(25),
            r#"{"roster":["Huyền Vũ lão tổ"],"segments":[{"speaker":"Huyền Vũ lão tổ","text":"Ừ."}]}"#,
        )
        .unwrap();
        let seg = layout.seg_dir("vieneu", 25);
        std::fs::create_dir_all(&seg).unwrap();
        std::fs::write(seg.join("0000_Adam.wav"), vec![0u8; 2000]).unwrap();
        std::fs::write(layout.final_mp3(25), vec![0u8; 2000]).unwrap();

        let msg = inner
            .apply_reconcile(&[("Huyền Vũ".into(), vec!["Huyền Vũ lão tổ".into()])])
            .unwrap();
        assert!(msg.contains("Huyền Vũ <= Huyền Vũ lão tổ"), "{msg}");

        let bible: Value = bm_core::read_json(&layout.bible()).unwrap();
        let chars = bible["characters"].as_array().unwrap();
        assert_eq!(chars.len(), 1);
        assert!(chars[0]["proper_aliases"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == "Huyền Vũ lão tổ"));

        let cast = bm_core::cast::read_cast("vieneu", &layout.cast("vieneu"));
        assert_eq!(cast["Huyền Vũ"], "Đức Trí", "canonical keeps its voice");
        assert!(
            !cast.contains_key("Huyền Vũ lão tổ"),
            "absorbed key disappears"
        );

        let script: Value = bm_core::read_json(&layout.script(25)).unwrap();
        assert_eq!(
            script["segments"][0]["speaker"],
            serde_json::json!("Huyền Vũ")
        );
        assert_eq!(script["roster"], serde_json::json!(["Huyền Vũ"]));

        assert!(!seg.join("0000_Adam.wav").exists(), "loser's cache goes");
        assert!(!layout.final_mp3(25).exists(), "stale product goes away");
        assert!(
            !inner.render_takes_done(25),
            "the folded speaker's takes are work again"
        );
        assert_eq!(inner.tasks["merge:25"].state, TaskState::Pending);
        assert!(
            inner.events.iter().any(|e| e.text.contains("reconcile")),
            "logged"
        );
    }

    #[test]
    fn reconcile_with_no_merges_changes_nothing() {
        let (_d, mut inner) = fixture();
        let msg = inner.apply_reconcile(&[]).unwrap();
        assert!(msg.contains("nothing to fold"), "{msg}");
    }

    #[test]
    fn reconcile_folds_cast_only_variants_missing_from_the_bible() {
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::write(
            layout.bible(),
            serde_json::to_string(&serde_json::json!({"characters": [
                {"name": "Huyền Vũ", "personality": "cold", "voice_hint": "adult male",
                 "proper_aliases": ["Huyền Vũ"], "first_seen": "10", "chapters_seen": ["10"]}
            ]}))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            layout.cast("vieneu"),
            r#"{"Huyền Vũ":"Đức Trí","Huyền Vũ lão tổ":"Adam","Narrator":"Đức Trí"}"#,
        )
        .unwrap();
        std::fs::write(
            layout.script(25),
            r#"{"roster":["Huyền Vũ lão tổ"],"segments":[{"speaker":"Huyền Vũ lão tổ","text":"Ừ."}]}"#,
        )
        .unwrap();

        let msg = inner
            .apply_reconcile(&[("Huyền Vũ".into(), vec!["Huyền Vũ lão tổ".into()])])
            .unwrap();
        assert!(msg.contains("Huyền Vũ <= Huyền Vũ lão tổ"), "{msg}");

        let cast = bm_core::cast::read_cast("vieneu", &layout.cast("vieneu"));
        assert!(
            !cast.contains_key("Huyền Vũ lão tổ"),
            "absorbed key disappears"
        );
        let script: Value = bm_core::read_json(&layout.script(25)).unwrap();
        assert_eq!(
            script["segments"][0]["speaker"],
            serde_json::json!("Huyền Vũ")
        );
    }

    #[test]
    fn swap_admits_anything_in_the_catalogue() {
        // No machine-local roster exists: the shipped catalogue applies and it
        // restricts nothing, so any preset swaps.
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::create_dir_all(layout.bm_state()).unwrap();
        std::fs::write(layout.cast("vieneu"), r#"{"A":"Đức Trí"}"#).unwrap();

        assert!(inner.op_swap_voice("A", "Minh Đức").is_ok());

        // An admitted voice still swaps.
        let msg = inner.op_swap_voice("A", "Quang Sơn").unwrap();
        assert!(msg.contains("->"), "{msg}");

        // A stray roster file is ignored, not parsed: there is no overlay.
        std::fs::write(layout.bm_state().join("voices.json"), "{ this is not json").unwrap();
        assert!(inner.op_swap_voice("A", "Đức Trí").is_ok());
    }

    #[test]
    fn swap_admits_a_fresh_pool_sample_but_nothing_else_undeclared() {
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::create_dir_all(layout.bm_state()).unwrap();
        std::fs::write(layout.cast("vieneu"), r#"{"A":"Đức Trí"}"#).unwrap();
        std::fs::write(
            layout.root.join("voice-pool.json"),
            r#"{"young-female-1": {"file": "refs/young-female-1.mp3", "tags": ["young", "female"]}}"#,
        )
        .unwrap();

        // Pooled samples are vetted at adding: assignable with nobody on them.
        assert!(inner.op_swap_voice("A", "young-female-1").is_ok());
        // Anything else undeclared still needs a prior assignment to be trusted.
        let err = inner
            .op_swap_voice("A", "Chưa Từng Có")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("neither a preset nor an enrolled clone"),
            "{err}"
        );
    }

    #[test]
    fn swap_refuses_a_clone_no_bake_holds() {
        // A swap queues renders on every worker: a clone the bake lacks 500s
        // everywhere instead. Refuse it here, naming the fix — unless there
        // is no bake to check against (fixtures, fresh checkouts).
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::create_dir_all(layout.bm_state()).unwrap();
        std::fs::write(layout.cast("vieneu"), r#"{"A":"Đức Trí"}"#).unwrap();
        std::fs::write(
            layout.root.join("voice-pool.json"),
            r#"{"young-female-1": {"file": "refs/young-female-1.mp3", "tags": ["young", "female"]}}"#,
        )
        .unwrap();
        // No bake here: nothing to check against, swap proceeds.
        assert!(inner.op_swap_voice("A", "young-female-1").is_ok());
        // A bake that lacks the clone: refused loudly, before any render.
        std::fs::create_dir_all(layout.root.join("models")).unwrap();
        std::fs::write(
            layout.root.join("models/voices.json"),
            r#"{"presets":{"Đức Trí":{"speaker_emb":[1.0],"codes":[]}}}"#,
        )
        .unwrap();
        std::fs::write(layout.cast("vieneu"), r#"{"A":"Đức Trí"}"#).unwrap();
        let err = inner
            .op_swap_voice("A", "young-female-1")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not enrolled"), "{err}");
        // Enrolled in the bake: admitted again.
        std::fs::write(
            layout.root.join("models/voices.json"),
            r#"{"presets":{"Đức Trí":{"speaker_emb":[1.0],"codes":[]},"young-female-1":{"speaker_emb":[2.0],"codes":[]}}}"#,
        )
        .unwrap();
        assert!(inner.op_swap_voice("A", "young-female-1").is_ok());
    }

    fn busy_inner() -> (tempfile::TempDir, Inner) {
        // One chapter mid-render on w1: assigned task + fresh beat with task.
        let (d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::write(layout.cast("vieneu"), r#"{"A":"Đức Trí"}"#).unwrap();
        let mut t = Task::new(1, Stage::Render);
        t.state = TaskState::Assigned;
        t.assigned_to = Some("w1".into());
        t.lease_until = Some(now_secs() + 5000);
        inner.tasks.insert("render:1".into(), t);
        inner.beats.insert(
            "w1".into(),
            bm_proto::Heartbeat {
                worker_id: "w1".into(),
                addr: "127.0.0.1".into(),
                task_id: Some("render:1".into()),
                stage: Some(Stage::Render),
                chapter: Some(1),
                progress: 0.5,
                activity: "render".into(),
                eta_secs: None,
                ts: now_secs(),
                hostname: "box".into(),
                alias: String::new(),
                cpu_pct: None,
                mem_pct: None,
                mem_gb: None,
                sidecars: None,
                sidecar_gb: None,
                capabilities: vec![],
                sidecar_keep: None,
            },
        );
        (d, inner)
    }

    #[test]
    fn swap_refuses_while_workers_are_mid_play() {
        let (_d, mut inner) = busy_inner();
        let err = inner
            .op_swap_voice("A", "Quang Sơn")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("mid-play") && err.contains("render:1"),
            "{err}"
        );
        assert!(err.contains('X'), "names the way out: {err}");
    }

    #[test]
    fn swap_proceeds_on_stale_or_idle_evidence() {
        let (_d, mut inner) = busy_inner();
        // Stale beats + ghost assignment: the reaper's business, not a block.
        for b in inner.beats.values_mut() {
            b.ts = now_secs().saturating_sub(3600);
            b.task_id = None;
        }
        for t in inner.tasks.values_mut() {
            t.assigned_to = Some("ghost".into());
        }
        assert!(inner.op_swap_voice("A", "Quang Sơn").is_ok());
    }

    #[test]
    fn enqueue_seeds_crawl_done_so_the_digest_is_offerable() {
        // Empty boot (B reconciles nothing) + text on disk + no script: the
        // digest must become offerable, which needs crawl:1 Done, not missing.
        // Before the fix the digest waited forever while workers idled.
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::create_dir_all(layout.chapters()).unwrap();
        std::fs::write(layout.chapter_txt(1), "Chương 1: X\n\nbody\n").unwrap();

        let (crawls, digests) = inner.enqueue_translate(1, 1);
        assert_eq!((crawls, digests), (0, 1));
        assert_eq!(inner.tasks["crawl:1"].state, TaskState::Done);
        assert_eq!(inner.tasks["digest:1"].state, TaskState::Pending);

        inner.workers.insert("w1".into(), "127.0.0.1".into());
        let offer = inner.offer("w1").expect("digest:1 must be offered");
        assert_eq!(offer.task_id, "digest:1");
    }

    #[test]
    fn translate_heals_a_range_missing_downstream_tasks() {
        // Ledger holds only digest:done (hand-reset, older builds): after a
        // translate the full chain exists again instead of idling with nothing
        // offerable.
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::create_dir_all(layout.chapters()).unwrap();
        std::fs::write(layout.chapter_txt(1), "Chương 1: X\n\nbody\n").unwrap();
        std::fs::write(
            layout.script(1),
            r#"{"roster":["Narrator"],"segments":[{"speaker":"Narrator","text":"x"}]}"#,
        )
        .unwrap();
        inner.reconcile(1, 1);
        assert!(
            !inner.render_take_ids(1).is_empty(),
            "render recreated, one row per take"
        );
        assert!(inner.tasks.contains_key("merge:1"), "merge recreated");
        assert_eq!(inner.tasks["digest:1"].state, TaskState::Done);
        assert!(!inner.render_takes_done(1), "the takes are pending");
    }

    #[test]
    fn eta_reports_a_total_even_with_no_measurements() {
        let (_d, inner) = fixture();
        let msg = inner.op_eta(1, 10);
        assert!(msg.contains("total"), "{msg}");
        assert!(msg.contains("(guess)"), "{msg}");
    }

    #[test]
    fn the_eta_counts_takes_not_offers_so_batching_does_not_move_it() {
        // The coupling batching could have broken, and the reason it did not.
        // `:eta` sums `secs_per_unit × pending rows` per stage, and a render row
        // is one *take* whatever the batch size — so the estimate must be
        // identical whether ten takes travel per offer or one. If a later change
        // made the estimator count offers, or made a record's `units` the offer
        // count instead of the take count, this is what would catch it.
        let (_d, mut inner) = fixture();
        render_chapter(&mut inner, 1, 25);

        inner.settings.render_batch = 1;
        let single = inner.op_eta(1, 1);
        inner.settings.render_batch = 25;
        let batched = inner.op_eta(1, 1);
        assert_eq!(
            single, batched,
            "the batch size is a scheduling detail, not a cost"
        );
        assert!(
            single.contains("render"),
            "and it still prices the render: {single}"
        );

        // The take count is what the estimate is made of: handing the chapter
        // out in batches must not shrink the remaining work.
        let offer = inner
            .offer("w1")
            .expect("a full-chapter batch is offerable");
        assert_eq!(
            offer.render_units.as_ref().unwrap().len(),
            25,
            "all twenty-five takes in one offer"
        );
        assert_eq!(
            inner
                .tasks
                .values()
                .filter(|t| t.stage == Stage::Render && !t.state.is_terminal())
                .count(),
            25,
            "and twenty-five rows still owe work — an offer is not a unit of work"
        );
    }

    #[test]
    fn completions_feed_the_stats_pane_ledger() {
        // Only genuine completions count: the Done path records worker +
        // stage + duration, failures and stale reports record nothing.
        let (_d, mut inner) = fixture();
        let mut t = Task::new(1, Stage::Digest);
        t.state = TaskState::Assigned;
        t.assigned_to = Some("w1".into());
        inner.tasks.insert("digest:1".into(), t);
        let done = |task: &str, worker: &str, ok: bool| Complete {
            worker_id: worker.into(),
            task_id: task.into(),
            ok,
            detail: String::new(),
            duration_secs: 30.0,
            bible_delta: None,
            units: 0,
            script: None,
            text: None,
            mp3_b64: None,
            unit_files: Vec::new(),
        };
        let msg = inner.complete(&done("digest:1", "w1", true));
        assert!(msg.contains("done"), "{msg}");
        let summary = inner.stats.summary();
        assert_eq!(summary["counts"]["w1"]["digest"], 1);
        assert_eq!(summary["avg_task_secs"]["digest"], 30.0);
        // A failure on the next chapter moves the task, not the ledger.
        let mut t = Task::new(2, Stage::Digest);
        t.state = TaskState::Assigned;
        t.assigned_to = Some("w1".into());
        inner.tasks.insert("digest:2".into(), t);
        inner.complete(&done("digest:2", "w1", false));
        let summary = inner.stats.summary();
        assert_eq!(
            summary["counts"]["w1"]["digest"], 1,
            "failures count nothing"
        );
        assert!(summary["counts"]["w1"].get("crawl").is_none());
    }

    #[test]
    fn reap_frees_dead_workers_tasks_but_not_after_a_reboot() {
        let (_d, mut inner) = fixture();
        let now = now_secs();
        let mut t = Task::new(2, Stage::Digest);
        t.state = TaskState::Assigned;
        t.assigned_to = Some("ghost".into());
        t.lease_until = Some(now + 5000);
        inner.tasks.insert("digest:2".into(), t);

        // Fresh boot: beats haven't arrived yet — hands off.
        assert!(inner.reap().is_empty(), "boot grace must hold");
        assert_eq!(inner.tasks["digest:2"].state, TaskState::Assigned);

        // Long after boot with still no beat: the worker is gone, free it.
        inner.started_at = now.saturating_sub(1000);
        let freed = inner.reap();
        assert_eq!(freed, vec!["digest:2".to_string()]);
        assert_eq!(inner.tasks["digest:2"].state, TaskState::Pending);

        // A live beat protects the assignment again.
        let mut t = Task::new(3, Stage::Digest);
        t.state = TaskState::Assigned;
        t.assigned_to = Some("w-live".into());
        t.lease_until = Some(now + 5000);
        inner.tasks.insert("digest:3".into(), t);
        inner.beats.insert(
            "w-live".into(),
            bm_proto::Heartbeat {
                worker_id: "w-live".into(),
                addr: "127.0.0.1".into(),
                task_id: Some("digest:3".into()),
                stage: Some(Stage::Digest),
                chapter: Some(3),
                progress: 0.5,
                activity: "digest".into(),
                eta_secs: None,
                ts: now_secs(),
                hostname: "box".into(),
                alias: String::new(),
                cpu_pct: None,
                mem_pct: None,
                mem_gb: None,
                sidecars: None,
                sidecar_gb: None,
                capabilities: vec![],
                sidecar_keep: None,
            },
        );
        assert!(inner.reap().is_empty(), "live worker untouched");
    }

    #[test]
    fn an_expiry_on_a_live_worker_is_reported_as_a_hang_not_a_lost_box() {
        // The blind spot this closes. "Silence is not failure" is right for a
        // worker that died: it costs no strike and needs no human. A worker that
        // is **alive and stuck** looks identical from this side — fresh beat,
        // task never finished — so on 2026-09-22 two digest rows were requeued
        // silently for ~80 minutes while the TUI showed a percentage that never
        // moved and the events pane offered nothing but routine-looking
        // warnings. The distinction is available (the beat is fresh), so the
        // reaper now says it out loud.
        let (_d, mut inner) = fixture();
        let now = now_secs();
        let arm = |inner: &mut Inner, who: &str, expires_at: u64| {
            let t = inner.tasks.get_mut("digest:9").unwrap();
            t.state = TaskState::Running;
            t.assigned_to = Some(who.into());
            t.lease_until = Some(expires_at);
            let _ = t;
        };
        let mut t = Task::new(9, Stage::Digest);
        t.state = TaskState::Running;
        t.assigned_to = Some("w-live".into());
        t.lease_until = Some(now - 5);
        inner.tasks.insert("digest:9".into(), t);
        inner
            .beats
            .insert("w-live".into(), beat_with_load("w-live", "127.0.0.1", None));

        let freed = inner.reap();
        assert_eq!(
            freed,
            vec!["digest:9".to_string()],
            "the requeue itself is unchanged"
        );
        assert_eq!(inner.tasks["digest:9"].state, TaskState::Pending);
        assert_eq!(
            inner.tasks["digest:9"].attempts, 0,
            "still strike-free — silence is still not failure"
        );
        assert_eq!(
            inner.tasks["digest:9"].expiries, 1,
            "but the expiry is counted now"
        );

        let text = inner
            .recent_events(5)
            .iter()
            .map(|e| e.text.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("still beating"),
            "a live holder is the whole point: {text}"
        );
        assert!(text.contains("digest:9"), "the row is named: {text}");
        assert!(text.contains("w-live"), "and so is the worker: {text}");
        assert!(
            text.contains("log tail"),
            "and it says where to look next: {text}"
        );

        // The second one escalates: by then the loop is the story, not a hiccup.
        arm(&mut inner, "w-live", now - 1);
        inner
            .beats
            .insert("w-live".into(), beat_with_load("w-live", "127.0.0.1", None));
        inner.reap();
        assert_eq!(inner.tasks["digest:9"].expiries, 2);
        let last = inner.recent_events(1).into_iter().next().unwrap().clone();
        assert_eq!(last.level, "error", "the repeat is an error: {}", last.text);
        assert!(last.text.contains("2×"), "and counts them: {}", last.text);
        assert!(
            last.text.contains("hang, not a hiccup"),
            "and calls it what it is: {}",
            last.text
        );

        // A *dead* holder is the case the strike-free rule was written for, and
        // it must not be dressed up as a hang: no counter, no such event.
        let mut t = Task::new(10, Stage::Digest);
        t.state = TaskState::Running;
        t.assigned_to = Some("ghost".into());
        t.lease_until = Some(now - 5);
        inner.tasks.insert("digest:10".into(), t);
        inner.reap();
        assert_eq!(
            inner.tasks["digest:10"].expiries, 0,
            "a lost box is not a hang and is not counted as one"
        );
        let text = inner
            .recent_events(8)
            .iter()
            .map(|e| e.text.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !text.contains("digest:10 expired while"),
            "and no hang event is invented for it: {text}"
        );
    }

    #[test]
    fn a_settled_row_forgets_its_silent_expiry_streak() {
        // The counter means "this assignment kept expiring while its worker was
        // alive". Once the assignment actually resolves that story is over, and a
        // stale count would make every later, legitimate expiry look like a
        // repeat — which is how a diagnostic turns into a false alarm.
        let (_d, mut inner) = fixture();
        let now = now_secs();
        let mut t = Task::new(11, Stage::Digest);
        t.state = TaskState::Running;
        t.assigned_to = Some("w-live".into());
        t.lease_until = Some(now - 5);
        t.expiries = 3;
        inner.tasks.insert("digest:11".into(), t);
        inner
            .beats
            .insert("w-live".into(), beat_with_load("w-live", "127.0.0.1", None));
        inner.reap();
        assert_eq!(
            inner.tasks["digest:11"].expiries, 4,
            "still counting while it keeps happening"
        );

        // Now the worker finally answers, successfully.
        let t = inner.tasks.get_mut("digest:11").unwrap();
        t.state = TaskState::Running;
        t.assigned_to = Some("w-live".into());
        inner.complete(&completion("w-live", "digest:11", true, "ok"));
        assert_eq!(
            inner.tasks["digest:11"].expiries, 0,
            "a completion ends the streak"
        );

        // And a *reported failure* does too: the worker got far enough to say
        // something, so it was not stuck.
        let mut t = Task::new(12, Stage::Digest);
        t.state = TaskState::Running;
        t.assigned_to = Some("w-live".into());
        t.expiries = 2;
        inner.tasks.insert("digest:12".into(), t);
        inner.complete(&completion("w-live", "digest:12", false, "engine died"));
        assert_eq!(inner.tasks["digest:12"].expiries, 0);
    }

    #[test]
    fn requeue_orphans_frees_dead_workers_tasks_only() {
        let (_d, mut inner) = fixture();
        let now = now_secs();
        // Ghost-held task, live-held task, and an unassigned one.
        for (id, ch, who) in [("digest:2", 2, "ghost"), ("digest:3", 3, "w-live")] {
            let mut t = Task::new(ch, Stage::Digest);
            t.state = TaskState::Assigned;
            t.assigned_to = Some(who.into());
            t.lease_until = Some(now + 5000);
            inner.tasks.insert(id.into(), t);
        }
        inner
            .tasks
            .insert("digest:4".into(), Task::new(4, Stage::Digest));
        inner.beats.insert(
            "w-live".into(),
            bm_proto::Heartbeat {
                worker_id: "w-live".into(),
                addr: "127.0.0.1".into(),
                task_id: Some("digest:3".into()),
                stage: Some(Stage::Digest),
                chapter: Some(3),
                progress: 0.5,
                activity: "digest".into(),
                eta_secs: None,
                ts: now,
                hostname: "box".into(),
                alias: String::new(),
                cpu_pct: None,
                mem_pct: None,
                mem_gb: None,
                sidecars: None,
                sidecar_gb: None,
                capabilities: vec![],
                sidecar_keep: None,
            },
        );
        let msg = inner.op_requeue_orphans();
        assert!(msg.contains("digest:2"), "{msg}");
        assert!(!msg.contains("digest:3"), "live worker untouched: {msg}");
        assert_eq!(inner.tasks["digest:2"].state, TaskState::Pending);
        assert_eq!(
            inner.tasks["digest:2"].attempts, 0,
            "strikes untouched (none here)"
        );
        assert_eq!(inner.tasks["digest:3"].state, TaskState::Assigned);
        assert!(
            inner.op_requeue_orphans().contains("no orphaned"),
            "second run is a no-op"
        );
    }

    #[test]
    fn retry_shelved_resets_strikes_and_reoffers_the_chapter() {
        let (_d, mut inner) = fixture();
        let mut t = Task::new(2, Stage::Digest);
        t.state = TaskState::Shelved;
        t.attempts = 3;
        inner.tasks.insert("digest:2".into(), t);
        let mut c = Task::new(2, Stage::Crawl);
        c.state = TaskState::Done;
        inner.tasks.insert("crawl:2".into(), c);
        inner.workers.insert("w1".into(), "127.0.0.1".into());

        let msg = inner.op_retry_shelved();
        assert!(msg.contains("digest:2"), "{msg}");
        assert_eq!(inner.tasks["digest:2"].state, TaskState::Pending);
        assert_eq!(
            inner.tasks["digest:2"].attempts, 0,
            "manual retry forgives strikes"
        );
        // The chapter-level shelve gate lifts, so the task is offerable again.
        assert!(inner.offer("w1").is_some(), "retried task must be offered");
        assert!(
            inner.op_retry_shelved().contains("no shelved"),
            "second run is a no-op"
        );
    }

    #[test]
    fn retry_chapter_takes_only_that_chapters_shelved_tasks() {
        // The middle scope, `:retry 24`: narrower than the blanket retry, which
        // forgives every strike in the ledger, and wider than `F`, which names
        // one stage. A neighbouring chapter's shelved task must not move.
        let (_d, mut inner) = fixture();
        for (chapter, stage, state) in [
            (24u32, Stage::Digest, TaskState::Shelved),
            (24, Stage::Render, TaskState::Shelved),
            (24, Stage::Merge, TaskState::Pending),
            (25, Stage::Digest, TaskState::Shelved),
            (24, Stage::Crawl, TaskState::Done),
        ] {
            let mut t = Task::new(chapter, stage);
            t.state = state;
            t.attempts = 3;
            inner.tasks.insert(t.id(), t);
        }

        let msg = inner.op_retry_chapter(24);
        assert!(msg.contains("ch24"), "{msg}");
        assert!(msg.contains("digest:24"), "{msg}");
        assert!(msg.contains("render:24"), "{msg}");
        assert_eq!(inner.tasks["digest:24"].state, TaskState::Pending);
        assert_eq!(inner.tasks["render:24"].state, TaskState::Pending);
        assert_eq!(inner.tasks["digest:24"].attempts, 0);
        assert_eq!(
            inner.tasks["digest:25"].state,
            TaskState::Shelved,
            "another chapter is untouched"
        );
        assert_eq!(
            inner.tasks["merge:24"].state,
            TaskState::Pending,
            "only shelved tasks move"
        );
        assert_eq!(
            inner.tasks["crawl:24"].state,
            TaskState::Done,
            "and never a finished one"
        );
        assert!(
            inner.op_retry_chapter(24).contains("no shelved"),
            "second run is a no-op"
        );
        assert!(
            inner
                .op_retry_chapter(99)
                .contains("no shelved tasks on ch99"),
            "and it says which chapter it looked at"
        );
    }

    fn completion(worker: &str, task: &str, ok: bool, detail: &str) -> Complete {
        Complete {
            worker_id: worker.into(),
            task_id: task.into(),
            ok,
            detail: detail.into(),
            duration_secs: 12.5,
            bible_delta: None,
            units: 0,
            script: None,
            text: None,
            mp3_b64: None,
            unit_files: Vec::new(),
        }
    }

    #[test]
    fn an_operators_digest_report_lands_and_releases_the_box_working_on_it() {
        // The manual digest reports over the **same endpoint a worker does**,
        // under the reserved `operator` id. Two things have to hold, and neither
        // is obvious from either side alone:
        //
        // 1. It is accepted even though the row belongs to somebody else. That
        //    bypass is the entire reason a manual digest can finish a chapter a
        //    box is already grinding on.
        // 2. Accepting it **is** the release. Marking the row Done clears the
        //    assignment, so the box's own report later finds a row it no longer
        //    owns and is dropped as stale — which is option A, with nothing new
        //    on the wire and no instruction the worker has to understand.
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();

        // A worker is mid-digest on chapter 7, holding the row.
        let mut t = Task::new(7, Stage::Digest);
        t.state = TaskState::Running;
        t.assigned_to = Some("w1".into());
        t.lease_until = Some(now_secs() + 600);
        inner.tasks.insert("digest:7".into(), t);

        // The operator finishes it by hand.
        let mut c = completion(
            bm_proto::MANUAL_WORKER,
            "digest:7",
            true,
            "digest ch7 by hand",
        );
        c.script = Some(serde_json::json!({
            "title": "Bảy", "atmosphere": "quiet", "roster": ["Narrator"],
            "mentions": {}, "fixes": [],
            "segments": [{"speaker": "Narrator", "text": "Xong."}],
        }));
        c.bible_delta = Some(serde_json::json!({
            "new_characters": [], "new_aliases": {},
            "roster": ["Narrator"], "segments": [],
        }));
        let line = inner.complete(&c);
        assert!(
            !line.contains("stale"),
            "the operator's report is taken: {line}"
        );

        let row = &inner.tasks["digest:7"];
        assert_eq!(
            row.state,
            TaskState::Done,
            "the chapter is digested: {line}"
        );
        assert_eq!(row.assigned_to, None, "and the box is released from it");
        assert!(
            layout.script(7).is_file(),
            "the script landed where every downstream stage reads it"
        );
        let written: Value = bm_core::read_json(&layout.script(7)).unwrap();
        assert_eq!(written["roster"], serde_json::json!(["Narrator"]));
        // The inductor is the single bible writer, and a manual digest does not
        // get to be the exception — the delta went through the same merge.
        let bible: Value = bm_core::read_json(&layout.bible()).unwrap();
        assert!(
            bible.get("characters").is_some(),
            "the bible was written: {bible}"
        );

        // And the box's own report, arriving after, changes nothing.
        let late = inner.complete(&completion(
            "w1",
            "digest:7",
            true,
            "worker got there second",
        ));
        assert!(
            late.contains("stale"),
            "the box's late report is dropped, not applied: {late}"
        );
        assert_eq!(
            inner.tasks["digest:7"].state,
            TaskState::Done,
            "and the row is still the operator's answer"
        );
    }

    #[test]
    fn a_manual_digest_for_a_chapter_with_no_row_creates_it() {
        // Working ahead of the enqueue: the operator digests the next chapter
        // before any worker task exists for it, so there is no row to report
        // against. The report creates it rather than bouncing as unknown —
        // otherwise the D screen could never offer that chapter.
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        assert!(!inner.tasks.contains_key("digest:8"));

        let mut c = completion(
            bm_proto::MANUAL_WORKER,
            "digest:8",
            true,
            "digest ch8 by hand",
        );
        c.script = Some(serde_json::json!({
            "title": "Tám", "atmosphere": "quiet", "roster": ["Narrator"],
            "mentions": {}, "fixes": [],
            "segments": [{"speaker": "Narrator", "text": "Xong."}],
        }));
        c.bible_delta = Some(serde_json::json!({
            "new_characters": [], "new_aliases": {},
            "roster": ["Narrator"], "segments": [],
        }));
        let line = inner.complete(&c);
        assert!(!line.contains("unknown task"), "{line}");
        assert_eq!(inner.tasks["digest:8"].state, TaskState::Done, "{line}");
        assert!(
            layout.script(8).is_file(),
            "the script landed where downstream stages read it"
        );
    }

    #[test]
    fn a_workers_late_failure_after_a_manual_digest_is_stale_not_a_strike() {
        // The race the other way: the box was grinding on the chapter the
        // operator just finished, and its failure arrives after. It must not
        // strike the row — three such late failures would shelve a digested
        // chapter and send the next worker to redo finished work.
        let (_d, mut inner) = fixture();
        let mut t = Task::new(7, Stage::Digest);
        t.state = TaskState::Running;
        t.assigned_to = Some("w1".into());
        t.lease_until = Some(now_secs() + 600);
        inner.tasks.insert("digest:7".into(), t);

        let mut c = completion(
            bm_proto::MANUAL_WORKER,
            "digest:7",
            true,
            "digest ch7 by hand",
        );
        c.script = Some(serde_json::json!({
            "title": "Bảy", "atmosphere": "quiet", "roster": ["Narrator"],
            "mentions": {}, "fixes": [],
            "segments": [{"speaker": "Narrator", "text": "Xong."}],
        }));
        let _ = inner.complete(&c);

        let late = inner.complete(&completion("w1", "digest:7", false, "model 503"));
        assert!(late.contains("stale"), "{late}");
        let row = &inner.tasks["digest:7"];
        assert_eq!(row.state, TaskState::Done);
        assert_eq!(row.attempts, 0, "no strike from a race the operator won");
    }

    #[test]
    fn recast_fixes_speakers_and_requeues_only_what_the_edit_reached() {
        // ch112's shape: narration given to the character it describes, and
        // a quote defaulted to Narrator. The op rewrites the named segments,
        // refuses unknown voices, and the plan invalidation requeues the mix.
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::write(
            layout.script(9),
            r#"{"title":"T","atmosphere":"q","roster":["Narrator","A"],"mentions":{},
                "segments":[{"speaker":"A","text":"Nàng nhíu mày."},{"speaker":"Narrator","text":"Đi thôi."}],"fixes":[]}"#,
        )
        .unwrap();
        std::fs::write(
            layout.cast("vieneu"),
            r#"{"Narrator":"Đức Trí","A":"Adam"}"#,
        )
        .unwrap();
        inner.materialize_render_takes(9);

        let msg = inner
            .op_recast(
                9,
                &[
                    bm_proto::SpeakerFix {
                        index: 0,
                        speaker: "Narrator".into(),
                    },
                    bm_proto::SpeakerFix {
                        index: 1,
                        speaker: "A".into(),
                    },
                ],
                &[],
            )
            .expect("two legal fixes apply");
        assert!(msg.contains("#0 A→Narrator"), "{msg}");
        assert!(msg.contains("#1 Narrator→A"), "{msg}");
        let back: Value = bm_core::read_json(&layout.script(9)).unwrap();
        assert_eq!(
            back["segments"][0]["speaker"],
            serde_json::json!("Narrator")
        );
        assert_eq!(back["segments"][1]["speaker"], serde_json::json!("A"));
        assert_eq!(
            inner.tasks.get("merge:9").map(|t| t.state),
            Some(TaskState::Pending),
            "the mix comes back: {msg}"
        );

        // Same fixes again: nothing to do, and no requeue churn.
        let again = inner
            .op_recast(
                9,
                &[bm_proto::SpeakerFix {
                    index: 0,
                    speaker: "Narrator".into(),
                }],
                &[],
            )
            .expect("an idempotent fix is not an error");
        assert!(again.contains("nothing changed"), "{again}");

        // A voice nobody holds would requeue into an unplannable row.
        let err = inner
            .op_recast(
                9,
                &[bm_proto::SpeakerFix {
                    index: 0,
                    speaker: "Ghost".into(),
                }],
                &[],
            )
            .expect_err("unknown voices are refused");
        assert!(err.to_string().contains("no voice"), "{err}");

        // A sound item has no speaker to move.
        let mut data: Value = bm_core::read_json(&layout.script(9)).unwrap();
        data["segments"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"sound": "coin"}));
        let _ = bm_core::atomic_write(
            &layout.script(9),
            &serde_json::to_string_pretty(&data).unwrap_or_default(),
        );
        let err = inner
            .op_recast(
                9,
                &[bm_proto::SpeakerFix {
                    index: 2,
                    speaker: "A".into(),
                }],
                &[],
            )
            .expect_err("sounds are refused");
        assert!(err.to_string().contains("sound"), "{err}");

        // Deleting the duplicated line: indexes run against the script as
        // the operator sees it, sounds included, and the mix comes back.
        let msg = inner
            .op_recast(9, &[], &[1])
            .expect("removing a duplicated line applies");
        assert!(msg.contains("removed 1 duplicated segments"), "{msg}");
        let back: Value = bm_core::read_json(&layout.script(9)).unwrap();
        assert_eq!(back["segments"].as_array().unwrap().len(), 2);
        assert_eq!(
            inner.tasks.get("merge:9").map(|t| t.state),
            Some(TaskState::Pending),
            "deleting requeues the mix too"
        );
    }

    fn racing_digest(inner: &mut Inner, chapter: u32) {
        let mut c = Task::new(chapter, Stage::Crawl);
        c.state = TaskState::Done;
        inner.tasks.insert(c.id(), c);
        let d = Task::new(chapter, Stage::Digest);
        inner.tasks.insert(d.id(), d);
        inner.workers.insert("w1".into(), "127.0.0.1".into());
        inner.workers.insert("w2".into(), "127.0.0.1".into());
        inner.machines.insert(
            "127.0.0.1".into(),
            Machine::new("127.0.0.1", "local", 22, None, "both"),
        );
    }

    #[test]
    fn an_idle_digest_worker_joins_the_head_digest_instead_of_idling() {
        // The bottleneck racing exists for: the N-1→N chain leaves exactly
        // one digest offerable, so a second digest worker would sit out a
        // 20-minute LLM call. It joins as a racer — same row, same snapshot.
        let (_d, mut inner) = fixture();
        racing_digest(&mut inner, 1);
        let first = inner.offer("w1").expect("head digest is offerable");
        assert_eq!(first.task_id, "digest:1");
        assert_eq!(inner.tasks["digest:1"].assigned_to.as_deref(), Some("w1"));

        let second = inner.offer("w2").expect("w2 races the same row");
        assert_eq!(second.task_id, "digest:1");
        let row = &inner.tasks["digest:1"];
        assert_eq!(row.state, TaskState::Assigned);
        assert_eq!(
            row.assigned_to.as_deref(),
            Some("w1"),
            "primary keeps its seat"
        );
        assert_eq!(row.racers, vec!["w2".to_string()]);

        assert!(
            inner.offer("w2").is_none(),
            "a holder is never offered its own row twice"
        );
    }

    #[test]
    fn the_first_digest_report_wins_and_the_loser_is_stale_strike_free() {
        let (_d, mut inner) = fixture();
        racing_digest(&mut inner, 1);
        let _ = inner.offer("w1").expect("w1 takes it");
        let _ = inner.offer("w2").expect("w2 races it");

        let win = inner.complete(&completion("w2", "digest:1", true, "digest ch1 via gemini"));
        assert!(!win.contains("stale"), "{win}");
        assert_eq!(inner.tasks["digest:1"].state, TaskState::Done);

        let late = inner.complete(&completion(
            "w1",
            "digest:1",
            true,
            "digest ch1 via opencode",
        ));
        assert!(late.contains("stale"), "{late}");
        let row = &inner.tasks["digest:1"];
        assert_eq!(row.state, TaskState::Done);
        assert_eq!(row.attempts, 0, "the loser strikes nothing");
        assert!(row.racers.is_empty() && row.assigned_to.is_none());
    }

    #[test]
    fn a_failing_racer_costs_no_strike_while_the_race_runs() {
        // One bad box in a race must not park the chapter: the failure drops
        // just that holder, and only the last holder's failure strikes.
        let (_d, mut inner) = fixture();
        racing_digest(&mut inner, 1);
        let _ = inner.offer("w1").expect("w1 takes it");
        let _ = inner.offer("w2").expect("w2 races it");

        let out = inner.complete(&completion("w2", "digest:1", false, "model 503"));
        assert!(out.contains("still racing"), "{out}");
        let row = &inner.tasks["digest:1"];
        assert_eq!(row.state, TaskState::Assigned);
        assert_eq!(row.assigned_to.as_deref(), Some("w1"));
        assert!(row.racers.is_empty());
        assert_eq!(row.attempts, 0, "a racing failure is not a strike");

        let last = inner.complete(&completion("w1", "digest:1", false, "model 503"));
        assert!(!last.contains("still racing"), "{last}");
        let row = &inner.tasks["digest:1"];
        assert_eq!(row.state, TaskState::Pending);
        assert_eq!(row.attempts, 1, "the last holder's failure strikes once");
    }

    #[test]
    fn digest_shelves_at_fifteen_not_three() {
        let (_d, mut inner) = fixture();
        racing_digest(&mut inner, 1);
        let _ = inner.offer("w1").expect("w1 takes it");
        // Two ordinary failures: a crawl would be one strike from shelved,
        // a digest is barely started.
        inner.tasks.get_mut("digest:1").unwrap().attempts = 2;
        let line = inner.complete(&completion("w1", "digest:1", false, "model 503"));
        assert!(line.contains("failed"), "{line}");
        assert_eq!(inner.tasks["digest:1"].state, TaskState::Pending);
        assert_eq!(inner.tasks["digest:1"].attempts, 3);
        let last = inner.recent_events(1).into_iter().next().unwrap().clone();
        assert!(last.text.contains("will retry"), "{last:?}");
    }

    #[test]
    fn a_merge_that_outruns_its_plan_replans_instead_of_failing_again() {
        // The stale-plan loop: a digest lands after the render plan was
        // built, so the recorded take list is short of the timeline. Without
        // a heal the merge fails the same way three times into shelved, and
        // a retry just re-offers the same stale plan. The failure rebuilds
        // the plan from the current script instead.
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::write(
            layout.script(7),
            r#"{"segments":[{"speaker":"A","text":"x"},{"speaker":"B","text":"y"}]}"#,
        )
        .unwrap();
        std::fs::write(layout.cast("vieneu"), r#"{"A":"Đức Trí","B":"Adam"}"#).unwrap();
        inner.materialize_render_takes(7);
        assert!(inner.tasks.contains_key("render:7:1"));
        assert!(!inner.tasks.contains_key("render:7:2"));

        // The digest lands afterwards: three runs under a two-take plan.
        std::fs::write(
            layout.script(7),
            r#"{"segments":[{"speaker":"A","text":"x"},{"speaker":"B","text":"y"},{"speaker":"A","text":"z"}]}"#,
        )
        .unwrap();
        let mut m = Task::new(7, Stage::Merge);
        m.state = TaskState::Running;
        m.assigned_to = Some("w1".into());
        m.lease_until = Some(now_secs() + 600);
        inner.tasks.insert("merge:7".into(), m);

        let line = inner.complete(&completion(
            "w1",
            "merge:7",
            false,
            "timeline has 3 turns for 2 rendered segments — the render plan and the timeline disagree",
        ));
        assert!(!line.contains("stale"), "{line}");
        assert!(
            inner.tasks.contains_key("render:7:2"),
            "the new run is work again: {line}"
        );
        assert_eq!(
            inner.tasks["merge:7"].state,
            TaskState::Pending,
            "the merge keeps its strike and retries against the fresh plan"
        );
    }

    #[test]
    fn a_render_report_with_missing_files_is_rejected_by_name() {
        // The completion gate: the worker's word is not evidence. Mutate one
        // wav below the completeness threshold and the `ok` report must fail
        // naming exactly that file, so the next offer repeats it.
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::write(
            layout.script(6),
            r#"{"segments":[{"speaker":"A","text":"x"},{"speaker":"A","text":"y"},{"speaker":"B","text":"z"}]}"#,
        )
        .unwrap();
        std::fs::write(layout.cast("vieneu"), r#"{"A":"Đức Trí","B":"Adam"}"#).unwrap();
        let seg = layout.seg_dir("vieneu", 6);
        std::fs::create_dir_all(&seg).unwrap();
        std::fs::write(seg.join("0000-0001_Đức Trí.wav"), vec![0u8; 2000]).unwrap();
        std::fs::write(seg.join("0002_Adam.wav"), vec![0u8; 500]).unwrap();
        // The plan is what names the file, so a report about a take whose
        // bytes never landed fails with *that* name — which the next offer
        // then repeats, because the plan's diff made exactly it work again.
        let plan = inner.materialize_render_takes(6).expect("plannable");
        let missing = plan.takes[1].file.clone();
        assert!(!missing.is_empty());
        let mut t = Task::new_take(6, 1);
        t.state = TaskState::Running;
        t.assigned_to = Some("w1".into());
        inner.tasks.insert(t.id(), t);

        let msg = inner.complete(&completion(
            "w1",
            "render:6:1",
            true,
            "render ch6 (1 calls)",
        ));
        assert!(
            msg.contains("failed"),
            "an incomplete ok-report fails: {msg}"
        );
        assert!(
            msg.contains(&missing),
            "the missing take file is named: {msg}"
        );
        assert_eq!(inner.tasks["render:6:1"].state, TaskState::Pending);
        assert_eq!(inner.tasks["render:6:1"].attempts, 1);
    }

    #[test]
    fn a_failed_task_lands_in_the_event_stream_with_its_cause() {
        // The whole point of the event buffer: a digest that dies on a worker
        // must say *why* in the TUI, not just flip a row back to Pending.
        let (_d, mut inner) = fixture();
        let mut t = Task::new(4, Stage::Digest);
        t.state = TaskState::Running;
        t.assigned_to = Some("w1".into());
        inner.tasks.insert("digest:4".into(), t);

        inner.complete(&completion(
            "w1",
            "digest:4",
            false,
            "opencode exited 1: model 'claude' unavailable",
        ));

        let events = inner.recent_events(10);
        let last = events.last().expect("a failure must record an event");
        assert_eq!(
            last.level, "warn",
            "a first failure retries, so it is a warning"
        );
        assert!(
            last.text.contains("w1") && last.text.contains("digest:4"),
            "{}",
            last.text
        );
        assert!(
            last.text.contains("model 'claude' unavailable"),
            "the worker's reason must survive: {}",
            last.text
        );
        assert_eq!(
            inner.tasks["digest:4"].state,
            TaskState::Pending,
            "one strike, not shelved"
        );
    }

    #[test]
    fn the_fifteenth_digest_failure_escalates_to_error_and_names_the_retry_key() {
        // Digest shelves at 15, not 3: two LLM calls plus repairs against a
        // rate-limited tier fail in ways a stuck ffmpeg does not, and each
        // racing wave re-proves the prompt before the chapter is parked.
        let (_d, mut inner) = fixture();
        let mut t = Task::new(4, Stage::Digest);
        t.state = TaskState::Running;
        t.attempts = 14;
        t.assigned_to = Some("w1".into());
        inner.tasks.insert("digest:4".into(), t);

        inner.complete(&completion(
            "w1",
            "digest:4",
            false,
            "digest returned no segments",
        ));

        let last = inner.recent_events(1).into_iter().next().unwrap().clone();
        assert_eq!(
            last.level, "error",
            "fifteen strikes is an error, not a warning"
        );
        // Case-insensitive on purpose: the word is deliberately capitalised in
        // the event so it stands out from the "will retry" failures beside it,
        // and the test should pin the meaning rather than the letter case.
        assert!(
            last.text.to_lowercase().contains("shelved"),
            "{}",
            last.text
        );
        assert!(
            last.text.contains("no further retries"),
            "and says what shelving costs — nothing will retry it: {}",
            last.text
        );
        assert!(
            last.text.contains('u'),
            "the way out must be named: {}",
            last.text
        );
        assert_eq!(inner.tasks["digest:4"].state, TaskState::Shelved);
        assert!(
            inner.tasks["digest:4"].expiries == 0,
            "a reported failure is an answer: the silent-expiry streak is not this \
             assignment's story and is reset"
        );
    }

    #[test]
    fn shutdown_op_latches_the_flag_the_next_heartbeat_reads() {
        let (_d, mut inner) = fixture();
        assert!(!inner.shutdown_requested, "a fresh backend asks nothing");
        let msg = inner.op_shutdown_workers();
        assert!(inner.shutdown_requested);
        assert!(msg.contains("shutdown"), "{msg}");
        // In-memory only: the ledger save carries tasks/machines/workers,
        // so a reboot clears the latch instead of murdering new workers.
        std::fs::create_dir_all(inner.layout.bm_state()).unwrap();
        inner.save();
        let ledger = std::fs::read_to_string(inner.layout.bm_state().join("ledger.json")).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&ledger).unwrap();
        assert!(doc.get("shutdown_requested").is_none());
    }

    #[test]
    fn drain_latch_fires_only_on_an_empty_queue_and_only_once() {
        let (_d, mut inner) = fixture();
        // Armed with nothing unfinished: fires at once (an idle backend has
        // no completion left to trip it).
        assert_eq!(
            inner.op_shutdown_when_idle(),
            "queue already drained — workers exiting on next beat"
        );
        assert!(inner.shutdown_requested);
        assert!(
            !inner.shutdown_when_idle,
            "one-shot: the arm disarms as it fires"
        );

        // Rearm with work outstanding: a pending task blocks the fire.
        inner.shutdown_requested = false;
        let mut t = Task::new(4, Stage::Digest);
        inner.tasks.insert("digest:4".into(), t.clone());
        t.state = TaskState::Done;
        inner.op_shutdown_when_idle();
        assert!(!inner.shutdown_requested, "a pending task must block");
        assert!(inner.shutdown_when_idle, "the arm survives");

        // Shelved is parked, not work: it must not block.
        t.state = TaskState::Shelved;
        inner.tasks.insert("digest:4".into(), t);
        inner.maybe_auto_shutdown();
        assert!(inner.shutdown_requested);
        assert!(!inner.shutdown_when_idle);
    }

    #[test]
    fn the_last_completion_trips_the_armed_latch() {
        let (_d, mut inner) = fixture();
        // A merge is the last stage: completing it with its mp3 on disk
        // leaves nothing unfinished, so the armed latch must fire.
        std::fs::create_dir_all(inner.layout.bm_state()).unwrap();
        let mut t = Task::new(4, Stage::Merge);
        t.state = TaskState::Running;
        t.assigned_to = Some("w1".into());
        inner.tasks.insert("merge:4".into(), t);
        std::fs::write(inner.layout.final_mp3(4), b"fake-mp3").unwrap();
        inner.op_shutdown_when_idle();
        assert!(!inner.shutdown_requested, "a running task must block");
        inner.complete(&completion("w1", "merge:4", true, "merge ch4 -> out.mp3"));
        assert!(
            inner.shutdown_requested,
            "the draining completion must fire it"
        );
    }

    #[test]
    fn events_are_capped_and_ids_stay_in_order() {
        let (_d, mut inner) = fixture();
        for i in 0..(EVENT_CAP + 25) {
            inner.push_event("info", format!("event {i}"));
        }
        let all = inner.recent_events(EVENT_CAP * 2);
        assert_eq!(
            all.len(),
            EVENT_CAP,
            "the buffer must not grow without bound"
        );
        let ids: Vec<u64> = all.iter().map(|e| e.id).collect();
        assert!(
            ids.windows(2).all(|w| w[0] < w[1]),
            "ids must ascend: {ids:?}"
        );
        assert_eq!(*ids.last().unwrap(), (EVENT_CAP + 24) as u64);
        // `limit` is a tail window, not a reordering.
        let tail = inner.recent_events(3);
        assert_eq!(
            tail.iter().map(|e| e.text.clone()).collect::<Vec<_>>(),
            vec![
                format!("event {}", EVENT_CAP + 22),
                format!("event {}", EVENT_CAP + 23),
                format!("event {}", EVENT_CAP + 24),
            ]
        );
        assert!(inner.recent_events(0).is_empty());
    }

    #[test]
    fn reap_explains_a_requeue_instead_of_flipping_a_row_silently() {
        let (_d, mut inner) = fixture();
        let mut t = Task::new(5, Stage::Render);
        t.state = TaskState::Running;
        t.assigned_to = Some("w9".into());
        t.lease_until = Some(now_secs().saturating_sub(5));
        inner.tasks.insert("render:5".into(), t);

        let requeued = inner.reap();
        assert_eq!(requeued, vec!["render:5".to_string()]);
        assert_eq!(inner.tasks["render:5"].state, TaskState::Pending);
        assert_eq!(
            inner.tasks["render:5"].attempts, 0,
            "silence is not a strike"
        );

        let last = inner.recent_events(1).into_iter().next().unwrap().clone();
        assert_eq!(last.level, "warn");
        assert!(last.text.contains("lease expired"), "{}", last.text);
        assert!(last.text.contains("render:5"), "{}", last.text);

        // A quiet reap stays quiet: no event, nothing to re-announce every 10s.
        let before = inner.events.len();
        assert!(inner.reap().is_empty());
        assert_eq!(
            inner.events.len(),
            before,
            "nothing happened, nothing logged"
        );
    }

    #[test]
    fn retry_task_targets_one_chapter_and_force_clears_its_artifact() {
        let (_d, mut inner) = fixture();
        let script = inner.layout.script(3);
        std::fs::write(&script, r#"{"segments":[]}"#).unwrap();
        let mut t = Task::new(3, Stage::Digest);
        t.state = TaskState::Shelved;
        t.attempts = 3;
        t.assigned_to = Some("w1".into());
        t.lease_until = Some(now_secs() + 60);
        t.detail = "digest failed: no such model".into();
        inner.tasks.insert("digest:3".into(), t);
        let mut other = Task::new(4, Stage::Digest);
        other.state = TaskState::Shelved;
        inner.tasks.insert("digest:4".into(), other);

        // Without force the artifact is left alone: reconcile may still see it.
        let msg = inner.op_retry_task(Stage::Digest, 3, false);
        assert!(msg.contains("digest:3"), "{msg}");
        assert_eq!(inner.tasks["digest:3"].state, TaskState::Pending);
        assert_eq!(
            inner.tasks["digest:3"].attempts, 0,
            "a manual retry forgives strikes"
        );
        assert!(inner.tasks["digest:3"].assigned_to.is_none());
        assert!(inner.tasks["digest:3"].lease_until.is_none());
        assert!(inner.tasks["digest:3"].detail.contains("requeued"));
        assert!(script.exists(), "a plain retry keeps the artifact");
        assert_eq!(
            inner.tasks["digest:4"].state,
            TaskState::Shelved,
            "only the named chapter is touched"
        );

        // Forced: the stale script goes, so the stage really re-runs.
        let msg = inner.op_retry_task(Stage::Digest, 3, true);
        assert!(msg.contains("forced"), "{msg}");
        assert!(!script.exists(), "force must delete what made it look done");

        // And the operator action is in the stream, with the state it replaced.
        let last = inner.recent_events(1).into_iter().next().unwrap().clone();
        assert_eq!(last.level, "ok");
        assert!(last.text.contains("digest:3"), "{}", last.text);
        assert!(last.text.contains("was pending"), "{}", last.text);

        assert!(inner
            .op_retry_task(Stage::Merge, 99, false)
            .contains("not found"));
    }

    #[test]
    fn forcing_render_requeues_the_merge_for_that_chapter() {
        let (_d, mut inner) = fixture();
        let mut merge = Task::new(24, Stage::Merge);
        merge.state = TaskState::Done;
        inner.tasks.insert(merge.id(), merge);
        for pos in 0..2 {
            let mut render = Task::new_take(24, pos);
            render.state = TaskState::Done;
            inner.tasks.insert(render.id(), render);
        }
        std::fs::write(inner.layout.final_mp3(24), vec![0u8; 2000]).unwrap();

        let msg = inner.op_retry_task(Stage::Render, 24, true);
        assert!(msg.contains("forced"), "{msg}");
        assert_eq!(inner.tasks["merge:24"].state, TaskState::Pending);
        assert!(!inner.layout.final_mp3(24).exists());
        assert!(inner
            .tasks
            .values()
            .filter(|t| t.stage == Stage::Render && t.chapter == 24)
            .all(|t| t.state == TaskState::Pending));
    }

    #[test]
    fn forcing_digest_removes_that_chapters_render_and_merge_rows() {
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::write(layout.script(3), r#"{"segments":[]}"#).unwrap();
        std::fs::write(layout.final_mp3(3), vec![0u8; 2000]).unwrap();
        let mut digest = Task::new(3, Stage::Digest);
        digest.state = TaskState::Done;
        inner.tasks.insert(digest.id(), digest);
        for pos in 0..2 {
            let render = Task::new_take(3, pos);
            inner.tasks.insert(render.id(), render);
        }
        let merge = Task::new(3, Stage::Merge);
        inner.tasks.insert(merge.id(), merge);

        inner.op_retry_task(Stage::Digest, 3, true);

        assert_eq!(inner.tasks["digest:3"].state, TaskState::Pending);
        assert!(!inner
            .tasks
            .values()
            .any(|t| t.stage == Stage::Render && t.chapter == 3));
        assert!(!inner
            .tasks
            .values()
            .any(|t| t.stage == Stage::Merge && t.chapter == 3));
        assert!(!layout.script(3).exists());
        assert!(!layout.final_mp3(3).exists());
    }

    #[test]
    fn a_forced_merge_retry_forces_the_render_that_feeds_it() {
        // The failure this exists for: `merge:24 FAILED (shelved — press u to
        // retry): 42 segments missing in .../segments-vieneu-24: run the render
        // stage first`. Nothing in the merge stage produces segments, so
        // re-offering the merge fails again on the same box — and the operator
        // had to work out that the fix was `F` on a *different* row. Force now
        // means force the producer.
        let (_d, mut inner) = fixture();
        let engine = inner.settings.engine.clone();
        let seg = inner.layout.seg_dir(&engine, 24);
        std::fs::create_dir_all(&seg).unwrap();
        std::fs::write(seg.join("0000_Đức Trí.wav"), vec![0u8; 2000]).unwrap();
        for (stage, state) in [
            (Stage::Render, TaskState::Done),
            (Stage::Merge, TaskState::Shelved),
        ] {
            let mut t = Task::new(24, stage);
            t.state = state;
            t.attempts = 3;
            t.assigned_to = Some("marmot".into());
            inner.tasks.insert(t.id(), t);
        }
        assert!(
            !inner.layout.final_mp3(24).exists(),
            "the merge failed, so nothing was published"
        );

        let msg = inner.op_retry_task(Stage::Merge, 24, true);
        assert!(msg.contains("merge:24"), "{msg}");
        assert!(
            msg.contains("+ render"),
            "the cascade is operator-facing: {msg}"
        );
        assert_eq!(inner.tasks["merge:24"].state, TaskState::Pending);
        assert_eq!(inner.tasks["render:24"].state, TaskState::Pending);
        assert_eq!(inner.tasks["render:24"].attempts, 0);
        assert!(
            !seg.exists(),
            "the render cache goes, or the re-offer stays the no-op this fixes"
        );
        // Both stages reach the stream, so the log explains the state change
        // rather than leaving a render row that moved on its own.
        let evs: Vec<String> = inner
            .recent_events(2)
            .into_iter()
            .map(|e| e.text.clone())
            .collect();
        assert!(evs.iter().any(|e| e.contains("render:24")), "{evs:?}");
        assert!(evs.iter().any(|e| e.contains("merge:24")), "{evs:?}");
    }

    #[test]
    fn a_merge_retry_cascades_only_when_forced_and_nothing_was_published() {
        // Two guards, both about not deleting more than was asked for: an
        // unforced retry deletes nothing at all (that is the whole difference
        // from `F`), and a chapter that already has an mp3 keeps its segments,
        // because those are that file's provenance.
        let (_d, mut inner) = fixture();
        let engine = inner.settings.engine.clone();
        let seg = inner.layout.seg_dir(&engine, 24);
        std::fs::create_dir_all(&seg).unwrap();
        let keep = seg.join("0000_Đức Trí.wav");
        std::fs::write(&keep, vec![0u8; 2000]).unwrap();
        let mut r = Task::new(24, Stage::Render);
        r.state = TaskState::Done;
        inner.tasks.insert(r.id(), r);
        let mut m = Task::new(24, Stage::Merge);
        m.state = TaskState::Shelved;
        inner.tasks.insert(m.id(), m);

        // Unforced: the merge is requeued and nothing else is touched.
        let msg = inner.op_retry_task(Stage::Merge, 24, false);
        assert!(!msg.contains("render"), "{msg}");
        assert_eq!(inner.tasks["render:24"].state, TaskState::Done);
        assert!(keep.exists());

        // Forced, but this chapter is already published: the stale mp3 goes
        // (it is about to be rebuilt) and the segments stay.
        std::fs::write(inner.layout.final_mp3(24), vec![0u8; 2000]).unwrap();
        let msg = inner.op_retry_task(Stage::Merge, 24, true);
        assert!(!msg.contains("render"), "no cascade to announce: {msg}");
        assert_eq!(
            inner.tasks["render:24"].state,
            TaskState::Done,
            "left alone"
        );
        assert!(keep.exists(), "provenance survives");
        assert!(
            !inner.layout.final_mp3(24).exists(),
            "the stale product goes"
        );
    }

    #[test]
    fn offer_skips_a_merge_whose_segments_are_missing_and_heals_its_render() {
        // The failure this exists for: `merge:24 FAILED (will retry): 42
        // segments missing in .../segments-vieneu-24: run the render stage
        // first`. The ledger said `render:Done` but the files were not on
        // the disk the merge would run against — so the offer, not the
        // third strike, is where it stops.
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        let engine = inner.settings.engine.clone();
        std::fs::write(
            layout.script(9),
            r#"{"segments":[{"speaker":"A","text":"x"},{"speaker":"A","text":"y"},{"speaker":"B","text":"z"}]}"#,
        )
        .unwrap();
        std::fs::write(layout.cast(&engine), r#"{"A":"Đức Trí","B":"Adam"}"#).unwrap();
        let seg = layout.seg_dir(&engine, 9);
        std::fs::create_dir_all(&seg).unwrap();
        std::fs::write(seg.join("0000-0001_Đức Trí.wav"), vec![0u8; 2000]).unwrap();
        for (stage, state) in [
            (Stage::Crawl, TaskState::Done),
            (Stage::Digest, TaskState::Done),
            (Stage::Render, TaskState::Done),
        ] {
            let mut t = Task::new(9, stage);
            t.state = state;
            inner.tasks.insert(t.id(), t);
        }
        inner
            .tasks
            .insert(Task::new(9, Stage::Merge).id(), Task::new(9, Stage::Merge));

        // One run wav short of the set: the merge is not offered, its render
        // is requeued, and the worker leaves with the healing render instead
        // of idling behind a chapter it could not have merged.
        let offer = inner.offer("w1").expect("the render is offerable");
        assert_eq!(offer.task_id, "render:9:1", "not the starved merge");
        assert_eq!(inner.tasks["merge:9"].state, TaskState::Pending);
        assert_eq!(inner.tasks["render:9:1"].state, TaskState::Assigned);
        assert_eq!(
            inner.tasks["render:9:0"].state,
            TaskState::Done,
            "the take already on disk is kept — only the gap re-speaks"
        );
        assert!(
            inner.tasks["render:9:1"]
                .detail
                .contains("segments missing"),
            "the requeue names its cause: {}",
            inner.tasks["render:9:1"].detail
        );
    }

    #[test]
    fn offer_hands_over_a_merge_whose_segments_are_home() {
        // Same chapter complete: the guard is not a veto on merges, only on
        // merges the box would fail.
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        let engine = inner.settings.engine.clone();
        std::fs::write(
            layout.script(9),
            r#"{"segments":[{"speaker":"A","text":"x"},{"speaker":"A","text":"y"},{"speaker":"B","text":"z"}]}"#,
        )
        .unwrap();
        std::fs::write(layout.cast(&engine), r#"{"A":"Đức Trí","B":"Adam"}"#).unwrap();
        let seg = layout.seg_dir(&engine, 9);
        std::fs::create_dir_all(&seg).unwrap();
        std::fs::write(seg.join("0000-0001_Đức Trí.wav"), vec![0u8; 2000]).unwrap();
        std::fs::write(seg.join("0002_Adam.wav"), vec![0u8; 2000]).unwrap();
        for (stage, state) in [
            (Stage::Crawl, TaskState::Done),
            (Stage::Digest, TaskState::Done),
            (Stage::Render, TaskState::Done),
        ] {
            let mut t = Task::new(9, stage);
            t.state = state;
            inner.tasks.insert(t.id(), t);
        }
        inner
            .tasks
            .insert(Task::new(9, Stage::Merge).id(), Task::new(9, Stage::Merge));

        let offer = inner.offer("w1").expect("a ready merge is offerable");
        assert_eq!(offer.task_id, "merge:9");
        assert_eq!(inner.tasks["render:9"].state, TaskState::Done);
    }

    #[test]
    fn a_merge_offer_carries_the_plans_takes_in_mix_order() {
        // The mixer cannot re-derive a content-addressed take name from the
        // script and the cast — the name is a hash of the inputs, not a
        // function of them — so the offer has to carry the plan's file list:
        // the same list the renderer wrote and the completion gate proved.
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::write(
            layout.script(9),
            r#"{"segments":[{"speaker":"A","text":"x"},{"speaker":"B","text":"z"}]}"#,
        )
        .unwrap();
        std::fs::write(layout.cast("vieneu"), r#"{"A":"Đức Trí","B":"Adam"}"#).unwrap();
        let seg = layout.seg_dir("vieneu", 9);
        std::fs::create_dir_all(&seg).unwrap();
        inner.materialize_render_takes(9);
        let plan = bm_core::assemble::RenderPlan::load(&layout.plan(9)).unwrap();
        for t in &plan.takes {
            std::fs::write(seg.join(&t.file), vec![0u8; 2000]).unwrap();
        }
        inner.materialize_render_takes(9);
        for stage in [Stage::Crawl, Stage::Digest] {
            let mut t = Task::new(9, stage);
            t.state = TaskState::Done;
            inner.tasks.insert(t.id(), t);
        }
        inner.ensure_task(9, Stage::Merge);
        inner.workers.insert("w1".into(), "127.0.0.1".into());

        let offer = inner.offer("w1").expect("the merge is offerable");
        assert_eq!(offer.task_id, "merge:9");
        assert_eq!(
            offer.merge_takes,
            plan.files(),
            "the plan's names, in mix order"
        );
        assert!(
            offer.merge_takes.iter().all(|f| f.starts_with("t-")),
            "content-addressed, so nothing else could have named them: {:?}",
            offer.merge_takes
        );
        assert!(offer.script.is_some(), "the turns still need the script");
        assert!(
            offer.render_units.is_none(),
            "and no render payload rides along"
        );
    }

    #[test]
    fn a_merge_failing_on_missing_segments_requeues_its_render() {
        // The offer guard only sees this disk. A remote that was wiped (or
        // never rendered the chapter) fails the same way with a healthy disk
        // here — so the failure heals the render instead of burning three
        // strikes into shelved and waiting for a manual force.
        let (_d, mut inner) = fixture();
        let mut r = Task::new(9, Stage::Render);
        r.state = TaskState::Done;
        inner.tasks.insert(r.id(), r);
        let mut m = Task::new(9, Stage::Merge);
        m.state = TaskState::Running;
        m.assigned_to = Some("marmot".into());
        inner.tasks.insert(m.id(), m);

        let msg = inner.complete(&completion(
            "marmot",
            "merge:9",
            false,
            "merge ch9 failed: 42 segments missing in /home/thang/bm-worker/data/audio/segments-vieneu-09 (e.g. title_Đức Trí.wav): run the render stage first",
        ));
        assert!(msg.contains("failed"), "{msg}");
        assert_eq!(inner.tasks["merge:9"].state, TaskState::Pending);
        assert_eq!(
            inner.tasks["merge:9"].attempts, 1,
            "the strike still counts"
        );
        assert_eq!(
            inner.tasks["render:9"].state,
            TaskState::Pending,
            "the render is requeued beside the retry"
        );

        // Published is the veto: then the segments are that mp3's
        // provenance, not cache, and nothing re-renders on a failure's word.
        std::fs::create_dir_all(inner.layout.final_mp3(9).parent().unwrap()).unwrap();
        std::fs::write(inner.layout.final_mp3(9), vec![0u8; 2000]).unwrap();
        let mut r = Task::new(9, Stage::Render);
        r.state = TaskState::Done;
        inner.tasks.insert(r.id(), r);
        let m = inner.tasks.get_mut("merge:9").unwrap();
        m.state = TaskState::Running;
        m.assigned_to = Some("marmot".into());
        let msg = inner.complete(&completion(
            "marmot",
            "merge:9",
            false,
            "merge ch9 failed: 1 segments missing in /home/thang/bm-worker/data/audio/segments-vieneu-09 (e.g. 0000_Adam.wav): run the render stage first",
        ));
        assert!(msg.contains("failed"), "{msg}");
        assert_eq!(
            inner.tasks["render:9"].state,
            TaskState::Done,
            "a published chapter keeps its segments"
        );
    }

    #[test]
    fn a_merge_that_fails_missing_segments_keeps_its_strike() {
        // A merge pulls the pieces it lacks from the inductor, so `segments
        // missing` means the render never produced the audio or the inductor
        // lost it — the chapter's own failure, and the strike stands. The
        // heal refills local gaps; nothing is re-homed, because nothing is
        // pinned.
        let (_d, mut inner) = fixture();
        let files = render_chapter(&mut inner, 9, 4);
        for f in &files {
            land(&inner, 9, f);
        }
        inner.materialize_render_takes(9);
        assert!(inner.render_takes_done(9), "every take is home");
        inner.ensure_task(9, Stage::Merge);
        {
            let m = inner.tasks.get_mut("merge:9").unwrap();
            m.state = TaskState::Running;
            m.assigned_to = Some("w1".into());
            m.affinity = Some("192.168.2.2".into());
        }

        let msg = inner.complete(&completion(
            "w1",
            "merge:9",
            false,
            "merge ch9 failed: 2 segments missing in /home/thang/bm-worker/data/audio/segments-vieneu-9 (e.g. t-ade693a0a8a3a449.wav): run the render stage first",
        ));
        let m = &inner.tasks["merge:9"];
        assert_eq!(m.state, TaskState::Pending, "it retries: {msg}");
        assert_eq!(m.attempts, 1, "and the strike stands");
        assert_eq!(
            m.affinity.as_deref(),
            Some("192.168.2.2"),
            "the failure rewrites no pin — there is nothing to re-home to"
        );
        assert!(
            inner.render_takes_done(9),
            "nothing re-renders — the audio was never the problem"
        );

        // A second identical failure strikes again — no re-home, no excuse.
        let m = inner.tasks.get_mut("merge:9").unwrap();
        m.state = TaskState::Running;
        m.assigned_to = Some("w1".into());
        inner.complete(&completion(
            "w1",
            "merge:9",
            false,
            "merge ch9 failed: 2 segments missing in /home/thang/bm-worker/data/audio/segments-vieneu-9: run the render stage first",
        ));
        assert_eq!(
            inner.tasks["merge:9"].attempts, 2,
            "repeated missing pieces keep striking"
        );
    }

    #[test]
    fn stale_pins_gate_nothing() {
        // Rows may still carry affinity from the pinning era (the load
        // migration releases them, but a test sets them by hand). The offer
        // ignores pins on every stage, and taking work writes none.
        let (_d, mut inner) = fixture();
        render_chapter(&mut inner, 9, 4);
        for t in inner.tasks.values_mut() {
            if t.stage == Stage::Render && t.chapter == 9 {
                t.affinity = Some("192.168.2.2".into());
            }
        }
        inner.ensure_task(9, Stage::Merge);
        inner.tasks.get_mut("merge:9").unwrap().affinity = Some("192.168.2.2".into());
        inner.workers.insert("local".into(), "127.0.0.1".into());

        let offer = inner
            .offer("local")
            .expect("any box takes any take, pins or not");
        assert!(
            offer.task_id.starts_with("render:9:"),
            "it takes a take of the chapter: {}",
            offer.task_id
        );
        assert_eq!(
            inner.tasks[&offer.task_id].assigned_to.as_deref(),
            Some("local"),
            "stale pins gate nothing — the take is assigned"
        );
        assert_eq!(inner.tasks["merge:9"].state, TaskState::Pending);
    }

    #[test]
    fn completions_write_no_pins() {
        // Completions used to point the merge at whoever rendered. Now they
        // record nothing: the merge pulls its pieces, so who spoke what is
        // scheduling trivia.
        let (_d, mut inner) = fixture();
        let files = render_chapter(&mut inner, 9, 3);
        inner.ensure_task(9, Stage::Merge);
        {
            let t = inner.tasks.get_mut("render:9:1").unwrap();
            t.state = TaskState::Running;
            t.assigned_to = Some("w1".into());
        }
        land(&inner, 9, &files[1]);
        inner.complete(&completion(
            "w1",
            "render:9:1",
            true,
            "render ch9 (1 calls)",
        ));

        assert_eq!(
            inner.tasks["merge:9"].affinity, None,
            "a completion writes no pin"
        );

        // And the ordinary case: a remote completion writes no pin either.
        let files = render_chapter(&mut inner, 11, 2);
        inner.ensure_task(11, Stage::Merge);
        {
            let t = inner.tasks.get_mut("render:11:0").unwrap();
            t.state = TaskState::Running;
            t.assigned_to = Some("w1".into());
        }
        land(&inner, 11, &files[0]);
        inner.complete(&completion(
            "w1",
            "render:11:0",
            true,
            "render ch11 (1 calls)",
        ));
        assert_eq!(
            inner.tasks["merge:11"].affinity, None,
            "still no pin — merges run anywhere"
        );
    }

    #[test]
    fn a_voice_swap_on_a_split_chapter_leaves_the_rerender_unpinned() {
        // A chapter spoken on two boxes, then a voice swap re-speaks one
        // take: the re-render is offerable to any box and the merge stays
        // exactly as it was — scheduling writes no pins, so a split chapter
        // can never strand work behind a stale one.
        let (_d, mut inner) = fixture();
        let files = render_chapter(&mut inner, 9, 4);
        for t in inner.tasks.values_mut() {
            if t.stage == Stage::Render && t.chapter == 9 {
                t.affinity = Some("192.168.2.2".into());
            }
        }
        inner.ensure_task(9, Stage::Merge);
        inner.tasks.get_mut("merge:9").unwrap().affinity = Some("192.168.2.2".into());
        inner.workers.insert("local".into(), "127.0.0.1".into());
        inner.caps.insert(
            "local".into(),
            vec!["render".into(), "render-segments".into(), "merge".into()],
        );

        // 1. The split: the local node takes a take of the chapter, stale
        //    pins or not, and nothing is rewritten.
        let offer = inner.offer("local").expect("any box takes any take");
        assert!(offer.task_id.starts_with("render:9:"), "{}", offer.task_id);
        let local_take = offer.task_id.clone();
        assert_eq!(
            inner.tasks["merge:9"].affinity.as_deref(),
            Some("192.168.2.2"),
            "the offer rewrites no pin — the stale one is the migration's job"
        );

        // 2. Both stores fill: the local take lands here, the rest remotely.
        //    Every file lands before any report, because the completion gate
        //    reads the whole chapter — `collect_units` pulls each unit home
        //    before the completion is applied, and this is that invariant.
        //    This is the state ch148 was actually in.
        for f in &files {
            land(&inner, 9, f);
        }
        {
            let t = inner.tasks.get_mut(&local_take).unwrap();
            t.state = TaskState::Running;
            t.assigned_to = Some("local".into());
        }
        inner.complete(&completion(
            "local",
            &local_take,
            true,
            "render ch9 (1 calls)",
        ));
        for i in 0..files.len() {
            let id = format!("render:9:{i}");
            if id == local_take {
                continue;
            }
            let t = inner.tasks.get_mut(&id).unwrap();
            t.state = TaskState::Running;
            t.assigned_to = Some("w1".into());
            inner.complete(&completion("w1", &id, true, "render ch9 (1 calls)"));
        }
        assert!(inner.render_takes_done(9), "the chapter is complete here");
        assert_eq!(
            inner.tasks["merge:9"].affinity.as_deref(),
            Some("192.168.2.2"),
            "and no completion rewrote the stale pin either way"
        );

        // 3. The voice swap. The cast is rewritten first — that is what a swap
        //    is — and the invalidation then renames every take the speaker
        //    produced. Those re-renders stay unpinned: any box speaks them.
        std::fs::write(inner.layout.cast("vieneu"), r#"{"A":"Adam","B":"Adam"}"#).unwrap();
        inner.invalidate_character("vieneu", "A", "Đức Trí");
        let requeued: Vec<String> = inner
            .tasks
            .iter()
            .filter(|(_, t)| {
                t.stage == Stage::Render && t.chapter == 9 && t.state == TaskState::Pending
            })
            .map(|(id, _)| id.clone())
            .collect();
        assert!(!requeued.is_empty(), "the swap requeued something");
        for id in &requeued {
            assert_eq!(
                inner.tasks[id].affinity, None,
                "a re-render after a swap is offerable to any box: {id}"
            );
        }
        assert_eq!(
            inner.tasks["merge:9"].affinity, None,
            "and the merge stays unpinned"
        );
    }

    #[test]
    fn a_swap_on_a_chapter_the_remote_holds_leaves_the_rerender_unpinned() {
        // Takes are independent: a re-render after a swap is offerable to any
        // box, wherever the chapter was spoken. The merge is unpinned too —
        // it pulls its pieces, wherever it runs.
        let (_d, mut inner) = fixture();
        let files = render_chapter(&mut inner, 9, 4);
        for t in inner.tasks.values_mut() {
            if t.stage == Stage::Render && t.chapter == 9 {
                t.affinity = Some("192.168.2.2".into());
            }
        }
        inner.ensure_task(9, Stage::Merge);
        inner.tasks.get_mut("merge:9").unwrap().affinity = Some("192.168.2.2".into());

        // The remote rendered the chapter in full and every unit came home —
        // so this disk is complete, and the remote is still the right box.
        for f in &files {
            land(&inner, 9, f);
        }
        for i in 0..files.len() {
            let id = format!("render:9:{i}");
            let t = inner.tasks.get_mut(&id).unwrap();
            t.state = TaskState::Running;
            t.assigned_to = Some("w1".into());
            inner.complete(&completion("w1", &id, true, "render ch9 (1 calls)"));
        }
        assert!(inner.render_takes_done(9), "complete on this disk");
        assert_eq!(
            inner.tasks["merge:9"].affinity.as_deref(),
            Some("192.168.2.2"),
            "and still the remote's to merge"
        );

        // The swap. A chapter the remote holds whole is still re-spoken by
        // whoever asks first — no warm-box pin.
        std::fs::write(inner.layout.cast("vieneu"), r#"{"A":"Adam","B":"Adam"}"#).unwrap();
        inner.invalidate_character("vieneu", "A", "Đức Trí");
        let requeued: Vec<String> = inner
            .tasks
            .iter()
            .filter(|(_, t)| {
                t.stage == Stage::Render && t.chapter == 9 && t.state == TaskState::Pending
            })
            .map(|(id, _)| id.clone())
            .collect();
        assert!(!requeued.is_empty(), "the swap requeued something");
        for id in &requeued {
            assert_eq!(
                inner.tasks[id].affinity, None,
                "re-renders stay unpinned: {id}"
            );
        }
    }

    #[test]
    fn load_releases_every_stale_pin() {
        // The pinning era wrote affinity on render and merge rows; the new
        // scheduler pins nothing, and loading releases it all so idle boxes
        // see the work.
        let (_d, mut inner) = fixture();
        for (id, stage) in [
            ("render:1:0", Stage::Render),
            ("render:1:1", Stage::Render),
            ("merge:1", Stage::Merge),
        ] {
            let mut t = Task::new(1, stage);
            t.affinity = Some("127.0.0.1".into());
            inner.tasks.insert(id.into(), t);
        }
        assert_eq!(inner.release_pins(), 3);
        assert_eq!(inner.tasks["render:1:0"].affinity, None);
        assert_eq!(inner.tasks["render:1:1"].affinity, None);
        assert_eq!(inner.tasks["merge:1"].affinity, None);
        assert_eq!(inner.release_pins(), 0, "idempotent");
    }

    #[test]
    fn a_render_offer_freezes_the_voices_it_hands_out() {
        // The filenames a worker writes embed the voice, so a plan that is not
        // persisted is re-derived later — by the completion gate and by the
        // merger — from whatever the cast file says then. Assignment is
        // least-used over the whole file, so any other chapter's write moves
        // it. That is the whole of "render done, then 20 segments missing":
        // the audio was on disk under names nothing would look up again.
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        let engine = inner.settings.engine.clone();
        std::fs::write(
            layout.script(187),
            r#"{"roster":["Narrator","Hám Thiên Khuyết"],
                "segments":[{"speaker":"Hám Thiên Khuyết","text":"Chết đi!"}]}"#,
        )
        .unwrap();
        // A speaker with no entry: exactly the state the offer path used to
        // leave behind.
        std::fs::write(layout.cast(&engine), r#"{"Narrator":"Đức Trí"}"#).unwrap();

        let plan = inner
            .materialize_render_takes(187)
            .expect("planning must succeed");
        assert_eq!(plan.takes.len(), 1);
        let offered = plan.takes[0].file.clone();
        assert!(offered.ends_with(".wav"), "{offered}");

        // 1. The decision is on disk, so every later reader sees it. The file
        // name is content-addressed and so says nothing about the voice — the
        // take's recorded voice is the claim, and the persist is what makes it
        // readable by the completion gate later.
        let cast = bm_core::cast::read_cast(&engine, &layout.cast(&engine));
        let voice = cast
            .get("Hám Thiên Khuyết")
            .expect("the offer must persist the voice it handed out");
        assert_eq!(plan.takes[0].voice, *voice);

        // 2. The prover the completion gate uses agrees with the offer.
        let expected = crate::segments::expected_names(&layout, &engine, 187)
            .expect("the chapter is plannable");
        assert!(
            expected.contains(&offered),
            "the gate's set must contain what was offered: {expected:?}"
        );

        // 3. And it stays put when another chapter renders and saves.
        std::fs::write(
            layout.script(188),
            r#"{"roster":["Narrator","Người Khác"],
                "segments":[{"speaker":"Người Khác","text":"x"}]}"#,
        )
        .unwrap();
        let _ = inner
            .materialize_render_takes(188)
            .expect("planning must succeed");
        let after = crate::segments::expected_names(&layout, &engine, 187).unwrap();
        assert_eq!(
            expected, after,
            "another chapter's render must not move this chapter's expected set"
        );
    }

    #[test]
    fn a_variant_speaker_name_still_plans() {
        // The bible says `Vân bá` is a character *and* an alias of `Lão giả`,
        // which sits earlier in the file; and `Nam tử bị thương` is a
        // case-variant alias of `Quản Vân Bằng`. Both real, both shelved a
        // chapter with `cast has no voice for ...` while the cast held the
        // voice under the canonical name.
        let (_d, inner) = fixture();
        let layout = inner.layout.clone();
        let engine = inner.settings.engine.clone();
        std::fs::write(
            layout.bible(),
            r#"{"characters":[
                {"name":"Lão giả","voice_hint":"elderly male",
                 "proper_aliases":["Lão giả","Kim lão","Vân bá"]},
                {"name":"Vân bá","voice_hint":"adult male","proper_aliases":["Ngao Vân"]},
                {"name":"Quản Vân Bằng","voice_hint":"old male",
                 "proper_aliases":["Quản Vân Bằng","nam tử bị thương"]}]}"#,
        )
        .unwrap();
        std::fs::write(
            layout.script(180),
            r#"{"roster":["Narrator","Vân bá"],
                "segments":[{"speaker":"Vân bá","text":"Đi thôi."}]}"#,
        )
        .unwrap();
        std::fs::write(
            layout.script(182),
            r#"{"roster":["Narrator","Nam tử bị thương"],
                "segments":[{"speaker":"Nam tử bị thương","text":"Cứu ta."}]}"#,
        )
        .unwrap();

        let a = inner.plan_units(180).expect("Vân bá must plan");
        assert_eq!(a.len(), 1);
        let b = inner.plan_units(182).expect("Nam tử bị thương must plan");
        assert_eq!(b.len(), 1);
        assert!(
            crate::segments::expected_names(&layout, &engine, 180).is_some(),
            "the gate must be able to prove ch180 too"
        );
    }

    /// A local worker with every capability, so the offer tests exercise the
    /// policy alone and not a capability gate.
    fn offer_fixture(inner: &mut Inner) {
        inner.machines.insert(
            "127.0.0.1".into(),
            Machine::new("127.0.0.1", "local", 22, None, "both"),
        );
        inner.workers.insert("w1".into(), "127.0.0.1".into());
        inner.caps.insert(
            "w1".into(),
            vec![
                "crawl".into(),
                "digest".into(),
                "render".into(),
                "merge".into(),
                "render-segments".into(),
            ],
        );
    }

    /// ch1: render ready. ch2: merge ready. Both would be assignable at once.
    fn two_ready_stages(inner: &mut Inner) {
        for (stage, state) in [
            (Stage::Crawl, TaskState::Done),
            (Stage::Digest, TaskState::Done),
            (Stage::Render, TaskState::Pending),
        ] {
            let mut t = Task::new(1, stage);
            t.state = state;
            inner.tasks.insert(format!("{stage}:1"), t);
        }
        for stage in Stage::ALL {
            let mut t = Task::new(2, stage);
            t.state = if stage == Stage::Merge {
                TaskState::Pending
            } else {
                TaskState::Done
            };
            inner.tasks.insert(format!("{stage}:2"), t);
        }
    }

    #[test]
    fn offer_prefers_the_stage_the_policy_lists_first() {
        let (_d, mut inner) = fixture();
        offer_fixture(&mut inner);
        two_ready_stages(&mut inner);
        // The default policy leads with merge, so the ready merge beats the
        // equally-ready render — the "finish chapters first" rule.
        let offer = inner.offer("w1").expect("a ready task is offerable");
        assert_eq!(offer.task_id, "merge:2");
    }

    #[test]
    fn offer_refuses_a_box_the_inductor_knows_is_not_ready() {
        // The readiness gate. In the inverted protocol `offer` is only reached
        // after a heartbeat, so this is the invariant *stated* rather than a
        // live path — and it is worth stating: a task handed to a box that is
        // booting, being pushed to, or known-broken fails slowly and strikes
        // the chapter for the inductor's mistake.
        let (_d, mut inner) = fixture();
        offer_fixture(&mut inner);
        two_ready_stages(&mut inner);
        for state in [
            MachineState::Initializing,
            MachineState::Probing,
            MachineState::Provisioning,
            MachineState::Configured,
            MachineState::Offline,
            MachineState::Error,
        ] {
            inner
                .machines
                .get_mut("127.0.0.1")
                .unwrap()
                .set_state(state);
            assert!(
                inner.offer("w1").is_none(),
                "{state:?} must not be offered work"
            );
        }
        // Nothing was consumed while the gate held, so the queue is intact and
        // the one state that works gets it.
        inner
            .machines
            .get_mut("127.0.0.1")
            .unwrap()
            .set_state(MachineState::Online);
        let offer = inner.offer("w1").expect("online is the state that works");
        assert_eq!(offer.task_id, "merge:2");
    }

    #[test]
    fn offer_still_serves_a_machine_with_no_opinion_formed() {
        // `Unknown` is not "not ready" — it is "never contacted": a hand-written
        // ledger, or the legacy pull worker asking before its first beat. Both
        // were offered work before the gate existed, and a live worker asking
        // for work is its own evidence the box is up.
        let (_d, mut inner) = fixture();
        offer_fixture(&mut inner);
        two_ready_stages(&mut inner);
        assert_eq!(inner.machines["127.0.0.1"].state, MachineState::Unknown);
        assert!(inner.offer("w1").is_some(), "no opinion is not a refusal");
    }

    #[test]
    fn silence_does_not_refute_a_box_that_is_coming_up() {
        // What the dispatcher does with a box that fails to answer `/status`.
        // A booting box cannot answer, and stamping Offline on it is the
        // fresh-pool-looks-broken bug: the pane would call a box that is
        // twenty seconds into its first boot "gone".
        let (_d, mut inner) = fixture();
        for state in [
            MachineState::Initializing,
            MachineState::Probing,
            MachineState::Provisioning,
            MachineState::Configured,
        ] {
            let mut m = Machine::new("3.121.112.113", "ubuntu", 22, None, "worker");
            m.set_state(state);
            inner.machines.insert(m.addr.clone(), m);
            inner.note_silence("3.121.112.113");
            assert_eq!(
                inner.machines["3.121.112.113"].state, state,
                "{state:?} is on its way up — silence is not news"
            );
        }
        // A box that *was* claiming to be online is refuted by silence.
        inner
            .machines
            .get_mut("3.121.112.113")
            .unwrap()
            .set_state(MachineState::Online);
        inner.note_silence("3.121.112.113");
        assert_eq!(inner.machines["3.121.112.113"].state, MachineState::Offline);
        // And an address we have never heard of is not created by the report.
        inner.note_silence("10.9.9.9");
        assert!(!inner.machines.contains_key("10.9.9.9"));
    }

    #[test]
    fn a_box_that_never_came_up_is_retired_instead_of_left_booting() {
        // `Initializing` is the one state with a deadline. A state with no exit
        // condition is a lie: a box terminated before it booted, or launched
        // into a subnet this machine cannot dial, would sit in "initializing"
        // for ever with the pane implying it is about to work.
        let (_d, mut inner) = fixture();
        let mut m = Machine::new("3.121.112.113", "ubuntu", 22, None, "worker");
        m.set_state(MachineState::Initializing);
        m.note = "EC2 i-09def58f197d3092c (pending)".into();
        inner.machines.insert(m.addr.clone(), m);

        // Twenty seconds in is still a boot, and the deadline must not fire.
        assert!(
            inner.expire_initializing().is_empty(),
            "a fresh launch is not a failure"
        );
        assert_eq!(
            inner.machines["3.121.112.113"].state,
            MachineState::Initializing
        );

        // Past the deadline it becomes a verdict, naming the address.
        inner.machines.get_mut("3.121.112.113").unwrap().state_since =
            now_secs() - (BOOT_DEADLINE_SECS + 1);
        let retired = inner.expire_initializing();
        assert_eq!(retired.len(), 1, "one line per box retired: {retired:?}");
        assert!(retired[0].contains("3.121.112.113"), "{}", retired[0]);
        assert!(retired[0].contains("never answered ssh"), "{}", retired[0]);

        let m = &inner.machines["3.121.112.113"];
        assert_eq!(m.state, MachineState::Error);
        // The EC2 id is the box's one stable identity — relink matches by it,
        // so a verdict may never erase it.
        assert!(m.note.contains("i-09def58f197d3092c"), "{}", m.note);
        // One-shot: the next pass has nothing left to retire.
        assert!(inner.expire_initializing().is_empty());
    }

    #[test]
    fn a_record_from_before_the_stamp_is_adopted_not_expired() {
        // `state_since == 0` means the record predates the field. Reading a
        // missing timestamp as "infinitely old" would retire a box on the
        // strength of a gap in the ledger.
        let (_d, mut inner) = fixture();
        let mut m = Machine::new("10.0.0.9", "ubuntu", 22, None, "worker");
        m.state = MachineState::Initializing;
        m.state_since = 0;
        inner.machines.insert(m.addr.clone(), m);

        assert!(
            inner.expire_initializing().is_empty(),
            "adopt, do not expire"
        );
        let m = &inner.machines["10.0.0.9"];
        assert!(m.state_since > 0, "the clock was adopted");
        assert_eq!(m.state, MachineState::Initializing, "and it keeps booting");
    }

    #[test]
    fn offer_skips_a_stage_the_policy_disabled() {
        let (_d, mut inner) = fixture();
        offer_fixture(&mut inner);
        two_ready_stages(&mut inner);
        // Merge off: the same pending work must route to render instead,
        // never sit unassigned while a merge-capable box is idle.
        inner.machines.get_mut("127.0.0.1").unwrap().task_policy = Some(vec![
            bm_proto::TaskPref {
                stage: Stage::Merge,
                enabled: false,
            },
            bm_proto::TaskPref {
                stage: Stage::Render,
                enabled: true,
            },
            bm_proto::TaskPref {
                stage: Stage::Digest,
                enabled: true,
            },
            bm_proto::TaskPref {
                stage: Stage::Crawl,
                enabled: true,
            },
        ]);
        let offer = inner.offer("w1").expect("render is offerable");
        assert_eq!(offer.task_id, "render:1");
    }

    /// Reorder the local box's policy so `first` leads; the rest follow in
    /// their canonical order. Every stage stays enabled.
    fn lead_with(inner: &mut Inner, first: Stage) {
        let mut list: Vec<bm_proto::TaskPref> = Vec::new();
        for s in [
            first,
            Stage::Crawl,
            Stage::Digest,
            Stage::Render,
            Stage::Merge,
        ] {
            if !list.iter().any(|p| p.stage == s) {
                list.push(bm_proto::TaskPref {
                    stage: s,
                    enabled: true,
                });
            }
        }
        inner.machines.get_mut("127.0.0.1").unwrap().task_policy = Some(list);
    }

    #[test]
    fn offer_respects_a_reordered_policy() {
        let (_d, mut inner) = fixture();
        offer_fixture(&mut inner);
        // ch1 crawl pending; ch2 digest ready (its crawl is done).
        inner
            .tasks
            .insert("crawl:1".into(), Task::new(1, Stage::Crawl));
        for (stage, state) in [
            (Stage::Crawl, TaskState::Done),
            (Stage::Digest, TaskState::Pending),
        ] {
            let mut t = Task::new(2, stage);
            t.state = state;
            inner.tasks.insert(format!("{stage}:2"), t);
        }
        // Crawl first → the crawl wins even though digest is also ready.
        lead_with(&mut inner, Stage::Crawl);
        let offer = inner.offer("w1").expect("crawl:1 is offerable");
        assert_eq!(offer.task_id, "crawl:1");
        // Digest first → the ready digest wins over the pending crawl.
        if let Some(t) = inner.tasks.get_mut("crawl:1") {
            t.state = TaskState::Pending;
            t.assigned_to = None;
            t.lease_until = None;
        }
        lead_with(&mut inner, Stage::Digest);
        let offer = inner.offer("w1").expect("digest:2 is offerable");
        assert_eq!(offer.task_id, "digest:2");
    }

    fn instance(id: &str, public: &str, private: &str) -> bm_core::provision::AwsInstance {
        bm_core::provision::AwsInstance {
            id: id.into(),
            instance_type: "t3.large".into(),
            state: "running".into(),
            az: "eu-central-1a".into(),
            spot: true,
            public_ip: public.into(),
            private_ip: private.into(),
            profile: "p".into(),
            launch_time: String::new(),
        }
    }

    #[test]
    fn relink_repoints_a_rotated_public_address() {
        let (_d, mut inner) = fixture();
        let mut m = Machine::new("18.1.1.1", "ubuntu", 22, None, "worker");
        m.name = "box-1".into();
        m.note = "EC2 i-0123456789abcdef0 (running)".into();
        inner.machines.insert("18.1.1.1".into(), m);
        inner.workers.insert("w1".into(), "18.1.1.1".into());

        let lines =
            inner.relink_drifted(&[instance("i-0123456789abcdef0", "52.2.2.2", "172.31.21.86")]);
        assert!(inner.machines.contains_key("52.2.2.2"));
        assert!(!inner.machines.contains_key("18.1.1.1"));
        assert_eq!(
            inner.machines["52.2.2.2"].name, "box-1",
            "the handle travels"
        );
        assert_eq!(
            inner.workers.get("w1").map(String::as_str),
            Some("52.2.2.2"),
            "the worker map follows the box"
        );
        assert!(
            lines.iter().any(|l| l.contains("address rotated")),
            "the repair is explained: {lines:?}"
        );
        // The config file moved with it, not just the in-memory map.
        let boxes = bm_core::provision::load_boxes(&inner.layout.machines());
        assert!(boxes.iter().any(|b| b.addr == "52.2.2.2"));
        assert!(!boxes.iter().any(|b| b.addr == "18.1.1.1"));
    }

    #[test]
    fn relink_folds_a_private_address_ghost() {
        let (_d, mut inner) = fixture();
        let mut m = Machine::new("52.2.2.2", "ubuntu", 22, None, "worker");
        m.name = "box-1".into();
        m.note = "EC2 i-0123456789abcdef0 (running)".into();
        inner.machines.insert("52.2.2.2".into(), m);
        // The agent reported its VPC-private address and `observe` minted a
        // second machine for the same box.
        inner.machines.insert(
            "172.31.21.86".into(),
            Machine::new("172.31.21.86", "unknown", 22, None, "worker"),
        );
        inner.workers.insert("w1".into(), "172.31.21.86".into());

        let lines =
            inner.relink_drifted(&[instance("i-0123456789abcdef0", "52.2.2.2", "172.31.21.86")]);
        assert!(!inner.machines.contains_key("172.31.21.86"));
        assert!(inner.machines.contains_key("52.2.2.2"));
        assert_eq!(
            inner.workers.get("w1").map(String::as_str),
            Some("52.2.2.2")
        );
        assert!(
            lines.iter().any(|l| l.contains("ghost")),
            "the fold is explained: {lines:?}"
        );
    }

    // -----------------------------------------------------------------
    // Batched render offers (`Settings::render_batch`)
    // -----------------------------------------------------------------

    /// A chapter planned to `n` takes, all of them work, with a capable worker
    /// registered. Returns the planned filenames in mix order.
    fn render_chapter(inner: &mut Inner, chapter: u32, n: usize) -> Vec<String> {
        let layout = inner.layout.clone();
        let segs: Vec<String> = (0..n)
            .map(|i| {
                let speaker = if i % 2 == 0 { "A" } else { "B" };
                format!(r#"{{"speaker":"{speaker}","text":"line {i}"}}"#)
            })
            .collect();
        std::fs::write(
            layout.script(chapter),
            format!(r#"{{"segments":[{}]}}"#, segs.join(",")),
        )
        .unwrap();
        std::fs::write(layout.cast("vieneu"), r#"{"A":"Đức Trí","B":"Adam"}"#).unwrap();
        for stage in [Stage::Crawl, Stage::Digest] {
            let mut t = Task::new(chapter, stage);
            t.state = TaskState::Done;
            inner.tasks.insert(format!("{stage}:{chapter}"), t);
        }
        inner.workers.insert("w1".into(), "192.168.2.2".into());
        inner.caps.insert(
            "w1".into(),
            vec![
                "crawl".into(),
                "digest".into(),
                "render".into(),
                "render-segments".into(),
                "merge".into(),
            ],
        );
        inner
            .materialize_render_takes(chapter)
            .expect("the chapter plans")
            .files()
    }

    /// Write a take's wav at the size the gate accepts as landed.
    fn land(inner: &Inner, chapter: u32, file: &str) {
        let seg = inner.layout.seg_dir("vieneu", chapter);
        std::fs::create_dir_all(&seg).unwrap();
        std::fs::write(seg.join(file), vec![0u8; 2000]).unwrap();
    }

    #[test]
    fn a_render_offer_carries_one_chapter_slice_of_the_configured_size() {
        // The whole point of the batch: one offer, five takes. The ledger still
        // holds one row per take — the grouping is recorded on the row the
        // offer names, so the report has something to settle against.
        let (_d, mut inner) = fixture();
        let files = render_chapter(&mut inner, 1, 25);

        let offer = inner.offer("w1").expect("a render is offerable");
        assert_eq!(
            offer.task_id, "render:1:0",
            "the batch is named by its first row"
        );
        let units = offer.render_units.as_ref().expect("planned, not legacy");
        assert_eq!(
            units.len(),
            bm_core::config::DEFAULT_RENDER_BATCH as usize,
            "five takes travel: {units:?}"
        );
        // In mix order, and the payload matches the plan the merge will read.
        assert_eq!(
            units.iter().map(|u| u.name.clone()).collect::<Vec<_>>(),
            files[..5].to_vec(),
            "the units are the chapter's first five takes, in order"
        );
        assert!(
            units.iter().all(|u| !u.take_key.is_empty()),
            "every unit carries its own content-addressed key"
        );

        // Every assigned row, and only those.
        for pos in 0..5 {
            let t = &inner.tasks[&format!("render:1:{pos}")];
            assert_eq!(
                t.state,
                TaskState::Assigned,
                "render:1:{pos} is in the batch"
            );
            assert_eq!(t.assigned_to.as_deref(), Some("w1"));
        }
        assert_eq!(
            inner.tasks["render:1:5"].state,
            TaskState::Pending,
            "the sixth take is not in this batch"
        );
        assert_eq!(
            inner.tasks["render:1:0"].batch,
            (1..5).map(|p| format!("render:1:{p}")).collect::<Vec<_>>(),
            "the grouping is recorded once, on the row the offer names"
        );
        assert!(
            inner.tasks["render:1:4"].batch.is_empty(),
            "and not repeated on every member — one fact, one place"
        );

        // **Nothing pins.** No take carries affinity, so the next worker to
        // ask takes the next slice of this chapter instead of opening
        // another one. See `a_second_worker_deepens_the_chapter_the_first_one_opened`.
        for pos in 0..6 {
            assert_eq!(
                inner.tasks[&format!("render:1:{pos}")].affinity,
                None,
                "render:1:{pos} stays unpinned: any box takes the next slice"
            );
        }
        assert_eq!(
            inner.tasks["render:1:24"].affinity, None,
            "and so is the last — takes are never pinned"
        );
    }

    #[test]
    fn a_second_worker_deepens_the_chapter_the_first_one_opened() {
        // **The scheduler's whole shape**: takes are never pinned, so the
        // second worker to ask deepens the chapter the first one opened
        // instead of opening a new one — and small batches keep every worker
        // cycling back to the scheduler, where a ready merge outranks the
        // next render slice.
        //
        // Two chapters are rendered, because the symptom is not "the second
        // worker idles" — it is "the second worker opens a *different* chapter",
        // and only a second chapter can show that.
        let (_d, mut inner) = fixture();
        render_chapter(&mut inner, 1, 25);
        render_chapter(&mut inner, 2, 25);

        let first = inner.offer("w1").expect("the first worker opens ch1");
        assert_eq!(first.chapter, 1);
        assert_eq!(first.task_id, "render:1:0", "from the front of the chapter");

        // A different box asks for work. It must deepen ch1, not open ch2.
        inner.workers.insert("w2".into(), "127.0.0.2".into());
        inner
            .caps
            .insert("w2".into(), vec!["render-segments".into()]);
        let second = inner.offer("w2").expect("the second worker gets work");
        assert_eq!(
            second.chapter, 1,
            "the second worker deepens ch1 rather than opening another chapter"
        );
        assert_eq!(
            second.task_id, "render:1:5",
            "exactly where the first batch stopped — no take is handed out twice"
        );
        assert_eq!(
            second.render_units.as_ref().unwrap().len(),
            bm_core::config::DEFAULT_RENDER_BATCH as usize,
            "and it gets a full batch of the same chapter"
        );
        assert_eq!(
            inner.tasks["render:1:0"].assigned_to.as_deref(),
            Some("w1"),
            "the first batch is still w1's"
        );
    }

    #[test]
    fn a_chapter_spoken_by_two_boxes_needs_no_merge_pin() {
        // The other half of sharing a chapter: completions record nothing
        // about who rendered what, because the merge pulls the pieces it
        // lacks from the inductor. The merge row simply exists, unpinned,
        // for whoever asks first.
        let (_d, mut inner) = fixture();
        let files = render_chapter(&mut inner, 9, 3);
        inner.workers.insert("w2".into(), "192.0.2.9".into());
        // `collect_units` pulls every unit home before a completion is applied,
        // so by the time either report lands this disk holds the chapter.
        for f in &files {
            land(&inner, 9, f);
        }

        // w1 speaks the first take.
        {
            let t = inner.tasks.get_mut("render:9:0").unwrap();
            t.state = TaskState::Running;
            t.assigned_to = Some("w1".into());
        }
        inner.complete(&completion(
            "w1",
            "render:9:0",
            true,
            "render ch9 (1 calls)",
        ));
        assert_eq!(
            inner.tasks["merge:9"].affinity, None,
            "the first completion writes no pin"
        );

        // w2 speaks the second. The chapter is now on two stores, and neither
        // of them is the one that can be proved complete.
        {
            let t = inner.tasks.get_mut("render:9:1").unwrap();
            t.state = TaskState::Running;
            t.assigned_to = Some("w2".into());
        }
        inner.complete(&completion(
            "w2",
            "render:9:1",
            true,
            "render ch9 (1 calls)",
        ));
        assert_eq!(
            inner.tasks["merge:9"].affinity, None,
            "nor does the second — the merge stays unpinned"
        );
        assert_eq!(inner.tasks["merge:9"].state, TaskState::Pending);
    }

    #[test]
    fn digests_chain_in_order() {
        // Chapter N reads the bible chapter N-1 wrote, so digest:N is
        // offerable only after digest:N-1 is Done. Chapter 1 has no
        // predecessor; a missing previous row (a range starting here) counts
        // as satisfied rather than deadlocking work that was never enqueued.
        let (_d, mut inner) = fixture();
        for (ch, stage, state) in [
            (1, Stage::Crawl, TaskState::Done),
            (1, Stage::Digest, TaskState::Done),
            (2, Stage::Crawl, TaskState::Done),
            (2, Stage::Digest, TaskState::Pending),
            (3, Stage::Crawl, TaskState::Done),
            (3, Stage::Digest, TaskState::Pending),
        ] {
            let mut t = Task::new(ch, stage);
            t.state = state;
            inner.tasks.insert(t.id(), t);
        }
        assert!(inner.upstream_done(1, Stage::Digest));
        assert!(
            inner.upstream_done(2, Stage::Digest),
            "crawl:2 done and digest:1 done — ch2 may go"
        );
        // Flip ch1 back to pending: ch2 must wait.
        inner.tasks.get_mut("digest:1").unwrap().state = TaskState::Pending;
        assert!(
            !inner.upstream_done(2, Stage::Digest),
            "digest:2 waits for digest:1"
        );
        assert!(
            !inner.upstream_done(3, Stage::Digest),
            "and transitively for the chain"
        );
        inner.tasks.get_mut("digest:1").unwrap().state = TaskState::Done;
        assert!(inner.upstream_done(2, Stage::Digest));
        // A chapter with no previous row is not blocked by it.
        inner.tasks.remove("digest:1");
        assert!(inner.upstream_done(2, Stage::Digest));
        // Other stages ignore the chain.
        assert!(inner.upstream_done(2, Stage::Crawl));
    }

    #[test]
    fn a_batch_never_spans_two_chapters() {
        // The pin, the merge's affinity, the progress line and the inductor's
        // unit collection are all keyed by chapter: an offer spanning chapters
        // would collect one chapter's wavs against another's report. Chapter 1
        // has fewer takes than the batch size, so the batch stops there rather
        // than reaching into chapter 2.
        let (_d, mut inner) = fixture();
        render_chapter(&mut inner, 1, 3);
        render_chapter(&mut inner, 2, 20);

        let first = inner.offer("w1").expect("ch1 renders first");
        assert_eq!(first.chapter, 1);
        assert_eq!(
            first.render_units.as_ref().unwrap().len(),
            3,
            "all of ch1 and none of ch2"
        );
        assert!(inner.tasks["render:2:0"].batch.is_empty());
        assert_eq!(inner.tasks["render:2:0"].state, TaskState::Pending);

        let second = inner.offer("w1").expect("then ch2");
        assert_eq!(second.chapter, 2);
        assert_eq!(
            second.render_units.as_ref().unwrap().len(),
            bm_core::config::DEFAULT_RENDER_BATCH as usize,
            "ch2 gets a full batch"
        );
    }

    #[test]
    fn the_workspace_batch_size_overrides_the_default_in_both_directions() {
        // One is the batch size that means "behave as before"; the cap is what
        // keeps a typo from holding a chapter on one box for hours.
        let (_d, mut inner) = fixture();
        let files = render_chapter(&mut inner, 1, 70);
        assert_eq!(files.len(), 70, "the chapter plans to seventy takes");

        inner.settings.render_batch = 1;
        let single = inner.offer("w1").expect("offerable");
        assert_eq!(single.render_units.as_ref().unwrap().len(), 1);
        assert!(
            inner.tasks["render:1:0"].batch.is_empty(),
            "no grouping to record"
        );

        // Hand the chapter back out: an absurd value is clamped, not obeyed,
        // and not refused — a workspace that cannot run at all is a worse
        // failure than one that runs slower than it asked.
        for t in inner.tasks.values_mut() {
            if t.stage == Stage::Render {
                t.state = TaskState::Pending;
                t.assigned_to = None;
                t.lease_until = None;
                t.batch.clear();
            }
        }
        inner.settings.render_batch = 100_000;
        let clamped = inner.offer("w1").expect("offerable");
        assert_eq!(
            clamped.render_units.as_ref().unwrap().len(),
            bm_core::config::MAX_RENDER_BATCH as usize,
            "100000 takes per offer is clamped to the cap, not obeyed"
        );
    }

    #[test]
    fn a_batched_report_settles_every_row_it_covered() {
        // The gate is per file, and the settle is per row: a report that
        // verified only the take it was named for would leave four rows
        // Assigned to a worker that has already answered, and their leases
        // would expire into a second render of takes that landed.
        let (_d, mut inner) = fixture();
        let files = render_chapter(&mut inner, 4, 12);
        let offer = inner.offer("w1").expect("a render is offerable");
        let units = offer.render_units.as_ref().unwrap();
        assert_eq!(units.len(), 5);
        for u in units {
            land(&inner, 4, &u.name);
        }

        let line = inner.complete(&{
            let mut c = completion("w1", "render:4:0", true, "render ch4 (5 calls)");
            c.units = 5;
            c
        });
        assert!(line.contains("done"), "{line}");
        for pos in 0..5 {
            let t = &inner.tasks[&format!("render:4:{pos}")];
            assert_eq!(
                t.state,
                TaskState::Done,
                "render:4:{pos} settled by the batch"
            );
            assert!(t.assigned_to.is_none(), "and released");
            assert!(t.batch.is_empty(), "the grouping is consumed");
        }
        assert_eq!(
            inner.tasks["render:4:5"].state,
            TaskState::Pending,
            "a take outside the batch is untouched"
        );
        assert_eq!(
            inner.tasks["merge:4"].affinity, None,
            "completions write no pin — the merge runs anywhere"
        );
        assert!(files.len() >= 12);
    }

    #[test]
    fn a_batched_report_with_one_take_missing_fails_the_whole_batch_by_name() {
        // A batch is one answer about one offer: a worker whose fifth unit
        // never landed did not fail only the first take. And the detail names
        // the file, so the retry is targeted rather than a re-speak of the
        // chapter.
        let (_d, mut inner) = fixture();
        render_chapter(&mut inner, 5, 12);
        let offer = inner.offer("w1").expect("a render is offerable");
        let units = offer.render_units.as_ref().unwrap().clone();
        for u in units.iter().take(4) {
            land(&inner, 5, &u.name);
        }
        let absent = units[4].name.clone();

        let line = inner.complete(&{
            let mut c = completion("w1", "render:5:0", true, "render ch5 (5 calls)");
            c.units = 5;
            c
        });
        assert!(line.contains("failed"), "{line}");
        for pos in 0..5 {
            let t = &inner.tasks[&format!("render:5:{pos}")];
            assert_eq!(t.state, TaskState::Pending, "the batch is requeued as one");
            assert_eq!(t.attempts, 1, "and struck as one");
            assert!(
                t.detail.contains(&absent),
                "the reason names the file: {}",
                t.detail
            );
            assert!(t.batch.is_empty());
        }
        assert_eq!(
            inner.tasks["render:5:5"].attempts, 0,
            "a take outside the batch takes no strike"
        );
    }

    #[test]
    fn a_batch_that_strikes_out_shelves_every_row_it_held() {
        // Three strikes is the rule; a batch must not shelter sixty rows from
        // it by never finishing, or the chapter retries for ever instead of
        // shelving for an operator to look at.
        let (_d, mut inner) = fixture();
        render_chapter(&mut inner, 6, 3);
        for _ in 0..3 {
            let offer = inner.offer("w1").expect("still offerable");
            assert_eq!(offer.render_units.as_ref().unwrap().len(), 3);
            inner.complete(&completion("w1", "render:6:0", false, "engine died"));
        }
        for pos in 0..3 {
            assert_eq!(
                inner.tasks[&format!("render:6:{pos}")].state,
                TaskState::Shelved,
                "render:6:{pos} shelves with its batch"
            );
        }
        assert!(
            inner.offer("w1").is_none(),
            "a shelved chapter stops being offered"
        );
    }

    #[test]
    fn an_offer_that_names_an_unplannable_take_still_reaches_the_gate() {
        // The degenerate case the batch truncation has to preserve: a take this
        // store cannot resolve gets an empty payload and is assigned alone, so
        // the completion gate fails it by name instead of the scheduler
        // skipping it for ever.
        let (_d, mut inner) = fixture();
        // A script with no cast file: `plan_units` cannot name the voices, so
        // the chapter has no takes at all and keeps its chapter-granular row.
        std::fs::write(
            inner.layout.script(9),
            r#"{"segments":[{"speaker":"A","text":"x"}]}"#,
        )
        .unwrap();
        for stage in [Stage::Crawl, Stage::Digest] {
            let mut t = Task::new(9, stage);
            t.state = TaskState::Done;
            inner.tasks.insert(format!("{stage}:9"), t);
        }
        inner.workers.insert("w1".into(), "192.168.2.2".into());
        inner
            .caps
            .insert("w1".into(), vec!["render-segments".into()]);
        inner.tasks.insert("render:9".into(), {
            let mut t = Task::new(9, Stage::Render);
            t.detail = "requeued: unplannable".into();
            t
        });

        let offer = inner.offer("w1").expect("the row is still offered");
        assert_eq!(offer.task_id, "render:9");
        assert_eq!(
            offer.render_units.as_deref().map(<[_]>::len),
            Some(0),
            "no unit to speak — and `Some([])`, not `None`, which would read as an old inductor"
        );
    }
}
