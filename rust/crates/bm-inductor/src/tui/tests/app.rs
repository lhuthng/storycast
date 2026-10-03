use super::*;

#[test]
fn add_machine_rejects_whitespace_addresses() {
    // The bind prompt is a tuple now (`addr [user [port [key]]]`), so a
    let mut app = App::new("http://x");
    let p = TextPrompt::new(TextKind::AddMachine, "t", "h", "192.168.2.7 extra");
    match submit_text(&mut app, &p) {
        Ok(Job::AddMachine { m, .. }) => {
            assert_eq!(m.addr, "192.168.2.7");
            assert_eq!(m.ssh_user, "extra");
        }
        other => panic!("second token is the user now, got {other:?}"),
    }
    let p = TextPrompt::new(TextKind::AddMachine, "t", "h", "  ");
    assert!(submit_text(&mut app, &p).unwrap_err().contains("empty"));
}

#[test]
fn scroll_clamping_keeps_the_cursor_visible() {
    let mut scroll = 0;
    clamp_scroll(0, &mut scroll, 100, 10);
    assert_eq!(scroll, 0);
    clamp_scroll(15, &mut scroll, 100, 10);
    assert_eq!(
        scroll, 8,
        "cursor 15 keeps two lookahead rows in a 10-row window"
    );
    clamp_scroll(2, &mut scroll, 100, 10);
    assert_eq!(scroll, 2);
    // A short list must not scroll past its end.
    let mut s2 = 5;
    clamp_scroll(0, &mut s2, 3, 10);
    assert_eq!(s2, 0);
    // Near the end the padding collapses: there is nothing below to show.
    let mut s3 = 0;
    clamp_scroll(99, &mut s3, 100, 10);
    assert_eq!(s3, 90);
}

#[test]
fn seen_label_says_never_rather_than_a_fifty_year_uptime() {
    let mut m = Machine::new("10.0.0.5", "u", 22, None, "worker");
    assert_eq!(seen_label(&m), "never");
    m.last_seen = bm_proto::now_secs().saturating_sub(5);
    assert_eq!(seen_label(&m), "5s");
    m.last_seen = bm_proto::now_secs().saturating_sub(120);
    assert_eq!(seen_label(&m), "2m");
    m.last_seen = bm_proto::now_secs().saturating_sub(7200);
    assert_eq!(seen_label(&m), "2h");
}

#[test]
fn state_age_says_unknown_rather_than_a_fifty_year_boot() {
    // The same trap `seen_label` has, one field over: `state_since == 0` means
    let mut m = Machine::new("10.0.0.5", "u", 22, None, "worker");
    assert_eq!(state_age_label(&m), "—", "never stamped is not 0s ago");

    m.set_state(MachineState::Initializing);
    assert_eq!(state_age_label(&m), "0s", "just launched");

    m.state_since = bm_proto::now_secs().saturating_sub(45);
    assert_eq!(state_age_label(&m), "45s");
    m.state_since = bm_proto::now_secs().saturating_sub(120);
    assert_eq!(state_age_label(&m), "2m", "a long boot reads in minutes");
    m.state_since = bm_proto::now_secs().saturating_sub(7200);
    assert_eq!(state_age_label(&m), "2h");
}

#[test]
fn a_booting_box_that_never_answered_ssh_is_not_called_broken() {
    // `:prov` seconds after `:up` is the likeliest way to meet a box whose
    assert_eq!(
        verdict_after_failed_provision(true, false),
        MachineState::Initializing
    );
}

#[test]
fn a_box_that_answered_but_failed_a_step_is_broken_even_while_booting() {
    // The boundary that makes the rule above safe rather than a blanket
    assert_eq!(
        verdict_after_failed_provision(true, true),
        MachineState::Error
    );
}

#[test]
fn an_unreachable_box_we_never_thought_was_booting_is_broken() {
    // The other half of the boundary: without this, every unreachable box
    assert_eq!(
        verdict_after_failed_provision(false, false),
        MachineState::Error
    );
}

#[test]
fn stages_and_states_have_distinct_palettes() {
    // The old build coloured the Workers stage column with the task-state
    assert_eq!(stage_color("render"), Color::Cyan);
    assert_eq!(state_color("online"), Color::Green);
    assert_ne!(stage_color("render"), state_color("render"));
}

#[test]
fn users_of_lists_every_character_on_a_voice() {
    let mut cast = BTreeMap::new();
    cast.insert("Narrator".to_string(), "Đức Trí".to_string());
    cast.insert("A".to_string(), "Đức Trí".to_string());
    cast.insert("B".to_string(), "Adam".to_string());
    assert_eq!(users_of(&cast, "Đức Trí").len(), 2);
    assert_eq!(users_of(&cast, "Adam"), vec!["B".to_string()]);
    assert!(users_of(&cast, "Nobody").is_empty());
}

#[test]
fn urlencode_leaves_hostnames_alone_and_escapes_the_rest() {
    assert_eq!(urlencode("192.168.2.7"), "192.168.2.7");
    assert_eq!(urlencode("host name"), "host%20name");
}

#[test]
fn wall_clock_stamps_read_as_local_hh_mm_ss() {
    // Shape, not value: the machine's timezone is whatever it is.
    for epoch in [1u64, 1_789_485_796u64] {
        let s = wall_hms(epoch);
        assert_eq!(s.len(), 8, "{s}");
        assert_eq!(&s[2..3], ":");
        assert_eq!(&s[5..6], ":");
        assert!(
            s.chars().filter(|c| *c != ':').all(|c| c.is_ascii_digit()),
            "{s}"
        );
    }
}

#[test]
fn log_heads_alias_machines_and_workers_but_not_sentences() {
    assert_eq!(log_head("[192.168.2.2] enrolled x"), Some("192.168.2.2"));
    assert_eq!(
        log_head("localhost-4578: render done"),
        Some("localhost-4578")
    );
    assert_eq!(
        log_head("DESKTOP-V1JNVB0-18150: digest done"),
        Some("DESKTOP-V1JNVB0-18150")
    );
    assert_eq!(log_head("reconcile: nothing to fold"), None);
    assert_eq!(log_head("render:52 done"), None);
    assert_eq!(log_head("backend starting"), None);
    assert_eq!(log_head("[broken"), None);
}

#[test]
fn retry_scopes_narrow_by_argument_and_refuse_a_bare_stage() {
    // `:retry` is the only way to aim a requeue at one chapter from the main
    fn retry(stage: Option<Stage>, chapter: Option<u32>) -> Option<Command> {
        Some(Command::Retry { stage, chapter })
    }
    assert_eq!(
        command_key("retry"),
        retry(None, None),
        "no argument is the blanket retry"
    );
    assert_eq!(command_key("retry 24"), retry(None, Some(24)));
    assert_eq!(
        command_key("retry render 24"),
        retry(Some(Stage::Render), Some(24))
    );
    assert_eq!(
        command_key("u merge 7"),
        retry(Some(Stage::Merge), Some(7)),
        "the single-letter form takes the same arguments"
    );
    assert_eq!(
        command_key("retry RENDER 24"),
        retry(Some(Stage::Render), Some(24)),
        "stage names are case-insensitive"
    );

    // Refusals. Each would otherwise run something at the wrong scope, and the
    assert_eq!(command_key("retry render"), None, "a bare stage is refused");
    assert_eq!(command_key("retry 0"), None, "chapter 0 is not a chapter");
    assert_eq!(command_key("retry ch24"), None, "no `ch` prefix");
    assert_eq!(command_key("retry r 24"), None, "no single-letter stages");
    assert_eq!(command_key("retry render 24 extra"), None, "one scope only");
    assert_eq!(command_key("retry boss 24"), None, "no such stage");
}

#[test]
fn command_line_maps_keys_and_words() {
    assert_eq!(command_key("m"), Some(Command::Reconcile));
    assert_eq!(command_key("B"), Some(Command::Backend));
    assert_eq!(command_key("?"), Some(Command::Key(KeyCode::Char('?'))));
    assert_eq!(
        command_key("u"),
        Some(Command::Retry {
            stage: None,
            chapter: None
        }),
        "single chars are commands"
    );
    assert_eq!(command_key("r"), Some(Command::Key(KeyCode::Char('r'))));
    assert_eq!(command_key("reconcile"), Some(Command::Reconcile));
    // The operator's case, verbatim: a character name is three words, so the
    assert_eq!(
        command_key("speaker 18 67 \"Thanh Sơn lão tổ\" \"Dịch Phong\""),
        Some(Command::FixSpeaker {
            chapter: 18,
            segment: 67,
            expect: "Thanh Sơn lão tổ".into(),
            speaker: "Dịch Phong".into(),
        })
    );
    // Single-word names need no quotes, and an unquoted multi-word one is
    assert_eq!(
        command_key("speaker 3 1 A Narrator"),
        Some(Command::FixSpeaker {
            chapter: 3,
            segment: 1,
            expect: "A".into(),
            speaker: "Narrator".into(),
        })
    );
    for bad in [
        "speaker 18 67 \"Thanh Sơn lão tổ\" Dịch Phong", // five arguments
        "speaker 18 67",                                 // nothing to change
        "speaker 0 67 A B",                              // chapter 0 is not a chapter
        "speaker 18 0 A B",                              // segments are 1-based
        "speaker eighteen 67 A B",
    ] {
        assert_eq!(command_key(bad), None, "{bad} must not parse");
    }
    assert_eq!(command_key("rerender"), Some(Command::Rerender));
    assert_eq!(command_key("remerge"), Some(Command::Remerge));
    // Merge names the pair: first survives, the rest are absorbed. Quotes
    assert_eq!(
        command_key("merge \"Huyền Vũ\" \"Huyền Vũ lão tổ\""),
        Some(Command::Merge {
            survivor: "Huyền Vũ".into(),
            absorbed: vec!["Huyền Vũ lão tổ".into()],
        })
    );
    assert_eq!(
        command_key("merge A B C"),
        Some(Command::Merge {
            survivor: "A".into(),
            absorbed: vec!["B".into(), "C".into()],
        })
    );
    for bad in ["merge A", "merge \"\" B", "merge A \"\""] {
        assert_eq!(command_key(bad), None, "{bad} must not parse");
    }
    // Bare `:merge` parses to the placeholder and is refused at dispatch
    assert_eq!(
        command_key("merge"),
        Some(Command::Merge {
            survivor: String::new(),
            absorbed: Vec::new(),
        })
    );
    assert_eq!(command_key("backend"), Some(Command::Backend));
    assert_eq!(command_key("stop"), Some(Command::Stop));
    assert_eq!(command_key("quit"), Some(Command::Key(KeyCode::Char('q'))));
    assert_eq!(command_key("exit"), Some(Command::Key(KeyCode::Char('q'))));
    assert_eq!(command_key("q"), Some(Command::Key(KeyCode::Char('q'))));
    assert_eq!(
        command_key("prov"),
        Some(Command::Provision { force: false })
    );
    assert_eq!(
        command_key("reprov"),
        Some(Command::Provision { force: true })
    );
    assert_eq!(command_key("remove"), Some(Command::DropMachine));
    assert_eq!(command_key("ADD"), Some(Command::AddMachine));
    assert_eq!(command_key("current"), Some(Command::AuditionCurrent));
    assert_eq!(command_key("cur"), Some(Command::AuditionCurrent));
    assert_eq!(command_key("try"), Some(Command::AuditionTry));
    assert_eq!(command_key("test"), Some(Command::AuditionTry));
    assert_eq!(command_key("another"), Some(Command::AuditionAnother));
    assert_eq!(command_key("change"), Some(Command::AuditionAnother));
    assert_eq!(command_key("next"), Some(Command::AuditionAnother));
    assert_eq!(command_key("drain"), Some(Command::ShutdownWhenIdle));
    assert_eq!(
        command_key("colour"),
        Some(Command::Key(KeyCode::Char('C')))
    );
    assert_eq!(command_key("color"), Some(Command::Key(KeyCode::Char('C'))));
    assert_eq!(command_key(":"), None, "a bare colon reopens nothing");
    assert_eq!(command_key("frobnicate"), None);
    assert_eq!(command_key(""), None);
}

#[test]
fn app_starts_on_normal_with_a_hint_not_a_blank_status() {
    let app = App::new("http://127.0.0.1:8901/");
    assert_eq!(
        app.api, "http://127.0.0.1:8901",
        "trailing slash is trimmed"
    );
    assert!(matches!(app.screen, Screen::Normal));
    assert!(!app.status.text.is_empty());
    assert_eq!(app.pending, 0);
    assert!(app.colour());
}
