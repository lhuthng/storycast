//! Sound-design clip pools: the effect layer's beds and the music layer's tracks.
//!
//! The voice pool (`pool.rs`) answers "which of these clips may stand in for
//! this character". This is the same shape one level down: a JSON registry
//! `sound -> {tags, files, looped}`, the filename only the *suggestion* and the
//! registry the truth — so a scene asks for the tag `rain` and the pool decides
//! which rain clip answers. Two registries exist, one per layer
//! (`assets/effect-pool.json`, `assets/music-pool.json`), and the code is the
//! same for both: a layer is a pool plus a level.
//!
//! **A pool entry is a sound, and a sound has one or more files.** The clips are
//! named `<sound>-<n>.mp3` by hand — `day-1`, `day-2`, `day-3` are three takes
//! of *day*, not three sounds called "day one", "day two", "day three". The
//! number is a file index inside the family, and it carries no meaning: nothing
//! in the mix reads it. An earlier revision made each file its own entry, which
//! promoted the index into part of the identity and then needed invented tags
//! (`stinger`, `calm`, `street`) to tell the siblings apart — tags that
//! described the *files* while claiming to describe the sound. `pick` therefore
//! returns the sound's name, never a filename.
//!
//! Picks are deterministic. A merge that reruns must reproduce the same audio,
//! so the seed is derived from the chapter and the tag set rather than from the
//! clock — the opposite of the audition picker, where a new sample on every
//! press is the point.
//!
//! Both registries sit in `assets/`, not at the repo root beside the voice
//! pool, and that placement is load-bearing: `assets/` is what provisioning
//! ships to a worker, so a clip and its registry travel together and a worker
//! merging a chapter can resolve the same tags the inductor would.

use std::collections::BTreeMap;
use std::path::Path;

/// Filename tags are shared with the voice pool: one parser, three pools.
pub use crate::pool::parse_sample_tags;

/// One pooled sound: every file that answers for it, and how they play.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Sound {
    /// What a scene or a palette entry matches on.
    #[serde(default)]
    pub tags: Vec<String>,
    /// The takes, in the registry's order — `day-1`, `day-2`, `day-3`. The
    /// order is the tie-break, so a reordered list is a different mix.
    #[serde(default)]
    pub files: Vec<String>,
    /// A loop is stretched to fill its window; a one-shot plays once and its
    /// window falls silent after it (a sword clash). Per sound, because every
    /// take of a bed is a bed.
    #[serde(default = "loops")]
    pub looped: bool,
}

fn loops() -> bool {
    true
}

/// `sound -> sound`. Ordered so the file on disk diffs cleanly — and so a pick
/// over a tie is reproducible without a second sort.
pub type ClipPool = BTreeMap<String, Sound>;

/// A resolved pick: which sound answered, and which of its takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picked {
    /// The sound's name — `day`. This is what the log reports, because it is
    /// what the scene map asked for and the only part of the answer that means
    /// anything to a reader.
    pub sound: String,
    /// The take, relative to `assets/` (`effects/day-2.mp3`) — the same
    /// convention the registry uses, resolved by the caller against the
    /// directory the registry itself came from. Implementation detail: chosen so
    /// a chapter's audio is reproducible, not so it can be referred to by name.
    pub file: String,
    pub looped: bool,
}

/// Read a registry. A missing or broken file is "no pool", not an error: a
/// bookkeeping file must never fail a render, and an empty pool already means
/// "this layer is silent here".
///
/// An entry with no `files` cannot answer and is kept out of the pick, so a
/// registry left in the old one-file-per-entry shape resolves to silence rather
/// than to a guess. `every_shipped_*` in `ambience` is what catches that, by
/// asserting every shipped rule and palette value still finds a clip.
pub fn load_pool(path: &Path) -> ClipPool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return ClipPool::new();
    };
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(&text) else {
        return ClipPool::new();
    };
    let Some(obj) = doc.as_object() else {
        return ClipPool::new();
    };
    obj.iter()
        .filter(|(k, _)| !k.starts_with('_'))
        .filter_map(|(k, v)| {
            serde_json::from_value::<Sound>(v.clone())
                .ok()
                .map(|s| (k.clone(), s))
        })
        .collect()
}

/// Best-overlap pick among the sounds matching `tags`, then one take of it.
///
/// Scoring rather than set intersection: a scene tagged `[night, calm]` should
/// prefer a sound tagged `[night, calm]` over one tagged `[night, dark]`, and
/// `[night]` alone should still find either. A tie is broken by `seed`, so the
/// choice varies between chapters without varying between two runs of the same
/// chapter. The same seed then picks the take, so which of `day-1/2/3` plays is
/// reproducible too.
///
/// `None` means "no suitable track", which the caller must read as *the layer
/// is absent here* — never as "use a silent file".
pub fn pick(pool: &ClipPool, tags: &[String], seed: u64) -> Option<Picked> {
    if tags.is_empty() {
        return None;
    }
    let mut best = 0usize;
    let mut cands: Vec<(&str, &Sound)> = Vec::new();
    for (name, sound) in pool {
        if sound.files.is_empty() {
            continue;
        }
        let score = sound.tags.iter().filter(|t| tags.contains(t)).count();
        // Only the maximum-overlap set is a candidate. Both halves of this guard
        // are load-bearing. `score < best` is the one that was missing: without
        // it a sound sharing *one* tag still lands in `cands` whenever it sorts
        // after the winner, so a 2-of-2 match can lose a seed roll to a 1-of-2
        // one — the exact property this function exists to provide. `score == 0`
        // is the same guard at the bottom of the range, where `best` is still 0
        // and `score < best` cannot see it.
        if score == 0 || score < best {
            continue;
        }
        if score > best {
            best = score;
            cands.clear();
        }
        cands.push((name.as_str(), sound));
    }
    // The guard has to come *before* the index, not inside it: `seed % 0` panics,
    // and an empty `cands` is the ordinary case of a scene naming tags no sound
    // answers. `cands.get(..)?` looks like it handles this and does not, because
    // the modulo is evaluated to build the argument.
    if cands.is_empty() {
        return None;
    }
    let (sound, entry) = cands[(seed % cands.len() as u64) as usize];
    // A second, decorrelated roll picks the take: two chapters that land on the
    // same sound should not also land on the same file, or the extra takes would
    // never play.
    let roll = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 33;
    let file = entry.files[(roll % entry.files.len() as u64) as usize].clone();
    Some(Picked {
        sound: sound.to_string(),
        file,
        looped: entry.looped,
    })
}

/// Seed for one pick: FNV-1a over the chapter and the tag set.
///
/// Deliberately not over the clock, and deliberately over the *tags* rather
/// than the span index for music: two consecutive spans that ask for the same
/// mood then resolve to the same track, so a scene change inside one mood is
/// continuous music instead of a crossfade into the same tune. Effects seed
/// with the span index as well (see the caller) so a chapter's two night scenes
/// do not both get the same night sound.
pub fn seed(chapter: u32, salt: usize, tags: &[String]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |s: &str| {
        for b in s.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    };
    eat(&chapter.to_string());
    eat(&salt.to_string());
    for t in tags {
        eat(t);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn tags(t: &[&str]) -> Vec<String> {
        t.iter().map(|s| s.to_string()).collect()
    }

    /// Two sounds, one of them with three takes — the shape the real registries
    /// have, where `day-1/2/3` is one sound.
    fn pool() -> ClipPool {
        let mut p = ClipPool::new();
        p.insert(
            "day".into(),
            Sound {
                tags: tags(&["day", "calm"]),
                files: vec![
                    "effects/day-1.mp3".into(),
                    "effects/day-2.mp3".into(),
                    "effects/day-3.mp3".into(),
                ],
                looped: true,
            },
        );
        p.insert(
            "night".into(),
            Sound {
                tags: tags(&["night"]),
                files: vec!["effects/night-1.mp3".into()],
                looped: true,
            },
        );
        p.insert(
            "rain".into(),
            Sound {
                tags: tags(&["rain", "calm"]),
                files: vec!["effects/rain-1.mp3".into()],
                looped: true,
            },
        );
        p.insert(
            "sword-fight".into(),
            Sound {
                tags: tags(&["battle", "sword"]),
                files: vec!["effects/sword-fight-1.mp3".into()],
                looped: false,
            },
        );
        p
    }

    /// The whole point of the shape: a scene asks for a *sound*, and the number
    /// on the file never becomes part of the answer.
    #[test]
    fn a_pick_names_the_sound_never_a_numbered_file() {
        let p = pool();
        let t = tags(&["day"]);
        for seed in 0..16 {
            let got = pick(&p, &t, seed).unwrap();
            assert_eq!(
                got.sound, "day",
                "seed {seed}: the sound is the family name"
            );
            assert!(
                got.file.starts_with("effects/day-"),
                "seed {seed}: the file is one of the family's, got {}",
                got.file
            );
        }
    }

    /// Every take in a family has to be reachable, or the extra ones are dead
    /// weight nobody notices. This is what a one-file-per-entry registry could
    /// not express and what made the numbering look like identity.
    #[test]
    fn every_take_of_a_sound_is_reachable() {
        let p = pool();
        let t = tags(&["day"]);
        let files: BTreeSet<String> = (0..64).map(|s| pick(&p, &t, s).unwrap().file).collect();
        assert_eq!(files.len(), 3, "all three takes must play: {files:?}");
    }

    #[test]
    fn the_best_overlap_wins_over_mere_intersection() {
        let p = pool();
        // Two shared tags beat one, whatever the seed.
        assert_eq!(pick(&p, &tags(&["day", "calm"]), 0).unwrap().sound, "day");
        assert_eq!(pick(&p, &tags(&["day", "calm"]), 9).unwrap().sound, "day");
        // `[night, dark]` shares only `night`, so it still finds the night sound
        // rather than nothing.
        assert_eq!(
            pick(&p, &tags(&["night", "dark"]), 7).unwrap().sound,
            "night"
        );
    }

    #[test]
    fn a_weaker_overlap_never_reaches_the_candidate_set() {
        // The winner must be decided by overlap, never by where its name sorts.
        // `zz-weak` sorts *after* `mm-strong`, so a candidate set that only
        // cleared on a strict improvement would offer both and let the seed roll
        // the loser.
        let mut p = ClipPool::new();
        for (name, t) in [
            ("aa-weak", &["night"][..]),
            ("mm-strong", &["night", "calm"][..]),
            ("zz-weak", &["night"][..]),
        ] {
            p.insert(
                name.into(),
                Sound {
                    tags: tags(t),
                    files: vec![format!("effects/{name}.mp3")],
                    looped: true,
                },
            );
        }
        for seed in 0..32 {
            assert_eq!(
                pick(&p, &tags(&["night", "calm"]), seed).unwrap().sound,
                "mm-strong",
                "seed {seed}: a one-tag sound must never win against a two-tag one"
            );
        }
    }

    #[test]
    fn a_sound_with_no_files_is_not_a_candidate() {
        // A registry left in the old one-file-per-entry shape resolves to
        // silence, not to a guess. Pinned because the failure is quiet.
        let mut p = ClipPool::new();
        p.insert(
            "day".into(),
            Sound {
                tags: tags(&["day"]),
                files: vec![],
                looped: true,
            },
        );
        assert!(pick(&p, &tags(&["day"]), 0).is_none());
    }

    /// Every one of these used to *panic*, not return `None`: the index was
    /// built as `seed % cands.len()`, and `% 0` traps before the `?` can see an
    /// empty vector. A scene naming tags nothing answers is ordinary, so this is
    /// the difference between a silent stretch and a dead merge.
    #[test]
    fn no_suitable_track_is_none_not_a_silent_file() {
        let p = pool();
        assert!(pick(&p, &tags(&["market"]), 0).is_none());
        assert!(pick(&p, &tags(&[]), 0).is_none(), "no tags, no opinion");
        assert!(pick(&ClipPool::new(), &tags(&["rain"]), 0).is_none());
    }

    #[test]
    fn a_pick_is_stable_for_a_chapter_and_moves_between_them() {
        let p = pool();
        let t = tags(&["day"]);
        let a = pick(&p, &t, seed(1, 0, &t)).unwrap();
        let again = pick(&p, &t, seed(1, 0, &t)).unwrap();
        assert_eq!(a, again, "same chapter, same tags, same take");

        let over: Vec<String> = (1..=8)
            .map(|c| pick(&p, &t, seed(c, 0, &t)).unwrap().file)
            .collect();
        assert!(
            over.iter().any(|n| *n != over[0]),
            "eight chapters must not all land on one take: {over:?}"
        );
    }

    #[test]
    fn one_shots_are_marked_by_the_registry_not_the_filename() {
        let p = pool();
        assert!(!p["sword-fight"].looped);
        assert!(p["day"].looped, "a bed loops by default");
        // The flag rides along on the pick, so the caller never has to look the
        // sound up a second time to learn how it plays.
        assert!(!pick(&p, &tags(&["battle", "sword"]), 0).unwrap().looped);
    }

    #[test]
    fn a_missing_or_broken_registry_is_an_empty_pool() {
        assert!(load_pool(Path::new("/nonexistent/effect-pool.json")).is_empty());
        let d = std::env::temp_dir().join("bm-clip-pool-broken");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("effect-pool.json"), "{ nope").unwrap();
        assert!(load_pool(&d.join("effect-pool.json")).is_empty());
        std::fs::write(d.join("effect-pool.json"), "[]").unwrap();
        assert!(load_pool(&d.join("effect-pool.json")).is_empty());
    }

    #[test]
    fn the_note_key_is_not_a_sound() {
        let d = std::env::temp_dir().join("bm-clip-pool-note");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("p.json"),
            r#"{"_note":"x","rain":{"tags":["rain"],"files":["effects/rain-1.mp3"]}}"#,
        )
        .unwrap();
        let p = load_pool(&d.join("p.json"));
        assert_eq!(p.len(), 1, "{p:?}");
        assert_eq!(p["rain"].files, vec!["effects/rain-1.mp3"]);
        assert!(p["rain"].looped, "looped defaults on");
    }

    #[test]
    fn filename_tags_come_from_the_one_shared_parser() {
        // The three pools must never disagree about what a filename means.
        assert_eq!(parse_sample_tags("night-1"), vec!["night"]);
        assert_eq!(parse_sample_tags("young-female-1"), vec!["young", "female"]);
    }
}
