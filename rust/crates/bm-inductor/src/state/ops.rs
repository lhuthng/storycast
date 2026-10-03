use super::Inner;
use bm_proto::{now_secs, Stage, Task, TaskState};

/// How long a box may sit in `Initializing` before the inductor stops
/// believing it is booting.
///
/// A stock Ubuntu AMI answers ssh roughly 30–60 s after `RunInstances` returns.
/// Five minutes is not a wait — it is the point past which "still booting"
/// stops being a believable explanation, so the state becomes a verdict instead
/// of a placeholder that never resolves.
pub const BOOT_DEADLINE_SECS: u64 = 300;

mod apply;
mod lifecycle;
mod recast;
mod retry;
impl Inner {
    /// ETA for the remaining range, from measured throughput divided by
    /// live workers (heartbeat within the last 90s).
    pub fn op_eta(&self, start: u32, count: u32) -> String {
        let in_range = |t: &Task| t.chapter >= start && t.chapter < start + count;
        let pending = |stage: Stage| {
            self.tasks
                .values()
                .filter(|t| t.stage == stage && in_range(t) && !t.state.is_terminal())
                .count() as u64
        };
        let workers = self
            .beats
            .values()
            .filter(|b| now_secs().saturating_sub(b.ts) < 90)
            .count()
            .max(1) as u64;
        // Render's unit is already one take — a pending render task *is* one
        // TTS call — so it needs no calls-per-chapter scaling any more. That
        // estimate existed only while a render task was a whole chapter, and
        // leaving it in place would multiply the render ETA by forty.
        let remaining = [
            (Stage::Crawl, pending(Stage::Crawl)),
            (Stage::Digest, pending(Stage::Digest)),
            (Stage::Render, pending(Stage::Render)),
            (Stage::Merge, pending(Stage::Merge)),
        ];
        let etas = bm_core::eta::estimate_job(&self.layout.stats(), &remaining, workers);
        let total: u64 = etas.iter().map(|e| e.secs).sum();
        let mut parts: Vec<String> = etas
            .iter()
            .map(|e| {
                format!(
                    "{} {}{}",
                    e.stage,
                    bm_core::eta::human(e.secs),
                    if e.estimated_from_fallback {
                        " (guess)"
                    } else {
                        ""
                    }
                )
            })
            .collect();
        parts.push(format!("total {}", bm_core::eta::human(total)));
        format!(
            "ch{start}..{} over {workers} worker{}: {}",
            start + count - 1,
            if workers == 1 { "" } else { "s" },
            parts.join(" · ")
        )
    }
}
