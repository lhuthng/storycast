//! The babble guard: deciding whether a generated chunk is believable.
//!
//! A port of `core_utils.babble_suspect` / `babble_prefer` /
//! `count_speech_bursts`, and the retry loop around them.
//!
//! Why this exists: the generator stops at an end-of-speech token, and on a very
//! short chunk it sometimes misses. The model then keeps talking — inventing
//! words that were never in the text. The guard catches that after the fact and
//! generates again.
//!
//! For chunks of at most [`MAX_SYLLABLES`] syllables and no emotion cue, it
//! compares speech bursts with syllables. Past three syllables those bursts merge
//! and the count stops meaning anything, so long chunks are not judged that way.
//! They still get one unambiguous check: reaching the model-generated frame cap
//! means the generator failed to emit its end-of-speech token. Accepting such a
//! take can preserve a babbled tail or a repeated phrase, so it is regenerated.
//!
//! Two signals, from an A/B over 720 chunks:
//!
//! * **More bursts than syllables.** Each syllable is one energy burst; extra
//!   bursts mean extra speech.
//! * **A one-or-two-syllable chunk that ran to the frame ceiling.** Every case of
//!   invented words was a one-syllable chunk sitting at 12-13 of 13 frames, while
//!   a normal one-syllable chunk ends at 6-9. The cap is not a limit the model
//!   respects, it is a symptom when reached.

use crate::framecap::{is_cue_only, syllable_count};

/// Only chunks this short or shorter are checked.
pub const MAX_SYLLABLES: usize = 3;
/// How many times a suspect chunk is regenerated before giving up.
pub const MAX_RETRIES: usize = 2;

/// A burst is energy above this many dB below the chunk's peak.
const BURST_THRESH_DB: f64 = -18.0;
/// Two bursts closer than this are one burst (aspirated onsets).
const BURST_MIN_GAP_MS: usize = 60;
/// Bursts shorter than this do not count.
const BURST_MIN_MS: usize = 30;

/// `(suspect, syllables, bursts, frames)` for one generated chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verdict {
    pub suspect: bool,
    pub syllables: usize,
    pub bursts: usize,
    pub frames: usize,
}

impl Verdict {
    /// Is `self` a better generation than `old`?
    ///
    /// Better first means "not suspect at all"; otherwise fewer bursts wins, and
    /// on a tie the shorter one — a chunk that stopped earlier is less likely to
    /// have run on.
    pub fn better_than(&self, old: &Verdict) -> bool {
        if self.suspect != old.suspect {
            return !self.suspect;
        }
        self.bursts < old.bursts || (self.bursts == old.bursts && self.frames < old.frames)
    }

    /// The one-line reason, for a log.
    pub fn describe(&self, tries: usize, cap: usize) -> String {
        let what = if self.syllables > 0 {
            format!("chunk {} syllables: {} bursts", self.syllables, self.bursts)
        } else {
            "cue on its own".to_string()
        };
        format!(
            "babble guard: {what}, {}/{} frames after {tries} regeneration(s){}",
            self.frames,
            cap,
            if self.suspect {
                " — still suspect"
            } else {
                ""
            }
        )
    }
}

/// Approximate syllable count from a waveform: how many energy bursts it has.
///
/// A 10 ms envelope, thresholded 18 dB below the chunk's own peak, with bursts
/// closer than 60 ms merged. The threshold is relative to the peak on purpose —
/// an absolute one would depend on how loud the render happened to be.
pub fn count_speech_bursts(wav: &[f32], sample_rate: usize) -> usize {
    let hop = (sample_rate / 100).max(1);
    let n = wav.len() / hop;
    if n == 0 {
        return 0;
    }
    let env: Vec<f32> = (0..n)
        .map(|i| {
            let block = &wav[i * hop..(i + 1) * hop];
            let mean_sq: f32 = block.iter().map(|v| v * v).sum::<f32>() / block.len() as f32;
            mean_sq.sqrt()
        })
        .collect();
    let peak = env.iter().cloned().fold(0f32, f32::max);
    if peak <= 1e-6 {
        return 0;
    }
    let thresh = peak * 10f32.powf((BURST_THRESH_DB / 20.0) as f32);
    let min_gap = (BURST_MIN_GAP_MS / 10).max(1);
    let min_len = (BURST_MIN_MS / 10).max(1);

    let mut bursts: Vec<(usize, usize)> = Vec::new();
    let mut start: Option<usize> = None;
    let mut last_on = 0usize;
    for (i, v) in env.iter().enumerate() {
        if *v > thresh {
            match start {
                None => start = Some(i),
                Some(_) if i - last_on > min_gap => {
                    bursts.push((start.expect("set above"), last_on));
                    start = Some(i);
                }
                Some(_) => {}
            }
            last_on = i;
        }
    }
    if let Some(s) = start {
        bursts.push((s, last_on));
    }
    bursts.iter().filter(|(a, b)| b - a + 1 >= min_len).count()
}

/// Judge one generated chunk. `cap_frames` is the ceiling it was generated
/// under, and `frames` how many it actually produced.
pub fn suspect(
    pcm: &[f32],
    sample_rate: usize,
    phonemes: &str,
    cap_frames: usize,
    frames: usize,
) -> Verdict {
    if is_cue_only(phonemes) {
        // A run of laughter is many bursts, so counting is meaningless here —
        // only the ceiling rule applies.
        return Verdict {
            suspect: frames >= cap_frames.saturating_sub(1),
            syllables: 0,
            bursts: 0,
            frames,
        };
    }
    let syl = syllable_count(phonemes);
    let hit_cap = frames >= cap_frames.saturating_sub(1);
    if syl == 0 || phonemes.contains("<|emotion_") {
        return Verdict {
            suspect: false,
            syllables: syl,
            bursts: 0,
            frames: 0,
        };
    }
    if syl > MAX_SYLLABLES {
        // Burst counting is not reliable for continuous long-form speech, but
        // running into the hard frame ceiling is: the generator did not stop.
        // Do not use a duration heuristic here. The ceiling is the model's own
        // failure signal, and regenerating is safer than publishing a take that
        // may contain the babbled or repeated tail seen in production.
        return Verdict {
            suspect: hit_cap,
            syllables: syl,
            bursts: 0,
            frames,
        };
    }
    let bursts = count_speech_bursts(pcm, sample_rate);
    let short_hit_cap = syl <= 2 && hit_cap;
    Verdict {
        suspect: bursts > syl || short_hit_cap,
        syllables: syl,
        bursts,
        frames,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A click train: `count` bursts of 200 ms separated by 300 ms of silence.
    fn clicks(count: usize, sample_rate: usize) -> Vec<f32> {
        let on = sample_rate / 5;
        let off = sample_rate * 3 / 10;
        let mut out = Vec::new();
        for _ in 0..count {
            out.extend(std::iter::repeat_n(0.5f32, on));
            out.extend(std::iter::repeat_n(0.0f32, off));
        }
        out
    }

    #[test]
    fn a_burst_per_click() {
        assert_eq!(count_speech_bursts(&clicks(3, 48_000), 48_000), 3);
        assert_eq!(count_speech_bursts(&clicks(1, 48_000), 48_000), 1);
    }

    /// Silence has no peak to threshold against, and must not divide by it.
    #[test]
    fn silence_has_no_bursts() {
        assert_eq!(count_speech_bursts(&vec![0.0f32; 48_000], 48_000), 0);
    }

    /// Bursts closer together than the gap threshold merge into one.
    #[test]
    fn a_short_gap_does_not_split_a_burst() {
        let sr = 48_000;
        let mut w = vec![0.5f32; sr / 5];
        w.extend(std::iter::repeat_n(0.0f32, sr / 100)); // 10 ms gap
        w.extend(std::iter::repeat_n(0.5f32, sr / 5));
        assert_eq!(count_speech_bursts(&w, sr), 1);
    }

    #[test]
    fn more_bursts_than_syllables_is_suspect() {
        // Two syllables, three bursts.
        let v = suspect(&clicks(3, 48_000), 48_000, "ti ˈtɤ", 30, 10);
        assert!(v.suspect, "{v:?}");
        assert_eq!(v.bursts, 3);
        // Two syllables, two bursts, nowhere near the ceiling: fine.
        let v = suspect(&clicks(2, 48_000), 48_000, "ti ˈtɤ", 30, 10);
        assert!(!v.suspect, "{v:?}");
    }

    #[test]
    fn a_short_chunk_at_the_ceiling_is_suspect_however_few_bursts() {
        // One syllable, one burst — but it ran to 12 of 13 frames.
        let v = suspect(&clicks(1, 48_000), 48_000, "ti", 13, 12);
        assert!(v.suspect, "{v:?}");
        // The same chunk ending at 8 frames is what a normal one does.
        let v = suspect(&clicks(1, 48_000), 48_000, "ti", 13, 8);
        assert!(!v.suspect, "{v:?}");
    }

    /// Burst counting is meaningless for long chunks, but a long generation that
    /// reaches its hard frame cap missed the stop token and must be retried.
    #[test]
    fn a_long_chunk_is_only_suspect_when_it_hits_the_frame_cap() {
        let v = suspect(&clicks(9, 48_000), 48_000, "a b c d e f g", 40, 30);
        assert!(!v.suspect, "burst disagreement alone is not enough: {v:?}");
        assert_eq!(
            v.frames, 30,
            "a skipped burst check still reports its length"
        );

        let v = suspect(&clicks(9, 48_000), 48_000, "a b c d e f g", 40, 40);
        assert!(v.suspect, "the hard cap is an unambiguous failure: {v:?}");
        assert_eq!(v.frames, 40);
    }

    #[test]
    fn a_cue_uses_only_the_ceiling_rule() {
        let v = suspect(&clicks(5, 48_000), 48_000, "<|emotion_1|>", 13, 12);
        assert!(v.suspect);
        assert_eq!(v.syllables, 0);
        let v = suspect(&clicks(5, 48_000), 48_000, "<|emotion_1|>", 13, 6);
        assert!(!v.suspect);
    }

    #[test]
    fn preferring_a_regeneration() {
        let bad = Verdict {
            suspect: true,
            syllables: 1,
            bursts: 5,
            frames: 13,
        };
        let ok = Verdict {
            suspect: false,
            syllables: 1,
            bursts: 1,
            frames: 7,
        };
        assert!(ok.better_than(&bad));
        assert!(!bad.better_than(&ok));

        // Both suspect: fewer bursts wins, then shorter.
        let worse = Verdict {
            suspect: true,
            syllables: 1,
            bursts: 6,
            frames: 9,
        };
        assert!(bad.better_than(&worse));
        let same_bursts_longer = Verdict {
            suspect: true,
            syllables: 1,
            bursts: 5,
            frames: 20,
        };
        assert!(bad.better_than(&same_bursts_longer));
    }
}
