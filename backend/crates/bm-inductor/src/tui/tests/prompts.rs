use super::*;

#[test]
fn workspace_prompt_parses_list_use_and_new() {
    // One prompt, three verbs, parsed at submit so a typo keeps the prompt
    let mut app = App::new("http://x");
    let prompt = |buf: &str| TextPrompt::new(TextKind::Workspace, "t", "h", buf);

    assert!(matches!(
        submit_text(&mut app, &prompt("")),
        Ok(Job::Workspace {
            req: WorkspaceReq::List,
            ..
        })
    ));
    assert!(matches!(
        submit_text(&mut app, &prompt("  beyond-myriads  ")),
        Ok(Job::Workspace {
            req: WorkspaceReq::Use(ref n),
            ..
        }) if n == "beyond-myriads"
    ));
    assert!(matches!(
        submit_text(&mut app, &prompt("new second-book")),
        Ok(Job::Workspace {
            req: WorkspaceReq::New { name: ref n, profile: None, crawler: None },
            ..
        }) if n == "second-book"
    ));
    // `--profile` selects a preset at creation, and the id is checked against
    let presets = app.layout.root.join("profiles/presets.json");
    std::fs::create_dir_all(presets.parent().unwrap()).unwrap();
    std::fs::write(
        &presets,
        r#"{"jnovel-en": {"label": "JNovel (en)", "pack": "",
            "pack_deps": ["common"], "adapter": "jnovel-en-US", "engine": "pocket"}}"#,
    )
    .unwrap();
    assert!(matches!(
        submit_text(&mut app, &prompt("new second-book --profile jnovel-en")),
        Ok(Job::Workspace {
            req: WorkspaceReq::New { name: ref n, profile: Some(ref p), crawler: None },
            ..
        }) if n == "second-book" && p == "jnovel-en"
    ));
    // An unknown preset is a refused line, not a queued job.
    let err = submit_text(&mut app, &prompt("new second-book --profile nope")).unwrap_err();
    assert!(err.contains("no preset"), "{err}");
    assert!(
        submit_text(&mut app, &prompt("new second-book --profile")).is_err(),
        "a bare --profile is refused"
    );

    // A name is one path segment: `../x` would escape workspaces/.
    for bad in ["../x", "a/b", "new ", "."] {
        assert!(
            submit_text(&mut app, &prompt(bad)).is_err(),
            "“{bad}” must be refused"
        );
    }
}

#[test]
fn the_ws_command_line_carries_the_name_to_the_prompt() {
    // `:ws <name>` matched no arm in the splitter, fell through to the word
    let prefill = |line: &str| match command_key(line) {
        Some(Command::Workspace { prefill }) => prefill,
        other => panic!("`:{line}` gave {other:?}, not the workspace prompt"),
    };

    assert_eq!(prefill("ws beyond-myriads"), "beyond-myriads");
    // A book's title has spaces in it, so every word after `ws` is the name and
    assert_eq!(prefill("ws beyond myriads"), "beyond myriads");
    // The whole line travels, which is what carries the other two verbs:
    assert_eq!(prefill("ws new second-book"), "new second-book");
    assert_eq!(
        prefill("workspace new second-book --profile jnovel-en"),
        "new second-book --profile jnovel-en"
    );

    // Bare `:ws` is the picker — the whole point of it — and the word table
    assert!(
        matches!(command_key("ws"), Some(Command::WorkspacePick)),
        "{:?}",
        command_key("ws")
    );
    assert!(
        matches!(command_key("workspace"), Some(Command::WorkspacePick)),
        "{:?}",
        command_key("workspace")
    );
    assert_eq!(
        WORDS
            .iter()
            .find(|w| w.names.contains(&"ws"))
            .expect("ws is in the word table")
            .cmd,
        Command::WorkspacePick,
        "the word table must not claim bare `:ws` opens the prompt"
    );
}

#[test]
fn the_ws_command_line_ends_in_the_workspace_it_named() {
    // The splitter test above proves the name is carried; this proves it is
    let mut app = App::new("http://x");
    let http = reqwest::Client::new();
    let (job_tx, _job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();

    do_command(
        &mut app,
        command_key("ws beyond myriads").unwrap(),
        &http,
        &job_tx,
    );
    let Screen::Text(p) = app.screen.clone() else {
        panic!("the prompt opens, got {:?}", app.screen);
    };
    assert_eq!(
        p.buf, "beyond myriads",
        "the prompt must hold what was typed, not the workspace in force"
    );
    // Enter from there is the switch the recipe meant all along.
    assert!(matches!(
        submit_text(&mut app, &p),
        Ok(Job::Workspace {
            req: WorkspaceReq::Use(ref n),
            ..
        }) if n == "beyond myriads"
    ));
}

#[test]
fn profile_prompt_parses_list_load_and_pack() {
    let mut app = App::new("http://x");
    let prompt = |buf: &str| TextPrompt::new(TextKind::Profile, "t", "h", buf);

    assert!(matches!(
        submit_text(&mut app, &prompt("")),
        Ok(Job::Profile {
            req: ProfileReq::List,
            ..
        })
    ));
    // A bare name loads, the common case needs no verb.
    assert!(matches!(
        submit_text(&mut app, &prompt("xianxia")),
        Ok(Job::Profile {
            req: ProfileReq::Load(ref n),
            ..
        }) if n == "xianxia"
    ));
    assert!(matches!(
        submit_text(&mut app, &prompt("pack xianxia")),
        Ok(Job::Profile {
            req: ProfileReq::Pack(ref n),
            ..
        }) if n == "xianxia"
    ));
    assert!(submit_text(&mut app, &prompt("pack ")).is_err());
}

#[test]
fn the_footer_names_the_active_workspace_and_the_loaded_profile() {
    // `default` is the implicit root workspace, not a missing name: a fresh
    let l = bm_core::Layout::new("/repo");
    assert_eq!(super::model::workspace_label(&l), "default");
    let named = bm_core::Layout {
        root: "/repo".into(),
        work: "/repo/workspaces/beyond-myriads".into(),
        ..bm_core::Layout::new("/repo")
    };
    assert_eq!(super::model::workspace_label(&named), "beyond-myriads");
    // No profile is the state every runner refuses to start in, so it is
    assert_eq!(super::model::profile_label(None), "none");
    let p = bm_core::profile::Binding {
        pack: bm_core::profile::Pointer {
            name: "xianxia".into(),
            hash: "0123456789abcdef".into(),
            ..Default::default()
        },
        ..Default::default()
    };
    assert_eq!(
        super::model::profile_label(Some(&p)),
        "xianxia (0123456789ab)"
    );
}
