//! Where the render time actually goes.
//!
//! Written because a claim needed checking. `matvec` was rewritten to break its
//! floating-point dependency chain on the theory that it was "the whole gap"
//! against the Python reference — the change was real and made the port ~11%
//! faster, but the gap only closed from 1.41x to 1.25x. So the theory was wrong
//! by two thirds, and guessing again would have been the second mistake. This
//! measures the split instead.
//!
//! Always compiled, deliberately: a `cfg`-gated version would mean the profiled
//! build is not the shipped build, which is the one thing a profiler must not
//! be. The cost is one `Instant::now()` pair and one uncontended `fetch_add` per
//! counted operation — tens of nanoseconds against a render measured in seconds.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// The countable operations, one bucket each. Separate buckets rather than one
/// `onnx` total because "the ONNX layer is slow" and "one of the three graphs is
/// slow" call for completely different fixes, and the total cannot tell them
/// apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum Kind {
    /// The projection from a hidden state to logits — `x @ table.T`.
    Matvec = 0,
    /// `vieneu_prefill.onnx`, once per chunk.
    Prefill = 1,
    /// `vieneu_decode_step.onnx`, once per frame.
    Decode = 2,
    /// `vieneu_acoustic_cached.onnx`, 16x per frame.
    Acoustic = 3,
    /// The MOSS codec's decode graph, once per chunk.
    Codec = 4,
}

const N: usize = 5;
const NAMES: [&str; N] = ["matvec", "prefill", "decode", "acoustic", "codec"];

static NS: [AtomicU64; N] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];
static CALLS: [AtomicU64; N] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

pub fn add(kind: Kind, d: Duration) {
    let i = kind as usize;
    NS[i].fetch_add(d.as_nanos() as u64, Ordering::Relaxed);
    CALLS[i].fetch_add(1, Ordering::Relaxed);
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Row {
    pub name: &'static str,
    pub ns: u64,
    pub calls: u64,
}

impl Row {
    pub fn ms(&self) -> f64 {
        self.ns as f64 / 1e6
    }

    pub fn us_per_call(&self) -> f64 {
        self.ns as f64 / 1e3 / self.calls.max(1) as f64
    }
}

pub fn snapshot() -> Vec<Row> {
    (0..N)
        .map(|i| Row {
            name: NAMES[i],
            ns: NS[i].load(Ordering::Relaxed),
            calls: CALLS[i].load(Ordering::Relaxed),
        })
        .collect()
}

pub fn total_ms() -> f64 {
    snapshot().iter().map(|r| r.ms()).sum()
}

pub fn total_calls() -> u64 {
    snapshot().iter().map(|r| r.calls).sum()
}

pub fn reset() {
    for i in 0..N {
        NS[i].store(0, Ordering::Relaxed);
        CALLS[i].store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_are_independent_and_resettable() {
        reset();
        add(Kind::Matvec, Duration::from_micros(10));
        add(Kind::Matvec, Duration::from_micros(10));
        add(Kind::Acoustic, Duration::from_micros(5));

        let s = snapshot();
        let get = |n: &str| *s.iter().find(|r| r.name == n).unwrap();
        assert_eq!(get("matvec").calls, 2);
        assert_eq!(get("matvec").ns, 20_000);
        assert_eq!(get("acoustic").calls, 1);
        assert_eq!(get("decode").calls, 0);
        assert_eq!(total_calls(), 3);

        reset();
        assert_eq!(total_calls(), 0);
        assert_eq!(total_ms(), 0.0);
    }
}
