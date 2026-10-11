//! Choosing a real line to audition a voice on.

use std::collections::HashMap;
use std::path::PathBuf;

/// A sample has to be a sentence. `data/script-01.json` really does contain
pub(crate) const MIN_LINE_CHARS: usize = 40;

/// ...and it has to end. A 400-character line is half a minute of audio, which is
pub(crate) const MAX_LINE_CHARS: usize = 160;

/// Per-character cap. 4 836 segments across 100 scripts is ~400 KB of text —
const PER_CHARACTER_CAP: usize = 200;

/// Every `data/script/NN.json` in the workspace, sorted.
pub(crate) fn script_files(layout: &bm_core::Layout) -> Vec<PathBuf> {
    layout.scripts()
}

/// A script, as far as a line index reads it: every script in the book is
/// walked whenever the auditioner needs its lines, so the read is typed rather
/// than a `Value` tree the walk then picks apart.
#[derive(serde::Deserialize)]
struct ScriptSegments {
    #[serde(default)]
    segments: Vec<ScriptSegment>,
}

#[derive(serde::Deserialize)]
struct ScriptSegment {
    #[serde(default)]
    speaker: String,
    #[serde(default)]
    text: String,
}

/// `speaker -> every line they speak`, deduplicated, in script order.
pub(crate) fn index_lines(
    layout: &bm_core::Layout,
) -> Result<HashMap<String, Vec<String>>, String> {
    let files = script_files(layout);
    if files.is_empty() {
        return Err(format!(
            "no data/script/NN.json under {} — run :translate first",
            layout.script_dir().display()
        ));
    }
    let mut index: HashMap<String, Vec<String>> = HashMap::new();
    let mut seen: HashMap<String, std::collections::HashSet<String>> = HashMap::new();
    let mut parsed = 0usize;
    for path in &files {
        let Ok(doc) = bm_core::read_json::<ScriptSegments>(path) else {
            continue;
        };
        parsed += 1;
        for seg in &doc.segments {
            let speaker = seg.speaker.trim();
            let text = seg.text.trim();
            if speaker.is_empty() || text.is_empty() {
                continue;
            }
            let bucket = index.entry(speaker.to_string()).or_default();
            if bucket.len() >= PER_CHARACTER_CAP {
                continue;
            }
            // The same grunt recurs in every chapter; one copy is enough, and
            let marks = seen.entry(speaker.to_string()).or_default();
            if marks.insert(text.to_string()) {
                bucket.push(text.to_string());
            }
        }
    }
    if parsed == 0 {
        return Err(format!(
            "{} script file(s) under {} but none parsed",
            files.len(),
            layout.data().display()
        ));
    }
    Ok(index)
}

/// Pick one line for a character.
pub(crate) fn choose_line(lines: Option<&Vec<String>>, seed: u64) -> Option<String> {
    let all = lines?;
    let in_band: Vec<&String> = all
        .iter()
        .filter(|t| {
            let n = t.chars().count();
            (MIN_LINE_CHARS..=MAX_LINE_CHARS).contains(&n)
        })
        .collect();
    if !in_band.is_empty() {
        return Some(in_band[pick_index(in_band.len(), seed)].clone());
    }
    if all.is_empty() {
        return None;
    }
    Some(all[pick_index(all.len(), seed)].clone())
}

/// A line held on a screen, so an A/B plays the *same* sentence in two voices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuditionLine {
    pub(crate) character: String,
    pub(crate) text: String,
}

/// The line to use for `character`: the held one when it is already theirs, else
pub(crate) fn line_for(
    held: Option<&AuditionLine>,
    character: &str,
    index: Option<&HashMap<String, Vec<String>>>,
    seed: u64,
) -> Option<AuditionLine> {
    if let Some(h) = held {
        if h.character == character {
            return Some(h.clone());
        }
    }
    let text = choose_line(index.and_then(|i| i.get(character)), seed)?;
    Some(AuditionLine {
        character: character.to_string(),
        text,
    })
}

/// A deterministic index into `len`, for a given `seed`.
pub(crate) fn pick_index(len: usize, seed: u64) -> usize {
    if len == 0 {
        return 0;
    }
    let mut x = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    (x % len as u64) as usize
}

/// How long a rendered audition stays replayable. Short on purpose: a render
/// is stochastic, so a slot that outlived a comparison would make `T` sound
/// stuck rather than fast.
const AUDITION_TTL: std::time::Duration = std::time::Duration::from_secs(60);

/// One rendered audition, held briefly so `T` can replay it instead of buying
/// another sidecar render of the same sentence on the same voice. One slot:
/// the wav travels as base64 (~a few hundred KB), and the comparison under way
/// is always one held sentence, voice against voice.
#[derive(Debug, Clone, Default)]
pub(crate) struct AuditionCache {
    /// The voice and sentence the render on the wire was asked for.
    pub(crate) voice: String,
    pub(crate) text: String,
    /// The base64 wav once it landed — `None` while the render is still out.
    pub(crate) audio: Option<String>,
    /// When the wav landed; the TTL runs from here.
    pub(crate) at: Option<std::time::Instant>,
}

impl AuditionCache {
    /// Take the slot for a render going out now: nothing is cached about it
    /// until an answer arrives.
    pub(crate) fn request(&mut self, voice: &str, text: &str) {
        self.voice = voice.to_string();
        self.text = text.to_string();
        self.audio = None;
        self.at = None;
    }

    /// The wav already rendered for exactly this voice and sentence, while it
    /// is still fresh.
    pub(crate) fn fresh(&self, voice: &str, text: &str) -> Option<&str> {
        if self.voice != voice || self.text != text {
            return None;
        }
        let at = self.at?;
        if at.elapsed() >= AUDITION_TTL {
            return None;
        }
        self.audio.as_deref()
    }

    /// The answer to the request on the wire: keep it. An answer for any other
    /// voice is a late reply to a request this slot has already forgotten.
    pub(crate) fn fill(&mut self, voice: &str, audio: String) {
        if self.voice != voice {
            return;
        }
        self.audio = Some(audio);
        self.at = Some(std::time::Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// Long enough to clear `MIN_LINE_CHARS`, tagged so tests can name it.
    fn long(tag: &str) -> String {
        format!("{tag} {}", "nội dung ".repeat(6))
    }

    #[test]
    fn a_grunt_never_wins_over_a_sentence() {
        // The real data has "Ừm!" for a main character. If that is ever chosen,
        let l = lines(&["Ừm!", &long("a"), &long("b")]);
        for seed in 0..64 {
            let picked = choose_line(Some(&l), seed).unwrap();
            assert_ne!(picked, "Ừm!", "seed {seed} chose the grunt");
            assert!(picked.chars().count() >= MIN_LINE_CHARS, "{picked:?}");
        }
    }

    #[test]
    fn a_monologue_is_not_preferred_either() {
        let shout = "dài ".repeat(80);
        assert!(shout.chars().count() > MAX_LINE_CHARS);
        let l = lines(&[shout.as_str(), &long("a")]);
        for seed in 0..64 {
            assert_ne!(choose_line(Some(&l), seed).unwrap(), shout, "seed {seed}");
        }
    }

    #[test]
    fn a_character_who_only_grunts_still_gets_a_line() {
        // Falling back is better than refusing: "Ừm!" in the right voice still
        let l = lines(&["Ừm!", "Hừ."]);
        let picked = choose_line(Some(&l), 7).unwrap();
        assert!(l.contains(&picked), "{picked:?}");
    }

    #[test]
    fn no_lines_at_all_is_none_not_an_empty_string() {
        // A character the digest has not reached yet. `None` lets the caller say
        assert_eq!(choose_line(None, 1), None);
        assert_eq!(choose_line(Some(&vec![]), 1), None);
    }

    #[test]
    fn one_seed_always_picks_the_same_line() {
        let l = lines(&[&long("a"), &long("b"), &long("c"), &long("d")]);
        let a = choose_line(Some(&l), 42).unwrap();
        let b = choose_line(Some(&l), 42).unwrap();
        assert_eq!(a, b, "the same seed must be reproducible");

        // ...and across seeds the pick actually moves, or "random" is a lie.
        let distinct: std::collections::HashSet<String> =
            (0..64).filter_map(|s| choose_line(Some(&l), s)).collect();
        assert!(
            distinct.len() > 1,
            "every seed picked the same line: {distinct:?}"
        );
    }

    #[test]
    fn a_held_line_is_reused_for_the_same_character_and_replaced_for_another() {
        // The whole point of holding it: play the current voice, then the
        let mut idx = HashMap::new();
        idx.insert("Kiên".to_string(), lines(&[&long("k1"), &long("k2")]));
        idx.insert("Vũ".to_string(), lines(&[&long("v1"), &long("v2")]));

        let held = line_for(None, "Kiên", Some(&idx), 3).unwrap();
        assert_eq!(held.character, "Kiên");
        assert!(
            held.text.starts_with("k1") || held.text.starts_with("k2"),
            "{held:?}"
        );

        // Same character, different seed: the held line still wins, because
        let again = line_for(Some(&held), "Kiên", Some(&idx), 999).unwrap();
        assert_eq!(again, held, "a held line must survive a different seed");

        // Different character: the held line is not theirs, so it is replaced.
        let other = line_for(Some(&held), "Vũ", Some(&idx), 3).unwrap();
        assert_eq!(other.character, "Vũ");
        assert!(other.text.starts_with('v'), "Vũ got Kiên's line: {other:?}");
    }

    #[test]
    fn a_character_with_no_lines_yields_none_rather_than_silence() {
        let mut idx = HashMap::new();
        idx.insert("Kiên".to_string(), lines(&[&long("k1")]));
        assert_eq!(line_for(None, "Người Mới", Some(&idx), 1), None);
        // No index at all (still loading) is the same answer, not a panic.
        assert_eq!(line_for(None, "Kiên", None, 1), None);
    }

    #[test]
    fn the_cache_answers_only_the_pair_it_was_filled_for() {
        let mut c = AuditionCache::default();
        c.request("Adam", "một câu thử");
        assert!(
            c.fresh("Adam", "một câu thử").is_none(),
            "a render still in flight is not an answer"
        );
        c.fill("Adam", "wav".into());
        assert_eq!(c.fresh("Adam", "một câu thử"), Some("wav"));
        // Another voice, another sentence: neither is this slot's pair.
        assert!(c.fresh("Bắc Kỳ", "một câu thử").is_none());
        assert!(c.fresh("Adam", "câu khác").is_none());

        // A late answer for a voice the slot never asked for is dropped whole.
        let mut late = AuditionCache::default();
        late.request("Adam", "một câu thử");
        late.fill("Bắc Kỳ", "wrong".into());
        assert!(late.fresh("Adam", "một câu thử").is_none());
    }

    #[test]
    fn the_cache_forgets_after_a_minute() {
        let mut c = AuditionCache::default();
        c.request("Adam", "một câu thử");
        c.fill("Adam", "wav".into());
        c.at = Some(
            std::time::Instant::now()
                .checked_sub(AUDITION_TTL + std::time::Duration::from_secs(1))
                .expect("a monotonic clock reaches back a minute"),
        );
        assert!(
            c.fresh("Adam", "một câu thử").is_none(),
            "an expired slot renders again"
        );
    }

    #[test]
    fn pick_index_stays_in_range_including_for_zero() {
        assert_eq!(
            pick_index(0, 12345),
            0,
            "an empty list must not divide by zero"
        );
        for len in 1..40usize {
            for seed in 0..40u64 {
                assert!(pick_index(len, seed) < len, "len {len} seed {seed}");
            }
        }
    }

    #[test]
    fn the_index_is_built_from_scripts_and_skips_what_it_cannot_read() {
        let root = std::env::temp_dir().join(format!("bmlines{}", std::process::id()));
        let layout = bm_core::Layout::new(&root);
        layout.ensure().unwrap();
        // A readable script, a corrupt one, and a file in the script folder
        std::fs::write(
            layout.script(1),
            r#"{"segments":[
                 {"speaker":"Kiên","text":"một"},
                 {"speaker":"Kiên","text":"một"},
                 {"speaker":"Vũ","text":"hai"},
                 {"speaker":"","text":"ignored"}]}"#,
        )
        .unwrap();
        std::fs::write(layout.script(2), "{ this is not json").unwrap();
        std::fs::write(layout.script_dir().join("notes.json"), r#"{"segments":[]}"#).unwrap();

        let idx = index_lines(&layout).unwrap();
        // Deduplicated, and the corrupt script did not take the rest down.
        let kien = idx.get("Kiên").expect("Kiên is indexed");
        assert_eq!(kien, &vec!["một".to_string()], "the duplicate was dropped");
        assert_eq!(idx.get("Vũ"), Some(&vec!["hai".to_string()]));
        assert!(
            !idx.contains_key(""),
            "a nameless speaker is not a character"
        );
        assert_eq!(idx.len(), 2, "notes.json is not a script: {idx:?}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_empty_data_directory_says_what_to_do_about_it() {
        let root = std::env::temp_dir().join(format!("bmnodata{}", std::process::id()));
        std::fs::create_dir_all(root.join("data")).unwrap();
        let err = index_lines(&bm_core::Layout::new(&root)).unwrap_err();
        assert!(err.contains("translate"), "the fix must be named: {err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_real_corpus_yields_a_sentence_for_every_speaker_that_has_one() {
        // Gated on an env var so CI stays hermetic and fast — `data/` is
        let Ok(root) = std::env::var("BM_REAL_ROOT") else {
            return;
        };
        let t0 = std::time::Instant::now();
        // Through the resolver, not `new`: the corpus lives in the active
        let layout = bm_core::Layout::resolve(&root).expect("BM_REAL_ROOT must resolve");
        let index = index_lines(&layout).expect("the real corpus must index");
        println!(
            "scan took {:?} for {} files",
            t0.elapsed(),
            script_files(&layout).len()
        );
        assert!(
            index.len() > 20,
            "a 100-chapter corpus names far more than 20 speakers, got {}",
            index.len()
        );

        // The claim that matters: for a speaker with real dialogue, the chooser
        for speaker in ["Narrator", "Dịch Phong"] {
            let lines = index
                .get(speaker)
                .unwrap_or_else(|| panic!("{speaker} has no lines in the corpus"));
            let picked = choose_line(Some(lines), 1).expect("non-empty means pickable");
            assert!(
                picked.chars().count() >= MIN_LINE_CHARS,
                "{speaker} got a stub instead of a sentence: {picked:?}"
            );
            println!("{speaker} ({} lines): {picked}", lines.len());
        }
        println!("{} speakers indexed", index.len());
    }
}
