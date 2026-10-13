//! Release-mode benchmark of the ledger's hot calls — `offer`, `reap` and
//! `save` — against the ledger shapes a real run leaves behind. Hand-run only:
//! the figures mean nothing without optimisation, so a debug build is refused.

use super::*;
use std::hint::black_box;
use std::time::{Duration, Instant};

/// Chapters the ledger holds, and takes per chapter.
const CHAPTERS: [u32; 3] = [100, 400, 1200];
const TAKES: u32 = 40;
/// The merged prefix: a run a quarter of the way through its range.
const MERGED: f64 = 0.25;
/// Boxes in the fleet, the first of which is the one being asked.
const BOXES: u8 = 6;
const ASKER: &str = "bench-1";
/// Measured time per figure. Three runs are made regardless.
const BUDGET: Duration = Duration::from_secs(2);

/// A ledger mid-run: the first `MERGED` chapters are merged, the rest are
/// scripted and waiting for voice, and the frontier chapter has a plan on disk
/// so a render offer has something real to hand out.
fn seed(chapters: u32) -> (tempfile::TempDir, Inner) {
    let (dir, mut inner) = fixture();
    // Both of reap's passes: an inductor up for an hour, not one still inside
    // its 120s orphan grace.
    inner.started_at = now_secs() - 3600;
    let merged = (f64::from(chapters) * MERGED) as u32;
    for n in 1..=chapters {
        let done = n <= merged;
        for stage in [Stage::Crawl, Stage::Digest, Stage::Merge] {
            let mut t = Task::new(n, stage);
            // A scripted chapter has its text and its script; the mix waits.
            if done || stage != Stage::Merge {
                t.state = TaskState::Done;
            }
            inner.tasks.insert(t.id(), t);
        }
        for pos in 0..TAKES as usize {
            let mut t = Task::new_take(n, pos);
            if done {
                t.state = TaskState::Done;
            }
            inner.tasks.insert(t.id(), t);
        }
    }
    let engine = inner.settings.engine.clone();
    let quality = bm_core::assemble::TakeQuality::parse(&inner.settings.take_quality);
    let units: Vec<bm_core::assemble::RenderUnit> = (0..TAKES as usize).map(unit).collect();
    bm_core::assemble::RenderPlan::build(merged + 1, &engine, &units, quality)
        .save(&inner.layout.plan(merged + 1))
        .unwrap();
    for n in 1..=BOXES {
        register(&mut inner, n);
    }
    (dir, inner)
}

/// One spoken segment of the frontier chapter.
fn unit(i: usize) -> bm_core::assemble::RenderUnit {
    bm_core::assemble::RenderUnit {
        tag: format!("seg-{i:03}"),
        dest: std::path::PathBuf::from(format!("seg-{i:03}.wav")),
        speaker: if i == 0 {
            "Narrator".to_string()
        } else {
            format!("Char{}", i % 7)
        },
        voice: "Adam".to_string(),
        text: format!("line {i} of the chapter, spoken by somebody"),
        temperature: 0.7,
        silence_p: 0.2,
        indices: vec![i],
    }
}

/// One box the scheduler can offer to: a machine, a live beat, and the
/// capabilities a render offer is gated on.
fn register(inner: &mut Inner, n: u8) {
    let worker = format!("bench-{n}");
    let addr = format!("10.0.0.{n}");
    let mut m = Machine::new(&addr, "bench", 22, None, "worker");
    m.set_state(MachineState::Online);
    inner.machines.insert(addr.clone(), m);
    inner.workers.insert(worker.clone(), addr.clone());
    inner
        .beats
        .insert(worker.clone(), beat_with_load(&worker, &addr, None));
    inner
        .caps
        .insert(worker.clone(), vec!["render-segments".into(), "merge".into()]);
}

/// Put the ledger back the way an offer found it, so every run measures the
/// same starting state and not a ledger the last run already ate.
fn unreserve(inner: &mut Inner) {
    let now = now_secs();
    for t in inner.tasks.values_mut() {
        if matches!(t.state, TaskState::Assigned | TaskState::Running) {
            t.state = TaskState::Pending;
            t.clear_holders();
            t.lease_until = None;
            t.batch.clear();
            t.updated = now;
        }
    }
}

/// Every unclaimed row held by another live box: a busy fleet, where the worker
/// asking gets nothing. Every poll in that fleet pays this.
fn hold_all(inner: &mut Inner) {
    let others: Vec<String> = inner
        .workers
        .keys()
        .filter(|w| w.as_str() != ASKER)
        .cloned()
        .collect();
    let lease = now_secs() + 10_000;
    let mut i = 0;
    for t in inner.tasks.values_mut() {
        if t.state != TaskState::Pending {
            continue;
        }
        t.state = TaskState::Assigned;
        t.assigned_to = Some(others[i % others.len()].clone());
        t.lease_until = Some(lease);
        i += 1;
    }
}

/// A restart's read: a fresh ledger from the file just written.
fn reload(inner: &mut Inner) {
    let mut fresh = Inner::new(inner.layout.clone(), inner.settings.clone());
    fresh.load_ledger();
    black_box(fresh.tasks.len());
}

/// Median wall time of `f` over a fixed budget. `reset` runs before each run,
/// untimed, to put the state back the way `f` expects to find it.
fn timed<T>(state: &mut T, reset: fn(&mut T), f: fn(&mut T)) -> Duration {
    let mut samples: Vec<Duration> = Vec::new();
    let start = Instant::now();
    loop {
        reset(state);
        let t0 = Instant::now();
        f(state);
        samples.push(t0.elapsed());
        if samples.len() >= 3 && (start.elapsed() >= BUDGET || samples.len() >= 30) {
            break;
        }
    }
    samples.sort_unstable();
    samples[samples.len() / 2]
}

fn ms(d: Duration) -> String {
    format!("{:.3}", d.as_secs_f64() * 1e3)
}

fn human_bytes(b: u64) -> String {
    if b >= 1 << 20 {
        format!("{:.1}M", b as f64 / 1_048_576.0)
    } else {
        format!("{}k", b / 1024)
    }
}

/// `BM_BENCH_CHAPTERS=2000` (comma-separated) runs another size.
#[test]
#[ignore = "benchmark: cargo test -p bm-inductor --release perf -- --ignored --nocapture"]
fn scheduler_cost_by_ledger_size() {
    if cfg!(debug_assertions) {
        panic!("a debug build measures nothing — run this with --release");
    }
    let sizes: Vec<u32> = std::env::var("BM_BENCH_CHAPTERS")
        .map(|v| v.split(',').filter_map(|s| s.trim().parse().ok()).collect())
        .unwrap_or_else(|_| CHAPTERS.to_vec());
    println!(
        "{TAKES} takes/chapter, {:.0}% merged, {BOXES} boxes; median ms of ≥3 runs, {}s budget",
        MERGED * 100.0,
        BUDGET.as_secs()
    );
    println!(
        "{:>7} {:>7} {:>8} {:>11} {:>8} {:>8} {:>8}",
        "rows", "ledger", "offer", "offer/idle", "reap", "save", "load"
    );
    println!("offer is reap + scan + save; offer/idle is the poll that finds nothing");
    for chapters in sizes {
        let (_d, mut inner) = seed(chapters);
        let layout = inner.layout.clone();

        // Asserted once per shape: a figure that quietly measured a 204 where a
        // render offer was meant would be worse than no figure at all.
        assert!(
            inner.offer(ASKER).is_some(),
            "the cold shape must offer the frontier render"
        );
        unreserve(&mut inner);
        hold_all(&mut inner);
        assert!(
            inner.offer(ASKER).is_none(),
            "the idle shape must offer nothing"
        );
        unreserve(&mut inner);

        let save = timed(&mut inner, |_| {}, |i| i.save());
        let bytes = std::fs::metadata(layout.ledger())
            .map(|m| m.len())
            .unwrap_or(0);
        let load = timed(&mut inner, |_| {}, reload);
        let reap = timed(&mut inner, |_| {}, |i| {
            black_box(i.reap());
        });
        let cold = timed(&mut inner, unreserve, |i| {
            black_box(i.offer(ASKER));
        });
        unreserve(&mut inner);
        let idle = timed(&mut inner, hold_all, |i| {
            black_box(i.offer(ASKER));
        });

        println!(
            "{:>7} {:>7} {:>8} {:>11} {:>8} {:>8} {:>8}",
            inner.tasks.len(),
            human_bytes(bytes),
            ms(cold),
            ms(idle),
            ms(reap),
            ms(save),
            ms(load)
        );
    }
}
