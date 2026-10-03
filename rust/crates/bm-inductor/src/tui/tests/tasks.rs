use super::*;

#[tokio::test]
async fn the_tasks_screen_shows_every_task_and_the_failure_reason() {
    let mut app = tasks_app();
    app.screen = Screen::Tasks(TasksView::new());
    let text = render_text(&mut app, 140, 44);
    assert!(text.contains("3 tasks"), "{text}");
    assert!(text.contains("1 shelved"), "{text}");
    assert!(
        text.contains("digest:3"),
        "the offending task is named:\n{text}"
    );
    assert!(
        text.contains("crawl"),
        "finished work is still listed:\n{text}"
    );
    assert!(
        text.contains("opencode exited 1"),
        "the detail column carries the reason:\n{text}"
    );
    assert!(text.contains("u retry  ·  F force re-run"), "{text}");
    assert!(
        text.contains(worker_alias("w2").0),
        "the worker that failed:\n{text}"
    );
}

#[test]
fn filtering_the_ledger_matches_stage_state_and_chapter() {
    let app = tasks_app();
    let all = &app.tasks;
    let live = std::collections::BTreeSet::new();
    let f = |s: &str| filtered_tasks(all, s, Facet::All, &live);
    assert_eq!(f("").len(), 3, "no filter, everything");
    assert_eq!(f("   ").len(), 3, "whitespace is not a filter");
    assert_eq!(f("shelved").len(), 1);
    assert_eq!(f("  SHELVED ").len(), 1, "case and space insensitive");
    assert_eq!(f("render")[0].chapter, 3);
    assert_eq!(f("digest:3").len(), 1);
    assert_eq!(f("4").len(), 1, "a chapter number matches");
    assert!(f("merge").is_empty());
    assert!(f("shel").len() == 1, "a partial state name still matches");
}

#[test]
fn a_row_is_abandoned_only_when_every_box_holding_it_has_gone_quiet() {
    let (app, live) = ledger_with_a_silent_box();
    let by_id = |id: &str| app.tasks.iter().find(|t| t.id() == id).unwrap().clone();
    assert!(
        abandoned(&by_id("merge:7"), &live),
        "that box is not beating"
    );
    assert!(
        !abandoned(&by_id("render:8"), &live),
        "that box is answering"
    );
    assert!(
        !abandoned(&by_id("render:9"), &live),
        "a pending row has no holder to have lost"
    );
    // A terminal row is not waiting on anybody, whatever its `assigned_to`
    let mut shelved = by_id("merge:7");
    shelved.state = TaskState::Shelved;
    assert!(
        !abandoned(&shelved, &std::collections::BTreeSet::new()),
        "a shelved row is never abandoned"
    );

    // Racing: one live holder is enough to keep the row out of the list. The
    let mut racing = by_id("merge:7");
    racing.state = TaskState::Running;
    racing.racers = vec!["hcm-2".into()];
    assert!(!abandoned(&racing, &live), "one live racer is enough");
    racing.assigned_to = None;
    racing.racers = vec!["hcm-1".into()];
    assert!(
        abandoned(&racing, &live),
        "with no primary, the racers are what is left to be silent"
    );
}

#[test]
fn the_facet_cycle_wraps_and_reaches_every_chip() {
    assert_eq!(Facet::All.step(true), Facet::Crawl, "all leads to crawl");
    assert_eq!(Facet::All.step(false), Facet::Abandoned, "back wraps");
    assert_eq!(Facet::Abandoned.step(true), Facet::All, "forward wraps");
    // Stepping forward from `all` visits each chip exactly once: a member of
    let mut seen = vec![Facet::All];
    let mut f = Facet::All;
    for _ in 0..Facet::ALL.len() - 1 {
        f = f.step(true);
        seen.push(f);
    }
    assert_eq!(f.step(true), Facet::All, "the cycle closes");
    assert_eq!(
        seen.iter().map(|f| f.label()).collect::<Vec<_>>(),
        Facet::ALL.iter().map(|f| f.label()).collect::<Vec<_>>(),
        "the drawn order is the step order"
    );
}

#[test]
fn facets_narrow_the_ledger_without_taking_a_letter_away() {
    let (app, live) = ledger_with_a_silent_box();
    let all = &app.tasks;
    let n = |facet: Facet| filtered_tasks(all, "", facet, &live).len();
    assert_eq!(n(Facet::All), 3);
    assert_eq!(n(Facet::Merge), 1, "only merge");
    assert_eq!(n(Facet::Render), 2, "only render");
    assert_eq!(n(Facet::Crawl), 0, "a stage with no rows is empty, not all");
    assert_eq!(n(Facet::Queued), 1, "\"queued\" is `pending`");
    assert_eq!(n(Facet::Active), 2, "assigned and running together");
    assert_eq!(n(Facet::Done), 0);
    assert_eq!(n(Facet::Shelved), 0);
    assert_eq!(n(Facet::Failed), 0);
    assert_eq!(n(Facet::Abandoned), 1, "the one row that will not move");

    // Both narrowings apply at once, which is what keeps a chapter number
    assert_eq!(filtered_tasks(all, "8", Facet::Render, &live).len(), 1);
    assert_eq!(filtered_tasks(all, "9", Facet::Render, &live).len(), 1);
    assert!(
        filtered_tasks(all, "9", Facet::Merge, &live).is_empty(),
        "the facet still wins over a chapter number"
    );
}

#[tokio::test]
async fn the_arrow_keys_step_the_facet_and_the_bar_shows_where_it_is() {
    let http = reqwest::Client::new();
    let (job_tx, _job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let (mut app, _live) = ledger_with_a_silent_box();
    app.screen = Screen::Tasks(TasksView::new());

    handle_key(&mut app, key(KeyCode::Right), &http, &job_tx).await;
    match &app.screen {
        Screen::Tasks(v) => assert_eq!(v.facet, Facet::Crawl),
        other => panic!("{other:?}"),
    }
    assert!(
        app.status.text.contains("facet: crawl") && app.status.text.contains("0 task"),
        "the count belongs to the chip that was chosen: {:?}",
        app.status
    );
    let text = render_text(&mut app, 140, 44);
    assert!(
        text.contains("[crawl]"),
        "the active chip is bracketed:\n{text}"
    );
    assert!(text.contains("render"), "the other chips are still listed");

    handle_key(&mut app, key(KeyCode::Left), &http, &job_tx).await;
    match &app.screen {
        Screen::Tasks(v) => assert_eq!(v.facet, Facet::All, "left steps back off all"),
        other => panic!("{other:?}"),
    }
    handle_key(&mut app, key(KeyCode::Right), &http, &job_tx).await;
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
        &http,
        &job_tx,
    )
    .await;
    match &app.screen {
        Screen::Tasks(v) => {
            assert_eq!(v.facet, Facet::All, "Ctrl-U is the way back to everything");
            assert!(v.filter.is_empty());
        }
        other => panic!("{other:?}"),
    }
    // Tab is not this screen's to spend any more: it opens the jobs view — the
    handle_key(&mut app, key(KeyCode::Tab), &http, &job_tx).await;
    assert!(
        matches!(app.screen, Screen::Jobs { .. }),
        "{:?}",
        app.screen
    );
    handle_key(&mut app, key(KeyCode::Tab), &http, &job_tx).await;
    assert!(matches!(app.screen, Screen::Tasks(_)), "{:?}", app.screen);
}

#[tokio::test]
async fn a_dialog_answers_back_to_the_screen_that_asked() {
    // `W` on the ledger: everything one box holds goes back to the pool, behind
    let http = reqwest::Client::new();
    let (job_tx, _job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let (mut app, _live) = ledger_with_a_silent_box();
    app.screen = Screen::Tasks(TasksView::new());

    handle_key(&mut app, key(KeyCode::Char('W')), &http, &job_tx).await;
    assert!(matches!(app.screen, Screen::Confirm(_)), "{:?}", app.screen);
    handle_key(&mut app, key(KeyCode::Esc), &http, &job_tx).await;
    assert!(
        matches!(app.screen, Screen::Tasks(_)),
        "Esc from the dialog lands on the ledger it was asked from: {:?}",
        app.screen
    );
    // Not a one-way door: the ledger still closes to the dashboard.
    handle_key(&mut app, key(KeyCode::Esc), &http, &job_tx).await;
    assert!(matches!(app.screen, Screen::Normal), "{:?}", app.screen);
}

#[tokio::test]
async fn the_command_line_comes_back_to_the_screen_it_was_typed_in() {
    let http = reqwest::Client::new();
    let (job_tx, _job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = App::new("http://127.0.0.1:8901");
    app.screen = Screen::Cast(CastView::new());

    handle_key(&mut app, key(KeyCode::Char(':')), &http, &job_tx).await;
    assert!(matches!(app.screen, Screen::Text(_)), "{:?}", app.screen);
    handle_key(&mut app, key(KeyCode::Esc), &http, &job_tx).await;
    assert!(
        matches!(app.screen, Screen::Cast(_)),
        "a `:` typed on the cast table cancels back to it: {:?}",
        app.screen
    );
}

#[tokio::test]
async fn a_command_that_raises_a_dialog_never_reopens_the_prompt_under_it() {
    // `:rerender` asks first. That question belongs over the dashboard — the
    let http = reqwest::Client::new();
    let (job_tx, _job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = App::new("http://127.0.0.1:8901");

    type_command(&mut app, &http, &job_tx, "rerender").await;
    assert!(matches!(app.screen, Screen::Confirm(_)), "{:?}", app.screen);
    handle_key(&mut app, key(KeyCode::Esc), &http, &job_tx).await;
    assert!(
        matches!(app.screen, Screen::Normal),
        "the cancelled dialog lands on the dashboard, not on a spent prompt: {:?}",
        app.screen
    );
}

#[tokio::test]
async fn esc_walks_a_stack_of_layers_down_one_at_a_time() {
    // Cast → picker → `:` line, all three through the keys that open them, then
    let http = reqwest::Client::new();
    let (job_tx, _job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = App::new("http://127.0.0.1:8901");
    app.screen = Screen::Cast(CastView::new());

    type_command(&mut app, &http, &job_tx, "s").await;
    assert!(matches!(app.screen, Screen::Pick(_)), "{:?}", app.screen);
    handle_key(&mut app, key(KeyCode::Char(':')), &http, &job_tx).await;
    assert!(matches!(app.screen, Screen::Text(_)), "{:?}", app.screen);

    handle_key(&mut app, key(KeyCode::Esc), &http, &job_tx).await;
    assert!(
        matches!(app.screen, Screen::Pick(_)),
        "the prompt closes onto the picker, not past it: {:?}",
        app.screen
    );
    handle_key(&mut app, key(KeyCode::Esc), &http, &job_tx).await;
    assert!(
        matches!(app.screen, Screen::Cast(_)),
        "and the picker closes onto the cast table that started it: {:?}",
        app.screen
    );
    handle_key(&mut app, key(KeyCode::Esc), &http, &job_tx).await;
    assert!(matches!(app.screen, Screen::Normal), "{:?}", app.screen);
}

#[tokio::test]
async fn the_run_config_editor_closes_back_onto_the_run_screen() {
    // `e` on the system overview opens a prompt that never recorded where it
    let http = reqwest::Client::new();
    let (job_tx, _job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = App::new("http://127.0.0.1:8901");
    app.screen = Screen::Run;

    handle_key(&mut app, key(KeyCode::Char('e')), &http, &job_tx).await;
    assert!(matches!(app.screen, Screen::Text(_)), "{:?}", app.screen);
    handle_key(&mut app, key(KeyCode::Esc), &http, &job_tx).await;
    assert!(
        matches!(app.screen, Screen::Run),
        "Esc from the config editor lands on the screen that opened it: {:?}",
        app.screen
    );
}

#[tokio::test]
async fn esc_leaves_the_model_list_before_it_leaves_the_llm_screen() {
    // The fetched list is a step of the screen, like the picker's step 2: Esc
    let http = reqwest::Client::new();
    let (job_tx, _job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = App::new("http://127.0.0.1:8901");
    // One provider in `.bm/llm.json`, because an empty roster closes the screen
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".bm")).unwrap();
    std::fs::write(
        dir.path().join(".bm").join("llm.json"),
        r#"{"active":"","providers":{"google":{"kind":"gemini",
           "base_url":"https://generativelanguage.googleapis.com",
           "api_key":"","model":""}}}"#,
    )
    .unwrap();
    app.layout.root = dir.path().to_path_buf();

    let mut v = LlmView::new();
    v.picking = true;
    app.screen = Screen::Llm(v);
    // The list belongs to the provider it was fetched for, and the screen drops
    app.llm_models_for = "google".to_string();

    handle_key(&mut app, key(KeyCode::Esc), &http, &job_tx).await;
    match &app.screen {
        Screen::Llm(v) => assert!(!v.picking, "the list closed"),
        other => panic!("{other:?}"),
    }
    handle_key(&mut app, key(KeyCode::Esc), &http, &job_tx).await;
    assert!(matches!(app.screen, Screen::Normal), "{:?}", app.screen);
}

#[tokio::test]
async fn tab_opens_jobs_from_places_but_never_from_a_dialog() {
    let http = reqwest::Client::new();
    let (job_tx, _job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = App::new("http://127.0.0.1:8901");

    // A confirmation swallows every press: a Tab that swapped it for the jobs
    app.screen = Screen::Confirm(Confirm::rerender());
    handle_key(&mut app, key(KeyCode::Tab), &http, &job_tx).await;
    assert!(
        matches!(app.screen, Screen::Confirm(_)),
        "Tab must not answer a dialog: {:?}",
        app.screen
    );

    // On a place it is the footer's key again, from anywhere.
    app.screen = Screen::Digest(DigestView::new(vec![1, 2, 3]));
    handle_key(&mut app, key(KeyCode::Tab), &http, &job_tx).await;
    assert!(
        matches!(app.screen, Screen::Jobs { .. }),
        "{:?}",
        app.screen
    );
    handle_key(&mut app, key(KeyCode::Tab), &http, &job_tx).await;
    assert!(
        matches!(app.screen, Screen::Digest(_)),
        "Tab comes back to where it was pressed: {:?}",
        app.screen
    );
}

#[tokio::test]
async fn the_counts_line_names_the_rows_whose_worker_went_quiet() {
    let http = reqwest::Client::new();
    let (job_tx, _job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = App::new("http://127.0.0.1:8901");
    let mut orphaned = Task::new(7, Stage::Merge);
    orphaned.state = TaskState::Assigned;
    orphaned.assigned_to = Some("hcm-1".into());
    let mut working = Task::new(8, Stage::Render);
    working.state = TaskState::Running;
    working.assigned_to = Some("hcm-2".into());
    app.tasks = vec![orphaned, working];
    app.tasks.sort_by_key(|t| (t.chapter, t.stage));
    // Only the second box is beating, so chapter 7 is the row to unstick — and
    app.beats = vec![beat("hcm-2", "127.0.0.1", 0, "")];
    app.screen = Screen::Tasks(TasksView::new());

    let text = render_text(&mut app, 140, 44);
    assert!(
        text.contains("1 abandoned"),
        "the count is on the line that counts:\n{text}"
    );
    assert!(text.contains("[all]"), "one chip is always active:\n{text}");
    assert!(text.contains("queued"), "the chip the states cannot spell");

    // Step the cycle all the way round to `abandoned`, which is the chip this
    for _ in 0..Facet::ALL.len() - 1 {
        handle_key(&mut app, key(KeyCode::Right), &http, &job_tx).await;
    }
    match &app.screen {
        Screen::Tasks(v) => assert_eq!(v.facet, Facet::Abandoned),
        other => panic!("{other:?}"),
    }
    let text = render_text(&mut app, 140, 44);
    assert!(text.contains("[abandoned]"), "{text}");
    assert!(
        text.contains("merge:7") && !text.contains("render:8"),
        "only the silent row is left:\n{text}"
    );
    assert!(
        text.contains("x release") && text.contains("W all of hcm-1"),
        "the keys the row answers to are named on it:\n{text}"
    );
    assert!(
        !text.contains("x release · W all of \n"),
        "the holder is named, not left blank:\n{text}"
    );
    assert!(
        text.contains("Esc/q close"),
        "the last key on the hint row has to fit inside the overlay, or the one \
         key nobody can guess is the one that falls off the end:\n{text}"
    );
}

#[tokio::test]
async fn x_releases_the_highlighted_row_and_capital_x_overrides_a_live_holder() {
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = tasks_app();
    app.screen = Screen::Tasks(TasksView::new());

    // Row 0 is ch3's shelved digest: nothing holds it, so `x` says so rather
    // than round-tripping a release whose only possible answer is "holds
    // nothing".
    handle_key(&mut app, key(KeyCode::Char('x')), &http, &job_tx).await;
    assert!(
        job_rx.try_recv().is_err(),
        "nothing to release, nothing sent"
    );
    assert!(
        app.status.text.contains("nothing to release"),
        "{:?}",
        app.status
    );

    // Down to ch3's render, which w1 holds. The op names the row, not the
    handle_key(&mut app, key(KeyCode::Down), &http, &job_tx).await;
    handle_key(&mut app, key(KeyCode::Char('x')), &http, &job_tx).await;
    let req = last_op(&mut job_rx).expect("x dispatches a release");
    assert_eq!(req.op, Op::Release);
    assert_eq!(req.stage, Some(Stage::Render));
    assert_eq!(req.chapter, Some(3));
    assert_eq!(req.force, Some(false));
    assert_eq!(req.worker, None, "one row, not one box");
    assert!(
        matches!(app.screen, Screen::Tasks(_)),
        "the ledger stays open"
    );

    // `X` on the same row is the same call with the live-holder guard off — a
    handle_key(&mut app, key(KeyCode::Char('X')), &http, &job_tx).await;
    let req = last_op(&mut job_rx).expect("X dispatches too");
    assert_eq!(req.stage, Some(Stage::Render));
    assert_eq!(req.force, Some(true));
    assert_eq!(req.worker, None);
    assert!(
        app.status.text.contains("still beating"),
        "the one case with a price says so: {:?}",
        app.status
    );
}

#[tokio::test]
async fn w_asks_before_taking_a_whole_boxs_work_and_the_dialog_names_the_box() {
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = tasks_app();
    app.screen = Screen::Tasks(TasksView::new());

    // The row that is out with a box is the one `W` means; row 0 is shelved and
    handle_key(&mut app, key(KeyCode::Down), &http, &job_tx).await;
    handle_key(&mut app, key(KeyCode::Char('W')), &http, &job_tx).await;
    assert!(
        job_rx.try_recv().is_err(),
        "W asks first — nothing goes on the wire until it is answered"
    );
    match &app.screen {
        Screen::Confirm(c) => {
            match &c.action {
                ConfirmAction::ReleaseWorker {
                    worker,
                    count,
                    beating,
                    ..
                } => {
                    assert_eq!(worker, "w1");
                    assert_eq!(*count, 1, "the row count is named in the dialog");
                    assert!(!beating, "no beat has ever been seen from w1 here");
                }
                other => panic!("{other:?}"),
            }
            assert!(
                !c.danger,
                "a silent box's work is not a danger, it is the point"
            );
        }
        other => panic!("{other:?}"),
    }

    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    let req = last_op(&mut job_rx).expect("answering dispatches");
    assert_eq!(req.op, Op::Release);
    assert_eq!(req.worker.as_deref(), Some("w1"), "by box, not by row");
    assert_eq!(req.force, Some(false));
    assert_eq!(req.chapter, None, "a box scope carries no chapter");
    assert!(
        matches!(app.screen, Screen::Tasks(_)),
        "answering returns to the ledger, where the rows are visible: {:?}",
        app.screen
    );
}

#[tokio::test]
async fn a_requeues_every_assignment_whose_worker_went_quiet() {
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = tasks_app();
    app.screen = Screen::Tasks(TasksView::new());

    // `A` is the timed twin of `x` — `Op::Requeue`, which had no key in the
    handle_key(&mut app, key(KeyCode::Char('A')), &http, &job_tx).await;
    let req = last_op(&mut job_rx).expect("A dispatches");
    assert_eq!(req.op, Op::Requeue);
    assert_eq!(req.stage, None, "the whole ledger, not a chapter");
    assert_eq!(req.worker, None);
    assert!(matches!(app.screen, Screen::Tasks(_)));
    assert!(app.status.text.contains("went quiet"), "{:?}", app.status);
}

#[test]
fn an_empty_ledger_says_what_to_do_instead_of_drawing_nothing() {
    let mut app = App::new("http://127.0.0.1:8901");
    app.screen = Screen::Tasks(TasksView::new());
    let text = render_text(&mut app, 140, 44);
    assert!(text.contains("no tasks in the ledger yet"), "{text}");
    assert!(
        text.contains(":t (translate) to enqueue"),
        "the empty state names the gated command:\n{text}"
    );
}

#[tokio::test]
async fn k_opens_the_ledger_and_esc_closes_it() {
    let http = reqwest::Client::new();
    let (job_tx, _job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = tasks_app();
    handle_key(&mut app, key(KeyCode::Char('K')), &http, &job_tx).await;
    assert!(matches!(app.screen, Screen::Tasks(_)), "{:?}", app.screen);
    let text = render_text(&mut app, 140, 44);
    assert!(text.contains("Tasks —"), "the overlay is open:\n{text}");

    handle_key(&mut app, key(KeyCode::Esc), &http, &job_tx).await;
    assert!(matches!(app.screen, Screen::Normal), "{:?}", app.screen);
}

#[tokio::test]
async fn tab_opens_jobs_and_tab_closes_it_again() {
    // Regression guard for the key move: Jobs used to live only on `J`, and
    // the footer advertised a key nobody associated with "the other side of
    // the dashboard". Tab opens; Tab closes, the same toggle shape the
    let http = reqwest::Client::new();
    let (job_tx, _job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = App::new("http://127.0.0.1:8901");
    handle_key(&mut app, key(KeyCode::Tab), &http, &job_tx).await;
    assert!(
        matches!(app.screen, Screen::Jobs { .. }),
        "{:?}",
        app.screen
    );
    let text = render_text(&mut app, 100, 30);
    assert!(
        text.contains("jobs — all clear"),
        "the overlay draws its empty state:\n{text}"
    );
    assert!(
        text.contains("B starts the backend"),
        "the empty state names the real command, not a dead key:\n{text}"
    );
    // Tab closes what Tab opened, returning to wherever it came from.
    handle_key(&mut app, key(KeyCode::Tab), &http, &job_tx).await;
    assert!(matches!(app.screen, Screen::Normal), "{:?}", app.screen);
    // The mnemonic alias survives, and it too toggles.
    handle_key(&mut app, key(KeyCode::Char('J')), &http, &job_tx).await;
    assert!(
        matches!(app.screen, Screen::Jobs { .. }),
        "{:?}",
        app.screen
    );
    handle_key(&mut app, key(KeyCode::Char('J')), &http, &job_tx).await;
    assert!(matches!(app.screen, Screen::Normal), "{:?}", app.screen);
}

#[test]
fn the_jobs_overlay_sorts_running_first_and_spins_only_running_rows() {
    let mut app = App::new("http://127.0.0.1:8901");
    app.background_jobs = vec![
        BackgroundJob {
            id: 1,
            name: "queued one".into(),
            queued: std::time::Instant::now(),
            started: None,
            activity: "waiting".into(),
        },
        BackgroundJob {
            id: 2,
            name: "running one".into(),
            queued: std::time::Instant::now(),
            started: Some(std::time::Instant::now()),
            activity: "provisioning box-2".into(),
        },
    ];
    app.screen = Screen::Jobs {
        scroll: 0,
        previous: Box::new(Screen::Normal),
    };
    let text = render_text(&mut app, 100, 30);
    let run_at = text.find("running one").expect("running row draws");
    let queued_at = text.find("queued one").expect("queued row draws");
    assert!(run_at < queued_at, "running sorts above queued:\n{text}");
    assert!(
        text.contains("1 running · 1 queued"),
        "the title splits the footer's total:\n{text}"
    );
    assert!(
        text.contains("#1") && text.contains("#2"),
        "each row carries its id:\n{text}"
    );
}

#[test]
fn the_footer_names_the_ledger_when_work_is_shelved() {
    let mut app = tasks_app();
    // Wide enough that the footer is not clipped: the point is that the
    let text = render_text(&mut app, 200, 44);
    assert!(text.contains("1 shelved — K tasks"), "{text}");
}

#[tokio::test]
async fn enter_opens_the_task_page_and_shows_the_whole_reason() {
    let http = reqwest::Client::new();
    let (job_tx, _job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = tasks_app();
    app.screen = Screen::Tasks(TasksView::new());

    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    match &app.screen {
        Screen::TaskDetail(d) => assert_eq!((d.stage, d.chapter), (Stage::Digest, 3)),
        other => panic!("expected the detail page, got {other:?}"),
    }

    let text = render_text(&mut app, 140, 44);
    assert!(
        text.contains("opencode exited 1: model 'claude' unavailable"),
        "the first line of the reason:\n{text}"
    );
    assert!(
        text.contains("second line of the report"),
        "the *rest* of the reason, which the pane never showed:\n{text}"
    );
    assert!(text.contains("3 of 15 before it is shelved"), "{text}");
    assert!(
        text.contains(worker_alias("w2").0),
        "the worker that failed:\n{text}"
    );
    assert!(text.contains("lease"), "the lease is on the page:\n{text}");

    // Esc returns to the list, and to the same view of it.
    handle_key(&mut app, key(KeyCode::Esc), &http, &job_tx).await;
    assert!(matches!(app.screen, Screen::Tasks(_)), "{:?}", app.screen);
}
