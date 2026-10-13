use super::*;

#[tokio::test]
async fn creating_and_switching_a_workspace_moves_the_dashboard_with_it() {
    use super::super::jobs::run_jobs_with;
    use super::input::dispatch;
    let root = std::env::temp_dir().join(format!("bm-ws-move-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);

    let mut app = App::new("http://127.0.0.1:8901");
    app.layout = bm_core::Layout::new(&root);
    // Loaded the way the dashboard would have it: lines and a roster already
    app.lines = Some(std::collections::HashMap::from([(
        "Narrator".to_string(),
        vec![audition_line("một")],
    )]));
    app.roster = Some(roster_fixture());
    assert_eq!(app.layout.work, root, "it starts on the implicit default");

    // One real job at a time, because each run consumes the job channel.
    {
        let layout = app.layout.clone();
        let (job_tx, job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(
            dispatch(
                &mut app,
                &job_tx,
                Job::Workspace {
                    layout,
                    api: "http://127.0.0.1:8901".into(),
                    req: WorkspaceReq::New {
                        name: "book-a".into(),
                        profile: None,
                        crawler: None,
                    },
                }
            ),
            "a workspace request must be dispatchable"
        );
        drop(job_tx);
        run_jobs_with(job_rx, tx, the_workspace_job_without_its_cluster_guard).await;
        pump_like_the_main_loop(&mut app, &mut rx).await;
    }

    assert_eq!(
        app.layout.work,
        root.join("workspaces/book-a"),
        "creating a workspace selects it, and the dashboard follows it"
    );
    assert!(
        app.lines.is_none() && app.roster.is_none(),
        "the previous book's caches are dropped, not carried across the switch"
    );

    // A second workspace, then a switch back — the pair the first test cannot
    std::fs::create_dir_all(root.join("workspaces/book-b")).unwrap();
    {
        let layout = app.layout.clone();
        let (job_tx, job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(dispatch(
            &mut app,
            &job_tx,
            Job::Workspace {
                layout,
                api: "http://127.0.0.1:8901".into(),
                req: WorkspaceReq::Use("book-b".into()),
            }
        ));

        drop(job_tx);
        run_jobs_with(job_rx, tx, the_workspace_job_without_its_cluster_guard).await;
        pump_like_the_main_loop(&mut app, &mut rx).await;
    }
    assert_eq!(
        app.layout.work,
        root.join("workspaces/book-b"),
        "selecting an existing workspace moves the dashboard onto it"
    );
    {
        let layout = app.layout.clone();
        let (job_tx, job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(dispatch(
            &mut app,
            &job_tx,
            Job::Workspace {
                layout,
                api: "http://127.0.0.1:8901".into(),
                req: WorkspaceReq::Use("book-a".into()),
            }
        ));
        drop(job_tx);
        run_jobs_with(job_rx, tx, the_workspace_job_without_its_cluster_guard).await;
        pump_like_the_main_loop(&mut app, &mut rx).await;
    }
    assert_eq!(
        app.layout.work,
        root.join("workspaces/book-a"),
        "and switching back lands where it was asked to"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_workspace_prompt_lists_the_books_instead_of_demanding_a_name() {
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let root = two_workspaces_one_bad("list");

    let mut app = App::new("http://127.0.0.1:8901");
    app.layout = bm_core::Layout::new(&root);

    // `:ws` with nothing after it: the operator never has to remember a name,
    type_command(&mut app, &http, &job_tx, "ws").await;
    let Screen::WorkspaceList(ws) = app.screen.clone() else {
        panic!(":ws lists the books, got {:?}", app.screen);
    };
    assert_eq!(
        ws.list()
            .iter()
            .map(|i| i.label.as_str())
            .collect::<Vec<_>>(),
        vec!["book-a", "scratch"],
    );
    assert!(
        ws.list()[0].note.starts_with("active"),
        "the pointer's book is marked: {:?}",
        ws.list()[0].note
    );
    assert!(
        ws.list()[0].note.contains("1 chapter") && ws.list()[0].note.contains("1 script"),
        "a book's row says how far it has got: {:?}",
        ws.list()[0].note
    );
    assert!(
        ws.list()[1].note.contains("no settings.json"),
        "a directory that is not a book says why: {:?}",
        ws.list()[1].note
    );
    assert_eq!(
        ws.unusable.keys().copied().collect::<Vec<_>>(),
        vec![1],
        "only the row that cannot be switched is refused"
    );
    assert!(
        job_rx.try_recv().is_err(),
        "listing is not a job: it reads the tree"
    );

    // Esc closes it without moving anything.
    handle_key(&mut app, key(KeyCode::Esc), &http, &job_tx).await;
    assert!(matches!(app.screen, Screen::Normal), "{:?}", app.screen);
}

#[tokio::test]
async fn choosing_a_book_switches_it_and_a_directory_that_is_not_one_is_refused() {
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let root = two_workspaces_one_bad("choose");

    let mut app = App::new("http://127.0.0.1:8901");
    app.layout = bm_core::Layout::new(&root);
    type_command(&mut app, &http, &job_tx, "ws").await;

    // Down onto the directory that is not a workspace, Enter: refused where it
    handle_key(&mut app, key(KeyCode::Down), &http, &job_tx).await;
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    let Screen::WorkspaceList(ws) = app.screen.clone() else {
        panic!("the refusal stays on the list, got {:?}", app.screen);
    };
    assert!(
        ws.error
            .as_deref()
            .unwrap_or_default()
            .contains("no settings.json"),
        "{:?}",
        ws.error
    );
    assert!(
        job_rx.try_recv().is_err(),
        "a pointer write onto a directory no command can read is never queued"
    );

    // Up onto the book, Enter: the switch is dispatched and the screen closes.
    handle_key(&mut app, key(KeyCode::Up), &http, &job_tx).await;
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    match job_rx.try_recv().map(Job::into_bare) {
        Ok(Job::Workspace { req, .. }) => {
            assert!(
                matches!(req, WorkspaceReq::Use(ref n) if n == "book-a"),
                "the highlighted row is what gets switched to: {req:?}"
            );
        }
        other => panic!("expected the switch, got {other:?}"),
    }
    assert!(
        matches!(app.screen, Screen::Normal),
        "the list closes once the switch is queued: {:?}",
        app.screen
    );
}

#[tokio::test]
async fn no_key_on_the_workspace_picker_asks_the_app_to_quit() {
    // `handle_key` answers one question: does the app keep running? A screen
    use super::input::Flow;
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let root = two_workspaces_one_bad("quit");

    let mut app = App::new("http://127.0.0.1:8901");
    app.layout = bm_core::Layout::new(&root);

    for c in ":ws".chars() {
        assert_eq!(
            handle_key(&mut app, key(KeyCode::Char(c)), &http, &job_tx).await,
            Flow::KeepRunning,
            "typing {c:?} must not exit"
        );
    }
    assert_eq!(
        handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await,
        Flow::KeepRunning,
        "opening the list must not exit"
    );
    assert!(
        matches!(app.screen, Screen::WorkspaceList(_)),
        "{:?}",
        app.screen
    );

    for k in [
        KeyCode::Down,
        KeyCode::Up,
        KeyCode::Char('j'),
        KeyCode::Char('k'),
        KeyCode::Home,
        KeyCode::Char('x'),
    ] {
        assert_eq!(
            handle_key(&mut app, key(k), &http, &job_tx).await,
            Flow::KeepRunning,
            "{k:?} must not exit the app"
        );
        assert!(
            matches!(app.screen, Screen::WorkspaceList(_)),
            "{k:?} must leave the picker up, got {:?}",
            app.screen
        );
    }

    // End puts the highlight on the directory that is not a workspace, so this
    assert_eq!(
        handle_key(&mut app, key(KeyCode::End), &http, &job_tx).await,
        Flow::KeepRunning
    );
    assert_eq!(
        handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await,
        Flow::KeepRunning,
        "a refused Enter must not exit either"
    );
    let Screen::WorkspaceList(ws) = app.screen.clone() else {
        panic!("the refusal stays up, got {:?}", app.screen);
    };
    assert!(
        ws.error
            .as_deref()
            .unwrap_or_default()
            .contains("no settings.json"),
        "{:?}",
        ws.error
    );
    assert!(job_rx.try_recv().is_err(), "nothing was queued");
}

/// Every screen, one battery of keys, one answer: the app keeps running.
#[tokio::test]
async fn only_the_dashboards_q_ends_the_app() {
    use super::input::Flow;
    let http = reqwest::Client::new();
    let layout = bm_core::Layout::new("");
    let battery = [
        KeyCode::Up,
        KeyCode::Down,
        KeyCode::Left,
        KeyCode::Right,
        KeyCode::Home,
        KeyCode::End,
        KeyCode::PageUp,
        KeyCode::PageDown,
        KeyCode::Tab,
        KeyCode::Backspace,
        KeyCode::Enter,
        KeyCode::Char('j'),
        KeyCode::Char('k'),
        KeyCode::Char('x'),
        KeyCode::Char('a'),
        KeyCode::Char('?'),
    ];

    // One screen per row of the `Screen` enum that needs nothing from the
    let screens: Vec<(&str, Screen)> = vec![
        ("dashboard", Screen::Normal),
        (
            "jobs",
            Screen::Jobs {
                scroll: 0,
                previous: Box::new(Screen::Normal),
            },
        ),
        ("help", Screen::Help { scroll: 0 }),
        (
            "crawl",
            Screen::Crawl {
                scroll: 0,
                expanded: false,
            },
        ),
        ("run", Screen::Run),
        (
            "command prompt",
            Screen::Text(TextPrompt::new(TextKind::Command, ":", "hint", "")),
        ),
        ("picker", Screen::Pick(Picker::new())),
        ("cast", Screen::Cast(CastView::new())),
        ("tasks", Screen::Tasks(TasksView::new())),
        ("digest", Screen::Digest(DigestView::new(vec![1]))),
        ("llm", Screen::Llm(LlmView::new())),
        ("sound", Screen::Sound(SoundView::new())),
        ("cloud", Screen::Cloud(CloudView::new())),
        ("script", Screen::Script(ScriptView::new(&layout))),
        (
            "policy",
            Screen::Policy(PolicyView::new("10.0.0.1".into(), "box".into(), vec![])),
        ),
        (
            "workspace new",
            Screen::WorkspaceNew(WorkspaceNew::new("book".into(), vec![])),
        ),
        ("machine", Screen::Machine("10.0.0.1".into())),
        // A confirm whose action is *not* quitting, so its Enter exercises the
        ("confirm (rerender)", Screen::Confirm(Confirm::rerender())),
    ];

    for (name, screen) in screens {
        for k in battery {
            let (job_tx, _job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
            let mut app = App::new("http://127.0.0.1:8901");
            app.layout = bm_core::Layout::new("");
            app.screen = screen.clone();
            assert_eq!(
                handle_key(&mut app, key(k), &http, &job_tx).await,
                Flow::KeepRunning,
                "{name}: {k:?} must not close the app (screen is now {:?})",
                app.screen
            );
        }
    }

    // The first way out: the dashboard's `q`, when nothing is in flight.
    let (job_tx, _job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = App::new("http://127.0.0.1:8901");
    app.layout = bm_core::Layout::new("");
    assert_eq!(
        handle_key(&mut app, key(KeyCode::Char('q')), &http, &job_tx).await,
        Flow::Quit,
        "q on the dashboard is still the way out"
    );

    // The second: the dialog `q` grows when work is in flight, which reaches
    let quitting = || {
        Screen::Confirm(Confirm {
            title: "Quit with work in flight?".into(),
            danger: true,
            body: vec!["1 background job(s) are still running.".into()],
            action: ConfirmAction::Quit,
        })
    };
    app.screen = quitting();
    assert_eq!(
        handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await,
        Flow::Quit,
        "answering yes to the quit dialog is the way out"
    );
    app.screen = quitting();
    assert_eq!(
        handle_key(&mut app, key(KeyCode::Esc), &http, &job_tx).await,
        Flow::KeepRunning,
        "changing your mind is not quitting"
    );
}
