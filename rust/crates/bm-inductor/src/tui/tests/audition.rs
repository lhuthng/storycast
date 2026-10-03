use super::*;

// --- auditioning --------------------------------------------------------

#[tokio::test]
async fn current_word_tests_the_current_voice_on_the_shown_line() {
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = audition_app();

    // `:current`: the current voice on the shown line, from cache only. The
    do_command(&mut app, Command::AuditionCurrent, &http, &job_tx);
    let req = last_op(&mut job_rx).expect(":current must dispatch a segment fetch");
    assert_eq!(req.op, Op::Segment);
    assert_eq!(
        req.voice.as_deref(),
        Some("Đức Trí"),
        "the current voice, not the pointed one"
    );
    assert_eq!(req.character.as_deref(), Some("Narrator"));
    let text = req
        .text
        .clone()
        .expect(":current always names the shown line");
    assert_eq!(
        app.audition.as_deref(),
        Some("Đức Trí"),
        "the fetch is marked in flight"
    );
    match &app.screen {
        Screen::Pick(p) => {
            assert_eq!(p.filter, "adam", ":current must not touch the filter");
            assert_eq!(
                p.line.as_ref().map(|l| l.text.clone()),
                Some(text.clone()),
                "the shown line is held before the audio arrives"
            );
        }
        other => panic!(":current must not assign, got {other:?}"),
    }

    // The served sentence is held and shown, so `:try` renders it exactly.
    let seg_key = op_key(&OpRequest {
        op: Op::Segment,
        ..Default::default()
    });
    app.apply(Ev::Done(DoneKind::Op {
        op: Op::Segment,
        key: seg_key,
        ok: true,
        voice: Some("Đức Trí".into()),
        audio_b64: None,
        line_speaker: Some("Narrator".into()),
        line_text: Some("câu đã render".into()),
    }));
    match &app.screen {
        Screen::Pick(p) => {
            let held = p.line.clone().expect("the served sentence is held");
            assert_eq!(
                (held.character.as_str(), held.text.as_str()),
                ("Narrator", "câu đã render")
            );
        }
        other => panic!("{other:?}"),
    }

    // `:try`: that exact served sentence, rendered with the pointed voice.
    finish_audition(&mut app, "Đức Trí");
    do_command(&mut app, Command::AuditionTry, &http, &job_tx);
    let req = last_op(&mut job_rx).expect(":try must dispatch an audition");
    assert_eq!(req.op, Op::PreviewVoice);
    assert_eq!(req.voice.as_deref(), Some("Adam"), "the pointed voice");
    let text = req.text.clone().expect("the line is sent as literal text");
    assert_eq!(
        text, "câu đã render",
        "the served sentence, not a fresh pick: {text:?}"
    );

    // ...and it is still held, so the next audition speaks the same sentence.
    let held = match &app.screen {
        Screen::Pick(p) => p.line.clone().expect("the line is held on the picker"),
        other => panic!("{other:?}"),
    };
    assert_eq!(held.character, "Narrator");
    assert_eq!(held.text, text);

    finish_audition(&mut app, "Adam");
    do_command(&mut app, Command::AuditionTry, &http, &job_tx);
    let again = last_op(&mut job_rx).expect("a second audition dispatches too");
    assert_eq!(
        again.text.as_deref(),
        Some(text.as_str()),
        "re-picking here would compare two voices on two sentences"
    );
}

#[tokio::test]
async fn another_word_rerolls_the_pointed_voice_on_another_line() {
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = audition_app();

    // Hear the candidate, which also picks and holds the line.
    do_command(&mut app, Command::AuditionTry, &http, &job_tx);
    let candidate = last_op(&mut job_rx).expect("dispatched");
    assert_eq!(candidate.voice.as_deref(), Some("Adam"));
    finish_audition(&mut app, "Adam");

    // `:another`: the pointed voice again, re-rolled, a render, never a
    do_command(&mut app, Command::AuditionAnother, &http, &job_tx);
    let reroll = last_op(&mut job_rx).expect(":another must dispatch");
    assert_eq!(reroll.op, Op::PreviewVoice);
    assert_eq!(reroll.voice.as_deref(), Some("Adam"), "the pointed voice");
    assert_eq!(reroll.character.as_deref(), Some("Narrator"));
    let text = reroll.text.clone().expect("a line is always sent");
    assert!(!text.is_empty(), "a reroll still names a line");
    let index = app.lines.as_ref().expect("fixture has lines")["Narrator"].clone();
    assert!(
        index.contains(&text),
        "rerolled from Narrator's lines: {text:?}"
    );
}

#[tokio::test]
async fn audition_without_a_backend_synthesizes_locally() {
    // No inductor, no worker, no sidecar: `:try` still auditions, on this box.
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = audition_app();
    app.conn = Conn::Unknown;
    app.layout = bm_core::Layout::new("/tmp/bm-offline-audition");

    do_command(&mut app, Command::AuditionTry, &http, &job_tx);
    match job_rx
        .try_recv()
        .expect(":try dispatches offline")
        .into_bare()
    {
        Job::PreviewLocal { voice, text, .. } => {
            assert_eq!(voice, "Adam", "the pointed voice");
            assert!(!text.is_empty(), "a line is always sent");
        }
        other => panic!("expected a local preview job, got {other:?}"),
    }
    assert_eq!(app.audition.as_deref(), Some("Adam"));
    assert!(
        app.status.text.contains("locally"),
        "say where it renders: {:?}",
        app.status
    );
}

#[tokio::test]
async fn controlled_letters_other_than_u_r_do_nothing() {
    // `^U` clears; every other controlled letter must leave the audio and
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = audition_app();
    if let Screen::Pick(p) = &mut app.screen {
        p.filter.clear();
    }
    for (code, mods) in [
        (KeyCode::Char('o'), KeyModifiers::CONTROL),
        (KeyCode::Char('n'), KeyModifiers::CONTROL),
    ] {
        handle_key(&mut app, KeyEvent::new(code, mods), &http, &job_tx).await;
    }
    assert!(job_rx.try_recv().is_err(), "no letter binding dispatches");
    match &app.screen {
        Screen::Pick(p) => assert_eq!(p.filter, "", "controlled letters must not type either"),
        other => panic!("must stay in the picker, got {other:?}"),
    }
}

#[tokio::test]
async fn a_second_audition_while_one_renders_is_refused_by_name() {
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = audition_app();
    app.audition = Some("Đức Trí".into());

    do_command(&mut app, Command::AuditionCurrent, &http, &job_tx);
    assert!(job_rx.try_recv().is_err(), "one render at a time");
    assert!(
        app.status.text.contains("Đức Trí"),
        "say what is rendering: {:?}",
        app.status
    );
    assert_eq!(
        app.audition.as_deref(),
        Some("Đức Trí"),
        "the running render keeps the marker"
    );
}

#[tokio::test]
async fn a_refused_audition_does_not_leave_the_screen_wedged() {
    // The bug this guards: the marker used to be set *before* the dispatch, and
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = audition_app();
    app.inflight.push(op_key(&OpRequest {
        op: Op::Segment,
        ..Default::default()
    }));

    do_command(&mut app, Command::AuditionCurrent, &http, &job_tx);
    assert!(job_rx.try_recv().is_err(), "nothing was dispatched");
    assert!(
        app.audition.is_none(),
        "a refused audition must not claim the marker"
    );
    assert!(
        app.status.text.contains("already running"),
        "{:?}",
        app.status
    );
}

#[tokio::test]
async fn plain_o_and_n_still_type_into_the_filter() {
    // Bare letters outside t/T focus the filter and type, o and n stand
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = audition_app();
    // Start from an empty filter so the assertion is about what was typed.
    if let Screen::Pick(p) = &mut app.screen {
        p.filter.clear();
    }
    for c in ['o', 'n'] {
        handle_key(&mut app, key(KeyCode::Char(c)), &http, &job_tx).await;
    }
    match &app.screen {
        Screen::Pick(p) => assert_eq!(p.filter, "on"),
        other => panic!("o and n must type, got {other:?}"),
    }
    assert!(job_rx.try_recv().is_err(), "typing must not dispatch");
}

#[tokio::test]
async fn t_still_types_in_picker_step_1() {
    // Picking a character needs every letter, `t` included (and step 2
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = audition_app();
    if let Screen::Pick(p) = &mut app.screen {
        p.stage = PickStage::Character;
        p.filter.clear();
    }
    for c in ['t', 'T'] {
        handle_key(&mut app, key(KeyCode::Char(c)), &http, &job_tx).await;
    }
    match &app.screen {
        Screen::Pick(p) => {
            assert_eq!(p.stage, PickStage::Character);
            assert_eq!(p.filter, "tT");
        }
        other => panic!("t must type in step 1, got {other:?}"),
    }
    assert!(job_rx.try_recv().is_err(), "typing must not dispatch");
}

#[tokio::test]
async fn audition_focus_plays_t_while_other_letters_filter() {
    // Step 2 opens in audition focus: t/T audition, any other letter
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = audition_app();

    // `t` auditions and never touches the filter.
    do_command(&mut app, Command::AuditionCurrent, &http, &job_tx);
    let _ = last_op(&mut job_rx).expect(":current dispatches");
    finish_audition(&mut app, "Đức Trí");
    // Direct keys do the same without the command line.
    handle_key(&mut app, key(KeyCode::Char('T')), &http, &job_tx).await;
    let req = last_op(&mut job_rx).expect("T dispatches an audition");
    assert_eq!(req.op, Op::PreviewVoice);
    match &app.screen {
        Screen::Pick(p) => {
            assert!(!p.filter_focus, "auditioning never focuses");
            assert_eq!(p.filter, "adam");
        }
        other => panic!("must stay in the picker, got {other:?}"),
    }

    // Any other letter focuses the filter and types.
    finish_audition(&mut app, "Adam");
    handle_key(&mut app, key(KeyCode::Char('o')), &http, &job_tx).await;
    match &app.screen {
        Screen::Pick(p) => {
            assert!(p.filter_focus, "typing focuses");
            assert_eq!(p.filter, "adamo");
        }
        other => panic!("must stay in the picker, got {other:?}"),
    }
    assert!(job_rx.try_recv().is_err(), "typing must not dispatch");
}

#[tokio::test]
async fn filter_focus_types_t_and_esc_blurs_back_to_audition() {
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = audition_app();
    // Focus via ^R, the quiet twin of typing a letter.
    let ctrl_r = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL);
    handle_key(&mut app, ctrl_r, &http, &job_tx).await;
    match &app.screen {
        Screen::Pick(p) => assert!(p.filter_focus, "^R focuses"),
        other => panic!("must stay in the picker, got {other:?}"),
    }
    // Focused, `t` types like every other letter, no audition.
    for c in ['t', 'T'] {
        handle_key(&mut app, key(KeyCode::Char(c)), &http, &job_tx).await;
    }
    match &app.screen {
        Screen::Pick(p) => assert_eq!(p.filter, "adamtT"),
        other => panic!("must stay in the picker, got {other:?}"),
    }
    assert!(job_rx.try_recv().is_err(), "focused typing never auditions");
    // First Esc blurs (stays put), second Esc steps back to step 1.
    handle_key(&mut app, key(KeyCode::Esc), &http, &job_tx).await;
    match &app.screen {
        Screen::Pick(p) => {
            assert!(!p.filter_focus, "first Esc blurs");
            assert_eq!(p.stage, PickStage::Voice, "blurring steps nowhere");
        }
        other => panic!("must stay in step 2, got {other:?}"),
    }
    handle_key(&mut app, key(KeyCode::Esc), &http, &job_tx).await;
    match &app.screen {
        Screen::Pick(p) => assert_eq!(p.stage, PickStage::Character),
        other => panic!("second Esc steps back, got {other:?}"),
    }
}

#[tokio::test]
async fn the_cast_overview_auditions_the_highlighted_speakers_own_voice() {
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = App::new("http://127.0.0.1:8901");
    app.conn = Conn::Up;
    app.roster = Some(roster_fixture());
    app.lines = Some(std::collections::HashMap::from([(
        "Narrator".to_string(),
        vec![audition_line("n")],
    )]));
    app.screen = Screen::Cast(CastView::new());

    // Narrator leads the table and is cast to Đức Trí: no candidate voice
    do_command(&mut app, Command::AuditionTry, &http, &job_tx);
    let req = last_op(&mut job_rx).expect(":try must dispatch");
    assert_eq!(req.voice.as_deref(), Some("Đức Trí"));
    assert_eq!(req.character.as_deref(), Some("Narrator"));
    assert!(req.text.is_some(), "the line came from the scripts");
}

#[tokio::test]
async fn current_word_in_the_cast_overview_tests_the_current_voice_on_the_shown_line() {
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = App::new("http://127.0.0.1:8901");
    app.conn = Conn::Up;
    app.roster = Some(roster_fixture());
    app.lines = Some(std::collections::HashMap::from([(
        "Narrator".to_string(),
        vec![audition_line("n")],
    )]));
    app.screen = Screen::Cast(CastView::new());

    do_command(&mut app, Command::AuditionCurrent, &http, &job_tx);
    let req = last_op(&mut job_rx).expect(":current must dispatch a segment fetch");
    assert_eq!(req.op, Op::Segment);
    assert_eq!(
        req.voice.as_deref(),
        Some("Đức Trí"),
        "the speaker's own voice"
    );
    assert_eq!(req.character.as_deref(), Some("Narrator"));
    assert!(
        req.text.as_deref().is_some_and(|t| !t.is_empty()),
        ":current always names the shown line"
    );
}

#[tokio::test]
async fn cast_keys_play_until_the_filter_takes_focus() {
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = cast_app();

    // `t` auditions in audition focus and never types.
    handle_key(&mut app, key(KeyCode::Char('t')), &http, &job_tx).await;
    let req = last_op(&mut job_rx).expect("t dispatches a segment fetch");
    assert_eq!(req.op, Op::Segment);
    match &app.screen {
        Screen::Cast(v) => {
            assert!(!v.filter_focus, "auditioning never focuses");
            assert!(v.filter.is_empty(), "t never types");
        }
        other => panic!("must stay in the overview, got {other:?}"),
    }

    // Any other letter focuses the filter and types; focused `t` types too.
    finish_audition(&mut app, "Đức Trí");
    handle_key(&mut app, key(KeyCode::Char('x')), &http, &job_tx).await;
    handle_key(&mut app, key(KeyCode::Char('t')), &http, &job_tx).await;
    match &app.screen {
        Screen::Cast(v) => {
            assert!(v.filter_focus, "typing focuses");
            assert_eq!(v.filter, "xt");
        }
        other => panic!("must stay in the overview, got {other:?}"),
    }
    assert!(job_rx.try_recv().is_err(), "focused typing never auditions");

    // First Esc blurs (stays put), second Esc closes to Normal.
    handle_key(&mut app, key(KeyCode::Esc), &http, &job_tx).await;
    match &app.screen {
        Screen::Cast(v) => assert!(!v.filter_focus, "first Esc blurs"),
        other => panic!("must stay in the overview, got {other:?}"),
    }
    handle_key(&mut app, key(KeyCode::Esc), &http, &job_tx).await;
    assert!(
        matches!(app.screen, Screen::Normal),
        "second Esc closes: {:?}",
        app.screen
    );
}

#[tokio::test]
async fn the_cast_overview_will_not_audition_an_unassigned_speaker() {
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = App::new("http://127.0.0.1:8901");
    app.roster = Some(roster_fixture());
    app.screen = Screen::Cast(CastView::new());
    // "Mới" is a known speaker the cast file has never assigned, so the table
    if let Screen::Cast(v) = &mut app.screen {
        v.filter = "moi".into();
    }
    do_command(&mut app, Command::AuditionCurrent, &http, &job_tx);
    assert!(
        job_rx.try_recv().is_err(),
        "an empty voice is nothing to play"
    );
    assert!(
        app.status.text.contains("no voice assigned"),
        "{:?}",
        app.status
    );
}

#[tokio::test]
async fn a_failed_line_index_says_which_half_is_out() {
    let mut app = audition_app();
    app.lines = None;
    app.lines_loading = true;
    app.apply(Ev::Lines(Err(
        "no data/script/NN.json under /r — run :translate first".into(),
    )));
    assert!(
        !app.lines_loading,
        "the guard must be released or nothing ever retries"
    );
    assert!(app.lines.is_none());
    assert!(
        app.status.text.contains("translate"),
        "name the fix: {:?}",
        app.status
    );
    assert!(
        app.status
            .text
            .contains(":current (rendered segments) still plays"),
        "the half that still works must be stated: {:?}",
        app.status
    );
}

#[tokio::test]
async fn every_finished_audition_replaces_the_auditioning_line() {
    // The bug this guards: "auditioning…" is written when the op is
    let key = op_key(&OpRequest {
        op: Op::PreviewVoice,
        ..Default::default()
    });
    let done = |ok: bool, audio_b64: Option<String>| DoneKind::Op {
        op: Op::PreviewVoice,
        key: key.clone(),
        ok,
        voice: Some("Adam".into()),
        audio_b64,
        line_speaker: None,
        line_text: None,
    };
    let in_flight = || {
        let mut app = audition_app();
        app.audition = Some("Adam".into());
        app.set_status(Level::Info, "auditioning Adam (sample) for “Narrator”…");
        app
    };

    // (1) Success with no audio: an older inductor on the other end.
    let mut app = in_flight();
    app.apply(Ev::Done(done(true, None)));
    assert!(app.audition.is_none(), "the marker is released");
    assert!(!app.status.text.contains("auditioning"), "{:?}", app.status);
    assert!(
        app.status.text.contains("restart"),
        "name the fix: {:?}",
        app.status
    );

    // (2) Failure: the error itself is in the event pane, but the bar must
    let mut app = in_flight();
    app.apply(Ev::Done(done(false, None)));
    assert!(!app.status.text.contains("auditioning"), "{:?}", app.status);

    // (3) Audio that is not base64 at all.
    let mut app = in_flight();
    app.apply(Ev::Done(done(true, Some("not base64 !!".into()))));
    assert!(matches!(app.status.level, Level::Error), "{:?}", app.status);
    assert!(app.status.text.contains("undecodable"), "{:?}", app.status);
}

#[tokio::test]
async fn a_completed_audition_writes_the_sample_next_to_the_speaker() {
    // Why the wire carries bytes and not a path: the file has to land on the
    let scratch = std::env::temp_dir().join(format!("bmaud-test-{}-play.wav", std::process::id()));
    let _ = std::fs::remove_file(&scratch);

    let mut app = audition_app();
    app.player = Player::silent_for_test(scratch.clone());
    app.audition = Some("Adam".into());
    app.inflight.push(op_key(&OpRequest {
        op: Op::PreviewVoice,
        ..Default::default()
    }));

    app.apply(Ev::Done(DoneKind::Op {
        op: Op::PreviewVoice,
        key: op_key(&OpRequest {
            op: Op::PreviewVoice,
            ..Default::default()
        }),
        ok: true,
        voice: Some("Adam".into()),
        audio_b64: Some(B64.encode(b"RIFF-fake-wav")),
        line_speaker: None,
        line_text: None,
    }));

    assert_eq!(std::fs::read(&scratch).unwrap(), b"RIFF-fake-wav");
    assert!(app.audition.is_none(), "the marker is released");
    assert!(app.inflight.is_empty(), "the in-flight slot is released");
    assert!(matches!(app.status.level, Level::Info), "{:?}", app.status);
    assert!(app.status.text.contains("playing"), "{:?}", app.status);

    let _ = std::fs::remove_file(&scratch);
}

#[tokio::test]
async fn the_line_index_releases_the_in_flight_count() {
    // The bug this guards: `job_load_lines` reported `Ev::Lines` and never
    // auditions until the process exited, a footer reading "1 job(s)
    // running" over a dashboard with nothing running, permanently.
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = audition_app();
    app.lines = None;
    app.ensure_lines(&job_tx);
    assert_eq!(app.pending, 1, "the dispatch is counted");

    let job = job_rx
        .try_recv()
        .expect("ensure_lines dispatches the index");
    assert!(matches!(job.bare(), Job::LoadLines { .. }), "{job:?}");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Ev>();
    run_job(job, tx).await;

    let mut saw_payload = false;
    while let Ok(ev) = rx.try_recv() {
        saw_payload |= matches!(ev, Ev::Lines(_));
        app.apply(ev);
    }
    assert!(saw_payload, "the index still reports what it found");
    assert_eq!(app.pending, 0, "and the count comes back down");
}

#[tokio::test]
async fn the_picker_shows_the_incumbent_the_held_line_and_the_keys() {
    let mut app = audition_app();
    if let Screen::Pick(p) = &mut app.screen {
        p.line = Some(AuditionLine {
            character: "Narrator".into(),
            text: "câu thử giọng".into(),
        });
    }
    let text = render_text(&mut app, 140, 44);
    assert!(
        text.contains("current:"),
        "the incumbent must be visible:\n{text}"
    );
    assert!(text.contains("Đức Trí"), "…and named, not implied:\n{text}");
    assert!(
        text.contains("câu thử giọng"),
        "the held line must be shown:\n{text}"
    );
    assert!(
        text.contains("T candidate"),
        "the keys must be advertised:\n{text}"
    );
    assert!(
        text.contains("^T another line"),
        "the reroll must be advertised:\n{text}"
    );
}

#[tokio::test]
async fn the_cast_overview_shows_the_line_it_will_audition() {
    let mut app = App::new("http://127.0.0.1:8901");
    app.roster = Some(roster_fixture());
    app.screen = Screen::Cast(CastView::new());
    if let Screen::Cast(v) = &mut app.screen {
        v.line = Some(AuditionLine {
            character: "Narrator".into(),
            text: "câu đang thử".into(),
        });
    }
    let text = render_text(&mut app, 140, 44);
    assert!(
        text.contains("câu đang thử"),
        "a random line is random until it is shown:\n{text}"
    );
    assert!(
        text.contains("assigns nothing"),
        "the words must say they are harmless:\n{text}"
    );
}

#[tokio::test]
async fn tasks_filter_accepts_j_and_k_instead_of_moving() {
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = tasks_app();
    app.screen = Screen::Tasks(TasksView::new());
    handle_key(&mut app, key(KeyCode::Char('j')), &http, &job_tx).await;
    match &app.screen {
        Screen::Tasks(v) => {
            assert_eq!(v.filter, "j");
            assert_eq!(v.cursor, 0, "typing resets the cursor, it never moves it");
        }
        other => panic!("typing must filter, got {other:?}"),
    }
    handle_key(&mut app, key(KeyCode::Char('k')), &http, &job_tx).await;
    match &app.screen {
        Screen::Tasks(v) => assert_eq!(v.filter, "jk"),
        other => panic!("typing must filter, got {other:?}"),
    }
    assert!(
        job_rx.try_recv().is_err(),
        "a filter keystroke must not dispatch"
    );
}
