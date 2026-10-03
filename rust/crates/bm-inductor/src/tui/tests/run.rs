use super::*;

#[tokio::test]
async fn run_screen_enters_and_launches_with_previewed_values() {
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);

    // `e` opens the config editor prefilled from the preview. The retired
    // `opencode` value maps to no slot, so the line spells the range only —
    // and saving it keeps the current analyzer rather than writing a dead one.
    let mut app = App::new("http://x");
    app.settings = Some(serde_json::json!({
        "start": 1, "count": 1, "analyzer": "opencode", "engine": "vieneu",
    }));
    app.screen = Screen::Run;
    handle_key(&mut app, key(KeyCode::Char('e')), &http, &job_tx).await;
    match &app.screen {
        Screen::Text(p) => assert_eq!(p.buf, "1 1 "),
        other => panic!("expected the config editor, got {other:?}"),
    }

    // `Enter` launches backend-if-needed plus the job: one dispatch.
    app.screen = Screen::Run;
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    assert!(matches!(app.screen, Screen::Normal));
    match job_rx.try_recv().map(Job::into_bare) {
        Ok(Job::StartBackend {
            start,
            count,
            enqueue,
            ..
        }) => {
            assert_eq!((start, count), (1, 1));
            assert!(enqueue, "the run screen always brings a job");
        }
        other => panic!("expected a start-backend job, got {other:?}"),
    }

    // Bare `:B` brings the backend and nothing else.
    let mut app = App::new("http://x");
    handle_key(&mut app, key(KeyCode::Char(':')), &http, &job_tx).await;
    app.screen = Screen::Text(TextPrompt::new(TextKind::Command, ":", "", "B"));
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    assert!(matches!(app.screen, Screen::Normal));
    match job_rx.try_recv().map(Job::into_bare) {
        Ok(Job::StartBackend { enqueue, .. }) => assert!(!enqueue, "bare :B carries no job"),
        other => panic!("expected a start-backend job, got {other:?}"),
    }

    // A second `:B` while the first sequence runs dispatches nothing;
    // `StartDone` re-arms it.
    assert!(app.backend_start_outstanding, "B marks the start in flight");
    app.screen = Screen::Text(TextPrompt::new(TextKind::Command, ":", "", "B"));
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    assert!(
        job_rx.try_recv().is_err(),
        "double B must not queue another start"
    );
    app.apply(Ev::Done(DoneKind::StartDone));
    assert!(!app.backend_start_outstanding, "StartDone re-arms B");
    // …but the cancel flag outlives it. `StartDone` used to be the end of the
    // catch-up; it is now the moment the catch-up *starts*, and the boxes it
    // handed out are still provisioning. Clearing the flag here would leave `X`
    // with nothing to set, and a provision mid-push would launch its worker
    // anyway, a cluster that is not quiet after a stop.
    assert!(
        app.start_cancel.is_some(),
        "the catch-up outlives the start job that made it"
    );

    // `Esc` just closes.
    app.screen = Screen::Run;
    handle_key(&mut app, key(KeyCode::Esc), &http, &job_tx).await;
    assert!(matches!(app.screen, Screen::Normal));
}

#[test]
fn the_start_guard_outlives_the_start_job() {
    // `B` now ends in seconds and hands its boxes to the scheduler. If the guard
    // were released then, a second `B` a moment later would be allowed and would
    // queue a duplicate push at every box, exactly the queueing this change
    // exists to remove. So the flag follows the catch-up, not the start job.
    let mut app = App::new("http://unused");
    app.catchup_jobs = vec![7, 8];
    app.backend_start_outstanding = true;

    app.apply(Ev::Done(DoneKind::StartDone));
    assert!(
        app.backend_start_outstanding,
        "two boxes are still joining, so B is still spoken for"
    );

    app.apply(Ev::JobFinished(7));
    assert!(
        app.backend_start_outstanding,
        "one box left — the sequence is not over"
    );

    app.apply(Ev::JobFinished(8));
    assert!(
        !app.backend_start_outstanding,
        "the last box finished, so B is free again"
    );
    assert!(app.catchup_jobs.is_empty());
    // A job that was never part of a start must not touch the flag.
    app.backend_start_outstanding = true;
    app.apply(Ev::JobFinished(99));
    assert!(app.backend_start_outstanding);
}

#[test]
fn cold_start_catches_up_every_box_despite_stale_online_states() {
    // `:X`, restart, `:B`: the machine list is the last live poll's, states
    // frozen `Online` (`state_failed` keeps the rows), inductor down. The
    // old skip trusted those states and provisioned nothing, the backend
    // came up with no workers and only a second `:B` (fresh states, Offline)
    // brought the boxes.
    use super::super::jobs::split_catchup;
    let stale = || {
        let mut lo = Machine::new("127.0.0.1", "me", 22, None, "worker");
        lo.set_state(MachineState::Online);
        let mut rmt = Machine::new("192.168.2.2", "thang", 22, None, "worker");
        rmt.set_state(MachineState::Online);
        vec![lo, rmt]
    };
    let (todo, online) = split_catchup(stale(), false, &|_| None);
    assert_eq!(todo.len(), 2, "cold start provisions everything");
    assert!(online.is_empty(), "nothing is known-online while down");
    // ...and a warm `B` keeps the skip: re-provisioning a beating box is
    // why `B` on a healthy cluster took minutes.
    let (todo, online) = split_catchup(stale(), true, &|_| None);
    assert!(todo.is_empty(), "nothing to catch up while healthy");
    assert_eq!(online.len(), 2);
}

#[tokio::test]
async fn machine_state_falls_back_to_the_workspace_ledger_while_down() {
    // Nothing answers on port 9 (discard): the API post fails fast and
    // the ledger patch carries the phase instead.
    //
    // With a workspace selected the *book's* ledger is the live one. Patching
    // `<root>/.bm/ledger.json`, which is what a root-only layout did, wrote
    // a file no scheduler reads, so the phase silently never appeared.
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path().join(".bm")).unwrap();
    std::fs::create_dir_all(d.path().join("workspaces/beyond-myriads")).unwrap();
    std::fs::write(d.path().join(".bm/active-workspace"), "beyond-myriads\n").unwrap();
    let layout = bm_core::Layout::resolve(d.path()).unwrap();
    std::fs::write(
        layout.ledger(),
        r#"{"tasks": [], "machines": [
                {"id": "a", "addr": "a", "ssh_user": "u", "ssh_port": 22, "role": "worker", "state": "unknown", "last_seen": 0, "note": ""}
            ]}"#,
    )
    .unwrap();
    set_machine_state(
        "http://127.0.0.1:9",
        &layout,
        "a",
        MachineState::Provisioning,
        "catching up",
    )
    .await;
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(layout.ledger()).unwrap()).unwrap();
    assert_eq!(doc["machines"][0]["state"], "provisioning");
    assert_eq!(doc["machines"][0]["note"], "catching up");
    assert!(
        !d.path().join(".bm/ledger.json").exists(),
        "the root ledger is not the live file any more"
    );
}

#[test]
fn machine_targets_fall_back_to_the_workspace_ledger_file() {
    // The trap: fresh TUI + dead inductor leaves app.machines empty, and
    // B used to default to local-only, silently dropping remotes. The
    // registry file (addr + ssh credentials) stands in instead.
    //
    // With a workspace selected that file is the *workspace's* ledger: reading
    // `<root>/.bm/ledger.json` found nothing, so the fallback quietly returned
    // local-only and the remotes were dropped again, the same bug, one layer
    // down.
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path().join(".bm")).unwrap();
    std::fs::create_dir_all(d.path().join("workspaces/beyond-myriads")).unwrap();
    std::fs::write(d.path().join(".bm/active-workspace"), "beyond-myriads\n").unwrap();
    let layout = bm_core::Layout::resolve(d.path()).unwrap();
    std::fs::write(
        layout.ledger(),
        r#"{"tasks": [], "machines": [
                {"id": "192.168.2.2", "addr": "192.168.2.2", "ssh_user": "thang", "ssh_port": 22, "ssh_key": "/k", "role": "worker", "state": "unknown", "last_seen": 0, "note": ""},
                {"id": "127.0.0.1", "addr": "127.0.0.1", "ssh_user": "local", "ssh_port": 22, "role": "worker", "state": "unknown", "last_seen": 0, "note": ""}
            ]}"#,
    )
    .unwrap();
    let found = registry_machines(&layout);
    assert_eq!(found.len(), 2);
    assert_eq!(found[1].addr, "192.168.2.2");
    assert_eq!(
        found[1].ssh_key.as_deref(),
        Some("/k"),
        "credentials ride along"
    );

    let mut app = App::new("http://x");
    app.layout = layout;
    assert_eq!(
        app.effective_machines().len(),
        2,
        "empty memory reads the file"
    );
    app.machines = vec![Machine::new("127.0.0.1", "local", 22, None, "worker")];
    assert_eq!(
        app.effective_machines().len(),
        1,
        "live data wins when present"
    );

    let nowhere = std::path::Path::new("/nonexistent-root-xyz");
    assert!(
        registry_machines(&bm_core::Layout::new(nowhere)).is_empty(),
        "missing file means local-only, not a crash"
    );
}

#[test]
fn the_run_screen_renders_without_a_roster_or_backend() {
    let mut app = App::new("http://127.0.0.1:8901");
    app.conn = Conn::Down("inductor unreachable at http://127.0.0.1:8901".into());
    app.screen = Screen::Run;
    let text = render_text(&mut app, 140, 44);
    assert!(text.contains("System"), "{text}");
    assert!(text.contains("Enter launches"), "{text}");
    assert!(text.contains("DOWN"), "no backend is attached:\n{text}");
}

#[test]
fn the_run_screen_shows_each_machines_work_split() {
    // Before launching, the operator can see where the work will land: each
    // box's handle, its kind, and the stage order it will be offered.
    let mut app = App::new("http://127.0.0.1:8901");
    let mut aws = named_machine("52.2.2.2", "box-1");
    aws.note = "EC2 i-0123456789abcdef0 (running)".into();
    let mut remote = named_machine("192.168.2.2", "box-2");
    remote.task_policy = Some(vec![
        TaskPref {
            stage: Stage::Merge,
            enabled: false,
        },
        TaskPref {
            stage: Stage::Render,
            enabled: true,
        },
        TaskPref {
            stage: Stage::Digest,
            enabled: true,
        },
        TaskPref {
            stage: Stage::Crawl,
            enabled: true,
        },
    ]);
    app.machines = vec![
        Machine::new("127.0.0.1", "local", 22, None, "both"),
        remote,
        aws,
    ];
    app.screen = Screen::Run;
    let text = render_text(&mut app, 140, 44);
    assert!(
        text.contains("work split"),
        "the section is titled:\n{text}"
    );
    for row in ["local (local)", "box-2 (rmt)", "box-1 (aws)"] {
        assert!(text.contains(row), "missing `{row}`:\n{text}");
    }
    assert!(text.contains("M>R>D>C"), "default order:\n{text}");
    assert!(text.contains("m>R>D>C"), "merge off is lower-case:\n{text}");
    assert!(
        text.contains("render > digest > crawl"),
        "the enabled chain skips the disabled stage:\n{text}"
    );
}

#[test]
fn a_cold_start_names_the_fix_instead_of_reqwest_prose() {
    // The complaint this answers: starting the TUI with no inductor up
    // logged `inductor unreachable at …: error sending request for url …`
    // as an ERROR. A refused connection is the normal cold start, so the
    // poll verdict names `:B` and fits on one line.
    //
    // Asserted on the **verdict** rather than through a socket. The socket
    // version bound an ephemeral port, dropped the listener, and then hoped no
    // other test was handed that port in between, a race with a real window
    // that failed once in six full-suite runs and passed 3/3 in isolation. The
    // trade: reqwest's own classification (`ECONNREFUSED` ⇒ `is_connect`) is no
    // longer exercised here. That is reqwest's contract rather than this repo's
    // logic, and it is now the single `refused` argument below.
    let api = "http://127.0.0.1:8901";

    let down = unreachable_verdict(api, true, "error sending request for url (http://…)");
    assert!(down.contains("inductor is down"), "names the state: {down}");
    assert!(down.contains(":B"), "names the fix: {down}");
    assert!(down.contains(api), "names where: {down}");
    assert!(
        !down.contains("error sending request"),
        "no reqwest prose on the branch where the fix is the answer: {down}"
    );

    // Anything that is *not* a refusal keeps the detail: a timeout or a reset
    // may be a sick inductor rather than an absent one, and telling those two
    // apart is the whole reason the branches exist.
    let sick = unreachable_verdict(api, false, "operation timed out");
    assert!(sick.contains("unreachable"), "{sick}");
    assert!(
        sick.contains("operation timed out"),
        "keeps the detail: {sick}"
    );
    assert!(
        !sick.contains(":B"),
        "a sick inductor is not a cold start, so it must not be told to start one: {sick}"
    );
}

#[test]
fn a_dead_inductor_warns_once_on_the_transition_down() {
    let mut app = App::new("http://127.0.0.1:8901");
    let down = "inductor is down at http://127.0.0.1:8901 — :B to start it";
    let before = app.events.len();
    app.state_failed(down.into());
    assert!(matches!(app.conn, Conn::Down(_)));
    assert_eq!(app.events.len(), before + 1, "the transition is logged");
    let line = app.events.back().unwrap();
    assert!(
        matches!(line.level, Level::Warn),
        "a cold start is not an error: {:?}",
        line.level
    );
    app.state_failed(down.into());
    assert_eq!(app.events.len(), before + 1, "repeats stay quiet");
}

#[test]
fn backend_live_parks_the_range_until_the_next_refresh() {
    let mut app = App::new("http://x");
    assert!(app.pending_enqueue.is_none());
    app.apply(Ev::BackendLive { start: 1, count: 1 });
    assert_eq!(app.pending_enqueue, Some((1, 1)));
}

#[tokio::test]
async fn retry_command_dispatches_the_retry_op_once() {
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
    let mut app = App::new("http://x");
    handle_key(&mut app, key(KeyCode::Char(':')), &http, &job_tx).await;
    app.screen = Screen::Text(TextPrompt::new(TextKind::Command, ":", "", "u"));
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    match job_rx.try_recv().map(Job::into_bare) {
        Ok(Job::Op { req, .. }) => assert_eq!(req.op, Op::Retry),
        other => panic!("expected a retry op, got {other:?}"),
    }
    // A second :u while one is in flight is refused, not queued twice.
    app.screen = Screen::Text(TextPrompt::new(TextKind::Command, ":", "", "u"));
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    assert!(
        job_rx.try_recv().is_err(),
        "duplicate retry must be refused"
    );
    // A bare `u` from Normal mode is a stray key: it must not dispatch.
    let (tx2, mut rx2) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app2 = App::new("http://x");
    handle_key(&mut app2, key(KeyCode::Char('u')), &http, &tx2).await;
    assert!(matches!(app2.screen, Screen::Normal));
    assert!(
        app2.status.text.contains("command line"),
        "{}",
        app2.status.text
    );
    assert!(
        rx2.try_recv().is_err(),
        "a stray u must never dispatch a retry"
    );
}

#[test]
fn range_label_names_the_configured_chapters() {
    assert_eq!(
        range_label(&Some(serde_json::json!({"start": 1, "count": 1}))).as_deref(),
        Some("chapters 1–1")
    );
    assert_eq!(
        range_label(&Some(serde_json::json!({"start": 21, "count": 80}))).as_deref(),
        Some("chapters 21–100")
    );
    assert!(range_label(&None).is_none());
    assert!(range_label(&Some(serde_json::json!({"start": 1, "count": 0}))).is_none());
}
