//! Sampling, and the per-codebook repetition history that feeds it.

use std::collections::{HashMap, VecDeque};

/// ~2.5 s of audio at 25 frame/s. Long enough to break a local loop, short
pub const DEFAULT_REP_WINDOW: usize = 64;

/// One codebook's sliding window: a multiset with FIFO eviction.
#[derive(Debug, Clone)]
pub struct ChannelWindow {
    seen: HashMap<i64, u32>,
    order: VecDeque<i64>,
    window: usize,
}

impl ChannelWindow {
    pub fn new(window: usize) -> Self {
        ChannelWindow {
            seen: HashMap::new(),
            order: VecDeque::new(),
            window,
        }
    }

    pub fn add(&mut self, code: i64) {
        *self.seen.entry(code).or_insert(0) += 1;
        if self.window > 0 {
            self.order.push_back(code);
            if self.order.len() > self.window {
                if let Some(old) = self.order.pop_front() {
                    match self.seen.get_mut(&old) {
                        Some(n) if *n > 1 => *n -= 1,
                        _ => {
                            self.seen.remove(&old);
                        }
                    }
                }
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }

    pub fn codes(&self) -> impl Iterator<Item = i64> + '_ {
        self.seen.keys().copied()
    }
}

#[derive(Debug, Clone)]
pub struct RepetitionHistory {
    pub channels: Vec<ChannelWindow>,
}

impl RepetitionHistory {
    pub fn new(n_channels: usize, window: usize) -> Self {
        RepetitionHistory {
            channels: (0..n_channels)
                .map(|_| ChannelWindow::new(window))
                .collect(),
        }
    }
}

/// SplitMix64: small, well-defined, and seedable, so a render can be reproduced
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed ^ 0x9e37_79b9_7f4a_7c15)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in [0, 1). 53 bits, the same resolution as NumPy's.
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// NumPy's `math.isclose(x, 1.0)` with default tolerances.
fn is_one(x: f64) -> bool {
    (x - 1.0).abs() <= 1e-9 * 1.0f64.max(x.abs())
}

#[derive(Debug, Clone, Copy)]
pub struct Sampling {
    pub temperature: f64,
    pub top_k: usize,
    pub top_p: f64,
    pub repetition_penalty: f64,
}

impl Default for Sampling {
    fn default() -> Self {
        // The reference's defaults in `infer`.
        Sampling {
            temperature: 0.8,
            top_k: 25,
            top_p: 0.95,
            repetition_penalty: 1.2,
        }
    }
}

/// The candidate set and its probabilities — everything `sample` decides with,
#[derive(Debug, Clone)]
pub struct Candidates {
    pub indices: Vec<usize>,
    pub probs: Vec<f32>,
}

/// Apply the repetition penalty in place, exactly as the reference does.
pub fn penalise(logits: &mut [f32], rep_pen: f64, prev: &ChannelWindow) {
    if is_one(rep_pen) || prev.is_empty() {
        return;
    }
    for i in prev.codes() {
        if let Some(v) = logits.get_mut(i as usize) {
            let x = *v as f64;
            *v = (if x < 0.0 { x * rep_pen } else { x / rep_pen }) as f32;
        }
    }
}

/// The top-k → top-p filter, up to but not including the draw.
pub fn candidates(logits: &[f32], s: &Sampling) -> Candidates {
    let v = logits.len();
    let k = s.top_k;
    let mut idx: Vec<usize> = if k > 0 && k < v {
        // The k largest, in index order. `select_nth_unstable` partitions like
        let mut all: Vec<usize> = (0..v).collect();
        all.select_nth_unstable_by(v - k, |a, b| {
            logits[*a]
                .partial_cmp(&logits[*b])
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        all[v - k..].to_vec()
    } else {
        (0..v).collect()
    };

    // Stable ascending by value, then reversed — the closest deterministic
    idx.sort_by(|a, b| {
        logits[*a]
            .partial_cmp(&logits[*b])
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    idx.reverse();

    // f32 throughout, because that is what NumPy does with a float32 array: the
    let mut p: Vec<f32> = idx.iter().map(|i| logits[*i]).collect();
    let max = p.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
    let mut sum = 0f32;
    for x in p.iter_mut() {
        *x = (*x - max).exp();
        sum += *x;
    }
    if sum > 0.0 {
        for x in p.iter_mut() {
            *x /= sum;
        }
    }

    if s.top_p > 0.0 && s.top_p < 1.0 {
        // `(cumsum(p) - p) < top_p` — the cumulative sum *before* this element,
        let top_p = s.top_p as f32;
        let mut acc = 0f32;
        for x in p.iter_mut() {
            let keep = acc < top_p;
            acc += *x;
            if !keep {
                *x = 0.0;
            }
        }
        let total: f32 = p.iter().sum();
        if total > 0.0 {
            for x in p.iter_mut() {
                *x /= total;
            }
        }
    }
    Candidates {
        indices: idx,
        probs: p,
    }
}

/// Draw an index from `p`, the way `np.random.choice` does: walk the cumulative
fn draw(c: &Candidates, rng: &mut Rng) -> usize {
    // NumPy draws in f64 against the cumulative sum of the (f32) probabilities.
    let u = rng.next_f64();
    let mut acc = 0f64;
    for (i, p) in c.probs.iter().enumerate() {
        acc += *p as f64;
        if u < acc {
            return c.indices[i];
        }
    }
    // Only reachable on floating-point shortfall; the last candidate is the
    *c.indices.last().expect("empty candidate set")
}

/// One code. Returns the chosen index.
pub fn sample(
    logits: &mut [f32],
    s: &Sampling,
    prev: Option<&ChannelWindow>,
    rng: &mut Rng,
) -> usize {
    if let Some(p) = prev {
        penalise(logits, s.repetition_penalty, p);
    }
    // The deterministic branch. `not (temperature and temperature > 0)` in the
    #[allow(clippy::neg_cmp_op_on_partial_ord)]
    if !(s.temperature > 0.0) {
        return argmax(logits);
    }
    // Divide, in f32, rather than multiply by a reciprocal: NumPy does
    let temp = s.temperature as f32;
    for x in logits.iter_mut() {
        *x /= temp;
    }
    let c = candidates(logits, s);
    draw(&c, rng)
}

/// First maximum, like `np.argmax`. Ties resolve to the lowest index, and that
pub fn argmax(x: &[f32]) -> usize {
    let mut best = 0;
    for (i, v) in x.iter().enumerate() {
        if *v > x[best] {
            best = i;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argmax_takes_the_first_maximum() {
        assert_eq!(argmax(&[1.0, 3.0, 3.0, 2.0]), 1);
        assert_eq!(argmax(&[-1.0, -2.0]), 0);
    }

    #[test]
    fn temperature_zero_is_argmax_and_ignores_the_penalty_set() {
        let mut h = RepetitionHistory::new(1, 64);
        h.channels[0].add(1);
        let mut logits = vec![0.0f32, 5.0, 5.0];
        let s = Sampling {
            temperature: 0.0,
            repetition_penalty: 1.2,
            ..Default::default()
        };
        let mut rng = Rng::new(1);
        // 1 is the first maximum and the penalty divides it by 1.2 -> 4.16,
        assert_eq!(sample(&mut logits, &s, Some(&h.channels[0]), &mut rng), 2);
    }

    #[test]
    fn the_penalty_moves_positive_down_and_negative_up() {
        let mut h = RepetitionHistory::new(1, 64);
        h.channels[0].add(0);
        h.channels[0].add(1);
        let mut logits = vec![2.0f32, -2.0];
        penalise(&mut logits, 1.2, &h.channels[0]);
        assert!((logits[0] as f64 - 2.0 / 1.2).abs() < 1e-6);
        assert!((logits[1] as f64 - -2.0 * 1.2).abs() < 1e-6);
    }

    #[test]
    fn the_window_evicts_and_forgets() {
        let mut w = ChannelWindow::new(3);
        for c in [1, 1, 2, 3] {
            w.add(c);
        }
        // Window holds the last three: 1, 2, 3. The first 1 was evicted but the
        assert!(w.seen.contains_key(&1));
        w.add(4);
        assert!(!w.seen.contains_key(&1), "both 1s should have aged out");
        assert_eq!(w.seen.len(), 3);
    }

    /// A window of 0 is the documented "old behaviour": penalise forever.
    #[test]
    fn window_zero_never_evicts() {
        let mut w = ChannelWindow::new(0);
        for c in 0..500 {
            w.add(c);
        }
        assert_eq!(w.seen.len(), 500);
    }

    #[test]
    fn top_k_narrows_and_top_p_keeps_a_prefix() {
        let logits = vec![10.0f32, 9.0, 1.0, 0.0];
        let c = candidates(
            &logits,
            &Sampling {
                top_k: 2,
                top_p: 1.0,
                ..Default::default()
            },
        );
        assert_eq!(c.indices, vec![0, 1]);
        assert!((c.probs.iter().sum::<f32>() - 1.0).abs() < 1e-6);

        // A nucleus narrower than the top-k set drops the tail candidate.
        let c = candidates(
            &logits,
            &Sampling {
                top_k: 4,
                top_p: 0.5,
                ..Default::default()
            },
        );
        assert_eq!(c.indices[0], 0);
        assert_eq!(c.probs[3], 0.0, "the tail must be zeroed, not dropped");
        assert!((c.probs.iter().sum::<f32>() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn the_draw_stays_inside_the_candidate_set() {
        let logits = vec![1.0f32, 2.0, 3.0];
        let s = Sampling {
            top_k: 2,
            top_p: 1.0,
            temperature: 1.0,
            repetition_penalty: 1.0,
        };
        let mut rng = Rng::new(7);
        for _ in 0..200 {
            let mut l = logits.clone();
            let i = sample(&mut l, &s, None, &mut rng);
            assert!(i == 1 || i == 2, "drew {i} outside the top-2 set");
        }
    }
}
