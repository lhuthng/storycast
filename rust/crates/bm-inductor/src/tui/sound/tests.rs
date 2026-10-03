use super::*;

fn sound(files: &[&str], tags: &[&str]) -> Sound {
    Sound {
        tags: tags.iter().map(|t| t.to_string()).collect(),
        files: files.iter().map(|f| f.to_string()).collect(),
        looped: true,
        dur_s: None,
        mode: None,
        hold: None,
        level: None,
    }
}

/// A checkout with the shipped assets copied in, so `check_files` and
/// `load` see the real registries without ever writing to the repo.
///
/// A `TempDir` rather than a named directory under the system temp root:
/// twelve of these per run, and a named one is litter the next run has to
/// remember to clear. The guard comes back with the path, so the caller has
/// to hold it — dropping it deletes the tree mid-test.
fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    bm_core::profile::install_fixture(&dir).expect("fixture profile");
    // Placeholder clips: the editor probes takes for length, so every
    // listed take exists as an (empty) file without shipping audio.
    for kind in PoolKind::ALL {
        let pool = bm_core::audio_pool::load_pool(&dir.join("assets").join(kind.registry()));
        for sound in pool.values() {
            for f in &sound.files {
                let p = dir.join("assets").join(f);
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(&p, b"").unwrap();
            }
        }
    }
    (tmp, dir)
}

#[test]
fn an_entry_nothing_reaches_is_removable_and_one_the_map_reaches_is_not() {
    let (_d, dir) = fixture();
    let data = load(&bm_core::Layout::new(&dir)).unwrap();
    let effect = rows(&data, PoolKind::Effect);
    assert!(!effect.is_empty());
    // Every shipped effect sound answers a shipped rule, so none is free.
    assert!(
        effect.iter().all(|r| r.in_use()),
        "a shipped effect sound reads as unused: {:?}",
        effect
            .iter()
            .filter(|r| !r.in_use())
            .map(|r| &r.name)
            .collect::<Vec<_>>()
    );
    let wind = effect.iter().find(|r| r.name == "wind").unwrap();
    assert!(
        wind.uses
            .iter()
            .any(|u| u.tags.iter().any(|t| t == "mountain")),
        "wind is reached by the mountain tag, not by its name: {:?}",
        wind.uses
    );
    assert!(wind.verdict().0.contains("remove disabled"));

    // The inject layer is reached by scripts, and there are none here.
    let inject = rows(&data, PoolKind::Inject);
    assert!(inject.iter().all(|r| !r.in_use()), "{inject:?}");
    assert!(inject.iter().all(|r| r.verdict().0.contains("removable")));
}

/// The two layers' usage comes from different files and must not be
/// swapped: a palette value is not a scene rule.
#[test]
fn the_music_layer_reads_the_palette_not_the_rules() {
    let (_d, dir) = fixture();
    let data = load(&bm_core::Layout::new(&dir)).unwrap();
    let music = rows(&data, PoolKind::Music);
    let tavern = music.iter().find(|r| r.name == "tavern").unwrap();
    assert!(tavern.in_use());
    assert!(
        tavern.uses.iter().all(|u| u.by.starts_with("palette ")),
        "{:?}",
        tavern.uses
    );
}

/// A script that places a sound is what makes an inject unremovable, and
/// the reason names the chapter so the operator can go and look.
#[test]
fn a_script_placing_an_inject_is_what_makes_it_unremovable() {
    let (_d, dir) = fixture();
    let layout = bm_core::Layout::new(&dir);
    layout.ensure().unwrap();
    std::fs::write(
        layout.script(9),
        r#"{"segments":[{"speaker":"A","text":"x"},{"sound":"coin"},{"stop":"cooking"}]}"#,
    )
    .unwrap();
    let data = load(&layout).unwrap();
    let inject = rows(&data, PoolKind::Inject);
    let coin = inject.iter().find(|r| r.name == "coin").unwrap();
    assert!(coin.in_use());
    assert_eq!(coin.uses[0].by, "ch09");
    assert_eq!(
        coin.uses[0].label(),
        "ch09",
        "a chapter has no tags to show"
    );
    let swoosh = inject.iter().find(|r| r.name == "swoosh").unwrap();
    assert!(!swoosh.in_use());
}

/// A registry naming a clip that is not there is the failure the merge only
/// warns about; the screen says it up front.
#[test]
fn a_registry_naming_a_missing_clip_is_flagged() {
    let (_d, dir) = fixture();
    let mut pool = audio_pool::load_pool(&dir.join("assets/effect-pool.json"));
    pool.insert("ghost".into(), sound(&["effects/ghost-1.mp3"], &["ghost"]));
    audio_pool::save_pool(
        &dir.join("assets/effect-pool.json"),
        PoolKind::Effect,
        &pool,
    )
    .unwrap();
    let data = load(&bm_core::Layout::new(&dir)).unwrap();
    let effect = rows(&data, PoolKind::Effect);
    let ghost = effect.iter().find(|r| r.name == "ghost").unwrap();
    assert_eq!(ghost.missing, vec!["effects/ghost-1.mp3"]);
    assert!(effect
        .iter()
        .find(|r| r.name == "rain")
        .unwrap()
        .missing
        .is_empty());
}

#[test]
fn an_entry_round_trips_through_the_prompt_line() {
    for layer in PoolKind::ALL {
        let s = Sound {
            tags: vec!["a".into(), "b".into()],
            files: vec![format!("{}/x-1.mp3", layer.dir())],
            // The music layer has no `looped` flag at all — it always loops
            // — so a music entry is written with the layer's own default.
            // The other two carry a one-shot, which has to survive the trip.
            looped: layer == PoolKind::Music,
            dur_s: (layer == PoolKind::Inject).then_some(1.5),
            mode: (layer == PoolKind::Inject).then(|| "trail".into()),
            hold: (layer == PoolKind::Inject).then_some(2.0),
            level: Some(0.4),
        };
        let line = describe("probe", &s, layer);
        let (name, back) = parse_entry(layer, &line, Some("probe")).unwrap();
        assert_eq!(name, "probe");
        assert_eq!(back, s, "{layer:?} lost a field: {line}");
    }
}

#[test]
fn the_prompt_refuses_what_the_mix_would_read_as_something_else() {
    let e = |buf: &str| parse_entry(PoolKind::Effect, buf, None).unwrap_err();
    // A level of zero is read as "unset" by the mixer, so it is not a level.
    assert!(e("name=a files=effects/a.mp3 tags=t level=0").contains("0 reads as"));
    assert!(e("name=a files=effects/a.mp3 tags=t level=9").contains("level must be"));
    // A sound nothing reaches can never play.
    assert!(e("name=a files=effects/a.mp3 tags=").contains("tags is empty"));
    // A sound with no take can never play either.
    assert!(e("name=a files= tags=t").contains("files is empty"));
    // A field the layer does not have.
    assert!(e("name=a files=effects/a.mp3 tags=t mode=hit").contains("not a field"));
    // Not a pair at all.
    assert!(e("name=a files=effects/a.mp3 tags=t oops").contains("not key=value"));
    // A missing name is only allowed when renaming an existing entry.
    assert!(e("files=effects/a.mp3 tags=t").contains("name= is required"));
    // `_` is how a registry marks its own notes.
    assert!(e("name=_note files=effects/a.mp3 tags=t").contains("may not start"));
}

/// The name is the pool's key: a script names an inject by it, and a pick
/// is seeded from it. A rename in place would be a different sound wearing
/// the old one's references.
#[test]
fn renaming_in_place_is_refused_and_keeping_the_name_is_not() {
    let same = parse_entry(
        PoolKind::Inject,
        "name=coin files=injects/coin-1.mp3 tags=coin",
        Some("coin"),
    );
    assert!(same.is_ok(), "{same:?}");
    let err = parse_entry(
        PoolKind::Inject,
        "name=penny files=injects/coin-1.mp3 tags=coin",
        Some("coin"),
    )
    .unwrap_err();
    assert!(err.contains("cannot become"), "{err}");
    // Omitting the name entirely keeps the one being edited.
    let (name, _) = parse_entry(
        PoolKind::Music,
        "files=music/market-bg-1.mp3 tags=market",
        Some("market"),
    )
    .unwrap();
    assert_eq!(name, "market");
}

#[test]
fn the_music_layer_has_no_looped_flag_and_the_inject_layer_has_a_mode() {
    let err = parse_entry(
        PoolKind::Music,
        "name=a files=music/a.mp3 tags=t looped=false",
        None,
    )
    .unwrap_err();
    assert!(err.contains("not a field"), "{err}");
    let err = parse_entry(
        PoolKind::Inject,
        "name=a files=injects/a.mp3 tags=t mode=whatever",
        None,
    )
    .unwrap_err();
    assert!(err.contains("mode “whatever” unknown"), "{err}");
    let (_, s) = parse_entry(
        PoolKind::Inject,
        "name=a files=injects/a.mp3 tags=t mode=overlap hold=1.5",
        None,
    )
    .unwrap();
    assert_eq!(s.mode.as_deref(), Some("overlap"));
    assert_eq!(s.hold, Some(1.5));
}

/// A bed does not play at the level its entry carries — the mode takes a
/// tenth (`InjectMode::gain`), and `render_gain` is what the detail line
/// under the table says so with.
///
/// Both halves are asserted, because the second is a layout decision that is
/// invisible until someone folds the two back together: `shape` is a
/// 23-column cell and must not carry the factor.
#[test]
fn a_bed_renders_at_a_tenth_and_the_table_cell_does_not_say_so() {
    let row = |mode: Option<&str>, level: Option<f64>| SoundRow {
        name: "boil".into(),
        sound: Sound {
            looped: true,
            dur_s: Some(51.0),
            mode: mode.map(str::to_string),
            level,
            ..sound(&["injects/boil-1.mp3"], &["water"])
        },
        uses: Vec::new(),
        missing: Vec::new(),
    };
    assert_eq!(row(Some("overlap"), Some(0.8)).render_gain(), Some(0.1));
    assert_eq!(row(Some("trail"), Some(0.5)).render_gain(), Some(0.1));
    assert_eq!(row(Some("hit"), Some(0.8)).render_gain(), None);
    // No `mode` at all is the mixer's `hit` default: nothing to say.
    assert_eq!(row(None, Some(1.0)).render_gain(), None);
    // And the table cell stays exactly as wide as it was: this is 21 of its
    // 23 columns, so there is no room for a factor in it.
    assert_eq!(
        row(Some("overlap"), Some(0.8)).shape(PoolKind::Inject),
        "overlap · loops · 51s"
    );
}

#[test]
fn a_level_prompt_accepts_a_number_and_clears_on_empty() {
    assert_eq!(parse_level_prompt(" 0.35 ").unwrap(), Some(0.35));
    assert_eq!(parse_level_prompt("").unwrap(), None);
    assert_eq!(parse_level_prompt("   ").unwrap(), None);
    assert!(parse_level_prompt("0").is_err());
    assert!(parse_level_prompt("loud").is_err());
}

/// The clip has to exist and to live in the layer's own directory — a
/// registry line that points at another layer's clips is a category error
/// that would survive forever.
#[test]
fn a_clip_must_exist_and_live_in_its_own_layers_directory() {
    let (_d, dir) = fixture();
    let ok = vec!["effects/rain-1.mp3".to_string()];
    assert!(check_files(&dir, PoolKind::Effect, &ok).is_ok());

    let elsewhere = vec!["music/market-bg-1.mp3".to_string()];
    let err = check_files(&dir, PoolKind::Effect, &elsewhere).unwrap_err();
    assert!(err.contains("own directory"), "{err}");

    let absent = vec!["effects/nope-1.mp3".to_string()];
    let err = check_files(&dir, PoolKind::Effect, &absent).unwrap_err();
    assert!(err.contains("no such clip"), "{err}");

    let escape = vec!["../secrets.mp3".to_string()];
    assert!(check_files(&dir, PoolKind::Effect, &escape)
        .unwrap_err()
        .contains("relative to assets/"));
}

/// Only what an edit *adds* is checked, so an entry whose clip has gone
/// missing stays editable — it is already flagged in red, and refusing a
/// tag change because somebody moved a file is how an entry becomes
/// unfixable from the screen.
#[test]
fn an_edit_is_checked_on_what_it_introduces_not_on_what_was_already_there() {
    let (_d, dir) = fixture();
    let gone = Sound {
        tags: vec!["rain".into()],
        files: vec!["effects/gone-1.mp3".into()],
        looped: true,
        dur_s: None,
        mode: None,
        hold: None,
        level: None,
    };
    // The same (broken) take, retagged: nothing new to check.
    let retagged = Sound {
        tags: vec!["rain".into(), "storm".into()],
        ..gone.clone()
    };
    assert!(introduced(Some(&gone), &retagged).is_empty());
    assert!(check_files(&dir, PoolKind::Effect, &introduced(Some(&gone), &retagged)).is_ok());

    // A take the edit adds is checked, and a missing one is refused.
    let replaced = Sound {
        files: vec!["effects/gone-1.mp3".into(), "effects/absent-1.mp3".into()],
        ..gone.clone()
    };
    assert_eq!(
        introduced(Some(&gone), &replaced),
        vec!["effects/absent-1.mp3"]
    );
    assert!(check_files(&dir, PoolKind::Effect, &introduced(Some(&gone), &replaced)).is_err());

    // A brand-new entry has every take introduced, so all of them are.
    assert_eq!(introduced(None, &replaced).len(), 2);
    // ...and the one that is there passes.
    let good = Sound {
        files: vec!["effects/rain-1.mp3".into()],
        ..gone.clone()
    };
    assert!(check_files(&dir, PoolKind::Effect, &introduced(None, &good)).is_ok());
}

/// The write path end to end: add, save, reload — and the guard is read off
/// the reloaded data, so the screen and the file cannot disagree.
#[test]
fn a_saved_pool_reloads_with_the_same_rows() {
    let (_d, dir) = fixture();
    let data = load(&bm_core::Layout::new(&dir)).unwrap();
    let mut pool = data.pools[&PoolKind::Inject].clone();
    let (name, s) = parse_entry(
        PoolKind::Inject,
        "name=kettle files=injects/coin-1.mp3 tags=kettle,whistle mode=hit level=0.8",
        None,
    )
    .unwrap();
    check_files(&dir, PoolKind::Inject, &s.files).unwrap();
    assert!(matches!(apply(&mut pool, &name, s), Edit::Added));
    save(&dir, PoolKind::Inject, &pool).unwrap();

    let back = load(&bm_core::Layout::new(&dir)).unwrap();
    let rows = rows(&back, PoolKind::Inject);
    assert_eq!(rows.len(), data.pools[&PoolKind::Inject].len() + 1);
    let kettle = rows.iter().find(|r| r.name == "kettle").unwrap();
    assert_eq!(kettle.sound.level, Some(0.8));
    assert_eq!(kettle.takes(), 1);
    assert!(!kettle.in_use(), "nothing places it yet");
}

/// `dur_s` is a fact about the clip, so it is probed, not typed. The fixture
/// clips are the real ones, so this either reads a duration or says so.
#[test]
fn the_longest_take_is_probed_rather_than_guessed() {
    let (_d, dir) = fixture();
    let pool = audio_pool::load_pool(&dir.join("assets/inject-pool.json"));
    let coin = &pool["coin"];
    match probe_longest(&dir, coin) {
        Some(d) => assert!(d > 0.0 && d < 10.0, "coin probes as {d}s"),
        // ffprobe absent is not a failure — it means "leave the field".
        None => eprintln!("ffprobe unavailable; dur_s would be left as it was"),
    }
    assert_eq!(
        probe_longest(&dir, &sound(&["injects/nope.mp3"], &["x"])),
        None
    );
}

#[test]
fn the_header_shows_the_layers_own_master_knob() {
    let (_d, dir) = fixture();
    let data = load(&bm_core::Layout::new(&dir)).unwrap();
    // Read from the map, not invented: whatever ships is what is shown.
    assert_eq!(
        master_level(&data.map, PoolKind::Effect),
        data.map.layers.effect.trim
    );
    assert_eq!(
        master_level(&data.map, PoolKind::Music),
        data.map.layers.music.level
    );
    assert_eq!(
        master_level(&data.map, PoolKind::Inject),
        data.map.layers.inject.level
    );
    assert!(PoolKind::Effect.master_knob().starts_with("layers.effect"));
}

/// Every layer's own field list is what the parser accepts — the hint, the
/// refusal message and the parser are one list, not three.
#[test]
fn the_field_list_is_the_parser_and_the_hint() {
    for layer in PoolKind::ALL {
        let hint = fields_hint(layer);
        for f in fields(layer) {
            assert!(
                hint.contains(&format!("{f}=")),
                "{layer:?}: {f} missing from the hint"
            );
        }
        assert!(!fields(layer).contains(&"mode") || layer == PoolKind::Inject);
        assert!(!fields(layer).contains(&"hold") || layer == PoolKind::Inject);
        assert!(!fields(layer).contains(&"dur_s") || layer == PoolKind::Inject);
        assert!(!fields(layer).contains(&"looped") || layer != PoolKind::Music);
    }
}

/// `summarise` is what the footer shows, so it has to be short and honest.
#[test]
fn the_use_summary_counts_what_it_does_not_show() {
    let uses: Vec<UseOf> = (0..4)
        .map(|i| UseOf {
            by: format!("rule{i}"),
            tags: vec!["t".into()],
        })
        .collect();
    assert_eq!(summarise(&uses, 2), "rule0 [t]; rule1 [t] (+2 more)");
    assert_eq!(summarise(&uses[..1], 2), "rule0 [t]");
}
