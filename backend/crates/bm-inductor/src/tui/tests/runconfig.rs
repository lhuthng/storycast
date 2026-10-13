use super::*;

#[test]
fn accents_are_folded_so_filters_ignore_diacritics() {
    assert_eq!(fold("Thái Sơn"), "thai son");
    assert_eq!(fold("Đức Trí"), "duc tri");
    assert_eq!(fold("Thục Đoan"), "thuc doan");
    assert_eq!(fold("Lạc Lan Tuyết"), "lac lan tuyet");
    assert!(matches("thai son", "Thái Sơn"));
    assert!(matches("duc", "Đức Trí"));
    assert!(
        matches("", "anything"),
        "an empty filter matches everything"
    );
    assert!(!matches("adam", "Thái Sơn"));
}

#[test]
fn neutral_does_not_fold_to_a_female_marker() {
    // Guards the Python/Rust twin of the same bug.
    assert_eq!(fold("neutral"), "neutral");
}

#[test]
fn text_prompt_edits_by_character_not_byte() {
    let mut p = TextPrompt::new(TextKind::AddMachine, "t", "h", "Đức");
    // Three characters, six bytes: a byte-indexed cursor would land inside
    assert_eq!(p.len(), 3);
    assert_eq!(p.cursor, 3);
    p.left();
    assert_eq!(p.cursor, 2, "cursor 2 sits before the third character");
    p.insert('x');
    assert_eq!(p.buf, "Đứxc");
    assert_eq!(p.cursor, 3);
    p.backspace();
    assert_eq!(p.buf, "Đức");
    assert_eq!(p.cursor, 2);
    p.home();
    p.delete();
    assert_eq!(p.buf, "ức");
    p.kill_to_start();
    assert_eq!(p.buf, "ức", "cursor is already at 0, so nothing is cut");
    p.end();
    assert_eq!(p.cursor, 2);
    p.kill_word();
    assert_eq!(p.buf, "");
}

#[test]
fn kill_word_stops_at_a_space() {
    let mut p = TextPrompt::new(TextKind::Translate, "t", "h", "21 80");
    p.kill_word();
    assert_eq!(p.buf, "21 ");
    p.kill_word();
    assert_eq!(p.buf, "");
}

#[test]
fn translate_prompt_rejects_garbage_instead_of_defaulting() {
    let mut app = App::new("http://x");
    let p = TextPrompt::new(TextKind::Translate, "t", "h", "abc 80");
    let err = submit_text(&mut app, &p).unwrap_err();
    assert!(err.contains("not a chapter number"), "{err}");

    let p = TextPrompt::new(TextKind::Translate, "t", "h", "21");
    assert!(submit_text(&mut app, &p).unwrap_err().contains("expected"));

    let p = TextPrompt::new(TextKind::Translate, "t", "h", "21 0");
    assert!(submit_text(&mut app, &p)
        .unwrap_err()
        .contains("at least 1"));

    let p = TextPrompt::new(TextKind::Translate, "t", "h", "21 80");
    assert!(submit_text(&mut app, &p).is_ok());
}

#[test]
fn run_config_parses_range_analyzer_and_models() {
    let (s, c, a, m) = parse_run_config("1 1", "opencode").unwrap();
    assert_eq!((s, c), (1, 1));
    assert_eq!(a, "opencode");
    assert!(m.is_none(), "omitted models stay out of the file");
    let (_, _, a, m) = parse_run_config("2 5 gemini 3.8-flash, 3.7-flash", "opencode").unwrap();
    assert_eq!(a, "gemini");
    assert_eq!(m.unwrap(), vec!["3.8-flash", "3.7-flash"]);
    assert!(parse_run_config("abc 80", "opencode")
        .unwrap_err()
        .contains("not a chapter number"));
    assert!(parse_run_config("1 1 watson", "opencode")
        .unwrap_err()
        .contains("unknown"));
    assert!(
        parse_run_config("1 1 gemini 3.8-flash 3.7-flash", "opencode")
            .unwrap_err()
            .contains("comma-separated")
    );
    assert!(parse_run_config("1 1 gemini ,", "opencode")
        .unwrap_err()
        .contains("empty"));
}

#[test]
fn run_config_save_persists_everything_it_parsed() {
    let dir = std::env::temp_dir().join("bm-runconfig-save");
    let _ = std::fs::remove_dir_all(&dir);
    let mut app = App::new("http://x");
    app.layout = bm_core::Layout::new(&dir);

    let msg = save_run_config(&app, "1 1 gemini 3.8-flash,3.7-flash").unwrap();
    assert!(msg.contains("ch1"), "{msg}");
    let saved: bm_core::config::Settings =
        bm_core::read_json(&bm_core::Layout::new(&dir).settings()).unwrap();
    assert_eq!((saved.start, saved.count), (1, 1));
    assert_eq!(saved.analyzer, "gemini");
    assert_eq!(saved.analyze_models, vec!["3.8-flash", "3.7-flash"]);

    // Omitted models keep the saved chain, a blank field must not wipe it.
    save_run_config(&app, "1 1 gemini").unwrap();
    let saved: bm_core::config::Settings =
        bm_core::read_json(&bm_core::Layout::new(&dir).settings()).unwrap();
    assert_eq!(saved.analyze_models, vec!["3.8-flash", "3.7-flash"]);

    assert!(save_run_config(&app, "1 1 watson")
        .unwrap_err()
        .contains("unknown"));
}

#[test]
fn run_preview_prefers_live_api_then_file_then_defaults() {
    // Live backend: its boot-time settings, labeled as such.
    let mut app = App::new("http://x");
    app.settings = Some(serde_json::json!({
        "start": 5, "count": 2, "analyzer": "gemini",
        "analyze_models": ["3.8-flash"], "engine": "vieneu",
    }));
    let cfg = run_preview(&app);
    assert!(cfg.live);
    assert_eq!((cfg.start, cfg.count), (5, 2));
    assert_eq!(cfg.analyzer, "gemini");
    assert_eq!(cfg.models, vec!["3.8-flash"]);
    app.settings.as_mut().unwrap()["inject_volume"] = serde_json::json!(0.25);
    assert_eq!(run_preview(&app).inject_volume, 0.25);
    assert_eq!(super::input::runconfig::mix_prefill(&app), "1.25 1 1 0.25");
    assert_eq!(
        (
            cfg.speed,
            cfg.effect_volume,
            cfg.music_volume,
            cfg.inject_volume
        ),
        (1.25, 1.0, 1.0, 1.0)
    );

    // Down backend: the saved file is what the next boot will use.
    let dir = std::env::temp_dir().join("bm-runconfig-preview");
    let _ = std::fs::remove_dir_all(&dir);
    let settings = bm_core::config::Settings {
        start: 1,
        count: 1,
        ..bm_core::config::Settings::default()
    };
    settings
        .save(&bm_core::Layout::new(&dir).settings())
        .unwrap();
    let mut app = App::new("http://x");
    app.layout = bm_core::Layout::new(dir);
    let cfg = run_preview(&app);
    assert!(!cfg.live);
    assert!(cfg.saved, "a settings file exists");
    assert_eq!((cfg.start, cfg.count), (1, 1));

    // Neither: honest defaults, labeled as nobody's choice.
    let app = App::new("http://x");
    let cfg = run_preview(&app);
    assert!(!cfg.live);
    assert!(!cfg.saved);
    assert_eq!((cfg.start, cfg.count), (1, 1));
    assert_eq!(
        (
            cfg.speed,
            cfg.effect_volume,
            cfg.music_volume,
            cfg.inject_volume
        ),
        (1.25, 1.0, 1.0, 1.0)
    );
}

#[test]
fn mix_config_parses_ranges_and_rejects_garbage() {
    // The prompt validates; the op itself saves, so a typo keeps the prompt
    assert_eq!(
        parse_mix_config("1.25 1.0 1.0 0.5").unwrap(),
        (1.25, 1.0, 1.0, Some(0.5))
    );
    assert_eq!(
        parse_mix_config("0.5 0 2 1").unwrap(),
        (0.5, 0.0, 2.0, Some(1.0))
    );
    assert!(parse_mix_config("1.25 1.0")
        .unwrap_err()
        .contains("expected"));
    assert_eq!(
        parse_mix_config("1.25 1.0 1.0").unwrap(),
        (1.25, 1.0, 1.0, None)
    );
    for input in ["1 1 1 -0.1", "1 1 1 NaN", "1 1 1 inf"] {
        assert!(parse_mix_config(input).unwrap_err().contains("inject"));
    }
    assert!(parse_mix_config("1 1 1 1 1")
        .unwrap_err()
        .contains("expected"));
    assert!(parse_mix_config("0.4 1 1 1").unwrap_err().contains("speed"));
    assert!(parse_mix_config("2.1 1 1 1").unwrap_err().contains("speed"));
    assert!(parse_mix_config("1 3 1 1").unwrap_err().contains("fx"));
    assert!(parse_mix_config("1 1 -0.1 1")
        .unwrap_err()
        .contains("music"));
    assert!(parse_mix_config("1 1 1 3").unwrap_err().contains("inject"));
    assert!(parse_mix_config("1 x 1 1")
        .unwrap_err()
        .contains("not a number"));
}
