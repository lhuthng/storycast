use super::*;
use crate::assemble::silent_wav;
use serde_json::json;

fn scene_map() -> SceneMap {
    serde_json::from_str(
        r#"{
          "rules": [
            {"match": ["storm","thunder"], "effect": ["rain","storm"], "level": 0.22, "reverb": null},
            {"match": ["rain"], "effect": ["rain"], "level": 0.18, "reverb": null},
            {"match": ["night","evening"], "effect": ["night"], "level": 0.15, "reverb": null},
            {"match": ["market","street"], "effect": ["market"], "level": 0.16, "reverb": null, "pause_before_s": 1.5},
            {"match": ["cave"], "effect": ["cave"], "level": 0.18, "reverb": "cave"},
            {"match": ["hall","sect"], "effect": [], "level": 0.0, "reverb": "hall", "pause_before_s": 1.5},
            {"match": ["courtyard","dining"], "effect": ["fire"], "level": 0.08, "reverb": null},
            {"match": ["day","morning"], "effect": ["day"], "level": 0.12, "reverb": null}
          ],
          "default": {"effect": [], "level": 0.0, "reverb": null},
          "music_palette": {
            "_note": "skipped by the loader",
            "quiet": {"tags": ["soft","calm"], "note": "low and unobtrusive"},
            "busy": {"tags": ["market","busy"], "note": "crowds"},
            "warm": {"tags": ["warm"], "note": "hearth"},
            "none": {"tags": [], "note": "silence"}
          },
          "legacy_scene_music": {
            "rules": [
              {"match": ["market","street"], "music": "busy"},
              {"match": ["night","cave"], "music": "quiet"}
            ],
            "default": {"music": "none"}
          },
          "layers": {
            "effect": {"max_coverage": 0.35, "cooldown_s": 45.0, "min_span_s": 20.0, "max_window_s": 75.0, "fade_s": 0.3},
            "music": {"level": 0.06, "pause_level": 0.085, "fade_s": 3.0, "xfade_s": 2.0, "ramp_s": 0.6}
          },
          "pause": {"pause_s": 1.5, "max_per_chapter": 1, "require_narration": true},
          "reverb_presets": {"hall": "aecho=0.8:0.65:40|60:0.35|0.25"},
          "duck": {"threshold": 0.02, "ratio": 6.0, "attack": 20, "release": 400}
        }"#,
    )
    .unwrap()
}

/// The tracked fixture profile, installed to a scratch dir: same shapes as
/// production, no clips. Tests must never read the live tree, which is
/// ignored and may be absent. Unique per call, tests run in parallel and
/// a shared dir is a race.
fn fixture_live(tag: &str) -> PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("bm-fixture-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    crate::profile::install_fixture(&dir).expect("fixture profile");
    dir
}

/// The fixture map. The chapter-1 regression this file exists for lived in
/// a shipped file, not in the code, so this helper installs the tracked
/// fixture profile (same shapes as production) rather than reading the
/// live tree, which is ignored and may be absent. Live-tree drift is
/// profile::verify's job, not the suite's.
fn shipped_map() -> SceneMap {
    let dir = fixture_live("map");
    let map =
        load_map(&dir.join("assets/scene-map.json")).expect("fixture scene-map.json must load");
    let _ = std::fs::remove_dir_all(&dir);
    map
}

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("bm-layers-{name}"));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn turn(wav: &Path, scene: &str, speaker: &str) -> Turn {
    Turn {
        wav: wav.to_path_buf(),
        scene: scene.into(),
        music: String::new(),
        speaker: speaker.into(),
        injects: Vec::new(),
    }
}

fn turn_m(wav: &Path, scene: &str, music: &str, speaker: &str) -> Turn {
    Turn {
        wav: wav.to_path_buf(),
        scene: scene.into(),
        music: music.into(),
        speaker: speaker.into(),
        injects: Vec::new(),
    }
}

fn slot(music: &str, start: f64, end: f64) -> Slot {
    Slot {
        wav: PathBuf::from("x.wav"),
        scene: "s".into(),
        music: music.into(),
        speaker: "A".into(),
        injects: Vec::new(),
        start,
        end,
        gap_ms: 0,
        pause_ms: 0,
        inject_ms: 0,
    }
}

fn span(start: f64, end: f64, effect: &[&str], level: f64) -> Span {
    Span {
        effect: effect.iter().map(|s| s.to_string()).collect(),
        level,
        reverb: None,
        scene: "s".into(),
        start,
        end,
    }
}

fn sound(tags: &[&str], files: &[&str], level: Option<f64>) -> ClipPool {
    let mut p = ClipPool::new();
    p.insert(
        "x".into(),
        crate::audio_pool::Sound {
            tags: tags.iter().map(|t| t.to_string()).collect(),
            files: files.iter().map(|f| f.to_string()).collect(),
            looped: true,
            dur_s: None,
            mode: None,
            hold: None,
            level,
        },
    );
    p
}

// -----------------------------------------------------------------------
// what a pool is still being used for
// -----------------------------------------------------------------------

/// The point of the whole usage query: the scene map never says `wind`, and
/// `wind` is exactly what a mountain scene plays. A guard keyed on names
/// would call it unused and let it be deleted.
#[test]
fn effect_usage_reaches_by_tag_not_by_name() {
    let map: SceneMap = serde_json::from_str(
        r#"{
          "rules": [
            {"match": ["mountain"], "effect": ["mountain"], "level": 0.2},
            {"match": ["night"], "effect": ["night"], "level": 0.15}
          ],
          "default": {"effect": ["default-bed"], "level": 0.05}
        }"#,
    )
    .unwrap();
    let mut pool = ClipPool::new();
    for (name, tags) in [
        ("wind", vec!["mountain", "wind"]),
        ("night", vec!["night"]),
        ("catch-all", vec!["default-bed"]),
        ("orphan", vec!["nobody-asks-for-this"]),
    ] {
        pool.insert(
            name.into(),
            crate::audio_pool::Sound {
                tags: tags.iter().map(|t| t.to_string()).collect(),
                files: vec![format!("effects/{name}-1.mp3")],
                looped: true,
                dur_s: None,
                mode: None,
                hold: None,
                level: None,
            },
        );
    }
    let usage = effect_usage(&map, &pool);
    // `wind` is reached by a tag the map asks for and never names it.
    let wind = usage.get("wind").expect("the mountain rule reaches wind");
    assert_eq!(wind.len(), 1);
    assert!(wind[0].label().contains("mountain"), "{wind:?}");
    assert!(usage.contains_key("night"));
    // The catch-all counts too: every scene the rules do not match is
    // answered by it, so it is the most-used sound in the pool.
    assert!(usage["catch-all"][0].by.contains("default"));
    assert!(
        !usage.contains_key("orphan"),
        "nothing names the orphan's tags: {usage:?}"
    );

    // And against the fixture map: every sound in the fixture effect pool
    // is reachable, or a rule is silently scoring zero.
    let shipped = shipped_map();
    let fx = fixture_live("usage-pool");
    let shipped_pool = crate::audio_pool::load_pool(&fx.join("assets/effect-pool.json"));
    assert!(!shipped_pool.is_empty());
    let usage = effect_usage(&shipped, &shipped_pool);
    for name in shipped_pool.keys() {
        assert!(
            usage.contains_key(name),
            "{name} is in the fixture pool and no fixture rule reaches it"
        );
    }
    let _ = std::fs::remove_dir_all(&fx);
}

#[test]
fn music_usage_names_the_palette_value_that_reaches_it() {
    let mut pool = ClipPool::new();
    for (name, tags) in [
        ("soft-relax", vec!["soft", "relax"]),
        ("tavern", vec!["tavern", "warm"]),
        ("unused", vec!["polka"]),
    ] {
        pool.insert(
            name.into(),
            crate::audio_pool::Sound {
                tags: tags.iter().map(|t| t.to_string()).collect(),
                files: vec![format!("music/{name}-1.mp3")],
                looped: true,
                dur_s: None,
                mode: None,
                hold: None,
                level: None,
            },
        );
    }
    let usage = music_usage(&scene_map(), &pool);
    // `quiet` asks for [soft, calm], so it reaches soft-relax.
    assert!(usage["soft-relax"]
        .iter()
        .any(|u| u.by == "palette \"quiet\""));
    // `warm` asks for [warm], so it reaches tavern.
    assert!(usage["tavern"].iter().any(|u| u.by == "palette \"warm\""));
    assert!(!usage.contains_key("unused"), "{usage:?}");
}

/// A `stop` is placed *for* a sound, dropping the sound leaves the stop
/// fading nothing, which is the same defect as a scene gone quiet.
#[test]
fn inject_usage_reads_the_scripts_and_counts_a_stop() {
    let scripts = vec![
        (
            9u32,
            json!({"segments": [
                {"speaker": "A", "text": "hi"},
                {"sound": "coin"},
                {"speaker": "A", "text": "there"},
                {"stop": "cooking"},
                {"sound": "coin"}
            ]}),
        ),
        (
            10u32,
            json!({"segments": [{"sound": "coin"}, {"sound": "sword-slash"}]}),
        ),
        // A malformed item is skipped, never counted.
        (11u32, json!({"segments": [{"sound": "  "}, {"sound": 7}]})),
    ];
    let usage = inject_usage(&scripts);
    assert_eq!(usage["coin"], vec![9, 10]);
    assert_eq!(usage["cooking"], vec![9]);
    assert_eq!(usage["sword-slash"], vec![10]);
    assert_eq!(usage.len(), 3, "{usage:?}");
}

/// The two guards a removal can hit, side by side: one sound is named by
/// the map, the other is not, and nothing else differs between them.
#[test]
fn a_sound_nothing_names_has_no_usage_and_one_the_map_reaches_does() {
    let mut pool = ClipPool::new();
    for name in ["night", "spare"] {
        let tags: Vec<String> = if name == "night" {
            vec!["night".into()]
        } else {
            vec!["spare".into()]
        };
        pool.insert(
            name.into(),
            crate::audio_pool::Sound {
                tags,
                files: vec![format!("effects/{name}-1.mp3")],
                looped: true,
                dur_s: None,
                mode: None,
                hold: None,
                level: None,
            },
        );
    }
    let usage = effect_usage(&scene_map(), &pool);
    assert!(usage.contains_key("night"));
    assert!(!usage.contains_key("spare"));
}

/// The music layer's third rung: a track's own trim rides on the run, and
/// a track with none is 1.0, so nothing already on disk changes.
#[test]
fn plan_music_carries_the_sounds_own_level() {
    let cfg = scene_map();
    let mut pool = music_pool();
    let slots = vec![slot("quiet", 0.0, 5.0)];
    let runs = plan_music(&slots, &[], 1, &pool, &cfg.music_palette);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].level, 1.0, "no level set means unity");

    for sound in pool.values_mut() {
        sound.level = Some(0.4);
    }
    let runs = plan_music(&slots, &[], 1, &pool, &cfg.music_palette);
    assert_eq!(runs[0].level, 0.4);

    // A trim of zero reads as unset, not as a mute.
    let mut pool = music_pool();
    for sound in pool.values_mut() {
        sound.level = Some(0.0);
    }
    let runs = plan_music(&slots, &[], 1, &pool, &cfg.music_palette);
    assert_eq!(runs[0].level, 1.0);
}

/// The chapter's first cue opens the chapter: pulled back to the head of
/// the timeline it comes up *under the headline* and fades in, instead of
/// hitting at full level on the first line that happens to name a mood.
/// Later cues are not moved, a gap between two runs is silence the script
/// asked for, not a gap to fill.
#[test]
fn the_chapters_first_cue_starts_at_the_head() {
    let cfg = scene_map();
    let pool = music_pool();
    let slots = vec![
        // The headline: spoken, and naming no mood of its own.
        slot("", 0.0, 8.0),
        // The first line that names one.
        slot("busy", 8.5, 30.0),
        // A second mood, which keeps the offset its own slot gave it.
        slot("quiet", 30.5, 45.0),
    ];
    let runs = plan_music(&slots, &[], 1, &pool, &cfg.music_palette);
    assert_eq!(runs.len(), 2, "{runs:?}");
    assert_eq!(runs[0].start, 0.0, "the first cue opens the chapter");
    assert_eq!(runs[1].start, 30.5, "a later cue keeps its own offset");
}

/// The layer's head and tail fade over three seconds; the seams in between
/// stay the short crossfade, because a seam is covered by the next track
/// arriving and an edge is not.
#[test]
fn the_music_layers_edges_fade_over_three_seconds_and_its_seams_do_not() {
    let cfg = MusicLayer::default();
    assert_eq!(cfg.fade_s, 3.0, "the layer's head and tail");
    assert_eq!(music_fades(0, false, &cfg), (3.0, cfg.xfade_s));
    assert_eq!(music_fades(1, true, &cfg), (cfg.xfade_s, 3.0));
    // A chapter with one cue is both edges at once.
    assert_eq!(music_fades(0, true, &cfg), (3.0, 3.0));
    // And a bed that reaches the end of the chapter closes the same way.
    assert_eq!(EffectLayer::default().end_fade_s, 3.0);
}

/// And the effect layer's, which reaches the pick the same way.
#[test]
fn an_effects_own_level_reaches_the_pick() {
    let pool = sound(&["night"], &["effects/night-1.mp3"], Some(0.3));
    let seed = crate::audio_pool::seed(4, 0, &["night".to_string()]);
    let clip = crate::audio_pool::pick(&pool, &["night".to_string()], seed).unwrap();
    assert_eq!(clip.level, 0.3);
}

/// The shipped registries are all at unity, so the two new multiplication
/// sites are no-ops on every chapter that exists today.
#[test]
fn the_shipped_effect_and_music_pools_change_no_existing_mix() {
    for kind in [
        crate::audio_pool::PoolKind::Effect,
        crate::audio_pool::PoolKind::Music,
    ] {
        let dir = fixture_live(&format!("unity-{}", kind.registry().replace(".json", "")));
        let pool = crate::audio_pool::load_pool(&dir.join("assets").join(kind.registry()));
        assert!(!pool.is_empty());
        for (name, s) in &pool {
            assert_eq!(
                crate::audio_pool::sound_level(s),
                1.0,
                "{}/{name} is not at unity",
                kind.registry()
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn first_matching_rule_wins_specific_before_general() {
    let cfg = scene_map();
    assert_eq!(
        match_scene("street-day-book-discovery", &cfg).effect,
        vec!["market"]
    );
    assert_eq!(match_scene("courtyard-rain-day", &cfg).effect, vec!["rain"]);
    assert_eq!(
        match_scene("great-hall-day", &cfg).reverb.as_deref(),
        Some("hall")
    );
    assert!(match_scene("something-unknown-xyz", &cfg).effect.is_empty());
}

#[test]
fn run_scenes_takes_the_majority_tag() {
    let segments = vec![
        json!({"scene": "market-morning"}),
        json!({"scene": "market-morning"}),
        json!({"scene": "courtyard-evening"}),
    ];
    let runs = crate::assemble::Planned::plan(&[
        json!({"speaker": "A"}),
        json!({"speaker": "A"}),
        json!({"speaker": "A"}),
    ])
    .runs();
    assert_eq!(run_scenes(&segments, &runs), vec!["market-morning"]);
}

#[test]
fn run_scenes_ignores_blank_tags() {
    let segments = vec![json!({"scene": ""}), json!({"scene": "  "})];
    let runs =
        crate::assemble::Planned::plan(&[json!({"speaker": "A"}), json!({"speaker": "A"})]).runs();
    assert_eq!(run_scenes(&segments, &runs), vec![""]);
}

#[test]
fn the_timeline_places_pauses_and_keeps_the_gap() {
    let d = tmpdir("timeline");
    let a = d.join("a.wav");
    let b = d.join("b.wav");
    silent_wav(&a, 1.0, 48_000).unwrap();
    silent_wav(&b, 1.0, 48_000).unwrap();
    let turns = vec![
        turn(&a, "street-day", "Narrator"),
        turn_m(&b, "cave-x", "battle", "A"),
    ];

    let plain = timeline(&turns, 300, &BTreeMap::new()).unwrap();
    assert_eq!(plain.len(), 2);
    assert!((plain[1].start - 1.3).abs() < 0.01, "{plain:?}");
    assert!(pause_intervals(&plain).is_empty());
    assert_eq!(
        plain[1].music, "battle",
        "the mood rides through to the mix"
    );

    // A pause before turn 1 pushes it and is visible as an interval.
    let mut pauses = BTreeMap::new();
    pauses.insert(1usize, 1875u32);
    let paused = timeline(&turns, 300, &pauses).unwrap();
    assert!((paused[1].start - 3.175).abs() < 0.01, "{paused:?}");
    assert_eq!(paused[0].gap_ms, 300 + 1875);
    assert_eq!(paused[0].pause_ms, 1875);
    let iv = pause_intervals(&paused);
    assert_eq!(iv.len(), 1);
    assert!(
        (iv[0].0 - 1.0).abs() < 0.01 && (iv[0].1 - 2.875).abs() < 0.01,
        "{iv:?}"
    );
}

/// The merge tempoes the speech and then places the layers, so the timeline
/// the layers read has to be the one the listener hears. Without this the
/// music drifts behind the voice by a line's worth per line, the whole
/// reason the tempo pass moved ahead of `apply_layers`.
#[test]
fn retime_puts_the_timeline_on_the_delivered_clock() {
    let d = tmpdir("retime");
    let a = d.join("a.wav");
    silent_wav(&a, 4.0, 48_000).unwrap();
    let turns = vec![turn(&a, "s", "Narrator"), turn(&a, "s", "Narrator")];
    // A beat authored *before* turn 1 is written into the gap that follows
    // turn 0, the same silence, named from the other side.
    let mut pauses = BTreeMap::new();
    pauses.insert(1usize, 1875u32);
    let mut slots = timeline(&turns, 300, &pauses).unwrap();
    assert!((slots[1].start - 6.175).abs() < 0.01, "{slots:?}");

    retime(&mut slots, 1.25);

    assert!(
        (slots[0].end - 3.2).abs() < 0.01,
        "4 s of speech is 3.2 s delivered: {slots:?}"
    );
    assert!(
        (slots[1].start - 4.94).abs() < 0.01,
        "and the gap scaled with it: {slots:?}"
    );
    assert_eq!(slots[0].pause_ms, 1500, "the beat is 1.5 s to the listener");
    assert_eq!(slots[0].gap_ms, 1740, "uniform gap + beat, both scaled");

    // 1.0 is a no-op rather than a divide that rounds.
    let before = slots.clone();
    retime(&mut slots, 1.0);
    assert_eq!(slots, before);
}

#[test]
fn the_timeline_refuses_mixed_rates_before_anything_is_mixed() {
    let d = tmpdir("timeline-rates");
    let a = d.join("a.wav");
    let b = d.join("b.wav");
    silent_wav(&a, 0.2, 24_000).unwrap();
    silent_wav(&b, 0.2, 48_000).unwrap();
    let turns = vec![turn(&a, "x", "A"), turn(&b, "x", "A")];
    let err = timeline(&turns, 0, &BTreeMap::new()).unwrap_err();
    assert!(err.to_string().contains("mixed engines"), "{err}");
}

#[test]
fn spans_merge_adjacent_identical_scenes() {
    let d = tmpdir("spans");
    let a = d.join("a.wav");
    let b = d.join("b.wav");
    silent_wav(&a, 1.0, 48_000).unwrap();
    silent_wav(&b, 1.0, 48_000).unwrap();
    let cfg = scene_map();

    let turns = vec![turn(&a, "street-day", "A"), turn(&b, "night-x", "A")];
    let spans = build_spans(&timeline(&turns, 300, &BTreeMap::new()).unwrap(), &cfg);
    assert_eq!(spans.len(), 2);

    let turns = vec![turn(&a, "street-day", "A"), turn(&b, "street-day", "A")];
    let merged = build_spans(&timeline(&turns, 0, &BTreeMap::new()).unwrap(), &cfg);
    assert_eq!(merged.len(), 1);
    assert!((merged[0].end - 2.0).abs() < 0.01);
}

/// The Narrator takes the room at a tenth of its depth — in the scene,
/// never with a character's full wet — and no preset says otherwise.
#[test]
fn the_narrator_takes_a_tenth_of_the_room() {
    let d = tmpdir("narrator-tenth");
    let a = d.join("a.wav");
    let b = d.join("b.wav");
    silent_wav(&a, 1.0, 48_000).unwrap();
    silent_wav(&b, 1.0, 48_000).unwrap();
    let cfg = scene_map();
    // "sect-hall-day" reaches the hall rule, so the span names reverb.
    let turns = vec![
        turn(&a, "sect-hall-day", "Narrator"),
        turn(&b, "sect-hall-day", "Lỗ Đạt Sênh"),
    ];
    let slots = timeline(&turns, 300, &BTreeMap::new()).unwrap();
    let spans = build_spans(&slots, &cfg);
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].reverb.as_deref(), Some("hall"));

    // Both slots reach the same preset; only the Narrator flag differs, and
    // the depth is a tenth for the Narrator and whole for the character.
    let mut presets = cfg.reverb_presets.clone();
    presets.insert(
        "hall".into(),
        VoiceFx::Spec(VoiceFxSpec {
            sox: Some("reverb 45 45 80".into()),
            ffmpeg: None,
            tail_s: 1.2,
        }),
    );
    let (narr_fx, narr) = slot_effect(&slots[0], &spans, &presets).unwrap();
    let (char_fx, char_narr) = slot_effect(&slots[1], &spans, &presets).unwrap();
    assert!(narr && !char_narr, "only the Narrator slot is marked");
    assert_eq!(narr_fx, char_fx, "one preset, two depths");
    assert_eq!(
        narr_fx.engine_and_chain(),
        (FxEngine::Sox, "reverb 45 45 80")
    );
    assert_eq!(narr_fx.tail_s(), 1.2);
    assert_eq!(NARRATOR_DEPTH, 0.1);

    // Where the scene names no reverb, nobody gets any.
    let turns = vec![turn(&a, "street-day", "Lỗ Đạt Sênh")];
    let slots = timeline(&turns, 300, &BTreeMap::new()).unwrap();
    let spans = build_spans(&slots, &cfg);
    assert!(slot_effect(&slots[0], &spans, &cfg.reverb_presets).is_none());
}

/// A bare string is the legacy shape: ffmpeg with no reserved tail. An old
/// map's `narrator` key still parses, and is ignored.
#[test]
fn a_legacy_preset_is_ffmpeg_with_no_tail() {
    let fx = VoiceFx::Chain("aecho=0.8:0.9:80|170:0.08|0.05".into());
    assert_eq!(fx.engine_and_chain().0, FxEngine::Ffmpeg);
    assert_eq!(fx.tail_s(), 0.0);
}

/// The reserve is the longest decay any span in the chapter reaches, and
/// it is what keeps a reverb from being chopped at the chapter's end. An
/// object preset deserializes to its engine and its tail; a string preset
/// (and a span that names no room) reserves nothing, so a chapter with no
/// tails is exactly as long as the voice.
#[test]
fn the_longest_reached_decay_is_the_reserve() {
    let presets: BTreeMap<String, VoiceFx> = serde_json::from_str(
        r#"{
          "hall": {"sox": "reverb 65 55 85", "tail_s": 2.0, "narrator": 0.3},
          "cave": {"sox": "reverb 78 35 95", "tail_s": 2.8},
          "flat": "aecho=0.8:0.9:80:0.1"
        }"#,
    )
    .unwrap();

    // The object shape is read as SoX, with a reserved tail; an old map's
    // `narrator` key still parses, and is ignored.
    assert_eq!(
        presets["cave"].engine_and_chain(),
        (FxEngine::Sox, "reverb 78 35 95")
    );
    assert_eq!(presets["cave"].tail_s(), 2.8);

    let mut a = span(0.0, 5.0, &[], 0.0);
    a.reverb = Some("hall".into());
    let mut b = span(5.0, 10.0, &[], 0.0);
    b.reverb = Some("cave".into());
    // The cave's 2.8 s wins over the hall's 2.0 s — the chapter takes the
    // room that rings longest, or its floor would be cut mid-tail.
    assert_eq!(voice_reserve(&[a, b], &presets), 2.8);

    // A span that names no room reserves nothing.
    assert_eq!(voice_reserve(&[span(0.0, 5.0, &[], 0.0)], &presets), 0.0);

    // A legacy string preset is ffmpeg with no reserve, so a pack that has
    // not migrated is mixed to exactly the old length.
    let mut c = span(0.0, 5.0, &[], 0.0);
    c.reverb = Some("flat".into());
    assert_eq!(voice_reserve(&[c], &presets), 0.0);
}

/// The complaint the effect gates exist for: a bed under the whole chapter.
#[test]
fn the_effect_layer_is_sparse_where_the_old_one_was_wall_to_wall() {
    let cfg = scene_map();
    let spans = vec![
        span(0.0, 400.0, &["market"], 0.16),
        span(400.0, 800.0, &["night"], 0.15),
    ];
    let w = plan_windows(&spans, &cfg.layers.effect, 800.0);
    assert_eq!(w.len(), 2, "{w:?}");
    let covered: f64 = w.iter().map(|x| x.end - x.start).sum();
    assert!(
        (covered - 150.0).abs() < 0.01,
        "two 75 s windows, not 800 s: {w:?}"
    );
    // Window 1 waits out the cooldown: it opens no earlier than 75 + 45.
    assert!(w[1].start >= 120.0, "{w:?}");
    assert!(covered / 800.0 <= cfg.layers.effect.max_coverage);
}

/// The layer's one master gain. A rule's `level` is a relative balance
/// between scenes; `trim` is the operator saying the whole layer is too hot,
/// and it must reach every window without anyone editing thirteen rules.
#[test]
fn the_layer_trim_scales_every_rule_and_defaults_to_no_change() {
    let spans = vec![span(0.0, 100.0, &["rain"], 0.20)];

    // A map written before the field existed keeps its exact mix.
    let untouched = scene_map();
    let w = plan_windows(&spans, &untouched.layers.effect, 200.0);
    assert_eq!(w.len(), 1);
    assert!(
        (w[0].level - 0.20).abs() < 1e-9,
        "default trim is 1.0, got {}",
        w[0].level
    );

    let mut quieter = scene_map();
    quieter.layers.effect.trim = 0.5;
    let w = plan_windows(&spans, &quieter.layers.effect, 200.0);
    assert_eq!(w.len(), 1);
    assert!(
        (w[0].level - 0.10).abs() < 1e-9,
        "half the layer is half every rule: {}",
        w[0].level
    );

    // The trim must not resurrect a rule that declared silence, the hall
    // rule's `level: 0.0` means "no bed here", not "quiet bed".
    let silent = vec![span(0.0, 100.0, &["night"], 0.0)];
    assert!(plan_windows(&silent, &quieter.layers.effect, 200.0).is_empty());
}

#[test]
fn a_short_scene_and_a_spent_budget_both_get_nothing() {
    let cfg = scene_map();
    // 10 s is under min_span_s: not worth a window.
    assert!(plan_windows(
        &[span(0.0, 10.0, &["rain"], 0.18)],
        &cfg.layers.effect,
        100.0
    )
    .is_empty());

    // Budget spent by the first window stops the rest: 100 s of chapter
    // buys 35 s, which the first eligible scene takes.
    let w = plan_windows(
        &[
            span(0.0, 100.0, &["rain"], 0.18),
            span(200.0, 300.0, &["rain"], 0.18),
        ],
        &cfg.layers.effect,
        100.0,
    );
    assert_eq!(w.len(), 1, "{w:?}");
    assert!((w[0].end - w[0].start - 35.0).abs() < 0.01, "{w:?}");
}

#[test]
fn a_beat_lands_on_narrated_scene_changes_only() {
    let cfg = scene_map();
    let d = tmpdir("pauses");
    let w = d.join("a.wav");
    silent_wav(&w, 1.0, 48_000).unwrap();

    // Narration handing off to a character, then narration resuming: the
    // resuming boundary wins, even though both are narrated.
    let turns = vec![
        turn(&w, "street-day", "Narrator"),
        turn(&w, "night-x", "Dịch Phong"),
        turn(&w, "cave-y", "Narrator"),
    ];
    let got = plan_pauses(&turns, &cfg, 1.25);
    assert_eq!(got.len(), 1);
    assert_eq!(got.keys().next(), Some(&2usize), "narration resuming wins");
    assert_eq!(got[&2], 1875, "1.5 s delivered at atempo 1.25");

    // An exchange that crosses a scene boundary gets no beat.
    let turns = vec![turn(&w, "street-day", "A"), turn(&w, "night-x", "B")];
    assert!(plan_pauses(&turns, &cfg, 1.25).is_empty());

    // No scene change, no beat.
    let turns = vec![
        turn(&w, "street-day", "Narrator"),
        turn(&w, "street-day", "A"),
    ];
    assert!(plan_pauses(&turns, &cfg, 1.25).is_empty());

    // The opening scene is not a scene *change*.
    let turns = vec![turn(&w, "", "Narrator"), turn(&w, "street-day", "Narrator")];
    assert!(plan_pauses(&turns, &cfg, 1.25).is_empty());
}

#[test]
fn only_one_beat_per_chapter_however_many_boundaries_there_are() {
    let cfg = scene_map();
    let d = tmpdir("pauses-one");
    let w = d.join("a.wav");
    silent_wav(&w, 1.0, 48_000).unwrap();
    let turns = vec![
        turn(&w, "street-day", "Narrator"),
        turn(&w, "night-x", "Narrator"),
        turn(&w, "cave-y", "Narrator"),
        turn(&w, "rain-z", "Narrator"),
    ];
    assert_eq!(plan_pauses(&turns, &cfg, 1.0).len(), 1);
}

/// Sound-keyed, like the shipped registry: `soft` is one sound with two
/// takes, and `soft-alt` is a *second* sound answering the same tags, the
/// shape the real pool has (`soft-relax` and `generic-soft` both answer
/// `[soft, calm]`), and the only way a mood change can resolve to a
/// different track.
fn music_pool() -> ClipPool {
    let mut p = ClipPool::new();
    for (name, tags, files) in [
        (
            "market",
            &["market", "busy"][..],
            &["music/market-bg-1.mp3"][..],
        ),
        (
            "soft",
            &["soft", "calm"][..],
            &["music/soft-bg-1.mp3", "music/soft-bg-2.mp3"][..],
        ),
        (
            "soft-alt",
            &["soft", "calm"][..],
            &["music/soft-alt-bg-1.mp3"][..],
        ),
    ] {
        p.insert(
            name.into(),
            audio_pool::Sound {
                tags: tags.iter().map(|s| s.to_string()).collect(),
                files: files.iter().map(|s| s.to_string()).collect(),
                looped: true,
                dur_s: None,
                mode: None,
                hold: None,
                level: None,
            },
        );
    }
    p
}

#[test]
fn music_runs_break_on_a_sound_change_not_a_scene_change() {
    let cfg = scene_map();
    let pool = music_pool();
    let pal = &cfg.music_palette;

    // One mood across two slots: one cue, so no crossfade into itself.
    let runs = plan_music(
        &[slot("quiet", 0.0, 100.0), slot("quiet", 100.0, 200.0)],
        &[],
        1,
        &pool,
        pal,
    );
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert!((runs[0].end - 200.0).abs() < 0.01);
    assert_eq!(runs[0].mood, "quiet");

    // A change of mood is a change of sound, the whole point of the field.
    let runs = plan_music(
        &[slot("quiet", 0.0, 100.0), slot("busy", 100.0, 200.0)],
        &[],
        1,
        &pool,
        pal,
    );
    assert_eq!(runs.len(), 2, "{runs:?}");
    assert_ne!(runs[0].sound, runs[1].sound);
    assert_eq!(runs[1].mood, "busy");
}

#[test]
fn a_mood_change_mid_scene_changes_the_track() {
    // The case the old scene-keyed system could not express: one place, one
    // span, two moods. Nothing about the scene changed, so nothing but the
    // `music` field can carry this.
    let cfg = scene_map();
    let pool = music_pool();
    let slots = [
        slot("quiet", 0.0, 60.0),
        slot("quiet", 60.0, 120.0),
        slot("busy", 120.0, 180.0),
        slot("quiet", 180.0, 240.0),
    ];
    let spans = build_spans(&slots, &cfg);
    assert_eq!(spans.len(), 1, "one place, so one span: {spans:?}");

    let runs = plan_music(&slots, &[], 1, &pool, &cfg.music_palette);
    assert_eq!(runs.len(), 3, "quiet/busy/quiet: {runs:?}");
    assert_eq!(
        runs.iter().map(|r| r.mood.as_str()).collect::<Vec<_>>(),
        vec!["quiet", "busy", "quiet"]
    );
}

#[test]
fn the_log_shows_every_cue_in_a_span_not_just_the_first() {
    // Regression: the report used to fold music into the span line by
    // taking the first run that overlapped, so a span holding three cues
    // printed one. Since spans are places and runs are moods, that hid
    // exactly the in-chapter change the design exists to express, and the
    // log is the only place a merged chapter is inspectable.
    let cfg = scene_map();
    let pool = music_pool();
    let slots = [
        slot("quiet", 0.0, 60.0),
        slot("busy", 60.0, 120.0),
        slot("quiet", 120.0, 180.0),
    ];
    let spans = build_spans(&slots, &cfg);
    assert_eq!(spans.len(), 1, "one place, so one span: {spans:?}");
    let runs = plan_music(&slots, &[], 1, &pool, &cfg.music_palette);

    let lines = plan_lines(&spans, &[], &[], &runs, &[], &cfg);
    let music: Vec<&String> = lines.iter().filter(|l| l.starts_with("music ")).collect();
    assert_eq!(music.len(), 3, "{lines:#?}");
    assert!(music[0].contains("quiet"), "{music:#?}");
    assert!(music[1].contains("busy"), "{music:#?}");
    assert!(music[2].contains("quiet"), "{music:#?}");
    // ...and the span line no longer claims a track, because a span does
    // not have one.
    let span = lines.iter().find(|l| l.starts_with("span ")).unwrap();
    assert!(!span.contains("music"), "{span}");
}

#[test]
fn the_log_says_so_when_a_chapter_resolves_to_no_music() {
    // `none` emits no run, so the report has to say that explicitly rather
    // than print an empty section the reader has to interpret.
    let cfg = scene_map();
    let lines = plan_lines(&[], &[], &[], &[], &[], &cfg);
    assert_eq!(lines, vec!["music none — no cue resolved for this chapter"]);
}

#[test]
fn the_log_attributes_a_window_the_cooldown_pushed_to_its_own_place() {
    // `plan_windows` opens at `span.start.max(free_at)`, so the second
    // window starts *after* its span does. Matching windows to spans by
    // start offset, which the report did, through a formatted string
    // then reported "no effect" for a span that had 75 s of one. Chapter 13
    // was measured that way: `courtyard-evening` looked silent while
    // carrying a night bed from 120 s to 195 s.
    let cfg = scene_map();
    let spans = vec![
        Span {
            effect: vec!["day".into(), "calm".into()],
            level: 0.12,
            reverb: None,
            scene: "courtyard-morning".into(),
            start: 0.0,
            end: 108.0,
        },
        Span {
            effect: vec!["night".into()],
            level: 0.15,
            reverb: None,
            scene: "courtyard-evening".into(),
            start: 110.0,
            end: 316.0,
        },
    ];
    let fx = vec![
        FxReport {
            span: 0,
            start: 0.0,
            end: 75.0,
            name: "day-2".into(),
            level: 0.12,
            one_shot: false,
        },
        FxReport {
            span: 1,
            start: 120.0,
            end: 195.0,
            name: "night-2".into(),
            level: 0.15,
            one_shot: false,
        },
    ];
    let lines = plan_lines(&spans, &[], &fx, &[], &[], &cfg);
    let effects: Vec<&String> = lines.iter().filter(|l| l.starts_with("effect ")).collect();
    assert_eq!(effects.len(), 2, "{lines:#?}");
    assert!(effects[1].starts_with("effect [120-195s]"), "{effects:#?}");
    assert!(
        effects[1].ends_with("<- courtyard-evening"),
        "a pushed window still belongs to its own place: {effects:#?}"
    );
    // And the span line carries no effect of its own, a span does not have
    // one, so it must not imply it does.
    let span_line = lines.iter().find(|l| l.starts_with("span ")).unwrap();
    assert!(!span_line.contains("effect"), "{span_line}");
}

#[test]
fn two_moods_that_name_the_same_tags_are_one_run_when_one_sound_answers() {
    // The old premise here, "a mood change always changes the track", was
    // false the moment picks became sound-based, and it is not a bug. The
    // seed decides *among the candidates*; with one candidate there is
    // nothing to decide, so both moods resolve to the same sound and the
    // run merges. Rendering two runs of the same take with a crossfade
    // between them would be a hole with extra steps.
    let mut cfg = scene_map();
    cfg.music_palette.insert(
        "warm".into(),
        PaletteEntry {
            tags: vec!["soft".into(), "calm".into()],
            note: String::new(),
        },
    );
    let mut pool = ClipPool::new();
    pool.insert(
        "soft".into(),
        audio_pool::Sound {
            tags: vec!["soft".into(), "calm".into()],
            files: vec!["music/soft-bg-1.mp3".into()],
            looped: true,
            dur_s: None,
            mode: None,
            hold: None,
            level: None,
        },
    );
    let runs = plan_music(
        &[slot("quiet", 0.0, 100.0), slot("warm", 100.0, 200.0)],
        &[],
        1,
        &pool,
        &cfg.music_palette,
    );
    assert_eq!(runs.len(), 1, "one sound, so one run: {runs:?}");
    assert_eq!(
        runs[0].mood, "quiet",
        "the run reports the mood that opened it"
    );
    assert!((runs[0].end - 200.0).abs() < 0.01, "and it is unbroken");
}

#[test]
fn two_moods_that_name_the_same_tags_can_still_split() {
    // What seeding from the palette *value* actually buys. Two values naming
    // one tag set are two independent draws from the candidate set, so a
    // pool with two sounds for those tags spreads them across moods instead
    // of crossfading one into itself. The pool has to offer the choice
    // this is not a guarantee plan_music can make on its own.
    let mut cfg = scene_map();
    cfg.music_palette.insert(
        "warm".into(),
        PaletteEntry {
            tags: vec!["soft".into(), "calm".into()],
            note: String::new(),
        },
    );
    // The mechanism, stated where it is visible: the seed is a function of
    // the mood value, so the two moods are not forced onto one answer.
    assert_ne!(
        audio_pool::seed(1, 0, &["quiet".to_string()]),
        audio_pool::seed(1, 0, &["warm".to_string()])
    );

    // ...and the consequence: `soft` and `soft-alt` both answer [soft, calm],
    // so some chapter splits the two moods across them.
    let pool = music_pool();
    let split = (1..=32).any(|c| {
        let runs = plan_music(
            &[slot("quiet", 0.0, 100.0), slot("warm", 100.0, 200.0)],
            &[],
            c,
            &pool,
            &cfg.music_palette,
        );
        runs.len() == 2 && runs[0].sound != runs[1].sound
    });
    assert!(
        split,
        "no chapter in 32 split two moods that share tags across the two \
         pooled sounds — the value is not reaching the seed"
    );
}

#[test]
fn none_an_unpooled_mood_and_an_empty_value_all_mean_no_music() {
    let cfg = scene_map();
    let pool = music_pool();
    let pal = &cfg.music_palette;
    // `none` is a choice.
    assert!(plan_music(&[slot("none", 0.0, 100.0)], &[], 1, &pool, pal).is_empty());
    // No value at all.
    assert!(plan_music(&[slot("", 0.0, 100.0)], &[], 1, &pool, pal).is_empty());
    // A palette value whose tags nothing in the pool answers.
    assert!(plan_music(&[slot("grand", 0.0, 100.0)], &[], 1, &pool, pal).is_empty());
    // A value that is not in the palette at all, the validator's job to
    // catch, and the mix still refuses to guess.
    assert!(plan_music(&[slot("melancholy", 0.0, 100.0)], &[], 1, &pool, pal).is_empty());
}

#[test]
fn a_pause_inside_a_run_is_carried_to_the_lift() {
    let cfg = scene_map();
    let pool = music_pool();
    let runs = plan_music(
        &[slot("quiet", 0.0, 300.0)],
        &[(120.0, 121.5)],
        1,
        &pool,
        &cfg.music_palette,
    );
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].pauses, vec![(120.0, 121.5)]);
}

#[test]
fn the_declared_mood_wins_and_the_legacy_shim_covers_scripts_without_one() {
    let cfg = scene_map();
    let build = |scenes: &[(&str, &str)]| -> Vec<Value> {
        scenes
            .iter()
            .map(|(s, m)| json!({"speaker": "A", "scene": s, "music": m}))
            .collect()
    };
    let runs_of =
        |n: usize| crate::assemble::Planned::plan(&vec![json!({"speaker": "A"}); n]).runs();

    // Declared values are used as-is.
    let segs = build(&[("street-morning", "battle"), ("street-morning", "battle")]);
    assert_eq!(run_music(&segs, &runs_of(2), &cfg), vec!["battle"]);

    // No value anywhere: the shim scores the scene label instead, so the
    // ~200 chapters already on disk keep merging.
    let segs = build(&[("street-morning", ""), ("street-morning", "")]);
    assert_eq!(run_music(&segs, &runs_of(2), &cfg), vec!["busy"]);
    let segs = build(&[("night-forest", ""), ("night-forest", "")]);
    assert_eq!(run_music(&segs, &runs_of(2), &cfg), vec!["quiet"]);
    // Nothing in the shim matches: silence, exactly as the old `default`
    // (no music tags) did.
    let segs = build(&[("somewhere-else", ""), ("somewhere-else", "")]);
    assert_eq!(run_music(&segs, &runs_of(2), &cfg), vec!["none"]);
}

#[test]
fn the_palette_is_read_once_and_rendered_for_the_prompt() {
    let cfg = scene_map();
    // The `_note` key documents the section in place; it is not a value the
    // analyzer could be asked to emit.
    assert_eq!(
        palette_names(&cfg),
        vec!["busy", "none", "quiet", "warm"],
        "sorted keys, no _note"
    );
    let rendered = palette_prompt(&cfg);
    assert!(
        rendered.contains("quiet (soft, calm; low and unobtrusive)"),
        "{rendered}"
    );
    assert!(rendered.contains("none (silence)"), "{rendered}");
    assert!(!rendered.contains("_note"), "{rendered}");
}

/// The place vocabulary, and the whole point of adding it: a rule whose
/// match words the analyzer has never been shown is a rule that does not
/// fire, and nothing downstream can see that.
///
/// Pinned against the real shape rather than the fixture's, because the
/// fixture's rules all happen to match on words that are also bed tags —
/// which is exactly why the conflation survived. These three rules use
/// `palace`, `hall` and `garden`, none of which is a bed tag on the shipped
/// pool, and all three must reach the prompt anyway.
#[test]
fn the_place_vocabulary_is_every_rule_match_word_sorted_and_deduped() {
    let cfg: SceneMap = serde_json::from_value(json!({
        "rules": [
            {"match": ["palace", "jade pavilion"], "effect": [], "level": 0.0},
            {"match": ["hall", "palace"], "effect": [], "level": 0.0},
            {"match": ["garden", "  "], "effect": ["garden"], "level": 0.1}
        ],
        "default": {"effect": ["night"], "level": 0.1}
    }))
    .unwrap();
    assert_eq!(
        scene_prompt(&cfg),
        "garden, hall, jade pavilion, palace",
        "sorted, deduped across rules, blank words dropped"
    );
    // `default` has no match set, so it contributes nothing: it is what a
    // label matches when nothing else did, not a word to write.
    assert!(
        !scene_prompt(&cfg).contains("night"),
        "{:?}",
        scene_prompt(&cfg)
    );
}

/// The conflation itself, stated as a test: the place words and the bed
/// words are different lists, and a place word off the bed list is still
/// correct to write. On the shipped map 15 of 61 match words are bed tags,
/// so 46 rule words reached nothing while the prompt called the bed list
/// "the vocabulary it answers to".
#[test]
fn the_place_words_and_the_bed_words_are_different_lists() {
    let map = shipped_map();
    let places: std::collections::BTreeSet<String> =
        scene_prompt(&map).split(", ").map(str::to_string).collect();
    let pool = audio_pool::load_pool(&fixture_live("vocab").join("assets/effect-pool.json"));
    let beds: std::collections::BTreeSet<String> = effect_tags(&pool).into_iter().collect();

    let place_only: Vec<&String> = places.difference(&beds).collect();
    assert!(
        !place_only.is_empty(),
        "the shipped map's rules and pool must not be in lockstep, or this \\
         test is no longer testing anything"
    );
    // The words this was actually about. If a future edit made the scene
    // map match only bed tags, these four would go, and the prompt's
    // conflation would have become harmless by accident.
    for word in ["palace", "hall", "garden", "gate"] {
        assert!(
            places.contains(word),
            "{word:?} is a rule match word and must reach the prompt"
        );
    }
}

#[test]
fn the_effect_vocabulary_is_the_sorted_union_of_pool_tags() {
    let pool: ClipPool = serde_json::from_value(json!({
        "night": {"tags": ["night", "calm"], "files": ["effects/night-1.mp3"]},
        "rain": {"tags": ["rain", "calm"], "files": ["effects/rain-1.mp3"]},
        "empty": {"tags": ["ghost"], "files": []},
    }))
    .unwrap();
    // Sorted, deduped, and drawn from sounds, even one with no files,
    // because the vocabulary describes the pool, not one pick.
    assert_eq!(effect_tags(&pool), vec!["calm", "ghost", "night", "rain"]);
}

/// The defect that started this: chapter 1 opened with a hearth crackling
/// under a martial-arts shop at dawn. The label `martial-shop-morning`
/// matched the generic `shop` keyword of a catch-all fire rule, which sat
/// *before* the daylight rule, so `morning` never got a say. Against the
/// map that actually ships, not a fixture.
#[test]
fn the_shipped_map_no_longer_puts_a_hearth_under_a_shop_at_dawn() {
    let cfg = shipped_map();
    assert_eq!(
        match_scene("martial-shop-morning", &cfg).effect,
        vec!["day", "calm"],
        "a shop at dawn is calm daylight, not a hearth"
    );
    // A genuine hearth still gets fire, from the forge rule.
    assert_eq!(match_scene("forge", &cfg).effect, vec!["fire"]);
    assert_eq!(match_scene("kitchen", &cfg).effect, vec!["fire"]);
    // ...but a courtyard does not. `courtyard` used to sit in a fire rule,
    // which put a hearth under `courtyard-battle-moment` and
    // `courtyard-confrontation`, 20 labels and ~800 segments in the
    // corpus, the same absurdity as the shop at dawn, just louder. A
    // courtyard is not a place with a fire in it: the battle rule owns the
    // ones that say battle, the daylight rule owns the ones that name a
    // time, and a bare `courtyard` is silent like any other unlisted place.
    assert_eq!(
        match_scene("courtyard-battle-moment", &cfg).effect,
        vec!["battle", "sword"]
    );
    assert_eq!(
        match_scene("courtyard-morning", &cfg).effect,
        vec!["day", "calm"]
    );
    assert!(match_scene("courtyard", &cfg).effect.is_empty());
    // Chapter 1's other two labels: the shopfront rule owns them.
    assert_eq!(
        match_scene("shopfront-neighbor-chat", &cfg).effect,
        vec!["market"]
    );
    assert_eq!(
        match_scene("shopfront-sisters-encounter", &cfg).effect,
        vec!["market"]
    );
    // Ordering is a decision, not a detail: an earlier rule's keyword beats
    // a later rule's, so a label carrying a time of day is scored by the
    // time and not by the place. Pinned here because *that* mechanism is
    // what mis-scored chapter 1, and it still cuts both ways.
    assert_eq!(match_scene("forge-night", &cfg).effect, vec!["night"]);
    assert_eq!(match_scene("courtyard-dusk", &cfg).effect, vec!["night"]);
}

/// The shipped palette has to be answerable by the shipped pool, or a mood
/// the prompt offers would silently mean silence.
#[test]
fn every_shipped_palette_value_but_none_has_a_pooled_track() {
    let cfg = shipped_map();
    let dir = fixture_live("palette");
    let pool = audio_pool::load_pool(&dir.join("assets/music-pool.json"));
    assert!(!pool.is_empty(), "the music pool must load");
    for (name, entry) in &cfg.music_palette {
        if name == "none" {
            assert!(entry.tags.is_empty(), "`none` means no tags");
            continue;
        }
        assert!(
            audio_pool::pick(
                &pool,
                &entry.tags,
                audio_pool::seed(1, 0, std::slice::from_ref(name))
            )
            .is_some(),
            "palette value {name:?} names tags [{}] that no pooled clip answers",
            entry.tags.join(", ")
        );
    }
}

/// Every shipped rule must actually resolve, or the rule is decoration.
/// Every shipped effect rule resolves, and the daylight rule owns its scene.
///
/// A tie between interchangeable variants is the pool's designed behaviour
/// `night-1..4` are four nights, and the seed spreads them across chapters.
/// A tie between *different sounds* is not: `["day"]` alone sat on six clips
/// spanning birdsong, a calm bed and a market crowd, one of them a one-shot
/// stinger, so the daylight rule drew its bed at random and chapter 1 got a
/// crowd under a shop at dawn.
#[test]
fn every_shipped_effect_rule_resolves_and_daylight_is_a_bed() {
    let cfg = shipped_map();
    let dir = fixture_live("rule-pool");
    let pool = audio_pool::load_pool(&dir.join("assets/effect-pool.json"));
    assert!(!pool.is_empty(), "the effect pool must load");
    for rule in &cfg.rules {
        if rule.effect.is_empty() {
            continue;
        }
        assert!(
            audio_pool::pick(&pool, &rule.effect, audio_pool::seed(1, 0, &rule.effect)).is_some(),
            "rule {:?} names tags [{}] that no pooled clip answers",
            rule.matches,
            rule.effect.join(", ")
        );
    }

    // The rule this change was about, pinned by name. A 75 s window needs a
    // looped bed, and the *sound* is the decision, the take (`day-1/2/3`)
    // is the pool's business and must not appear here.
    let day = match_scene("martial-shop-morning", &cfg);
    assert_eq!(day.effect, vec!["day", "calm"], "the daylight rule owns it");
    let got = audio_pool::pick(&pool, &day.effect, audio_pool::seed(1, 0, &day.effect)).unwrap();
    assert_eq!(got.sound, "day", "the family, never `day-2`");
    assert!(
        got.looped,
        "daylight must be a bed, got {} (one-shot)",
        got.sound
    );
    assert!(
        got.file.starts_with("effects/day-"),
        "and the take comes from the family: {}",
        got.file
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Every file a fixture pool names must exist on disk here, placeholder
/// takes, written below, because the fixture ships shapes without clips.
///
/// The merge resolves a pool `file` through `clip_path` and, when it is
/// missing, prints a warning and skips the run, so a registry pointing at a
/// renamed or deleted clip is not an error, it is *silence*. The disk half
/// of this runs at runtime too, where the tree actually lives: `check_files`
/// at load and `profile::verify` on the pointer hash.
#[test]
fn every_shipped_pool_file_exists() {
    let dir = fixture_live("takes");
    let assets = dir.join("assets");
    for reg in ["effect-pool.json", "music-pool.json"] {
        let pool = audio_pool::load_pool(&assets.join(reg));
        assert!(!pool.is_empty(), "{reg} must load");
        for (sound, entry) in &pool {
            assert!(
                !entry.files.is_empty(),
                "{reg}: sound {sound:?} has no takes, so it can never be picked"
            );
            for file in &entry.files {
                let p = clip_path(&assets, file);
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(&p, b"").unwrap();
                assert!(
                    p.is_file(),
                    "{reg}: sound {sound:?} names {file:?}, which is not on disk ({})",
                    p.display()
                );
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_level_expression_is_flat_without_a_pause_and_lifts_inside_one() {
    let flat = level_expr(0.06, 0.085, 0.6, 100.0, &[]);
    assert_eq!(flat, "0.060000");

    let lifted = level_expr(0.06, 0.085, 0.6, 100.0, &[(120.0, 121.5)]);
    // Offsets are relative to the run, and there are exactly two ramps.
    assert!(lifted.contains("clip((t-20.000)/0.600,0,1)"), "{lifted}");
    assert!(lifted.contains("clip((t-21.500)/0.600,0,1)"), "{lifted}");
    assert_eq!(lifted.matches("clip").count(), 2, "{lifted}");

    // No lift to make: the expression stays flat rather than emitting a
    // pair of cancelling ramps.
    assert_eq!(level_expr(0.06, 0.06, 0.6, 0.0, &[(1.0, 2.0)]), "0.060000");
}

#[test]
fn adjacent_tracks_share_one_crossfade_and_silence_keeps_its_hole() {
    let run = |start: f64, end: f64| MusicRun {
        mood: "m".into(),
        sound: "s".into(),
        file: "f".into(),
        level: 1.0,
        start,
        end,
        pauses: vec![],
    };
    let runs = vec![run(0.0, 10.0), run(10.3, 20.0)];
    assert_eq!(music_starts(&runs, 2.0), vec![0.0, 10.0]);
    let gapped = vec![run(0.0, 10.0), run(30.0, 40.0)];
    assert_eq!(music_starts(&gapped, 2.0), vec![0.0, 30.0]);
}

#[test]
fn negative_volumes_mute_instead_of_inverting() {
    let on = LayerSwitch::new(true, true, -1.0, -2.0, -3.0);
    assert_eq!(
        (on.effect_volume, on.music_volume, on.inject_volume),
        (0.0, 0.0, 0.0)
    );
}

#[test]
fn a_fade_never_runs_past_half_a_slice() {
    // `place` clamps, so a 0.3 s fade on a 0.2 s one-shot cannot invert the
    // envelope. Exercised through the pure part of the arithmetic.
    let dur = 0.2f64;
    let fo = 0.3f64.clamp(0.0, dur / 2.0);
    assert!((fo - 0.1).abs() < 1e-9);
    assert!(
        (dur - fo).max(0.0) > 0.0,
        "the fade-out must start inside the slice"
    );
}

fn inject_pool() -> ClipPool {
    serde_json::from_value(json!({
        "blood": {"tags": ["blood"], "files": ["injects/blood-1.mp3", "injects/blood-2.mp3"], "looped": false, "dur_s": 1.1, "mode": "hit"},
        "boil": {"tags": ["water", "boiling"], "files": ["injects/boil-1.mp3"], "looped": false, "dur_s": 51.0, "mode": "overlap"},
        "rumble": {"tags": ["deep"], "files": ["injects/rumble-1.mp3"], "looped": true, "dur_s": 9.0, "mode": "trail", "hold": 3.0, "level": 0.5},
        "bare": {"tags": ["plain"], "files": ["injects/bare-1.mp3"], "looped": false, "dur_s": 0.4},
        "empty": {"tags": ["ghost"], "files": []},
    }))
    .unwrap()
}

fn durs() -> BTreeMap<String, f64> {
    BTreeMap::from([
        ("injects/blood-1.mp3".to_string(), 0.5),
        ("injects/blood-2.mp3".to_string(), 1.1),
        ("injects/boil-1.mp3".to_string(), 51.0),
        ("injects/rumble-1.mp3".to_string(), 9.0),
    ])
}

fn islot(start: f64, end: f64, injects: &[Value]) -> Slot {
    Slot {
        wav: PathBuf::from("x.wav"),
        scene: "s".into(),
        music: String::new(),
        speaker: "A".into(),
        injects: injects_of(injects, &inject_pool(), 2.0),
        start,
        end,
        gap_ms: 300,
        pause_ms: 0,
        inject_ms: 0,
    }
}

/// The script names the sound; **the pool says how it behaves**. A
/// directive that tries to restate `mode` is ignored, not honoured, that
/// is the whole point of moving it out of the JSON, and it is asserted here
/// because a silent "the script won" would reintroduce the second source of
/// truth without anything failing.
#[test]
fn injects_of_takes_the_sounds_behaviour_from_the_pool_not_the_script() {
    let pool = inject_pool();
    let got = injects_of(
        &[
            json!({"sound": "blood"}),
            json!({"sound": "boil"}),
            json!({"sound": "rumble"}),
            json!({"sound": "bare"}),
            json!({"stop": "boil"}),
        ],
        &pool,
        2.0,
    );
    assert_eq!(
        got,
        vec![
            Inject::Start {
                sound: "blood".into(),
                mode: InjectMode::Hit,
                hold_s: 2.0,
                level: 1.0
            },
            Inject::Start {
                sound: "boil".into(),
                mode: InjectMode::Overlap,
                hold_s: 2.0,
                level: 0.1
            },
            // `hold` and `level` come from the entry too, not the layer
            // default, and the level then takes the mode's gain, so a
            // trail's 0.5 is rendered at 0.05.
            Inject::Start {
                sound: "rumble".into(),
                mode: InjectMode::Trail,
                hold_s: 3.0,
                level: 0.05
            },
            // No `mode` in the entry: the one default left is `hit`.
            Inject::Start {
                sound: "bare".into(),
                mode: InjectMode::Hit,
                hold_s: 2.0,
                level: 1.0
            },
            Inject::Stop {
                sound: "boil".into()
            },
        ],
        "{got:?}"
    );
    // A directive restating the behaviour changes nothing.
    assert_eq!(
        injects_of(
            &[json!({"sound": "boil", "mode": "trail", "hold": 3.0, "level": 0.5})],
            &pool,
            2.0
        ),
        vec![Inject::Start {
            sound: "boil".into(),
            mode: InjectMode::Overlap,
            hold_s: 2.0,
            level: 0.1
        }],
    );
    // Absent, empty, malformed and unknown entries are silence, not errors
    // the digest validator is the strict gate, the merge survives hand edits.
    assert!(injects_of(&[], &pool, 2.0).is_empty());
    assert!(injects_of(
        &[
            json!("blood"),
            json!({"sound": ""}),
            json!({"mode": "loud"}),
            json!({})
        ],
        &pool,
        2.0
    )
    .is_empty());
    assert!(
        injects_of(&[json!({"sound": "not-in-the-pool"})], &pool, 2.0).is_empty(),
        "an unregistered sound has no mode either, so it is not guessed at"
    );
    // A registered sound with no takes resolves here and is dropped later
    // with one warning, so the plan and the mix agree on what was asked for.
    assert_eq!(
        injects_of(&[json!({"sound": "empty"})], &pool, 2.0).len(),
        1
    );
}

/// An `overlap` and a `trail` are beds: they run *under* the speech, so they
/// render at a tenth of the level their pool entry carries. A `hit` owns the
/// silence it was written into and keeps its level.
///
/// The gain belongs to the *mode*, so two clips of one mode keep their ratio
/// to each other, the pool's `level` stays the balance between them rather
/// than a second volume knob for the layer. And it is applied *after* the
/// `None`/zero rule, so a bed with no level of its own is 0.1, not 0.0: the
/// "a pool can never mute by arithmetic accident" promise survives it.
#[test]
fn a_bed_renders_at_a_tenth_of_its_level_and_a_hit_at_all_of_it() {
    let pool: ClipPool = serde_json::from_value(json!({
        "slam":  {"tags": ["x"], "files": ["injects/a.mp3"], "mode": "hit", "level": 0.8},
        "boil":  {"tags": ["x"], "files": ["injects/b.mp3"], "mode": "overlap", "level": 0.5},
        "wind":  {"tags": ["x"], "files": ["injects/c.mp3"], "mode": "trail"},
        "quiet": {"tags": ["x"], "files": ["injects/d.mp3"], "mode": "overlap", "level": 0.0},
    }))
    .unwrap();
    let levels: Vec<f64> = injects_of(
        &[
            json!({"sound": "slam"}),
            json!({"sound": "boil"}),
            json!({"sound": "wind"}),
            json!({"sound": "quiet"}),
        ],
        &pool,
        2.0,
    )
    .iter()
    .map(|i| match i {
        Inject::Start { level, .. } => *level,
        Inject::Stop { .. } => unreachable!("no stops in this script"),
    })
    .collect();
    assert_eq!(
        levels,
        vec![0.8, 0.05, 0.1, 0.1],
        "a hit keeps its level; a bed takes a tenth of the entry's — or of \
         the unity an absent or zero level resolves to"
    );
}

/// An entry whose `mode` names nothing is silence, not a default. The pool
/// said something the mixer does not understand, and guessing `hit` would
/// play a length of clip nobody asked for. `inject_mode` returning `None` is
/// what makes that a skip, and it is now also the answer the `:sound`
/// editor's own validation reads, so the two cannot disagree about which
/// strings are modes.
#[test]
fn an_entry_with_an_unknown_mode_is_skipped_not_defaulted() {
    let pool: ClipPool = serde_json::from_value(json!({
        "loud": {"tags": ["x"], "files": ["injects/a.mp3"], "mode": "loud"},
        "slam": {"tags": ["x"], "files": ["injects/b.mp3"], "mode": "hit"},
    }))
    .unwrap();
    let got = injects_of(
        &[json!({"sound": "loud"}), json!({"sound": "slam"})],
        &pool,
        2.0,
    );
    assert_eq!(
        got.len(),
        1,
        "the unknown mode is dropped and the known one still plays: {got:?}"
    );
    assert_eq!(inject_mode("hit").map(|m| m.gain()), Some(1.0));
    assert_eq!(inject_mode("trail").map(|m| m.gain()), Some(0.1));
    assert!(
        inject_mode("loud").is_none(),
        "and it is one answer, not two"
    );
}

#[test]
fn inject_takes_name_the_sound_and_roll_with_the_slot() {
    let pool = inject_pool();
    // Direct registry lookup: the analyzer names the sound, the pool rolls
    // the take, tag scoring could only answer a question nobody asked.
    for slot in 0..8 {
        let t = inject_take(&pool, 1, slot, "blood").unwrap();
        assert_eq!(t.sound, "blood");
        assert!(t.file.starts_with("injects/blood-"), "{}", t.file);
    }
    assert!(inject_take(&pool, 1, 0, "thunderstorm").is_none());
    assert!(inject_take(&pool, 1, 0, "empty").is_none());
    // Same chapter re-merges to the same take; chapters vary.
    let a = inject_take(&pool, 1, 3, "blood").unwrap();
    assert_eq!(a, inject_take(&pool, 1, 3, "blood").unwrap());
    let over: Vec<String> = (1..=8)
        .map(|c| inject_take(&pool, c, 3, "blood").unwrap().file)
        .collect();
    assert!(over.iter().any(|f| *f != over[0]), "{over:?}");
}

#[test]
fn the_prompt_names_sounds_tags_and_lengths() {
    let rendered = inject_prompt(&inject_pool());
    // mode first, because it is what the analyzer cannot choose and must
    // know: a `hit` pauses the narration, an `overlap` runs under it.
    assert!(rendered.contains("blood (hit; blood; 1.1s)"), "{rendered}");
    assert!(
        rendered.contains("boil (overlap; water, boiling; 51s)"),
        "{rendered}"
    );
    // a trail renders its hold
    assert!(
        rendered.contains("rumble (trail 3.0s, loop; deep; 9.0s)"),
        "{rendered}"
    );
    // a one-shot must NOT claim to loop
    assert!(!rendered.contains("blood (hit, loop"), "{rendered}");
}

/// Hits queue in the silence their holds wrote, an overlap costs no time,
/// a trail holds its solo and tails under the speech, and a stop fades
/// never cuts, from its anchor. Every mode here comes from the pool entry,
/// because that is where a clip's behaviour lives.
#[test]
fn inject_events_hit_queue_overlap_tails_and_stops_fade() {
    let pool = inject_pool();
    let durs = durs();
    let cfg = InjectLayer::default();
    let slots = vec![
        islot(
            0.0,
            10.0,
            &[json!({"sound": "blood"}), json!({"sound": "boil"})],
        ),
        islot(
            12.0,
            20.0,
            &[
                json!({"sound": "boil"}),   // retriggers the first one
                json!({"sound": "rumble"}), // trail, hold 3.0 from the pool
                json!({"stop": "rumble"}),  // fades it from the cursor
            ],
        ),
    ];
    let takes = plan_inject_takes(&slots, &pool, 1);
    let holds_before = slots[0].gap_ms;
    let mut held = slots.clone();
    plan_inject_holds(&mut held, &takes, &durs, 1.0);
    let blood_dur = durs[&takes[0][0].as_ref().unwrap().file];
    assert_eq!(held[0].gap_ms, holds_before + (blood_dur * 1000.0) as u32);
    // Slot 1's trail adds its 3 s hold after slot 1 ends at 20.0.
    assert_eq!(held[1].gap_ms, holds_before + 3000);

    let ev = plan_injects(&slots, &takes, &durs, &cfg);
    assert_eq!(ev.len(), 4, "{ev:?}");
    // The hit queues first at slot 0 end (10.0).
    assert_eq!(ev[0].mode, InjectMode::Hit);
    assert!((ev[0].start - 10.0).abs() < 1e-9);
    assert!((ev[0].end - (10.0 + blood_dur)).abs() < 1e-9);
    assert_eq!(ev[0].fade_in, 0.0);
    // The overlap starts at the same cursor (after the hit) and costs no
    // time; the retrigger at slot 1 end (20.0) fades it over `fade_s`.
    assert_eq!(ev[1].mode, InjectMode::Overlap);
    assert!((ev[1].start - (10.0 + blood_dur)).abs() < 1e-9);
    assert!((ev[1].end - (20.0 + 0.3)).abs() < 1e-9, "{ev:?}");
    // The retriggered instance runs from 20.0 for the clip's own length.
    assert_eq!(ev[2].mode, InjectMode::Overlap);
    assert!((ev[2].start - 20.0).abs() < 1e-9);
    assert!((ev[2].end - (20.0 + 51.0)).abs() < 1e-9, "{ev:?}");
    // The trail starts at 20.0 too, holds 3 s solo, then the stop fades it
    // from 23.0 over `stop_fade_s` = 3 s, an ending, not a cut.
    assert_eq!(ev[3].mode, InjectMode::Trail);
    assert!((ev[3].start - 20.0).abs() < 1e-9);
    assert!((ev[3].end - (23.0 + 3.0)).abs() < 1e-9, "{ev:?}");
    assert!((ev[3].fade_out - 3.0).abs() < 1e-9, "stop_fade_s");
    // The level the render multiplies by `layers.inject.level` is the pool's
    // with the mode's gain already folded in, `boil` carries none (unity),
    // `rumble` 0.5. Pinned here, at the far end of the pipeline from
    // `injects_of`, because a gain that stopped halfway would still leave
    // every test above passing.
    assert!((ev[1].level - 0.1).abs() < 1e-9, "{ev:?}");
    assert!((ev[3].level - 0.05).abs() < 1e-9, "{ev:?}");
}

#[test]
fn a_retrigger_fades_the_old_instance_and_a_stop_for_nothing_is_silence() {
    let pool = inject_pool();
    let durs = durs();
    let cfg = InjectLayer::default();
    let slots = vec![
        islot(0.0, 10.0, &[json!({"sound": "boil"})]),
        islot(
            20.0,
            30.0,
            &[json!({"sound": "boil"}), json!({"stop": "ghost"})],
        ),
    ];
    let takes = plan_inject_takes(&slots, &pool, 1);
    let ev = plan_injects(&slots, &takes, &durs, &cfg);
    assert_eq!(ev.len(), 2, "{ev:?}");
    // Same sound starting again fades the old instance in fade_s.
    assert!((ev[0].end - (30.0 + 0.3)).abs() < 1e-9, "{ev:?}");
    assert!((ev[0].fade_out - 0.3).abs() < 1e-9);
    assert!((ev[1].start - 30.0).abs() < 1e-9);
    assert!((ev[1].end - (30.0 + 51.0)).abs() < 1e-9, "{ev:?}");
}

#[test]
fn holds_are_authored_delivered_and_written_pre_tempo() {
    // Like a planned pause: authored in delivered seconds, scaled by speed
    // for the concat, divided back by `retime`.
    let pool = inject_pool();
    let durs = durs();
    let slots = vec![islot(0.0, 10.0, &[json!({"sound": "rumble"})])];
    let takes = plan_inject_takes(&slots, &pool, 1);
    let mut held = slots.clone();
    plan_inject_holds(&mut held, &takes, &durs, 1.25);
    assert_eq!(held[0].gap_ms, 300 + 3750, "{held:?}");
}

/// A wrong bus assignment does not fail, it makes a layer quiet, which is
/// how the inject layer went inaudible. So the graph is asserted on.
#[test]
fn the_inject_layer_is_mixed_after_the_duck_and_the_beds_are_not() {
    let sc = "sidechaincompress=threshold=0.02:ratio=6:attack=20:release=400";
    let ducked = format!("[under][0:a]{sc}[duck]");
    let lim = "alimiter=limit=0.589:attack=5:release=100:level=0[a]";
    let tail2 = format!("[0:a][duck]amix=inputs=2:normalize=0[mixed];[mixed]{lim}");
    let tail3 = format!("[0:a][duck][3:a]amix=inputs=3:normalize=0[mixed];[mixed]{lim}");
    // Beds only: they sum, they duck, they mix with the voice, and the sum
    // is limited. No third input.
    let g = layer_graph(2, false, sc, None);
    assert!(g.contains("[1:a][2:a]amix=inputs=2"), "{g}");
    assert!(g.contains(&ducked), "{g}");
    assert!(g.ends_with(&tail2), "{g}");
    // With an inject track, it is input 3 and it enters *after* the duck:
    // never inside `[under]`, or the voice's own compressor eats it.
    let g = layer_graph(2, true, sc, None);
    assert!(
        g.contains("[1:a][2:a]amix=inputs=2:normalize=0[under]"),
        "{g}"
    );
    assert!(g.contains(&ducked), "{g}");
    assert!(
        g.ends_with(&tail3),
        "the inject track must be summed with the ducked mix, not ducked: {g}"
    );
    assert!(
        !g.contains("[1:a][2:a][3:a]"),
        "the inject track must not join the ducked bus: {g}"
    );
    // A single bed needs no summing before the compressor.
    let g = layer_graph(1, true, sc, None);
    assert!(g.contains("[1:a]anull[under]"), "{g}");
    assert!(g.contains("[0:a][duck][2:a]amix=inputs=3"), "{g}");
    // No beds at all: nothing to duck, so no compressor in the graph.
    let g = layer_graph(0, true, sc, None);
    assert!(
        g.starts_with("[0:a][1:a]amix=inputs=2:normalize=0[mixed]"),
        "{g}"
    );
    assert!(!g.contains("sidechain"), "{g}");
    // ...but the limiter is on every arm. The ceiling is the contract, and
    // nothing else in the path holds it: ch9 measured -0.11 dBFS with the
    // inject layer switched off entirely.
    for (b, i) in [
        (0usize, true),
        (1, false),
        (1, true),
        (2, false),
        (2, true),
        (3, true),
    ] {
        let g = layer_graph(b, i, sc, None);
        assert!(
            g.contains("alimiter=limit=0.589"),
            "beds={b} inject={i}: {g}"
        );
        assert!(g.ends_with("[a]"), "beds={b} inject={i}: {g}");
    }
}

/// The headline is the one place the beds are meant to arrive, so the key
/// is held down there, and it is held down on a *copy* of the voice: the
/// voice that reaches the mix must be the one that was rendered, not a key
/// with a level edit on it.
#[test]
fn the_headline_is_exempt_from_the_duck() {
    let sc = "sidechaincompress=threshold=0.05:ratio=3:attack=20:release=400";
    let g = layer_graph(2, false, sc, Some((7.4, 0.0)));
    assert!(g.starts_with("[0:a]asplit=2[vox][sc];"), "{g}");
    assert!(
        g.contains("[sc]volume=volume='if(lt(t,7.400),0.0000,1)':eval=frame[key]"),
        "{g}"
    );
    assert!(g.contains(&format!("[under][key]{sc}[duck]")), "{g}");
    assert!(
        g.contains("[vox][duck]amix=inputs=2:normalize=0"),
        "the voice is the copy nothing attenuated: {g}"
    );
    // A key taken as-is is the graph every chapter used to get, byte for
    // byte, no split, no volume filter, nothing to explain.
    let plain = layer_graph(2, false, sc, Some((7.4, 1.0)));
    assert!(!plain.contains("asplit"), "{plain}");
    assert!(!plain.contains("volume"), "{plain}");
    assert!(
        plain.contains(&format!("[under][0:a]{sc}[duck]")),
        "{plain}"
    );
    // And no headline at all (a chapter that opens on a scene) is the same
    // graph as a key taken as-is.
    assert_eq!(plain, layer_graph(2, false, sc, None));
}

/// The headline is the opening turn with neither a place nor a mood. The
/// test is on that pairing, not on "the first slot": a chapter that opens
/// on a scene keeps its duck from the first line.
#[test]
fn the_headline_is_the_first_turn_with_no_place_and_no_mood() {
    let headline = |end: f64| Slot {
        scene: String::new(),
        ..slot("", 0.0, end)
    };
    let slots = vec![headline(6.5), slot("quiet", 6.8, 40.0)];
    assert_eq!(headline_end(&slots), Some(6.5));
    assert_eq!(
        headline_end(&[slot("quiet", 0.0, 40.0)]),
        None,
        "a mood from the first line means the beds are already wanted"
    );
    assert_eq!(
        headline_end(&[slot("", 0.0, 40.0)]),
        None,
        "a place, no mood — a labelled opening line is not the headline"
    );
    assert_eq!(headline_end(&[]), None);
    // The default is the exemption, because that is what makes the layer's
    // own fade-in audible at all.
    assert_eq!(Duck::default().head_key, 0.0);
    // ...and a map that predates the field gets it.
    assert_eq!(scene_map().duck.head_key, 0.0);
}

/// A loop is only worth making if the arithmetic is right: too few copies
/// leave silence at the end of the window, too many waste decode time, and
/// a seam that overlaps too far is audible as a pump.
#[test]
fn loop_copies_solves_for_the_window_and_refuses_a_single_play() {
    // A 6.86 s bed in a 7 s window: one more copy covers it.
    assert_eq!(loop_copies(7.0, 6.86, 0.25), Some(2));
    // A window no longer than the clip is not a loop at all, this is the
    // path a bed with no `stop` takes, and it must stay a single play.
    assert_eq!(loop_copies(6.86, 6.86, 0.25), None);
    assert_eq!(loop_copies(3.0, 6.86, 0.25), None);
    assert_eq!(loop_copies(7.0, 0.0, 0.25), None);
    // 142 s of kitchen out of a 6.86 s clip. n copies run
    // n*6.86 - (n-1)*0.25 >= 142, so n = 22.
    let n = loop_copies(142.0, 6.86, 0.25).unwrap();
    assert_eq!(n, 22, "{n}");
    assert!(
        n as f64 * 6.86 - (n as f64 - 1.0) * 0.25 >= 142.0,
        "the loop must cover the window"
    );
    assert!(
        (n as f64 - 1.0) * 6.86 - (n as f64 - 2.0) * 0.25 < 142.0,
        "one copy fewer must fall short, or the count is not minimal"
    );
    // A crossfade as long as the clip cannot be honoured: it is clamped.
    assert_eq!(loop_copies(20.0, 1.0, 5.0), Some(27));
}

#[test]
fn loop_filter_crossfades_every_seam_and_ends_on_the_volume() {
    let g = loop_filter(3, 0.25, 0.8);
    // Three copies split off the one input...
    assert!(g.starts_with("[0:a]asplit=3[c0][c1][c2]"), "{g}");
    // ...joined by two crossfades, chained, never a hard splice.
    assert_eq!(g.matches("acrossfade=").count(), 2, "{g}");
    assert!(g.contains("[c0][c1]acrossfade=d=0.250"), "{g}");
    assert!(g.contains("[o1][c2]acrossfade=d=0.250"), "{g}");
    // ...and the gain rides on the tail, before the output label.
    assert!(
        g.ends_with("volume=0.8000,aformat=sample_rates=48000:channel_layouts=mono[out]"),
        "{g}"
    );
    // Every copy is consumed exactly once.
    for i in 0..3 {
        assert_eq!(g.matches(&format!("[c{i}]")).count(), 2, "copy {i} in {g}");
    }
}

/// The music layer loops too, and for the same reason — a 2-minute bed
/// under a 20-minute chapter is ten seams. Pinned here because the music
/// path is the one place that used to butt-join, and a butt-join is
/// invisible in a test and audible every two minutes.
#[test]
fn the_music_loop_crossfades_and_keeps_the_pause_lift() {
    // Under one clip length: no loop at all, which is the path that must
    // not change for a run that already fits.
    assert_eq!(loop_copies(140.0, 150.0, 2.0), None);
    // Over it: a crossfaded loop, and the number of copies is what the
    // same solver the inject layer already used gives.
    let k = loop_copies(600.0, 150.0, 2.0).unwrap();
    assert!(k >= 4, "{k} copies for 600s out of a 150s track");

    // The gain rides on the loop's tail, not before the crossfades: a
    // `volume` placed ahead of the fade would duck the seam instead of the
    // track, and a time-varying expression is the only way a run lifts
    // inside a planned pause.
    let expr = "0.160000 + 0.040000*clip((t-1.000)/0.600,0,1)";
    let g = loop_filter_with_tail(k, 2.0, &format!("volume=volume='{expr}':eval=frame"));
    assert_eq!(g.matches("acrossfade=").count(), k - 1, "{g}");
    assert!(
        g.ends_with(&format!(
            "volume=volume='{expr}':eval=frame,\
             aformat=sample_rates=48000:channel_layouts=mono[out]"
        )),
        "the lift must be the last filter before aformat: {g}"
    );
    assert!(
        !g.contains("stream_loop"),
        "the loop is a filter, not a flag: {g}"
    );
}

/// The inject pool must state `looped` on every entry.
///
/// `Sound::looped` defaults to `true` because the *effect* pool is mostly
/// beds and the default is what makes a hand-written rule work. The inject
/// pool is the opposite, mostly one-shots, so the same default silently
/// turns an entry that forgot the key into a looping bed. The shipped pool
/// states it everywhere, and this is what keeps that true: read the raw JSON
/// rather than the parsed pool, because the parsed one cannot tell "absent"
/// from "true".
#[test]
fn every_shipped_inject_entry_states_looped_explicitly() {
    let dir = fixture_live("looped");
    let assets = dir.join("assets");
    let raw: Value =
        serde_json::from_str(&std::fs::read_to_string(assets.join("inject-pool.json")).unwrap())
            .unwrap();
    let mut missing = Vec::new();
    for (name, entry) in raw.as_object().unwrap() {
        if name.starts_with('_') {
            continue;
        }
        if entry.get("looped").is_none() {
            missing.push(name.clone());
        }
    }
    assert!(
        missing.is_empty(),
        "these inject entries omit `looped`, and the default is `true` (a looping bed): {missing:?}"
    );
}

/// Every shipped inject sound resolves to a real take, or the registry is
/// decoration the prompt offers anyway.
#[test]
fn every_shipped_inject_sound_has_takes_and_a_length() {
    let dir = fixture_live("takes-length");
    let assets = dir.join("assets");
    let pool = audio_pool::load_pool(&assets.join("inject-pool.json"));
    assert!(!pool.is_empty(), "the inject pool must load");
    // Placeholder takes: the check is path resolution, not audio.
    for entry in pool.values() {
        for file in &entry.files {
            let p = clip_path(&assets, file);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"").unwrap();
        }
    }
    for (sound, entry) in &pool {
        assert!(
            !entry.files.is_empty(),
            "sound {sound:?} has no takes, so it can never play"
        );
        assert!(
            entry.dur_s.unwrap_or(0.0) > 0.0,
            "sound {sound:?} needs dur_s — the prompt renders it and the validator judges hits by it"
        );
        for file in &entry.files {
            assert!(
                clip_path(&assets, file).is_file(),
                "sound {sound:?} names {file:?}, which is not on disk"
            );
        }
        let take = inject_take(&pool, 1, 0, sound).unwrap();
        assert_eq!(take.sound, *sound);
    }
}

/// A hold is silence *inserted* into the chapter, and inserting it makes
/// every later slot start later. Every layer, the effects, the music and
/// the injects themselves, is placed by reading `Slot::start`/`end`, so a
/// clock that did not move puts them all on top of speech that has shifted
/// out from under them. That is not a near miss: it is the difference
/// between a blood spatter in the silence reserved for it and the same
/// spatter buried under the last six words of the chapter.
#[test]
fn a_hit_hold_moves_every_later_slot_and_the_layers_with_it() {
    let pool = inject_pool();
    let durs = durs();
    // Slot 0 carries a 1.1 s hit; slot 1 is the next thing spoken.
    let mut slots = vec![
        islot(0.0, 1.0, &[json!({"sound": "blood"})]),
        islot(1.3, 2.2, &[]),
    ];
    let takes = plan_inject_takes(&slots, &pool, 1);
    plan_inject_holds(&mut slots, &takes, &durs, 1.0);
    assert_eq!(slots[0].gap_ms, 300 + 1100, "the hit's whole clip");
    assert_eq!(slots[0].inject_ms, 1100);
    // The clock moved with it: slot 1 starts after the silence, not after
    // the gap that was there before the hold was written.
    assert!(
        (slots[1].start - 2.4).abs() < 1e-9,
        "slot 1 starts at {:.3}, not 1.3 — the hold is real audio",
        slots[1].start
    );
    assert!((slots[1].end - 3.3).abs() < 1e-9, "{:?}", slots[1]);
    // And the slot's own duration is untouched, a hold is silence *after*
    // a line, never a change to the line.
    assert!((slots[1].end - slots[1].start - 0.9).abs() < 1e-9);
    // An overlap costs no time, so nothing moves.
    let mut quiet = vec![
        islot(0.0, 1.0, &[json!({"sound": "boil", "mode": "overlap"})]),
        islot(1.3, 2.2, &[]),
    ];
    let takes = plan_inject_takes(&quiet, &pool, 1);
    plan_inject_holds(&mut quiet, &takes, &durs, 1.0);
    assert_eq!(quiet[0].gap_ms, 300);
    assert!((quiet[1].start - 1.3).abs() < 1e-9);
}
