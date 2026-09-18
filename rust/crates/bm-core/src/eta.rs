//! Throughput measurement and ETA.
//!
//! New in the cluster version. The legacy pipeline could not answer "how long
//! will the rest take?" because a single process had no separation between
//! measuring and doing. Here every finished task appends one line to
//! `.bm/stats.jsonl`, and the estimator turns those into a per-stage cost.

use crate::util::atomic_write;
use anyhow::Result;
use bm_proto::Stage;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// How many recent samples feed the estimate. Recent behaviour beats history:
/// the first chapter of a session is slow (model load), later ones are not.
const WINDOW: usize = 20;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatRecord {
    pub stage: String,
    /// Work units the sample covers — segments for render, chapters otherwise.
    pub units: u64,
    pub secs: f64,
    pub ts: u64,
    #[serde(default)]
    pub worker: String,
}

/// Append one measurement.
pub fn record(path: &Path, stage: Stage, units: u64, secs: f64, worker: &str) -> Result<()> {
    let rec = StatRecord {
        stage: stage.as_str().to_string(),
        units: units.max(1),
        secs,
        ts: bm_proto::now_secs(),
        worker: worker.to_string(),
    };
    let mut body = std::fs::read_to_string(path).unwrap_or_default();
    body.push_str(&serde_json::to_string(&rec)?);
    body.push('\n');
    atomic_write(path, &body)
}

pub fn read_stats(path: &Path) -> Vec<StatRecord> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<StatRecord>(l).ok())
        .collect()
}

/// Median of a small sample set. Shared by the per-unit estimator below
/// and the inductor's per-task averages behind the Stats pane.
///
/// Median rather than mean: one chapter that hit a rate limit for 10 minutes
/// would otherwise poison every estimate after it.
pub fn median(samples: &[f64]) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Some(sorted[sorted.len() / 2])
}

/// Median seconds per work unit for a stage, from the most recent samples.
pub fn secs_per_unit(path: &Path, stage: Stage) -> Option<f64> {
    let samples: Vec<f64> = read_stats(path)
        .into_iter()
        .filter(|r| r.stage == stage.as_str() && r.units > 0 && r.secs > 0.0)
        .map(|r| r.secs / r.units as f64)
        .collect();
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples;
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = sorted.len().min(WINDOW);
    median(&sorted[sorted.len() - n..])
}

/// Per-stage ETA for a batch of work, spread across `workers` machines.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageEta {
    pub stage: String,
    pub units: u64,
    pub secs: u64,
    /// True when no measurement exists yet and `secs` is a fallback guess.
    pub estimated_from_fallback: bool,
}

/// Fallback costs (seconds per unit) used before anything has been measured.
/// Deliberately pessimistic so the first ETA is not a pleasant lie.
fn fallback_secs_per_unit(stage: Stage) -> f64 {
    match stage {
        Stage::Crawl => 6.0,
        Stage::Digest => 45.0,
        // a run is ~2-6 lines of speech; ~4s of audio per line locally
        Stage::Render => 22.0,
        Stage::Merge => 20.0,
    }
}

/// Estimate one stage: `units` items of work, `workers` machines pulling in parallel.
pub fn estimate_stage(path: &Path, stage: Stage, units: u64, workers: u64) -> StageEta {
    let measured = secs_per_unit(path, stage);
    let per_unit = measured.unwrap_or_else(|| fallback_secs_per_unit(stage));
    let parallel = workers.max(1) as f64;
    StageEta {
        stage: stage.as_str().to_string(),
        units,
        secs: ((units as f64 * per_unit) / parallel).round() as u64,
        estimated_from_fallback: measured.is_none(),
    }
}

/// Estimate a whole job: how many units each stage must still do.
pub fn estimate_job(path: &Path, remaining: &[(Stage, u64)], workers: u64) -> Vec<StageEta> {
    remaining
        .iter()
        .map(|(stage, units)| estimate_stage(path, *stage, *units, workers))
        .collect()
}

/// `3h 12m` / `48s` — for the TUI and the CLI.
pub fn human(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("bm-eta-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.join("stats.jsonl")
    }

    #[test]
    fn no_samples_falls_back_and_says_so() {
        let p = tmp("empty");
        let e = estimate_stage(&p, Stage::Render, 100, 2);
        assert!(e.estimated_from_fallback);
        assert!(e.secs > 0);
    }

    #[test]
    fn median_ignores_a_single_disaster_sample() {
        let p = tmp("median");
        for _ in 0..5 {
            record(&p, Stage::Render, 10, 100.0, "w1").unwrap(); // 10 s/unit
        }
        record(&p, Stage::Render, 1, 6000.0, "w1").unwrap(); // 6000 s/unit outlier
        let per = secs_per_unit(&p, Stage::Render).unwrap();
        assert!(per < 100.0, "median should reject the outlier, got {per}");
    }

    #[test]
    fn parallel_workers_divide_the_time() {
        let p = tmp("parallel");
        for _ in 0..3 {
            record(&p, Stage::Digest, 1, 60.0, "w1").unwrap();
        }
        let one = estimate_stage(&p, Stage::Digest, 10, 1);
        let four = estimate_stage(&p, Stage::Digest, 10, 4);
        assert_eq!(one.secs, 600);
        assert_eq!(four.secs, 150);
        assert!(!four.estimated_from_fallback);
    }

    #[test]
    fn stats_are_per_stage() {
        let p = tmp("perstage");
        record(&p, Stage::Digest, 1, 30.0, "w").unwrap();
        assert!(secs_per_unit(&p, Stage::Render).is_none());
        assert_eq!(secs_per_unit(&p, Stage::Digest).unwrap(), 30.0);
    }

    #[test]
    fn human_formats_all_three_ranges() {
        assert_eq!(human(45), "45s");
        assert_eq!(human(150), "2m 30s");
        assert_eq!(human(3 * 3600 + 12 * 60), "3h 12m");
    }

    #[test]
    fn corrupt_stat_lines_are_skipped_not_fatal() {
        let p = tmp("corrupt");
        let mut body = std::fs::read_to_string(&p).unwrap_or_default();
        body.push_str("not json\n");
        std::fs::write(&p, body).unwrap();
        record(&p, Stage::Crawl, 1, 5.0, "w").unwrap();
        assert_eq!(secs_per_unit(&p, Stage::Crawl).unwrap(), 5.0);
    }
}
