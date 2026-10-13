//! How long a chunk is allowed to be, in frames.

/// Frames per phoneme, before the fixed lead-in allowance.
const MAX_FRAMES_PER_PHONE: f64 = 2.0;
/// Room for lead-in and fixed per-chunk cost.
const FRAME_CAP_SLACK: i64 = 24;
/// Ceiling for a single-word chunk (~1 s at 12.5 frame/s).
const SINGLE_WORD_MAX_FRAMES: i64 = 13;
/// Extra frames granted per additional syllable.
const SYLLABLE_CAP_PER_EXTRA: i64 = 5;
/// Past this many syllables the per-phoneme formula is the better estimator.
const SYLLABLE_CAP_MAX_SYL: usize = 4;
/// The most phonemes one syllable can plausibly carry.
const SINGLE_WORD_MAX_PHONES: usize = 24;

/// IPA vowel letters. A tone mark (`ɜ` here is a *tone*, not a vowel) must not
const IPA_VOWELS: &str = "aeiouyæɐɑɒɔəɘɛɜɤɯɵøœʉʊʌɪɨ";

/// Strip `<|emotion_N|>`, `<en>` and `</en>`.
pub fn strip_markup(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        if b[i..].starts_with(b"<|emotion_") {
            // `<|emotion_` digits `|>`; anything else is literal text.
            let mut j = i + b"<|emotion_".len();
            let digits_at = j;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            if j > digits_at && b[j..].starts_with(b"|>") {
                i = j + 2;
                continue;
            }
        } else if b[i..].starts_with(b"</en>") {
            i += 5;
            continue;
        } else if b[i..].starts_with(b"<en>") {
            i += 4;
            continue;
        }
        // Copy one whole character, so the cursor stays on a boundary.
        let ch = s[i..].chars().next().expect("in-bounds char");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Estimated syllable count, in the reference's sense.
pub fn syllable_count(phonemes: &str) -> usize {
    let stripped = strip_markup(phonemes);
    let mut total = 0;
    for tok in stripped.split_whitespace() {
        let (mut groups, mut in_v, mut consonant_seen) = (0usize, false, true);
        for ch in tok.chars() {
            if IPA_VOWELS.contains(ch) {
                if !in_v && consonant_seen {
                    groups += 1;
                }
                in_v = true;
                consonant_seen = false;
            } else if ch == 'ː' || ch == 'ˈ' || ch == 'ˌ' || ch.is_ascii_digit() {
                in_v = false;
            } else {
                in_v = false;
                consonant_seen = true;
            }
        }
        if tok.chars().any(|c| c.is_alphabetic()) {
            total += groups.max(1);
        }
    }
    total
}

/// A chunk that is nothing but an emotion cue — no syllable to speak.
pub fn is_cue_only(phonemes: &str) -> bool {
    phonemes.contains("<|emotion_") && !strip_markup(phonemes).chars().any(|c| c.is_alphabetic())
}

/// The frame ceiling for a chunk with this phoneme string.
pub fn max_expected_frames(phonemes: &str) -> usize {
    let eff_len = strip_markup(phonemes).chars().count();
    let mut cap = FRAME_CAP_SLACK + (MAX_FRAMES_PER_PHONE * eff_len as f64).ceil() as i64;

    if is_cue_only(phonemes) {
        // Measured over 48 generations: a natural chuckle / sigh / throat-clear
        return cap.min(SINGLE_WORD_MAX_FRAMES) as usize;
    }
    if !phonemes.contains("<|emotion_") {
        let syl = syllable_count(phonemes).max(1);
        // A "syllable" longer than the plausible phoneme count is glued text
        if syl <= SYLLABLE_CAP_MAX_SYL && eff_len <= SINGLE_WORD_MAX_PHONES * syl {
            cap = cap.min(SINGLE_WORD_MAX_FRAMES + SYLLABLE_CAP_PER_EXTRA * (syl as i64 - 1));
        }
    }
    cap.max(0) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markup_is_removed_and_english_tags_too() {
        assert_eq!(strip_markup("a<|emotion_1|>b"), "ab");
        assert_eq!(strip_markup("<en>hello</en>"), "hello");
        assert_eq!(strip_markup("x<|emotion_12|>y"), "xy");
        // Not a cue: no digits before `|>`.
        assert_eq!(strip_markup("a<|emotion_|>b"), "a<|emotion_|>b");
        // Case-sensitive, per the reference regex.
        assert_eq!(strip_markup("<EN>a</EN>"), "<EN>a</EN>");
    }

    #[test]
    fn lengths_are_characters_not_bytes() {
        // `ɛ` is two bytes. Thirty of them is 30 characters and 60 bytes, which
        let ph = "ɛ".repeat(30);
        assert_eq!(strip_markup(&ph).chars().count(), 30);
        assert_eq!(strip_markup(&ph).len(), 60);
        assert_eq!(max_expected_frames(&ph), 84);

        // …and below that allowance the single-word cap wins outright, which is
        let short = "kwˈaː";
        assert_eq!(strip_markup(short).chars().count(), 5);
        assert_eq!(strip_markup(short).len(), 7);
        assert_eq!(max_expected_frames(short), SINGLE_WORD_MAX_FRAMES as usize);
    }

    #[test]
    fn a_cue_only_chunk_is_capped_like_one_word() {
        assert!(is_cue_only("<|emotion_1|>"));
        assert!(is_cue_only("<|emotion_2|> ."));
        assert!(!is_cue_only("<|emotion_2|> xin"));
        // The cue cap wins even though the phoneme formula would allow more.
        assert_eq!(
            max_expected_frames("<|emotion_1|>"),
            SINGLE_WORD_MAX_FRAMES as usize
        );
    }

    #[test]
    fn a_short_vietnamese_chunk_gets_the_syllable_allowance() {
        // Two syllables, few phonemes: 13 + 5*(2-1) = 18 beats 24 + 2*len.
        let ph = "ti ˈtɤ"; // 2 syllables
        assert_eq!(syllable_count(ph), 2);
        assert_eq!(max_expected_frames(ph), 18);
    }

    #[test]
    fn glued_text_falls_back_to_the_per_phoneme_formula() {
        // One "syllable" of 40 phonemes is normalization glue, not a word.
        let ph = "a".repeat(40);
        assert_eq!(syllable_count(&ph), 1);
        // 24 + ceil(2*40) = 104, and the single-word allowance is refused
        assert_eq!(max_expected_frames(&ph), 104);
    }

    #[test]
    fn a_tone_mark_does_not_open_a_vowel_group() {
        // `ɜ` is in the vowel set but here it is a tone mark after a mark, so
        assert_eq!(syllable_count("kwˈaːɜ"), 1);
        assert_eq!(syllable_count("a ɛ"), 2);
    }
}
