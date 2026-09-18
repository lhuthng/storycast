//! Choosing a real line to audition a voice on.
//!
//! A voice *sample* answers "what does this voice sound like". A real line
//! answers "what will this character sound like", which is the question an
//! operator is actually asking when they swap a voice. The lines come from the
//! digested scripts — the same text the renderer will speak.
//!
//! The scan is a hundred file opens: measured at 26 ms warm on this repo (1.5 MB
//! of JSON across 100 scripts), which is not a reason to hide it, but it is I/O
//! on a path that could just as easily be a network mount. So it runs once per
//! session, in a background job, and the result is cached on the `App` — never on
//! the UI thread, and never twice.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A sample has to be a sentence. `data/script-01.json` really does contain
/// `{"speaker": "Dịch Phong", "text": "Ừm!"}`, and a two-character grunt says
/// nothing about how a voice carries a paragraph.
pub(crate) const MIN_LINE_CHARS: usize = 40;

/// ...and it has to end. A 400-character line is half a minute of audio, which is
/// not an audition, it is a scene. Lines in the band are preferred; anything
/// else is used only when a character has nothing in band.
pub(crate) const MAX_LINE_CHARS: usize = 160;

/// Per-character cap. 4 836 segments across 100 scripts is ~400 KB of text —
/// small, but a cap keeps one pathological script from being held forever.
const PER_CHARACTER_CAP: usize = 200;

/// Every `data/script-*.json` under `root`, sorted.
///
/// Sorted so the index is built in a stable order: `read_dir` order is arbitrary
/// and reorders on insert, which would make a "random" pick differ between two
/// runs of the same session for no reason anyone could see.
pub(crate) fn script_files(root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(root.join("data"))
        .map(|rd| {
            rd.filter_map(|e| e.ok().map(|x| x.path()))
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n.starts_with("script-") && n.ends_with(".json"))
                        .unwrap_or(false)
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// `speaker -> every line they speak`, deduplicated, in script order.
///
/// A script that will not parse is skipped rather than failing the whole index:
/// a half-digested run still has plenty of usable lines, and refusing to audition
/// anything because chapter 73 is corrupt would be the wrong trade.
pub(crate) fn index_lines(root: &Path) -> Result<HashMap<String, Vec<String>>, String> {
    let files = script_files(root);
    if files.is_empty() {
        return Err(format!(
            "no data/script-*.json under {} — run :translate first",
            root.display()
        ));
    }
    let mut index: HashMap<String, Vec<String>> = HashMap::new();
    let mut seen: HashMap<String, std::collections::HashSet<String>> = HashMap::new();
    let mut parsed = 0usize;
    for path in &files {
        let Ok(doc) = bm_core::read_json::<serde_json::Value>(path) else {
            continue;
        };
        parsed += 1;
        let Some(segments) = doc.get("segments").and_then(|s| s.as_array()) else {
            continue;
        };
        for seg in segments {
            let speaker = seg
                .get("speaker")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .trim();
            let text = seg
                .get("text")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .trim();
            if speaker.is_empty() || text.is_empty() {
                continue;
            }
            let bucket = index.entry(speaker.to_string()).or_default();
            if bucket.len() >= PER_CHARACTER_CAP {
                continue;
            }
            // The same grunt recurs in every chapter; one copy is enough, and
            // without this a character's whole bucket can be the same "Hừ."
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
            root.display()
        ));
    }
    Ok(index)
}

/// Pick one line for a character.
///
/// Prefers lines inside [`MIN_LINE_CHARS`]..=[`MAX_LINE_CHARS`]; falls back to
/// everything when a character only ever grunts. `None` means they have no lines
/// at all — a character the digest has not produced a script for yet.
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
///
/// The character is part of the value on purpose. A line cached without it would
/// silently belong to whoever was highlighted a moment ago, and moving the cursor
/// to another speaker would audition them on a sentence they never say — the
/// exact failure an audition exists to prevent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuditionLine {
    pub(crate) character: String,
    pub(crate) text: String,
}

/// The line to use for `character`: the held one when it is already theirs, else
/// a fresh pick.
///
/// `held` as `None` forces a re-pick, which is what the reroll key passes.
/// `None` back means the character has no lines yet — the caller says so rather
/// than playing silence.
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
///
/// No RNG dependency: the only requirement is that two sessions pick different
/// lines and that one seed always picks the same line, so a test can assert the
/// choice without pinning an RNG's internals. A splitmix-style mix is plenty.
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
        // the audition tells the operator nothing.
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
        // tells you the timbre, which is more than silence does.
        let l = lines(&["Ừm!", "Hừ."]);
        let picked = choose_line(Some(&l), 7).unwrap();
        assert!(l.contains(&picked), "{picked:?}");
    }

    #[test]
    fn no_lines_at_all_is_none_not_an_empty_string() {
        // A character the digest has not reached yet. `None` lets the caller say
        // so; an empty line would render silence and look like a broken voice.
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
        // candidate, and hear the *same* sentence twice.
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
        // re-picking here would break the A/B.
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
        let data = root.join("data");
        std::fs::create_dir_all(&data).unwrap();
        // A readable script, a corrupt one, and a file that is not a script.
        std::fs::write(
            data.join("script-01.json"),
            r#"{"segments":[
                 {"speaker":"Kiên","text":"một"},
                 {"speaker":"Kiên","text":"một"},
                 {"speaker":"Vũ","text":"hai"},
                 {"speaker":"","text":"ignored"}]}"#,
        )
        .unwrap();
        std::fs::write(data.join("script-02.json"), "{ this is not json").unwrap();
        std::fs::write(data.join("notes.json"), r#"{"segments":[]}"#).unwrap();

        let idx = index_lines(&root).unwrap();
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
        let err = index_lines(&root).unwrap_err();
        assert!(err.contains("translate"), "the fix must be named: {err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_real_corpus_yields_a_sentence_for_every_speaker_that_has_one() {
        // Gated on an env var so CI stays hermetic and fast — `data/` is
        // gitignored and the scan is ~3.8 s. Run it when the scan or the chooser
        // changes, because a fixture cannot tell you what the real text looks
        // like:
        //   BM_REAL_ROOT="$PWD" cargo test -p bm-inductor the_real_corpus -- --nocapture
        let Ok(root) = std::env::var("BM_REAL_ROOT") else {
            return;
        };
        let t0 = std::time::Instant::now();
        let index = index_lines(Path::new(&root)).expect("the real corpus must index");
        println!(
            "scan took {:?} for {} files",
            t0.elapsed(),
            script_files(Path::new(&root)).len()
        );
        assert!(
            index.len() > 20,
            "a 100-chapter corpus names far more than 20 speakers, got {}",
            index.len()
        );

        // The claim that matters: for a speaker with real dialogue, the chooser
        // finds a sentence. A grunt would mean the band is wrong for this text.
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
