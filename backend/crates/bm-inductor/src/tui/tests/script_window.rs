use super::*;

#[tokio::test]
async fn the_script_window_suggests_the_chapter_roster_first_then_alphabet() {
    // The picker's ordering rule, pinned: the open chapter's own roster is
    let (job_tx, _job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = App::new("http://127.0.0.1:8901");
    let dir = tempfile::tempdir().unwrap();
    app.layout.root = dir.path().to_path_buf();
    app.layout.ensure().unwrap();
    std::fs::create_dir_all(app.layout.script_dir()).unwrap();
    // The script carries a roster that is a *subset* of who actually speaks:
    std::fs::write(
        app.layout.script(7),
        r#"{"roster":["Mai","Narrator"],"segments":[
            {"speaker":"Mai","text":"Hi."},
            {"speaker":"Lan","text":"Chào."}]}"#,
    )
    .unwrap();

    let mut v = ScriptView::new(&app.layout);
    v.open = Some(7);
    v.segments = v.read_segments(&app.layout, 7);
    let pick = ScriptPick {
        segment: 1,
        expect: "Mai".into(),
        filter: String::new(),
        cursor: 0,
        scroll: 0,
        // The roster cache is what `s` fills from the open chapter's file.
        roster_cache: vec!["Mai".into(), "Narrator".into()],
    };
    let out = v.suggestions(&app, &pick);
    assert_eq!(
        out.iter().take(2).collect::<Vec<_>>(),
        vec!["Mai", "Narrator"],
        "the chapter's roster first: {out:?}"
    );
    // The universe beyond the roster: "Lan" joins because the script's
    assert_eq!(
        out.iter().skip(2).collect::<Vec<_>>(),
        vec!["Lan"],
        "segment speakers join, roster names never duplicate: {out:?}"
    );
    // Typing narrows without dropping the ordering rule.
    let filtered = v.suggestions(
        &app,
        &ScriptPick {
            filter: "mai".into(),
            ..pick.clone()
        },
    );
    assert_eq!(filtered, vec!["Mai"], "{filtered:?}");
    let _ = job_tx;
}

#[tokio::test]
async fn the_script_window_walks_a_chapter_and_repoints_a_segment() {
    // The full ladder with keys: open the window, filter to a chapter,
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = App::new("http://127.0.0.1:8901");
    let dir = tempfile::tempdir().unwrap();
    app.layout.root = dir.path().to_path_buf();
    app.layout.ensure().unwrap();
    std::fs::write(app.layout.bible(), r#"{"characters":[]}"#).unwrap();
    std::fs::create_dir_all(app.layout.script_dir()).unwrap();
    std::fs::write(
        app.layout.script(12),
        r#"{"roster":["Lan"],"segments":[
            {"speaker":"Narrator","text":"Trời hôm nay đẹp."},
            {"speaker":"Lan","text":"Chúng ta đi thôi."}]}"#,
    )
    .unwrap();

    // `:script` opens the window.
    handle_key(&mut app, key(KeyCode::Char(':')), &http, &job_tx).await;
    for c in "script".chars() {
        handle_key(&mut app, key(KeyCode::Char(c)), &http, &job_tx).await;
    }
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    assert!(matches!(app.screen, Screen::Script(_)), "{:?}", app.screen);

    // Filter `12`, Enter opens chapter 12.
    handle_key(&mut app, key(KeyCode::Char('1')), &http, &job_tx).await;
    handle_key(&mut app, key(KeyCode::Char('2')), &http, &job_tx).await;
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    let Screen::Script(v) = app.screen.clone() else {
        panic!("{:?}", app.screen)
    };
    assert_eq!(v.open, Some(12));
    assert_eq!(v.segments.len(), 2);

    // Down to segment 2 (Lan), `s` opens the picker.
    handle_key(&mut app, key(KeyCode::Down), &http, &job_tx).await;
    handle_key(&mut app, key(KeyCode::Char('s')), &http, &job_tx).await;
    let Screen::Script(v) = app.screen.clone() else {
        panic!("{:?}", app.screen)
    };
    let pick = v.pick.as_ref().expect("the picker is up");
    assert_eq!(pick.segment, 2);
    assert_eq!(pick.expect, "Lan", "the guard rides with the request");

    // Type "narr", the roster's Narrator is first so the cursor is on it.
    for c in "narr".chars() {
        handle_key(&mut app, key(KeyCode::Char(c)), &http, &job_tx).await;
    }
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;

    // The dispatched op is the same one `:speaker 12 2 Lan Narrator` runs;
    let req = match job_rx.try_recv() {
        Ok(crate::tui::jobs::Job::Tracked { job, .. }) => match *job {
            crate::tui::jobs::Job::Op { req, .. } => req,
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    };
    assert_eq!(req.op, bm_proto::Op::FixSpeaker);
    assert_eq!(req.chapter, Some(12));
    assert_eq!(req.segment, Some(2));
    assert_eq!(req.expect.as_deref(), Some("Lan"));
    assert_eq!(req.speaker.as_deref(), Some("Narrator"));

    // And the picker closed back onto the segments.
    let Screen::Script(v) = app.screen.clone() else {
        panic!("{:?}", app.screen)
    };
    assert!(v.pick.is_none(), "{:?}", v.pick);

    // Esc steps back: segments, then the list, then gone.
    handle_key(&mut app, key(KeyCode::Esc), &http, &job_tx).await;
    let Screen::Script(v) = app.screen.clone() else {
        panic!("{:?}", app.screen)
    };
    assert!(v.open.is_none(), "first Esc leaves the chapter");
    handle_key(&mut app, key(KeyCode::Esc), &http, &job_tx).await;
    assert!(
        matches!(app.screen, Screen::Normal),
        "second Esc closes the window: {:?}",
        app.screen
    );
}

#[tokio::test]
async fn the_script_window_shows_the_excerpt_chain_a_chapter_is_fed() {
    // `e` on an open chapter raises the excerpt panel: the chapter's own
    let http = reqwest::Client::new();
    let (job_tx, _job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = App::new("http://127.0.0.1:8901");
    let dir = tempfile::tempdir().unwrap();
    // `work` as well as `root`: every data path hangs off `work`, so setting
    app.layout.root = dir.path().to_path_buf();
    app.layout.work = dir.path().to_path_buf();
    app.layout.ensure().unwrap();
    std::fs::write(app.layout.bible(), r#"{"characters":[]}"#).unwrap();
    std::fs::create_dir_all(app.layout.script_dir()).unwrap();
    std::fs::write(
        app.layout.script(11),
        r#"{"excerpt":"Lan is wounded; the stranger stays unnamed.","segments":[]}"#,
    )
    .unwrap();
    std::fs::write(
        app.layout.script(12),
        r#"{"excerpt":"They reach the sect gate; the stranger is named Wei.","roster":["Lan"],"segments":[{"speaker":"Narrator","text":"Trời hôm nay đẹp."}]}"#,
    )
    .unwrap();

    // `:script`, filter to 12, Enter opens the chapter.
    handle_key(&mut app, key(KeyCode::Char(':')), &http, &job_tx).await;
    for c in "script".chars() {
        handle_key(&mut app, key(KeyCode::Char(c)), &http, &job_tx).await;
    }
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    handle_key(&mut app, key(KeyCode::Char('1')), &http, &job_tx).await;
    handle_key(&mut app, key(KeyCode::Char('2')), &http, &job_tx).await;
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;

    // `e` raises the panel and reads both halves.
    handle_key(&mut app, key(KeyCode::Char('e')), &http, &job_tx).await;
    let Screen::Script(v) = app.screen.clone() else {
        panic!("{:?}", app.screen)
    };
    assert!(v.excerpt_open, "the panel is up");
    assert_eq!(
        v.excerpt_own,
        "They reach the sect gate; the stranger is named Wei."
    );
    assert_eq!(
        v.excerpt_fed,
        vec![(
            11u32,
            "Lan is wounded; the stranger stays unnamed.".to_string()
        )],
        "default window is 1, so chapter 12 is fed chapter 11's excerpt"
    );

    // `Esc` drops the panel but keeps the chapter open.
    handle_key(&mut app, key(KeyCode::Esc), &http, &job_tx).await;
    let Screen::Script(v) = app.screen.clone() else {
        panic!("{:?}", app.screen)
    };
    assert!(!v.excerpt_open, "the panel closed");
    assert_eq!(v.open, Some(12), "but the chapter stayed open");

    // Then the ladder: Esc to the list, Esc closes the window.
    handle_key(&mut app, key(KeyCode::Esc), &http, &job_tx).await;
    handle_key(&mut app, key(KeyCode::Esc), &http, &job_tx).await;
    assert!(matches!(app.screen, Screen::Normal), "{:?}", app.screen);
}

#[tokio::test]
async fn the_script_chapter_list_walks_rows_and_columns_like_it_is_drawn() {
    // The list draws twelve chapters across a row, so the arrows owe the
    let http = reqwest::Client::new();
    let (job_tx, _job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = App::new("http://127.0.0.1:8901");
    let dir = tempfile::tempdir().unwrap();
    app.layout.root = dir.path().to_path_buf();
    app.layout.work = dir.path().to_path_buf();
    app.layout.ensure().unwrap();
    std::fs::write(app.layout.bible(), r#"{"characters":[]}"#).unwrap();
    std::fs::create_dir_all(app.layout.script_dir()).unwrap();
    // 25 chapters, so the last row is short — the case a naive `± PER_ROW`
    for n in 1..=25u32 {
        std::fs::write(
            app.layout.script(n),
            r#"{"segments":[{"speaker":"Narrator","text":"x"}]}"#,
        )
        .unwrap();
    }

    handle_key(&mut app, key(KeyCode::Char(':')), &http, &job_tx).await;
    for c in "script".chars() {
        handle_key(&mut app, key(KeyCode::Char(c)), &http, &job_tx).await;
    }
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;

    let selected = |app: &App| {
        let Screen::Script(v) = app.screen.clone() else {
            panic!("{:?}", app.screen)
        };
        v.selected()
    };
    let per = ScriptView::PER_ROW;
    assert_eq!(selected(&app), Some(1), "the list opens on chapter 1");

    // Down is a row: twelve chapters, not one.
    handle_key(&mut app, key(KeyCode::Down), &http, &job_tx).await;
    assert_eq!(
        selected(&app),
        Some(1 + per as u32),
        "↓ steps a whole row of {per}, not a single chapter"
    );

    // Right is a column, and is now bound at all.
    handle_key(&mut app, key(KeyCode::Right), &http, &job_tx).await;
    assert_eq!(selected(&app), Some(2 + per as u32), "→ steps one column");
    handle_key(&mut app, key(KeyCode::Left), &http, &job_tx).await;
    assert_eq!(selected(&app), Some(1 + per as u32), "← steps back");

    // Right off the last column wraps to the next row, the way a grid does.
    handle_key(&mut app, key(KeyCode::Char('l')), &http, &job_tx).await;
    handle_key(&mut app, key(KeyCode::Char('l')), &http, &job_tx).await;
    handle_key(&mut app, key(KeyCode::Char('l')), &http, &job_tx).await;
    handle_key(&mut app, key(KeyCode::Char('l')), &http, &job_tx).await;
    handle_key(&mut app, key(KeyCode::Char('l')), &http, &job_tx).await;
    handle_key(&mut app, key(KeyCode::Char('l')), &http, &job_tx).await;
    handle_key(&mut app, key(KeyCode::Char('l')), &http, &job_tx).await;
    handle_key(&mut app, key(KeyCode::Char('l')), &http, &job_tx).await;
    handle_key(&mut app, key(KeyCode::Char('l')), &http, &job_tx).await;
    // Right off the last column wraps to the next row's first. Park on ch12,
    handle_key(&mut app, key(KeyCode::Home), &http, &job_tx).await;
    for _ in 0..per - 1 {
        handle_key(&mut app, key(KeyCode::Char('l')), &http, &job_tx).await;
    }
    assert_eq!(
        selected(&app),
        Some(per as u32),
        "the last column of row 0 is chapter {per}"
    );
    handle_key(&mut app, key(KeyCode::Char('l')), &http, &job_tx).await;
    assert_eq!(
        selected(&app),
        Some(1 + per as u32),
        "→ off the last column wraps to the next row's first"
    );

    // The short last row: Down past the end clamps on the last chapter rather
    handle_key(&mut app, key(KeyCode::End), &http, &job_tx).await;
    assert_eq!(selected(&app), Some(25));
    handle_key(&mut app, key(KeyCode::Down), &http, &job_tx).await;
    assert_eq!(selected(&app), Some(25), "↓ at the end stays put");
    handle_key(&mut app, key(KeyCode::Right), &http, &job_tx).await;
    assert_eq!(selected(&app), Some(25), "→ at the end stays put");
    handle_key(&mut app, key(KeyCode::Up), &http, &job_tx).await;
    assert_eq!(selected(&app), Some(25 - per as u32), "↑ walks back up");

    // And the order itself: chapter 100 is on screen after 99, not between
    for n in [26u32, 99, 100, 101] {
        std::fs::write(
            app.layout.script(n),
            r#"{"segments":[{"speaker":"Narrator","text":"x"}]}"#,
        )
        .unwrap();
    }
    let chapters = app.layout.script_chapters();
    let at = |n: u32| chapters.iter().position(|c| *c == n).unwrap();
    assert!(
        at(100) > at(99) && at(100) < at(101) && at(100) > at(11),
        "100 must sort after 99 and after 11: {chapters:?}"
    );
}

#[tokio::test]
async fn esc_closes_the_excerpt_panel_and_the_next_esc_leaves_the_chapter() {
    // The panel's own Esc ladder: one press drops the overlay back onto the
    let http = reqwest::Client::new();
    let (job_tx, _job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = App::new("http://127.0.0.1:8901");
    let dir = tempfile::tempdir().unwrap();
    app.layout.root = dir.path().to_path_buf();
    app.layout.work = dir.path().to_path_buf();
    app.layout.ensure().unwrap();
    std::fs::write(app.layout.bible(), r#"{"characters":[]}"#).unwrap();
    std::fs::create_dir_all(app.layout.script_dir()).unwrap();
    std::fs::write(
        app.layout.script(12),
        r#"{"excerpt":"The chapter ends here.","roster":["Lan"],"segments":[{"speaker":"Narrator","text":"Trời hôm nay đẹp."}]}"#,
    )
    .unwrap();

    handle_key(&mut app, key(KeyCode::Char(':')), &http, &job_tx).await;
    for c in "script".chars() {
        handle_key(&mut app, key(KeyCode::Char(c)), &http, &job_tx).await;
    }
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    handle_key(&mut app, key(KeyCode::Char('1')), &http, &job_tx).await;
    handle_key(&mut app, key(KeyCode::Char('2')), &http, &job_tx).await;
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    handle_key(&mut app, key(KeyCode::Char('e')), &http, &job_tx).await;

    let Screen::Script(v) = app.screen.clone() else {
        panic!("expected the script view, got {:?}", app.screen)
    };
    assert!(v.excerpt_open, "`e` opens the panel");
    assert_eq!(v.open, Some(12));

    // One Esc: the overlay goes, the chapter stays open.
    handle_key(&mut app, key(KeyCode::Esc), &http, &job_tx).await;
    let Screen::Script(v) = app.screen.clone() else {
        panic!("Esc must not leave the script window, got {:?}", app.screen)
    };
    assert!(!v.excerpt_open, "the first Esc closes the panel");
    assert_eq!(v.open, Some(12), "and leaves the chapter open");
    assert!(
        !v.segments.is_empty(),
        "the segment list is what the panel was sitting over"
    );

    // The next Esc: out to the chapter list, as the footer says.
    handle_key(&mut app, key(KeyCode::Esc), &http, &job_tx).await;
    let Screen::Script(v) = app.screen.clone() else {
        panic!("expected the script view, got {:?}", app.screen)
    };
    assert_eq!(
        v.open, None,
        "the second Esc steps back to the chapter list"
    );
}

#[tokio::test]
async fn the_excerpt_panel_scrolls_vertically_and_says_when_there_is_nothing_to_scroll() {
    // The bug this pins: the panel used to hand its offset to
    let http = reqwest::Client::new();
    let (job_tx, _job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = App::new("http://127.0.0.1:8901");
    let dir = tempfile::tempdir().unwrap();
    app.layout.root = dir.path().to_path_buf();
    app.layout.work = dir.path().to_path_buf();
    app.layout.ensure().unwrap();
    std::fs::write(app.layout.bible(), r#"{"characters":[]}"#).unwrap();
    std::fs::create_dir_all(app.layout.script_dir()).unwrap();
    // A chapter whose excerpt is long enough that the body cannot fit the box:
    let long = (0..40)
        .map(|i| format!("sentence number {i} of a chapter end that will not fit one screen"))
        .collect::<Vec<_>>()
        .join(" ");
    std::fs::write(
        app.layout.script(12),
        format!(
            r#"{{"excerpt":{},"roster":["Lan"],"segments":[{{"speaker":"Narrator","text":"Trời hôm nay đẹp."}}]}}"#,
            serde_json::to_string(&long).unwrap()
        ),
    )
    .unwrap();

    // `:script`, filter to 12, Enter, `e`.
    handle_key(&mut app, key(KeyCode::Char(':')), &http, &job_tx).await;
    for c in "script".chars() {
        handle_key(&mut app, key(KeyCode::Char(c)), &http, &job_tx).await;
    }
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    handle_key(&mut app, key(KeyCode::Char('1')), &http, &job_tx).await;
    handle_key(&mut app, key(KeyCode::Char('2')), &http, &job_tx).await;
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    handle_key(&mut app, key(KeyCode::Char('e')), &http, &job_tx).await;

    // Render the window at the current scroll, as the dashboard does.
    let render = |app: &mut App, h: u16| -> String {
        let backend = ratatui::backend::TestBackend::new(90, h);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let Screen::Script(v) = app.screen.clone() else {
            panic!("{:?}", app.screen)
        };
        // `draw` rather than the script module directly: the overlay is a
        terminal.draw(|f| draw(f, app)).unwrap();
        let _ = &v;
        let buf = terminal.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let at_top = render(&mut app, 24);
    assert!(at_top.contains("sentence number 0"), "{at_top}");
    assert!(
        at_top.contains("PgUp PgDn Home scroll"),
        "the footer names the keys that move the window: {at_top}"
    );
    assert!(
        !at_top.contains("sentence number 39"),
        "the tail is below the fold at scroll 0: {at_top}"
    );

    // Down moves the window down the rows, and only down. The panel's first body
    handle_key(&mut app, key(KeyCode::Down), &http, &job_tx).await;
    let one_down = render(&mut app, 24);
    assert_ne!(one_down, at_top, "↓ must change what is on screen");
    assert!(
        !one_down.contains("this chapter (the next chapter is fed this)"),
        "the heading scrolled off the top: {one_down}"
    );
    assert!(
        one_down.contains("rows 2-"),
        "the footer reports the window it moved: {one_down}"
    );

    // Up walks it back.
    handle_key(&mut app, key(KeyCode::Up), &http, &job_tx).await;
    assert_eq!(render(&mut app, 24), at_top, "↑ is the inverse of ↓");

    // End reaches the last page, Home returns, and neither overflows the rows.
    handle_key(&mut app, key(KeyCode::End), &http, &job_tx).await;
    let at_end = render(&mut app, 24);
    assert!(at_end.contains("sentence number 39"), "{at_end}");
    assert!(!at_end.contains('\u{0}'), "no blank rows past the end");
    handle_key(&mut app, key(KeyCode::Home), &http, &job_tx).await;
    assert_eq!(render(&mut app, 24), at_top, "Home is the top");

    // And the case the operator actually met: an excerpt that fits on one screen
    std::fs::write(
        app.layout.script(12),
        r#"{"excerpt":"Short.","segments":[{"speaker":"Narrator","text":"Trời hôm nay đẹp."}]}"#,
    )
    .unwrap();
    handle_key(&mut app, key(KeyCode::Char('e')), &http, &job_tx).await;
    handle_key(&mut app, key(KeyCode::Char('e')), &http, &job_tx).await;
    let short = render(&mut app, 24);
    assert!(short.contains("Short."), "{short}");
    assert!(
        short.contains("rows 1-"),
        "a panel that fits names its rows and stops: {short}"
    );
}

/// The gate as the operator meets it: a held cluster says so, in the footer,
#[test]
fn the_footer_calls_out_a_held_cluster_and_claims_nothing_else() {
    let mut app = App::new("http://127.0.0.1:8901");
    app.apply_state(serde_json::json!({
        "tasks": [],
        "machines": [],
        "beats": [],
        "dispatch": {"held": true, "span": "ch4..100 · 3 done, 97 to go", "remaining": [4, 100, 3]},
    }));
    let text = render_text(&mut app, 160, 44);
    assert!(
        hint_visible(&text, "held · ch4..100 · 3 done, 97 to go — :go"),
        "the hold, what is left, and the way out of it:\n{text}"
    );

    // Distributing: no marker at all. An indicator that is always on is an
    app.apply_state(serde_json::json!({
        "tasks": [],
        "machines": [],
        "beats": [],
        "dispatch": {"held": false, "span": "ch4..100 · 3 done, 97 to go", "remaining": [4, 100, 3]},
    }));
    assert!(!render_text(&mut app, 160, 44).contains("held ·"));

    // An inductor older than the gate sends no `dispatch` key at all, and the
    let mut older = App::new("http://127.0.0.1:8901");
    older.apply_state(serde_json::json!({"tasks": [], "machines": [], "beats": []}));
    assert!(!render_text(&mut older, 160, 44).contains("held ·"));
}
