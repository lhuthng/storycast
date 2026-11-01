use super::*;

#[test]
fn bind_prompt_parses_the_tuple_and_falls_back_to_settings() {
    let mut app = bind_app();
    // Bare address: user/port from settings.ssh, no key means ssh decides.
    let m = bind_machine(&mut app, "192.168.2.7");
    assert_eq!(
        (m.addr.as_str(), m.ssh_user.as_str(), m.ssh_port, m.ssh_key),
        ("192.168.2.7", "op", 2222, None)
    );

    // Full tuple overrides everything.
    let dir = std::env::temp_dir().join("bm-bind-test");
    std::fs::create_dir_all(&dir).unwrap();
    let key = dir.join("id_bind");
    std::fs::write(&key, "k").unwrap();
    let m = bind_machine(&mut app, &format!("10.0.0.1 root 22 {}", key.display()));
    assert_eq!(m.ssh_user.as_str(), "root");
    assert_eq!(m.ssh_port, 22);
    assert_eq!(m.ssh_key.as_deref(), Some(key.to_str().unwrap()));

    // The key is the remainder of the line, so paths with spaces survive.
    let spaced = dir.join("my key");
    std::fs::write(&spaced, "k").unwrap();
    let m = bind_machine(&mut app, &format!("10.0.0.2 u 22 {}", spaced.display()));
    assert_eq!(
        m.ssh_key.as_deref(),
        Some(spaced.to_str().unwrap()),
        "key keeps its spacing"
    );

    assert!(submit_text(
        &mut app,
        &TextPrompt::new(TextKind::AddMachine, "t", "h", "")
    )
    .unwrap_err()
    .contains("address is empty"));
    assert!(submit_text(
        &mut app,
        &TextPrompt::new(TextKind::AddMachine, "t", "h", "h u xx")
    )
    .unwrap_err()
    .contains("not a number"));
}

#[test]
fn bind_prompt_with_a_missing_key_keeps_the_prompt_open() {
    // The keep-open contract: a mispointed key names the expanded path
    let mut app = bind_app();
    let home = std::env::var("HOME").unwrap();
    let err = submit_text(
        &mut app,
        &TextPrompt::new(
            TextKind::AddMachine,
            "t",
            "h",
            "10.0.0.3 u 22 ~/.ssh/no-such-key",
        ),
    )
    .unwrap_err();
    assert!(
        err.contains(&format!("{home}/.ssh/no-such-key")),
        "names the expanded path, got: {err}"
    );
}

#[test]
fn ssh_default_commands_save_validate_and_clear() {
    let dir = std::env::temp_dir().join("bm-ssh-defaults-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut app = App::new("http://x");
    app.layout = bm_core::Layout::new(&dir);
    let settings_path = dir.join(".bm").join("settings.json");
    let load = || bm_core::config::Settings::load(&settings_path);

    let key = dir.join("id_def");
    std::fs::write(&key, "k").unwrap();
    let msg = save_app_setting(&app, TextKind::SshKey, key.to_str().unwrap()).unwrap();
    assert!(msg.contains("saved"), "{msg}");
    assert_eq!(load().ssh.key.as_deref(), Some(key.to_str().unwrap()));
    // Clearing is a real answer: ssh decides per machine afterwards.
    save_app_setting(&app, TextKind::SshKey, "  ").unwrap();
    assert_eq!(load().ssh.key, None);
    // A missing file keeps the prompt open, it never saves garbage.
    let err = save_app_setting(&app, TextKind::SshKey, "/nonexistent/k").unwrap_err();
    assert!(err.contains("/nonexistent/k"), "{err}");
    assert_eq!(load().ssh.key, None);

    save_app_setting(&app, TextKind::SshUser, "worker").unwrap();
    assert_eq!(load().ssh.user, "worker");
    assert!(save_app_setting(&app, TextKind::SshUser, "  ")
        .unwrap_err()
        .contains("empty"));

    assert!(save_app_setting(&app, TextKind::SshPort, "abc")
        .unwrap_err()
        .contains("not a number"));
    save_app_setting(&app, TextKind::SshPort, "2222").unwrap();
    assert_eq!(load().ssh.port, 2222);
}

#[test]
fn the_models_release_setting_saves_a_repo_and_refuses_one_that_is_not() {
    // The setting decides whether a box fetches 363 MB from a CDN or receives
    let dir = std::env::temp_dir().join("bm-models-release-save");
    let _ = std::fs::remove_dir_all(&dir);
    let mut app = App::new("http://x");
    app.layout = bm_core::Layout::new(&dir);
    let load = || bm_core::config::Settings::load(&bm_core::Layout::new(&dir).settings());

    let msg = save_app_setting(&app, TextKind::ModelsRelease, "lhuthng/storycast").unwrap();
    assert!(msg.contains("lhuthng/storycast"), "{msg}");
    assert_eq!(load().models_release, "lhuthng/storycast");

    for bad in ["storycast", "a/b/c", "own er/name"] {
        let err = save_app_setting(&app, TextKind::ModelsRelease, bad).unwrap_err();
        assert!(err.contains("owner/name"), "`{bad}`: {err}");
    }
    // A refused value never lands, so the good one above is still in force.
    assert_eq!(load().models_release, "lhuthng/storycast");

    // Empty is the push, and it has to be reachable without a text editor:
    let msg = save_app_setting(&app, TextKind::ModelsRelease, "  ").unwrap();
    assert!(msg.contains("push"), "{msg}");
    assert_eq!(load().models_release, "");
}

/// The pack release setting, and the one thing it deliberately does **not** ask
#[test]
fn the_pack_release_setting_takes_a_repo_and_reads_the_tag_from_the_pointer() {
    let dir = std::env::temp_dir().join(format!("bm-tui-packrelease-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut app = App::new("http://x");
    app.layout = bm_core::Layout::new(&dir);
    let load = || bm_core::config::Settings::load(&bm_core::Layout::new(&dir).settings());

    // No profile loaded: the repo is still saved, and the message says why
    let msg = save_app_setting(&app, TextKind::PacksRelease, "lhuthng/storycast").unwrap();
    assert!(!msg.is_empty());
    assert_eq!(load().packs_release, "lhuthng/storycast");

    // A pointer with no version — every checkout from before versions existed.
    bm_core::profile::write_pointer(
        &dir,
        &bm_core::profile::Pointer {
            name: "xianxia".into(),
            hash: "aa".into(),
            ..Default::default()
        },
    )
    .unwrap();
    let msg = save_app_setting(&app, TextKind::PacksRelease, "lhuthng/storycast").unwrap();
    assert!(msg.contains("no version"), "{msg}");

    // Versioned: the message names the tag, which is the thing an operator
    bm_core::profile::write_pointer(
        &dir,
        &bm_core::profile::Pointer {
            name: "xianxia".into(),
            hash: "aa".into(),
            version: "0.1.0".into(),
        },
    )
    .unwrap();
    let msg = save_app_setting(&app, TextKind::PacksRelease, "lhuthng/storycast").unwrap();
    assert!(msg.contains("xianxia v0.1.0"), "{msg}");
    assert!(msg.contains("lhuthng/storycast"), "{msg}");

    // The repo is validated the same way the models one is.
    for bad in ["storycast", "a/b/c", "own er/name"] {
        let err = save_app_setting(&app, TextKind::PacksRelease, bad).unwrap_err();
        assert!(err.contains("owner/name"), "`{bad}`: {err}");
    }
    assert_eq!(
        load().packs_release,
        "lhuthng/storycast",
        "a refused value never lands"
    );

    // Empty is the push, which is what every box did before the setting.
    let msg = save_app_setting(&app, TextKind::PacksRelease, "  ").unwrap();
    assert!(msg.contains("push"), "{msg}");
    assert_eq!(load().packs_release, "");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn render_batch_parses_its_bounds_and_saves_to_this_workspaces_settings() {
    // The knob's own rules in one place. `0` is the value that would deadlock
    let dir = std::env::temp_dir().join("bm-renderbatch-save");
    let _ = std::fs::remove_dir_all(&dir);
    let mut app = App::new("http://x");
    app.layout = bm_core::Layout::new(&dir);
    let load = || bm_core::config::Settings::load(&bm_core::Layout::new(&dir).settings());

    assert_eq!(parse_render_batch("12").unwrap(), 12);
    assert_eq!(
        parse_render_batch(" 3 ").unwrap(),
        3,
        "whitespace is not a typo"
    );
    assert!(parse_render_batch("").unwrap_err().contains("not a number"));
    assert!(parse_render_batch("ten")
        .unwrap_err()
        .contains("not a number"));
    assert!(parse_render_batch("-2")
        .unwrap_err()
        .contains("not a number"));
    assert!(parse_render_batch("0").unwrap_err().contains("at least 1"));
    let too_big = parse_render_batch("65").unwrap_err();
    assert!(too_big.contains("at most 64"), "{too_big}");

    let msg = save_render_batch(&app, "4").unwrap();
    assert!(msg.contains("4 take"), "{msg}");
    assert_eq!(
        load().render_batch,
        4,
        "the file is what the scheduler reads"
    );

    // A refused value writes nothing, a typo must not clear what is in force.
    assert!(save_render_batch(&app, "0").is_err());
    assert_eq!(
        load().render_batch,
        4,
        "and the old value survives the typo"
    );
}

#[tokio::test]
async fn the_batch_command_opens_a_prefilled_prompt_and_enter_saves_it() {
    // End to end through the key chain: the word routes, the prompt opens on
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let dir = std::env::temp_dir().join("bm-renderbatch-prompt");
    let _ = std::fs::remove_dir_all(&dir);
    let mut app = App::new("http://x");
    app.layout = bm_core::Layout::new(&dir);

    assert_eq!(command_key("batch"), Some(Command::RenderBatch));
    assert_eq!(command_key("renderbatch"), Some(Command::RenderBatch));
    assert_eq!(
        command_key("b"),
        Some(Command::Key(KeyCode::Char('b'))),
        "no single-key form: `b` is not the batch"
    );

    // Nothing saved yet, so the prompt shows the compiled default.
    do_command(&mut app, Command::RenderBatch, &http, &job_tx);
    match &app.screen {
        Screen::Text(p) => {
            assert_eq!(p.kind, TextKind::RenderBatch);
            assert_eq!(
                p.buf,
                bm_core::config::DEFAULT_RENDER_BATCH.to_string(),
                "prefilled with what is in force"
            );
        }
        other => panic!("expected the batch prompt, got {other:?}"),
    }
    assert!(
        job_rx.try_recv().is_err(),
        "a save-only prompt dispatches nothing"
    );

    // A bad value keeps the prompt open with the problem stated in place.
    if let Screen::Text(p) = &mut app.screen {
        p.buf = "0".into();
    }
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    assert!(
        matches!(app.screen, Screen::Text(_)),
        "the prompt stays open: {:?}",
        app.screen
    );
    assert!(
        !bm_core::Layout::new(&dir).settings().is_file(),
        "and a refused value wrote nothing at all — no file, not an empty one"
    );

    // A good one saves and closes, and the next prompt shows the new value.
    if let Screen::Text(p) = &mut app.screen {
        p.buf = "6".into();
    }
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    assert!(matches!(app.screen, Screen::Normal), "{:?}", app.screen);
    let saved = bm_core::config::Settings::load(&bm_core::Layout::new(&dir).settings());
    assert_eq!(saved.render_batch, 6);
    assert!(
        job_rx.try_recv().is_err(),
        "and it still dispatched nothing — the scheduler reads the file"
    );

    do_command(&mut app, Command::RenderBatch, &http, &job_tx);
    match &app.screen {
        Screen::Text(p) => assert_eq!(p.buf, "6", "prefilled from the file, not the default"),
        other => panic!("expected the batch prompt, got {other:?}"),
    }
}

#[tokio::test]
async fn threads_word_edits_the_selected_boxs_sidecar_threads() {
    // `:threads` opens the prompt prefilled; `:threads <n>` dispatches at once.
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = App::new("http://x");
    assert_eq!(
        command_key("threads"),
        Some(Command::TtsThreads { threads: None })
    );
    assert_eq!(
        command_key("threads 8"),
        Some(Command::TtsThreads {
            threads: Some(Some(8))
        })
    );
    assert_eq!(
        command_key("threads clear"),
        Some(Command::TtsThreads {
            threads: Some(None)
        })
    );
    assert_eq!(
        command_key("threads 0"),
        None,
        "0 is not a count — clear instead"
    );
    assert_eq!(command_key("threads 99"), None, "over the 64 cap");
    assert_eq!(command_key("threads abc"), None);

    // No machine selected: the command says so instead of opening a prompt.
    do_command(
        &mut app,
        Command::TtsThreads { threads: None },
        &http,
        &job_tx,
    );
    assert!(matches!(app.screen, Screen::Normal));

    let mut m = Machine::new("192.168.2.2", "thang", 22, None, "worker");
    m.tts_threads = Some(3);
    app.machines = vec![m];
    do_command(
        &mut app,
        Command::TtsThreads {
            threads: Some(Some(8)),
        },
        &http,
        &job_tx,
    );
    assert!(
        matches!(app.screen, Screen::Normal),
        "inline sets it at once: {:?}",
        app.screen
    );
    match job_rx.try_recv() {
        Ok(job) => match job.bare() {
            Job::SetTtsThreads { addr, threads, .. } => {
                assert_eq!(addr, "192.168.2.2");
                assert_eq!(*threads, Some(8));
            }
            other => panic!("expected SetTtsThreads, got {other:?}"),
        },
        Err(e) => panic!("no job dispatched: {e}"),
    }
    do_command(
        &mut app,
        Command::TtsThreads { threads: None },
        &http,
        &job_tx,
    );
    match &app.screen {
        Screen::Text(p) => {
            assert_eq!(p.kind, TextKind::TtsThreads);
            assert_eq!(p.buf, "3", "prefilled with the box's override");
        }
        other => panic!("expected the threads prompt, got {other:?}"),
    }

    // A bad value keeps the prompt open with the operator's typing still in it.
    if let Screen::Text(p) = &mut app.screen {
        p.buf = "0".into();
    }
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    assert!(
        matches!(app.screen, Screen::Text(_)),
        "the prompt stays open: {:?}",
        app.screen
    );

    // A good one closes and dispatches the per-box API edit.
    if let Screen::Text(p) = &mut app.screen {
        p.buf = "6".into();
    }
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    assert!(matches!(app.screen, Screen::Normal), "{:?}", app.screen);
    match job_rx.try_recv() {
        Ok(job) => match job.bare() {
            Job::SetTtsThreads { addr, threads, .. } => {
                assert_eq!(addr, "192.168.2.2");
                assert_eq!(*threads, Some(6));
            }
            other => panic!("expected SetTtsThreads, got {other:?}"),
        },
        Err(e) => panic!("no job dispatched: {e}"),
    }
}

#[test]
fn ssh_default_words_route_to_their_commands() {
    assert_eq!(command_key("sshkey"), Some(Command::SshKey));
    assert_eq!(command_key("sshuser"), Some(Command::SshUser));
    assert_eq!(command_key("sshport"), Some(Command::SshPort));
}

#[tokio::test]
async fn cast_opens_only_from_the_command_line() {
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = App::new("http://127.0.0.1:8901");
    // Bare S is inert: the overview is gated behind :S like every other
    handle_key(&mut app, key(KeyCode::Char('S')), &http, &job_tx).await;
    assert!(
        matches!(app.screen, Screen::Normal),
        "bare S must not open anything: {:?}",
        app.screen
    );
    assert!(job_rx.try_recv().is_err(), "bare S must not dispatch");
    // Both spellings name the same command.
    assert_eq!(command_key("S"), Some(Command::Cast));
    assert_eq!(command_key("cast"), Some(Command::Cast));
    app.screen = Screen::Text(TextPrompt::new(TextKind::Command, ":", "", "S"));
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    assert!(
        matches!(app.screen, Screen::Cast(_)),
        ":S opens the overview: {:?}",
        app.screen
    );
}

#[tokio::test]
async fn roster_job_shows_disk_first_without_contacting_anyone() {
    use std::time::Duration;
    let d = tempfile::tempdir().unwrap();
    let layout = bm_core::Layout::new(d.path());
    layout.ensure().unwrap();
    std::fs::write(layout.cast("vieneu"), r#"{"A":"Đức Trí"}"#).unwrap();
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Ev>();
    // Nothing listens on :9, the inductor hop fails fast. The local roster
    super::super::jobs::job_load_roster(tx, "http://127.0.0.1:9".into(), http, layout).await;
    let first = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("the local roster arrives fast")
        .expect("channel open");
    match first {
        Ev::Roster(Ok(r)) => {
            assert_eq!(r.source, "offline");
            assert_eq!(r.cast.get("A").map(String::as_str), Some("Đức Trí"));
            assert!(
                !r.voices.is_empty(),
                "catalogue lists voices with no sidecar"
            );
        }
        Ev::Roster(Err(e)) => panic!("local roster failed: {e}"),
        _ => panic!("the first event must be the local roster"),
    }
    // ...then the job's Done (the dead inductor contributes no upgrade).
    let second = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("Done follows")
        .expect("channel open");
    assert!(matches!(second, Ev::Done(DoneKind::Other)));
}

#[tokio::test]
async fn quit_word_quits_from_the_picker_command_line() {
    // The filter owns every letter on picker/cast, so a bare `q` types
    let http = reqwest::Client::new();
    let (job_tx, _job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = audition_app();
    app.pending = 0;
    let pick = app.screen.clone();
    app.command_return = Some(pick);
    app.screen = Screen::Text(TextPrompt::new(TextKind::Command, ":", "", "quit"));
    let quit = handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    assert_eq!(
        quit,
        super::input::Flow::Quit,
        ":quit from the picker must quit"
    );
}

#[tokio::test]
async fn audition_words_need_the_picker_or_cast() {
    // From anywhere else they name the way there instead of dispatching.
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = App::new("http://127.0.0.1:8901");
    assert!(matches!(app.screen, Screen::Normal));
    do_command(&mut app, Command::AuditionTry, &http, &job_tx);
    assert!(job_rx.try_recv().is_err(), "nothing to audition on");
    assert!(
        app.status.text.contains(":s") || app.status.text.contains("picker"),
        "name the way there: {:?}",
        app.status
    );
}

#[tokio::test]
async fn enter_in_the_cast_overview_goes_nowhere() {
    // The overview is read-only: swapping happens only in the picker.
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = App::new("http://127.0.0.1:8901");
    app.roster = Some(roster_fixture());
    app.screen = Screen::Cast(CastView::new());
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    assert!(
        matches!(app.screen, Screen::Cast(_)),
        "Enter must not leave the overview: {:?}",
        app.screen
    );
    assert!(job_rx.try_recv().is_err(), "Enter must not dispatch");
}

#[tokio::test]
async fn enter_on_a_voice_locks_its_sentence_for_later() {
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = audition_app();
    // Hear the candidate on a real line first.
    do_command(&mut app, Command::AuditionTry, &http, &job_tx);
    let line = last_op(&mut job_rx)
        .expect("dispatched")
        .text
        .clone()
        .unwrap();
    finish_audition(&mut app, "Adam");
    // Point at an allowed voice and pick it.
    match &mut app.screen {
        Screen::Pick(p) => {
            p.filter.clear();
            p.cursor = 0;
        }
        other => panic!("{other:?}"),
    }
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    assert!(
        matches!(app.screen, Screen::Confirm(_)),
        "Enter asks first: {:?}",
        app.screen
    );
    assert_eq!(
        app.locked_lines.get("Narrator").map(|l| l.text.clone()),
        Some(line.clone()),
        "the picked sentence is locked, not re-picked"
    );
    // Reopening the picker for them resumes on the locked sentence.
    let mut p = Picker::new();
    p.filter = "Narrator".into();
    app.screen = Screen::Pick(p);
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    match &app.screen {
        Screen::Pick(p) => {
            assert_eq!(p.stage, PickStage::Voice);
            assert_eq!(
                p.line.as_ref().map(|l| l.text.clone()),
                Some(line),
                "no fresh random pick for a locked character"
            );
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn the_stage_folder_words_open_prefilled_prompts_that_ask_use_or_move() {
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let dir = std::env::temp_dir().join("bm-stage-folders-prompt");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut app = App::new("http://x");
    app.layout = bm_core::Layout::new(&dir);
    fn load(app: &App) -> bm_core::config::Settings {
        bm_core::config::Settings::load(&app.layout.settings())
    }

    assert_eq!(command_key("output"), Some(Command::OutputDir));
    assert_eq!(command_key("outdir"), Some(Command::OutputDir));
    assert_eq!(command_key("segments"), Some(Command::SegmentsDir));
    assert_eq!(command_key("segdir"), Some(Command::SegmentsDir));
    assert_eq!(
        command_key("o"),
        Some(Command::AwsDown { force: false }),
        "no single-key form: `o` still terminates the pool"
    );

    // Empty on the default folder is a no-op, not a rewrite.
    do_command(&mut app, Command::OutputDir, &http, &job_tx);
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    assert!(matches!(app.screen, Screen::Normal), "{:?}", app.screen);
    assert_eq!(load(&app).output_dir, "");

    // A chapter is already finished in the default folder.
    std::fs::create_dir_all(app.layout.output()).unwrap();
    std::fs::write(app.layout.final_mp3(1), "audio").unwrap();
    let old_mp3 = app.layout.final_mp3(1);

    do_command(&mut app, Command::OutputDir, &http, &job_tx);
    match &app.screen {
        Screen::Text(p) => {
            assert_eq!(p.kind, TextKind::OutputDir);
            assert_eq!(p.buf, "", "nothing saved yet, so the default is implied");
        }
        other => panic!("expected the output prompt, got {other:?}"),
    }
    assert!(
        job_rx.try_recv().is_err(),
        "the prompt itself dispatches nothing"
    );

    // Enter on a path only asks: nothing is saved, created or moved.
    let picked = dir.join("Drive Output");
    if let Screen::Text(p) = &mut app.screen {
        p.buf = picked.to_str().unwrap().into();
    }
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    match &app.screen {
        Screen::Confirm(c) => {
            assert_eq!(c.title, "Point the output folder here?");
            assert!(
                c.body.iter().any(|l| l.contains("Drive Output")),
                "{:?}",
                c.body
            );
            assert!(
                c.body
                    .iter()
                    .any(|l| l.contains("Enter keeps the 1 item(s)")),
                "{:?}",
                c.body
            );
            match &c.action {
                ConfirmAction::StageDir { change } => {
                    assert_eq!(change.kind, bm_core::stage_dir::StageDir::Output);
                    assert_eq!(change.to, picked);
                    assert_eq!(change.from, old_mp3.parent().unwrap());
                }
                other => panic!("expected the folder choice, got {other:?}"),
            }
        }
        other => panic!("expected the folder choice, got {other:?}"),
    }
    assert_eq!(
        load(&app).output_dir,
        "",
        "nothing is saved before the answer"
    );
    assert!(!picked.exists(), "and nothing is created");
    assert!(old_mp3.is_file(), "the pair stays where it is");

    // Esc cancels the choice and steps back to the path, which is still
    // editable: the setting stands and nothing runs.
    handle_key(&mut app, key(KeyCode::Esc), &http, &job_tx).await;
    match &app.screen {
        Screen::Text(p) => assert_eq!(p.buf, picked.to_str().unwrap()),
        other => panic!("Esc returns to the path prompt, got {other:?}"),
    }
    assert_eq!(load(&app).output_dir, "");
    assert!(job_rx.try_recv().is_err());
    handle_key(&mut app, key(KeyCode::Esc), &http, &job_tx).await;
    assert!(matches!(app.screen, Screen::Normal), "{:?}", app.screen);

    // Enter answers "point here only": saved, created, files untouched.
    do_command(&mut app, Command::OutputDir, &http, &job_tx);
    if let Screen::Text(p) = &mut app.screen {
        p.buf = picked.to_str().unwrap().into();
    }
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    assert!(matches!(app.screen, Screen::Normal), "{:?}", app.screen);
    assert!(picked.is_dir(), "the folder is created");
    assert_eq!(load(&app).output_dir, picked.to_str().unwrap());
    assert_eq!(app.layout.output(), picked);
    assert!(old_mp3.is_file(), "pointing here moves nothing");
    assert!(
        job_rx.try_recv().is_err(),
        "pointing here runs nothing in the background"
    );

    // The prompt comes back prefilled from the file: the inductor's snapshot
    // is its boot one and would still say empty.
    do_command(&mut app, Command::OutputDir, &http, &job_tx);
    match &app.screen {
        Screen::Text(p) => assert_eq!(p.buf, picked.to_str().unwrap()),
        other => panic!("expected the output prompt, got {other:?}"),
    }

    // One folder for both stages is refused, and the prompt stays for an edit.
    if let Screen::Text(p) = &mut app.screen {
        p.buf = app.layout.audio().display().to_string();
    }
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    match &app.screen {
        Screen::Text(_) => assert!(
            app.status.text.contains("other stage's folder"),
            "{}",
            app.status.text
        ),
        other => panic!("a refusal keeps the prompt, got {other:?}"),
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn the_stage_folder_choice_can_take_the_files_along() {
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let dir = std::env::temp_dir().join("bm-stage-folders-move");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut app = App::new("http://x");
    app.layout = bm_core::Layout::new(&dir);
    std::fs::create_dir_all(app.layout.output()).unwrap();
    std::fs::write(app.layout.final_mp3(1), "audio").unwrap();

    // `m` on the choice dispatches the move; the keypress itself saves nothing.
    let picked = dir.join("drive/out");
    do_command(&mut app, Command::OutputDir, &http, &job_tx);
    if let Screen::Text(p) = &mut app.screen {
        p.buf = picked.to_str().unwrap().into();
    }
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    handle_key(&mut app, key(KeyCode::Char('m')), &http, &job_tx).await;
    assert!(matches!(app.screen, Screen::Normal), "{:?}", app.screen);
    let moved = match job_rx.try_recv().expect("`m` dispatches the move") {
        Job::Tracked { job, .. } => *job,
        other => panic!("expected a tracked job, got {other:?}"),
    };
    assert!(
        matches!(
            &moved,
            Job::MoveStageDir { kind: bm_core::stage_dir::StageDir::Output, raw, .. }
                if raw.as_str() == picked.to_str().unwrap()
        ),
        "{moved:?}"
    );
    assert_eq!(
        bm_core::config::Settings::load(&app.layout.settings()).output_dir,
        "",
        "the job saves the setting, not the keypress"
    );

    // Against a dead inductor nothing can be writing: the job moves without a
    // hold, saves, and the layout follows.
    let (tx, _ev_rx) = tokio::sync::mpsc::unbounded_channel::<Ev>();
    run_job(moved, tx).await;
    assert!(!dir.join("output").exists(), "the whole folder went");
    assert!(
        app.layout.final_mp3(1).is_file(),
        "{}",
        app.layout.final_mp3(1).display()
    );
    assert_eq!(app.layout.output(), picked);
    assert_eq!(
        bm_core::config::Settings::load(&app.layout.settings()).output_dir,
        picked.to_str().unwrap()
    );
    std::fs::remove_dir_all(&dir).ok();
}
