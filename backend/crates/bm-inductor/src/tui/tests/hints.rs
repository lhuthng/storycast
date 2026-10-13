use super::*;

// --- the hint audit: every hint a screen draws is a promise about its keys

#[test]
fn the_cast_overview_hints_name_only_keys_the_screen_handles() {
    // The cast rows used to advertise a bare `v` (a gated `:` command) and a
    let mut app = App::new("http://127.0.0.1:8901");
    app.roster = Some(roster_fixture());
    app.screen = Screen::Cast(CastView {
        filter: "zzz".into(),
        ..CastView::new()
    });
    let text = render_text(&mut app, 140, 44);
    assert!(
        text.contains("Backspace widens it, Ctrl-U clears"),
        "the empty state names the real editing keys:\n{text}"
    );
    assert!(
        !text.contains("Backspace clears it"),
        "the dead advice is gone:\n{text}"
    );
    app.screen = Screen::Cast(CastView::new());
    let text = render_text(&mut app, 140, 44);
    assert!(
        text.contains("— :v fills gaps"),
        "the gated command, with its colon:\n{text}"
    );
    assert!(
        !text.contains("— v fills gaps"),
        "no bare `v`, which types into the filter:\n{text}"
    );
}

#[test]
fn the_cast_and_picker_empty_states_point_at_the_gated_commands() {
    // `t` and `v` were removed from Normal mode; an empty speaker list that
    let mut app = App::new("http://127.0.0.1:8901");
    // A roster that loaded but knows no speakers: the state the empty-body
    let mut roster = roster_fixture();
    roster.characters.clear();
    roster.cast.clear();
    app.roster = Some(roster);
    app.screen = Screen::Cast(CastView::new());
    let text = render_text(&mut app, 140, 44);
    assert!(
        text.contains(":t (translate) or :v (voices) first"),
        "\n{text}"
    );
    app.screen = Screen::Pick(Picker::new());
    let text = render_text(&mut app, 140, 44);
    assert!(
        text.contains(":t (translate) or :v (voices) first"),
        "the picker says the same thing the same way:\n{text}"
    );
}

#[test]
fn the_cloud_error_names_the_real_command_words() {
    // There are no `aws login` / `aws discover` words, the setup commands
    let mut app = App::new("http://127.0.0.1:8901");
    app.cloud_error = Some("no credentials".into());
    app.screen = Screen::Cloud(CloudView::new());
    let text = render_text(&mut app, 140, 44);
    assert!(
        text.contains("check :login / :discover, then r to retry"),
        "\n{text}"
    );
    assert!(!text.contains("aws login"), "{text}");
}

#[test]
fn screens_without_a_reload_key_do_not_advertise_one() {
    // Two overlays told the operator to press a key they do not handle:
    let mut app = App::new("http://127.0.0.1:8901");
    app.conn = Conn::Down("boom".to_string());
    app.screen = Screen::Run;
    let text = render_text(&mut app, 100, 34);
    assert!(
        text.contains("Esc, then R on the dashboard"),
        "the run screen names its own way out:\n{text}"
    );

    let mut app = App::new("http://127.0.0.1:8901");
    let mut m = named_machine("192.168.2.2", "hawk");
    // Every stage off: the one state in which the "none enabled" line draws.
    m.task_policy = Some(vec![
        TaskPref {
            stage: Stage::Merge,
            enabled: false,
        },
        TaskPref {
            stage: Stage::Render,
            enabled: false,
        },
        TaskPref {
            stage: Stage::Digest,
            enabled: false,
        },
        TaskPref {
            stage: Stage::Crawl,
            enabled: false,
        },
    ]);
    app.machines = vec![m];
    app.screen = Screen::Machine("192.168.2.2".to_string());
    let text = render_text(&mut app, 140, 44);
    assert!(
        text.contains("Esc, then P on the dashboard"),
        "the machine screen says where P actually lives:\n{text}"
    );
}

#[test]
fn the_task_ledger_hints_the_movement_it_actually_binds() {
    // Letters type into the filter here, so `j`/`k` never moved anything
    let mut app = tasks_app();
    app.screen = Screen::Tasks(TasksView::new());
    let text = render_text(&mut app, 140, 44);
    assert!(
        text.contains("↑/↓ move"),
        "the movement the ledger really has:\n{text}"
    );
    assert!(
        !text.contains("j/k"),
        "the keys that type into the filter are not advertised:\n{text}"
    );
}

#[test]
fn split_args_honours_quotes_so_a_speaker_name_is_one_argument() {
    // Everything `:speaker` needs, and the cases a shell would have opinions
    assert_eq!(
        split_args("18 67 \"Thanh Sơn lão tổ\" \"Dịch Phong\""),
        vec!["18", "67", "Thanh Sơn lão tổ", "Dịch Phong"]
    );
    assert_eq!(split_args("a b  c"), vec!["a", "b", "c"], "runs collapse");
    assert_eq!(
        split_args("\"\""),
        vec![""],
        "an empty quoted arg is an arg"
    );
    assert_eq!(
        split_args("a\"b c\"d"),
        vec!["ab cd"],
        "a quote mid-word opens"
    );
    assert_eq!(
        split_args("\"unterminated tail"),
        vec!["unterminated tail"],
        "an unterminated quote takes the rest rather than erroring"
    );
    assert!(
        split_args("   ").is_empty(),
        "whitespace is not an argument"
    );
}
