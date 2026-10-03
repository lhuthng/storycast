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
            dur_s: None,
            mode: None,
            hold: None,
            level: None,
        },
    );
    p.insert(
        "night".into(),
        Sound {
            tags: tags(&["night"]),
            files: vec!["effects/night-1.mp3".into()],
            looped: true,
            dur_s: None,
            mode: None,
            hold: None,
            level: None,
        },
    );
    p.insert(
        "rain".into(),
        Sound {
            tags: tags(&["rain", "calm"]),
            files: vec!["effects/rain-1.mp3".into()],
            looped: true,
            dur_s: None,
            mode: None,
            hold: None,
            level: None,
        },
    );
    p.insert(
        "sword-fight".into(),
        Sound {
            tags: tags(&["battle", "sword"]),
            files: vec!["effects/sword-fight-1.mp3".into()],
            looped: false,
            dur_s: None,
            mode: None,
            hold: None,
            level: None,
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
                dur_s: None,
                mode: None,
                hold: None,
                level: None,
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
            dur_s: None,
            mode: None,
            hold: None,
            level: None,
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

// -----------------------------------------------------------------------
// writing a registry
// -----------------------------------------------------------------------

/// A scratch copy of the fixture registry. Never write to the live tree
/// from a test: it is ignored and may be absent, and the writer's whole
/// promise is that it leaves its input alone.
fn shipped_copy(kind: PoolKind, tag: &str) -> (std::path::PathBuf, String) {
    let dir = std::env::temp_dir().join(format!("bm-pool-write-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    crate::profile::install_fixture(&dir).expect("fixture profile");
    let path = dir.join("assets").join(kind.registry());
    let original = std::fs::read_to_string(&path).expect("fixture registry");
    (path, original)
}

/// The property the whole writer exists for: an edit must cost the diff of
/// an edit, not the diff of a re-serialise. Every shipped registry is
/// hand-formatted — some arrays inline, some expanded, and a `_note` in
/// prose — so a round trip through the writer has to return the same bytes.
#[test]
fn saving_an_untouched_registry_rewrites_nothing() {
    for kind in PoolKind::ALL {
        let (path, original) = shipped_copy(kind, "noop");
        let pool = load_pool(&path);
        assert!(
            !pool.is_empty(),
            "{}: fixture did not load",
            kind.registry()
        );
        save_pool(&path, kind, &pool).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "{} was rewritten by a save that changed nothing",
            kind.registry()
        );
    }
}

/// Removal is surgical: the entry goes, everything around it — including
/// the notes and the neighbours' own spacing — is untouched.
#[test]
fn removing_an_entry_leaves_the_rest_byte_identical() {
    let (path, original) = shipped_copy(PoolKind::Music, "remove");
    let mut pool = load_pool(&path);
    assert!(pool.remove("market").is_some());
    save_pool(&path, PoolKind::Music, &pool).unwrap();
    let after = std::fs::read_to_string(&path).unwrap();

    // The block, exactly as it stands in the file, is gone...
    let block = "  \"market\": {\n    \"tags\": [\"market\", \"busy\"],\n    \"files\": [\"music/market-bg-1.mp3\"]\n  },\n";
    assert!(original.contains(block), "fixture moved; fix this test");
    assert!(!after.contains("\"market\""), "{after}");
    // ...and the file is the original with exactly that block cut out.
    assert_eq!(after, original.replace(block, ""), "{after}");
    // The registry still reads back as the pool we wrote.
    assert_eq!(load_pool(&path), pool);
}

/// A new entry is rendered in its layer's field order and lands in name
/// order, so the file stays diffable against the ones around it.
#[test]
fn a_new_entry_is_written_in_its_layers_field_order() {
    let (path, _) = shipped_copy(PoolKind::Inject, "add");
    let mut pool = load_pool(&path);
    pool.insert(
        "kettle".into(),
        Sound {
            tags: vec!["kettle".into(), "whistle".into()],
            files: vec!["injects/kettle-1.mp3".into()],
            looped: false,
            dur_s: Some(2.5),
            mode: Some("hit".into()),
            hold: None,
            level: Some(0.8),
        },
    );
    save_pool(&path, PoolKind::Inject, &pool).unwrap();
    let after = std::fs::read_to_string(&path).unwrap();
    // In name order, and in the file's field order: tags, files, mode,
    // level, looped, dur_s. Both lists are short enough to stay inline.
    assert!(
        after.contains(
            "  \"kettle\": {\n    \"tags\": [\"kettle\", \"whistle\"],\n    \"files\": [\"injects/kettle-1.mp3\"],\n    \"mode\": \"hit\",\n    \"level\": 0.8,\n    \"looped\": false,\n    \"dur_s\": 2.5\n  },\n"
        ),
        "{after}"
    );
    // ...and it sits between its neighbours by name, not at the end.
    let kettle = after.find("\"kettle\"").unwrap();
    assert!(after.find("\"intense\"").is_none());
    assert!(after.find("\"light-spell\"").unwrap() > kettle);
    assert!(after.find("\"fire-spell\"").unwrap() < kettle);
    assert_eq!(load_pool(&path)["kettle"].level, Some(0.8));
}

/// An edit to one number must not collapse an entry the author expanded.
#[test]
fn an_edited_entry_keeps_the_shape_it_had() {
    let (path, _) = shipped_copy(PoolKind::Inject, "shape");
    let mut pool = load_pool(&path);
    // `cooking` ships expanded, with two takes and a level of 0.8.
    pool.get_mut("cooking").unwrap().level = Some(0.35);
    save_pool(&path, PoolKind::Inject, &pool).unwrap();
    let after = std::fs::read_to_string(&path).unwrap();
    assert!(
        after.contains("  \"cooking\": {\n    \"tags\": [\n      \"cooking\",\n"),
        "an expanded entry came back inline:\n{after}"
    );
    assert!(after.contains("\"level\": 0.35"), "{after}");
    // And every other entry kept its own bytes.
    for other in ["\"boiling-water\"", "\"food-prep\"", "\"coin\""] {
        assert!(after.contains(other), "{other} went missing");
    }
}

/// A short list stays on one line when the entry is new — the same habit
/// the hand-written effect and music registries have.
#[test]
fn a_short_list_stays_inline_but_a_long_one_does_not() {
    let (path, _) = shipped_copy(PoolKind::Effect, "inline");
    let mut pool = load_pool(&path);
    let mk = |files: Vec<String>| Sound {
        tags: vec!["probe".into()],
        files,
        looped: true,
        dur_s: None,
        mode: None,
        hold: None,
        level: None,
    };
    pool.insert(
        "short-probe".into(),
        mk(vec!["effects/short-probe-1.mp3".into()]),
    );
    pool.insert(
        "long-probe".into(),
        mk((1..=6)
            .map(|n| format!("effects/long-probe-{n}.mp3"))
            .collect()),
    );
    save_pool(&path, PoolKind::Effect, &pool).unwrap();
    let after = std::fs::read_to_string(&path).unwrap();
    assert!(
        after.contains("\"files\": [\"effects/short-probe-1.mp3\"]"),
        "a one-take entry was expanded:\n{after}"
    );
    assert!(
        after.contains("\"files\": [\n      \"effects/long-probe-1.mp3\","),
        "six takes were kept on one line:\n{after}"
    );
    // A bed omits `looped`; a one-shot states it.
    assert!(
        after.contains("\"files\": [\"effects/short-probe-1.mp3\"]\n  }"),
        "{after}"
    );
    assert_eq!(load_pool(&path).len(), pool.len());
}

/// The `_note` is prose an author wrote, and it is the only written record
/// of why the pool is shaped the way it is. It must survive verbatim —
/// including its own line, not re-encoded.
#[test]
fn the_note_survives_verbatim() {
    let (path, original) = shipped_copy(PoolKind::Effect, "note");
    let mut pool = load_pool(&path);
    pool.remove("rain");
    save_pool(&path, PoolKind::Effect, &pool).unwrap();
    let after = std::fs::read_to_string(&path).unwrap();
    let note = original
        .lines()
        .find(|l| l.contains("\"_note\""))
        .expect("fixture has a note");
    assert!(after.contains(note), "the note did not survive");
    assert_eq!(
        after.lines().next().unwrap(),
        "{",
        "the note must stay the first member"
    );
}

/// A registry the scanner cannot read is refused, never replaced. Losing a
/// pool to a stray bracket would be silent and total.
#[test]
fn a_registry_this_writer_cannot_read_is_refused_not_replaced() {
    let dir = std::env::temp_dir().join("bm-pool-write-bad");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("music-pool.json");
    for bad in ["{ nope", "[]", "{\"a\": }", "{\"a\": 1", "{\"a\" 1}"] {
        std::fs::write(&path, bad).unwrap();
        let pool = load_pool(&path);
        let res = save_pool(&path, PoolKind::Music, &pool);
        assert!(res.is_err(), "{bad:?} was accepted");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            bad,
            "{bad:?} was overwritten"
        );
    }
}

/// A readable file whose members are not sounds: the `_`-prefixed ones are
/// notes and are kept, the rest *are* the pool — a key that will not parse
/// as a `Sound` is already invisible to `load_pool`, so writing drops it and
/// the file and the pool agree again. That is the one case where a save
/// removes something the operator did not name, so it is pinned here.
#[test]
fn a_member_that_is_not_a_sound_is_dropped_and_the_notes_are_kept() {
    let dir = std::env::temp_dir().join("bm-pool-write-junk");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("music-pool.json");
    std::fs::write(
        &path,
        "{\n  \"_note\": \"kept\",\n  \"broken\": 7,\n  \"market\": {\"tags\": [\"market\"], \"files\": [\"music/market-bg-1.mp3\"]}\n}\n",
    )
    .unwrap();
    let pool = load_pool(&path);
    assert_eq!(pool.len(), 1, "only `market` is a sound");
    save_pool(&path, PoolKind::Music, &pool).unwrap();
    let after = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        after,
        "{\n  \"_note\": \"kept\",\n  \"market\": {\"tags\": [\"market\"], \"files\": [\"music/market-bg-1.mp3\"]}\n}\n"
    );
}

/// A registry that does not exist yet is created rather than refused: an
/// operator adding the first sound to an empty layer is ordinary.
#[test]
fn a_missing_registry_is_created() {
    let dir = std::env::temp_dir().join("bm-pool-write-fresh");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("inject-pool.json");
    let mut pool = ClipPool::new();
    pool.insert(
        "coin".into(),
        Sound {
            tags: vec!["coin".into()],
            files: vec!["injects/coin-1.mp3".into()],
            looped: false,
            dur_s: Some(1.0),
            mode: Some("hit".into()),
            hold: None,
            level: None,
        },
    );
    save_pool(&path, PoolKind::Inject, &pool).unwrap();
    assert_eq!(load_pool(&path), pool);
}

/// The ladder: a sound's own trim is a plain multiplier, and an absent one
/// is 1.0 — so a registry written before the field existed mixes exactly as
/// it did.
#[test]
fn an_absent_level_is_one_and_a_level_rides_on_the_pick() {
    let mut p = ClipPool::new();
    let mut s = Sound {
        tags: tags(&["day"]),
        files: vec!["effects/day-1.mp3".into()],
        looped: true,
        dur_s: None,
        mode: None,
        hold: None,
        level: None,
    };
    assert_eq!(sound_level(&s), 1.0);
    p.insert("day".into(), s.clone());
    assert_eq!(pick(&p, &tags(&["day"]), 0).unwrap().level, 1.0);

    s.level = Some(0.25);
    p.insert("day".into(), s.clone());
    assert_eq!(sound_level(&s), 0.25);
    assert_eq!(pick(&p, &tags(&["day"]), 0).unwrap().level, 0.25);

    // Zero and negatives read as "unset", never as a mute: a pool cannot
    // silence a layer by arithmetic accident.
    s.level = Some(0.0);
    assert_eq!(sound_level(&s), 1.0);
    s.level = Some(-2.0);
    assert_eq!(sound_level(&s), 1.0);
}

/// Every shipped sound resolves to 1.0 today. If that ever stops being
/// true the mix has changed, and it should be a decision, not a surprise.
#[test]
fn the_shipped_registries_are_all_at_unity_today() {
    let dir = std::env::temp_dir().join("bm-pool-unity");
    let _ = std::fs::remove_dir_all(&dir);
    crate::profile::install_fixture(&dir).expect("fixture profile");
    for kind in PoolKind::ALL {
        let path = dir.join("assets").join(kind.registry());
        for (name, sound) in load_pool(&path) {
            if kind == PoolKind::Inject {
                continue; // the inject layer has always had per-sound trims
            }
            assert_eq!(
                sound_level(&sound),
                1.0,
                "{}/{name} carries a level; the effect and music layers used to ignore it",
                kind.registry()
            );
        }
    }
}

#[test]
fn the_registry_paths_are_spelled_once() {
    let l = crate::Layout::new("/repo");
    for kind in PoolKind::ALL {
        assert!(l.pool(kind).ends_with(kind.registry()));
        assert!(l.assets().join(kind.dir()).starts_with(l.assets()));
    }
    assert_eq!(PoolKind::ALL.len(), 3);
}
