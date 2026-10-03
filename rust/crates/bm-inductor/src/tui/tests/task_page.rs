use super::*;

#[tokio::test]
async fn retry_from_the_list_targets_the_highlighted_task_only() {
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = tasks_app();
    app.screen = Screen::Tasks(TasksView::new());

    // Type "shelved": letters filter, they never act.
    for c in "shelved".chars() {
        handle_key(&mut app, key(KeyCode::Char(c)), &http, &job_tx).await;
    }
    match &app.screen {
        Screen::Tasks(v) => assert_eq!(v.filter, "shelved"),
        other => panic!("typing must filter, got {other:?}"),
    }
    assert!(
        job_rx.try_recv().is_err(),
        "a filter keystroke must not dispatch"
    );

    handle_key(&mut app, key(KeyCode::Char('u')), &http, &job_tx).await;
    match job_rx.try_recv().map(Job::into_bare) {
        Ok(Job::Op { req, .. }) => {
            assert_eq!(req.op, Op::RetryTask);
            assert_eq!(req.stage, Some(Stage::Digest));
            assert_eq!(
                req.chapter,
                Some(3),
                "the filtered row, not the visible one"
            );
            assert_eq!(req.force, Some(false));
        }
        other => panic!("expected a retry-task op, got {other:?}"),
    }
    assert_eq!(app.tasks.len(), 3, "the ledger is untouched locally");
    assert!(
        app.status.text.contains("digest:3 requeued"),
        "{}",
        app.status.text
    );

    // F is a different job (force), so the duplicate guard lets it through.
    handle_key(&mut app, key(KeyCode::Char('F')), &http, &job_tx).await;
    match job_rx.try_recv().map(Job::into_bare) {
        Ok(Job::Op { req, .. }) => {
            assert_eq!(req.force, Some(true), "F asks for a forced re-run");
            assert_eq!(req.chapter, Some(3));
        }
        other => panic!("expected a forced retry-task op, got {other:?}"),
    }
}

#[tokio::test]
async fn tasks_bulk_keys_remerge_direct_and_rerender_asks_first() {
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = tasks_app();
    app.screen = Screen::Tasks(TasksView::new());

    // R needs no row and no confirm: every merge requeues at once.
    handle_key(&mut app, key(KeyCode::Char('R')), &http, &job_tx).await;
    match job_rx.try_recv().map(Job::into_bare) {
        Ok(Job::Op { req, .. }) => assert_eq!(req.op, Op::Remerge),
        other => panic!("expected a remerge op, got {other:?}"),
    }
    assert!(
        app.status.text.contains("render cache kept"),
        "{}",
        app.status.text
    );

    // E is the destructive one: confirm first, op only on Enter.
    handle_key(&mut app, key(KeyCode::Char('E')), &http, &job_tx).await;
    assert!(
        matches!(app.screen, Screen::Confirm(_)),
        "E opens a confirm, not an op"
    );
    assert!(
        job_rx.try_recv().is_err(),
        "nothing dispatches before confirm"
    );
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    match job_rx.try_recv().map(Job::into_bare) {
        Ok(Job::Op { req, .. }) => assert_eq!(req.op, Op::Rerender),
        other => panic!("expected a rerender op, got {other:?}"),
    }
}

#[tokio::test]
async fn m_asks_first_and_confirms_into_a_reconcile_op() {
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = App::new("http://127.0.0.1:8901");
    handle_key(&mut app, key(KeyCode::Char(':')), &http, &job_tx).await;
    app.screen = Screen::Text(TextPrompt::new(TextKind::Command, ":", "", "m"));
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    assert!(
        matches!(app.screen, Screen::Confirm(_)),
        ":m opens a confirm, not an op"
    );
    assert!(
        job_rx.try_recv().is_err(),
        "nothing dispatches before confirm"
    );
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    match job_rx.try_recv().map(Job::into_bare) {
        Ok(Job::Op { req, .. }) => assert_eq!(req.op, Op::Reconcile),
        other => panic!("expected a reconcile op, got {other:?}"),
    }
    assert!(
        app.status.text.contains("reconciling"),
        "{}",
        app.status.text
    );

    // A bare `m` from Normal mode is a stray key: it must never reach the
    // confirm, let alone dispatch.
    let (tx2, mut rx2) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app2 = App::new("http://127.0.0.1:8901");
    handle_key(&mut app2, key(KeyCode::Char('m')), &http, &tx2).await;
    assert!(
        matches!(app2.screen, Screen::Normal),
        "a stray m must not open confirm"
    );
    assert!(
        app2.status.text.contains("command line"),
        "{}",
        app2.status.text
    );
    assert!(rx2.try_recv().is_err(), "a stray m must never dispatch");
}

#[tokio::test]
async fn rerender_asks_first_and_confirms_into_a_rerender_op() {
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = App::new("http://127.0.0.1:8901");
    handle_key(&mut app, key(KeyCode::Char(':')), &http, &job_tx).await;
    app.screen = Screen::Text(TextPrompt::new(TextKind::Command, ":", "", "rerender"));
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    assert!(
        matches!(app.screen, Screen::Confirm(_)),
        ":rerender opens a confirm, not an op"
    );
    assert!(
        job_rx.try_recv().is_err(),
        "nothing dispatches before confirm"
    );
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    match job_rx.try_recv().map(Job::into_bare) {
        Ok(Job::Op { req, .. }) => assert_eq!(req.op, Op::Rerender),
        other => panic!("expected a rerender op, got {other:?}"),
    }
    assert!(
        app.status.text.contains("re-rendering"),
        "{}",
        app.status.text
    );
}

#[tokio::test]
async fn colon_opens_a_command_line_that_presses_keys() {
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = App::new("http://127.0.0.1:8901");
    handle_key(&mut app, key(KeyCode::Char(':')), &http, &job_tx).await;
    assert!(
        matches!(app.screen, Screen::Text(_)),
        ": opens the command line"
    );
    // `:r` refreshes: a state fetch against a dead inductor fails
    // quietly into the status line, dispatching no job.
    app.screen = Screen::Text(TextPrompt::new(TextKind::Command, ":", "", "r"));
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    assert!(
        matches!(app.screen, Screen::Normal),
        "submit closes the prompt"
    );
    assert!(job_rx.try_recv().is_err(), "refresh dispatches no job");
    // `:frobnicate` stays an error, `:q` quits through the normal path.
    app.screen = Screen::Text(TextPrompt::new(TextKind::Command, ":", "", "frobnicate"));
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    assert!(
        app.status.text.contains("unknown command"),
        "{}",
        app.status.text
    );
}

#[tokio::test]
async fn ctrl_u_clears_the_filter_and_never_requeues() {
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = tasks_app();
    app.screen = Screen::Tasks(TasksView::new());
    for c in "render".chars() {
        handle_key(&mut app, key(KeyCode::Char(c)), &http, &job_tx).await;
    }
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
        &http,
        &job_tx,
    )
    .await;
    match &app.screen {
        Screen::Tasks(v) => assert!(v.filter.is_empty(), "Ctrl-U clears the filter"),
        other => panic!("{other:?}"),
    }
    assert!(
        job_rx.try_recv().is_err(),
        "Ctrl-U must not be read as a plain `u` — that would re-queue a task"
    );
}

#[tokio::test]
async fn the_task_page_explains_a_batched_render_from_the_row_that_owns_it() {
    // Ten rows flipping to `assigned` on one box with nothing on screen to say
    // why is the state batching created, and the TUI is the operator's only
    // interface. The fact lives on the row the offer named, the one the
    // worker reports progress against, so that is where it is shown, and a
    // member row says nothing because it knows nothing.
    let mut app = tasks_app();
    let head = app
        .tasks
        .iter_mut()
        .find(|t| t.stage == Stage::Render)
        .expect("the fixture has a render row");
    head.take = Some(0);
    head.batch = vec!["render:3:1".into(), "render:3:2".into()];
    app.screen = Screen::TaskDetail(TaskDetail {
        stage: Stage::Render,
        chapter: 3,
        scroll: 0,
        list: TasksView::new(),
    });

    let text = render_text(&mut app, 140, 44);
    assert!(text.contains("batch"), "the row owns a batch: {text}");
    assert!(
        text.contains("3 takes"),
        "and says how many the offer carries: {text}"
    );
    assert!(
        text.contains("render:3:1") && text.contains("render:3:2"),
        "naming the other rows, so they can be found in the ledger: {text}"
    );

    // A row with no batch says nothing extra, the line is not decoration.
    let head = app
        .tasks
        .iter_mut()
        .find(|t| t.stage == Stage::Render)
        .unwrap();
    head.batch.clear();
    let text = render_text(&mut app, 140, 44);
    assert!(!text.contains("batch"), "no batch, no line: {text}");
}

#[tokio::test]
async fn retry_from_the_detail_page_stays_on_the_page() {
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = tasks_app();
    app.screen = Screen::TaskDetail(TaskDetail {
        stage: Stage::Digest,
        chapter: 3,
        scroll: 0,
        list: TasksView::new(),
    });
    handle_key(&mut app, key(KeyCode::Char('u')), &http, &job_tx).await;
    match job_rx.try_recv().map(Job::into_bare) {
        Ok(Job::Op { req, .. }) => assert_eq!(
            (req.op, req.stage, req.chapter),
            (Op::RetryTask, Some(Stage::Digest), Some(3))
        ),
        other => panic!("expected a retry-task op, got {other:?}"),
    }
    assert!(
        matches!(app.screen, Screen::TaskDetail(_)),
        "the page stays open so the state field can be watched changing: {:?}",
        app.screen
    );
    // The wake-up keys are on the page's own title, since this is where the
    // reason was read.
    let text = render_text(&mut app, 140, 44);
    assert!(text.contains("F force re-run"), "{text}");
    assert!(text.contains("Task digest:3"), "{text}");
}

#[test]
fn two_retries_of_different_chapters_do_not_suppress_each_other() {
    // The in-flight guard is keyed by what the op acts on: a blanket
    // per-op guard would silently refuse the second retry.
    let a = OpRequest {
        op: Op::RetryTask,
        stage: Some(Stage::Digest),
        chapter: Some(3),
        ..Default::default()
    };
    let b = OpRequest {
        chapter: Some(4),
        ..a.clone()
    };
    assert_ne!(op_key(&a), op_key(&b));
    assert_eq!(
        op_key(&a),
        op_key(&a.clone()),
        "the same job twice is a duplicate"
    );
    let forced = OpRequest {
        force: Some(true),
        ..a.clone()
    };
    assert_ne!(op_key(&a), op_key(&forced), "force is a different job");

    // `:go` and `:hold` are one op with one flag between them, so the flag has
    // to be in the key: otherwise a hold pressed while the go is still in
    // flight is refused as a duplicate — the opposite of what was asked.
    let dispatch = |go: bool| {
        op_key(&OpRequest {
            op: Op::Dispatch,
            go: Some(go),
            ..Default::default()
        })
    };
    assert_ne!(
        dispatch(true),
        dispatch(false),
        "a hold is not a duplicate go"
    );
}

#[tokio::test]
async fn go_and_hold_both_reach_the_wire_even_back_to_back() {
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = App::new("http://127.0.0.1:8901");
    // `dispatch` wraps every job in a `Tracked` row, so the op is inside it.
    let op_req = |job: Job| match job {
        Job::Tracked { job, .. } => match *job {
            Job::Op { req, .. } => req,
            other => panic!("expected an op, got {other:?}"),
        },
        other => panic!("expected a tracked job, got {other:?}"),
    };

    do_command(&mut app, Command::Dispatch { go: true }, &http, &job_tx);
    let req = op_req(job_rx.try_recv().expect("`:go` dispatches"));
    assert_eq!((req.op, req.go), (Op::Dispatch, Some(true)));

    // The hold follows before the go has answered — the state the in-flight
    // guard sees on a slow inductor, and the one it must not eat.
    do_command(&mut app, Command::Dispatch { go: false }, &http, &job_tx);
    let req = op_req(job_rx.try_recv().expect("`:hold` dispatches too"));
    assert_eq!((req.op, req.go), (Op::Dispatch, Some(false)));

    // Both directions are spelled in the word list, because a hold nobody can
    // find is a hold that reads as a broken cluster.
    for word in ["go", "hold"] {
        assert!(WORDS.iter().any(|w| w.names.contains(&word)), "{word}");
    }
}

#[test]
fn scheduler_events_reach_the_pane_exactly_once() {
    let mut app = App::new("http://x");
    let snapshot = serde_json::json!({
        "tasks": [], "machines": [], "beats": [],
        "events": [
            {"id": 0, "ts": 1, "level": "error",
             "text": "[w2] digest:3 FAILED (shelved — press u to retry): opencode exited 1"},
            {"id": 1, "ts": 2, "level": "ok", "text": "[w1] render:2 done in 4.2s"},
        ],
    });
    app.apply_state(snapshot.clone());
    assert!(
        app.events
            .iter()
            .any(|l| l.text.contains("digest:3 FAILED")),
        "{:?}",
        app.events
    );
    assert!(app.events.iter().any(|l| l.text.contains("done in 4.2s")));
    // Levels ride along, so a failure reads as a failure.
    assert!(app
        .events
        .iter()
        .any(|l| l.level == Level::Error && l.text.contains("FAILED")));

    // The poller resends the whole buffer every 800 ms: nothing may repeat.
    app.apply_state(snapshot.clone());
    app.apply_state(snapshot);
    assert_eq!(
        app.events
            .iter()
            .filter(|l| l.text.contains("FAILED"))
            .count(),
        1,
        "a repeated snapshot must not duplicate the log: {:?}",
        app.events
    );

    // Only a new id appends.
    app.apply_state(serde_json::json!({
        "events": [{"id": 2, "ts": 3, "level": "warn",
                    "text": "lease expired — requeued 1: render:3"}],
    }));
    assert!(app.events.iter().any(|l| l.text.contains("lease expired")));
    assert_eq!(
        app.events
            .iter()
            .filter(|l| l.text.contains("FAILED"))
            .count(),
        1
    );

    // A snapshot without the key (an older inductor) must not panic or clear.
    let before = app.events.len();
    app.apply_state(serde_json::json!({ "tasks": [] }));
    assert_eq!(app.events.len(), before);
}

#[test]
fn a_restarted_inductor_resets_the_event_cursor() {
    let mut app = App::new("http://x");
    app.apply_state(serde_json::json!({
        "events": [{"id": 7, "ts": 1, "level": "ok", "text": "seven"}],
    }));
    // A fresh inductor counts from zero again; without the reset its whole
    // history would look older than the last id we saw and be dropped.
    app.apply_state(serde_json::json!({
        "events": [
            {"id": 0, "ts": 2, "level": "info", "text": "after restart"},
            {"id": 1, "ts": 3, "level": "ok", "text": "and again"},
        ],
    }));
    assert!(
        app.events.iter().any(|l| l.text.contains("after restart")),
        "{:?}",
        app.events
    );
    assert!(app.events.iter().any(|l| l.text.contains("and again")));
    assert!(app
        .events
        .iter()
        .any(|l| l.text.contains("event stream reset")));
}

#[test]
fn both_key_lines_advertise_the_task_list() {
    assert!(
        KEYS_FULL.iter().any(|k| k.contains("K tasks")),
        "{KEYS_FULL:?}"
    );
    assert!(
        KEYS_COMPACT.iter().any(|k| k.contains("K tasks")),
        "{KEYS_COMPACT:?}"
    );
}

#[tokio::test]
async fn pick_filter_accepts_letters_instead_of_moving() {
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = App::new("http://127.0.0.1:8901");
    app.roster = Some(roster_fixture());
    app.screen = Screen::Pick(Picker::new());
    // "Kiên" starts with K: if movement still owned letters, this would
    // move the cursor instead of typing.
    handle_key(&mut app, key(KeyCode::Char('j')), &http, &job_tx).await;
    match &app.screen {
        Screen::Pick(p) => {
            assert_eq!(p.filter, "j");
            assert_eq!(p.cursor, 0, "typing resets the cursor, it never moves it");
        }
        other => panic!("typing must filter, got {other:?}"),
    }
    assert!(
        job_rx.try_recv().is_err(),
        "a filter keystroke must not dispatch"
    );
}

#[tokio::test]
async fn cast_filter_accepts_every_letter_including_q() {
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = App::new("http://127.0.0.1:8901");
    app.roster = Some(roster_fixture());
    app.screen = Screen::Cast(CastView::new());
    handle_key(&mut app, key(KeyCode::Char('j')), &http, &job_tx).await;
    match &app.screen {
        Screen::Cast(v) => {
            assert_eq!(v.filter, "j");
            assert_eq!(v.cursor, 0, "typing resets the cursor, it never moves it");
        }
        other => panic!("typing must filter, got {other:?}"),
    }
    // `q` closes other screens but must type here: it is Esc that closes.
    handle_key(&mut app, key(KeyCode::Char('q')), &http, &job_tx).await;
    match &app.screen {
        Screen::Cast(v) => assert_eq!(v.filter, "jq"),
        other => panic!("q must type into the filter, got {other:?}"),
    }
    assert!(
        job_rx.try_recv().is_err(),
        "a filter keystroke must not dispatch"
    );
}

#[tokio::test]
async fn pick_esc_steps_back_from_voice_to_character_then_closes() {
    let http = reqwest::Client::new();
    let (job_tx, _job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = App::new("http://127.0.0.1:8901");
    let mut p = Picker::new();
    p.stage = PickStage::Voice;
    p.character = "Kiên".into();
    p.filter = "duc".into();
    app.screen = Screen::Pick(p);
    handle_key(&mut app, key(KeyCode::Esc), &http, &job_tx).await;
    match &app.screen {
        Screen::Pick(p) => {
            assert_eq!(p.stage, PickStage::Character);
            assert!(p.filter.is_empty(), "stepping back clears the voice filter");
        }
        other => panic!("first Esc must step back a stage, got {other:?}"),
    }
    handle_key(&mut app, key(KeyCode::Esc), &http, &job_tx).await;
    assert!(
        matches!(app.screen, Screen::Normal),
        "second Esc closes: {:?}",
        app.screen
    );
}
