use super::*;

#[test]
fn policy_summary_marks_disabled_stages_lower_case() {
    let mut m = named_machine("192.168.2.2", "box-1");
    assert_eq!(super::model::policy_summary(&m), "M>R>D>C");
    m.task_policy = Some(vec![
        bm_proto::TaskPref {
            stage: Stage::Merge,
            enabled: false,
        },
        bm_proto::TaskPref {
            stage: Stage::Render,
            enabled: true,
        },
        bm_proto::TaskPref {
            stage: Stage::Digest,
            enabled: true,
        },
        bm_proto::TaskPref {
            stage: Stage::Crawl,
            enabled: true,
        },
    ]);
    assert_eq!(super::model::policy_summary(&m), "m>R>D>C");
}

#[test]
fn machine_kind_separates_local_aws_and_remote() {
    use super::model::{machine_kind, machine_label};
    let local = Machine::new("127.0.0.1", "local", 22, None, "both");
    assert_eq!(machine_kind(&local), "local");
    assert_eq!(machine_label(&local), "local");
    let mut aws = Machine::new("52.2.2.2", "ubuntu", 22, None, "worker");
    aws.note = "EC2 i-0123456789abcdef0 (running)".into();
    assert_eq!(machine_kind(&aws), "aws");
    // An unnamed hand-linked remote falls back to its ssh user.
    let remote = Machine::new("192.168.2.2", "thang", 22, None, "worker");
    assert_eq!(machine_kind(&remote), "rmt");
    assert_eq!(machine_label(&remote), "thang");
}

#[tokio::test]
async fn the_digest_manager_lists_chapters_and_hides_the_digested() {
    let mut app = App::new("http://127.0.0.1:8901");
    // The list comes from the ledger the panes already hold, so a chapter the
    app.tasks = vec![
        Task::new(11, Stage::Render),
        Task::new(7, Stage::Digest),
        Task::new(9, Stage::Digest),
        Task::new(7, Stage::Merge),
    ];
    let http = reqwest::Client::new();
    let (job_tx, _job_rx) = tokio::sync::mpsc::unbounded_channel();
    let press = |code| KeyEvent::new(code, KeyModifiers::NONE);

    handle_key(&mut app, press(KeyCode::Char('D')), &http, &job_tx).await;
    match &app.screen {
        Screen::Digest(v) => {
            assert_eq!(v.chapters, vec![7, 9, 11], "one row per chapter, sorted");
            assert!(!v.hide_done, "the whole book is shown to begin with");
            assert_eq!(v.cursor, 0);
        }
        other => panic!("D must open the digest manager: {other:?}"),
    }

    // Rendered, not just constructed: this is the test that would catch the
    let text = render_text(&mut app, 100, 32);
    assert!(text.contains("digest manager"), "{text}");
    for n in ["7", "9", "11"] {
        assert!(text.contains(n), "ch{n} is listed:\n{text}");
    }
    assert!(text.contains("3 chapters"), "the count is stated:\n{text}");
    // The keys, not the prose: a hint that is reworded should not fail a test
    for hint in ["Enter open", "f filter", "←→ chapter", "stop digest"] {
        assert!(
            hint_visible(&text, hint),
            "the {hint:?} hint is on screen:\n{text}"
        );
    }

    // `f` filters. It also has to keep the cursor *inside* the list it filters
    handle_key(&mut app, press(KeyCode::Down), &http, &job_tx).await;
    handle_key(&mut app, press(KeyCode::Down), &http, &job_tx).await;
    handle_key(&mut app, press(KeyCode::Char('f')), &http, &job_tx).await;
    match &app.screen {
        Screen::Digest(v) => {
            assert!(v.hide_done, "the filter is on");
            // Asserted through `Layout::digested`, which is the question the
            let rows = v.rows(&|n| app.layout.digested(n));
            assert!(
                v.cursor < rows.len(),
                "the cursor stayed inside the rows it indexes (cursor {}, rows {rows:?})",
                v.cursor
            );
            assert!(
                v.selected(&|n| app.layout.digested(n)).is_some(),
                "so it still points at a chapter"
            );
        }
        other => panic!("{other:?}"),
    }

    handle_key(&mut app, press(KeyCode::Esc), &http, &job_tx).await;
    assert!(
        matches!(app.screen, Screen::Normal),
        "Esc returns to the dashboard"
    );
}

#[test]
fn the_digest_chapter_page_names_the_round_and_the_last_thing_that_happened() {
    // Built directly rather than by opening a chapter: opening one builds a real
    let mut app = App::new("http://127.0.0.1:8901");
    let mut v = super::screen::DigestView::new(vec![7, 9]);
    v.open = Some(super::screen::DigestChapter {
        n: 9,
        round: bm_core::digest::Round::Attribution,
        prompt: "You are a Vietnamese web-novel dramaturg.".into(),
        cast: None,
        part: None,
        note: "cast pass invalid (roster: unknown speaker \"Lão Tam\"); raw saved".into(),
        done: false,
    });
    app.screen = Screen::Digest(v);

    for (w, h) in [(76, 20), (76, 24), (100, 32), (160, 50)] {
        let text = render_text(&mut app, w, h);
        assert!(text.contains("ch9"), "{w}x{h}: the chapter:\n{text}");
        assert!(text.contains("round 1"), "{w}x{h}: which round:\n{text}");
        assert!(
            text.contains("unknown speaker"),
            "{w}x{h}: the validator's own words, which are the instruction:\n{text}"
        );
        assert!(
            text.contains("clipboard:"),
            "{w}x{h}: and what is on it:\n{text}"
        );
        // The absence that matters: a chapter mid-round is not reported as done.
        assert!(
            !text.contains("reported to the inductor"),
            "{w}x{h}: nothing claims it landed yet:\n{text}"
        );
    }
}

#[test]
fn a_long_validator_complaint_does_not_push_the_chapter_page_off_its_own_box() {
    // The other half of the bug the grid had. A validator's complaint is the
    let mut app = App::new("http://127.0.0.1:8901");
    let mut v = super::screen::DigestView::new(vec![7]);
    v.open = Some(super::screen::DigestChapter {
        n: 7,
        round: bm_core::digest::Round::Staging,
        prompt: "You are a Vietnamese web-novel dramaturg.".into(),
        cast: Some(serde_json::json!({"roster": ["Narrator"]})),
        part: None,
        note: "script pass invalid (segment 12: unknown speaker \"Kẻ Không Có Trong \
               Cast\"; segment 19: music \"buồn\" is not in the palette (quiet, battle, \
               birds, calm); segment 27: missing `music` — when any segment declares \
               one, every segment must). raw saved to data/.last-analyze-raw.json"
            .into(),
        done: false,
    });
    app.screen = Screen::Digest(v);

    for (w, h) in [(76, 20), (76, 24), (100, 32), (160, 50)] {
        let text = render_text(&mut app, w, h);
        // The prompt's identity is the last thing drawn, so it is what falls off
        assert!(
            hint_visible(&text, "clipboard:"),
            "{w}x{h}: the clipboard line survived the complaint:\n{text}"
        );
        assert!(
            hint_visible(&text, "then press v"),
            "{w}x{h}: and so did the instruction:\n{text}"
        );
    }
}

#[tokio::test]
async fn the_digest_grid_scrolls_so_the_selection_is_never_off_screen() {
    // The complaint this answers: move down past the last visible row and the
    let mut app = App::new("http://127.0.0.1:8901");
    let http = reqwest::Client::new();
    let (job_tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let press = |code| KeyEvent::new(code, KeyModifiers::NONE);
    app.screen = Screen::Digest(super::screen::DigestView::new((1000..=1200).collect()));

    // A short terminal, deliberately: on a tall one all seventeen rows fit and the
    let (w, h) = (100, 20);
    let first = render_text(&mut app, w, h);
    assert!(
        first.contains("1000"),
        "the first screen starts at the top of the book:\n{first}"
    );
    assert!(
        first.contains("of 17"),
        "and says how many rows there are:\n{first}"
    );
    assert!(
        first.contains("ch1000 selected"),
        "and which chapter is under the cursor:\n{first}"
    );

    // Walk down the book. ↓ moves a whole row, so this is a realistic journey.
    for _ in 0..20 {
        handle_key(&mut app, press(KeyCode::Down), &http, &job_tx).await;
    }
    let last = render_text(&mut app, w, h);
    assert!(
        last.contains("1200"),
        "the last chapter is drawn — the selection is on screen:\n{last}"
    );
    assert!(
        !last.contains("1000"),
        "and the first row has scrolled away, so the window really moved:\n{last}"
    );
    assert!(
        last.contains("ch1200 selected"),
        "the footer names the chapter under the cursor:\n{last}"
    );
    assert!(
        !last.contains("rows 1-"),
        "and says which rows are on screen:\n{last}"
    );

    // Walking back up brings the top of the book back, so the window follows the
    for _ in 0..20 {
        handle_key(&mut app, press(KeyCode::Up), &http, &job_tx).await;
    }
    let back = render_text(&mut app, w, h);
    assert!(
        back.contains("1000") && !back.contains("1200"),
        "back at the top, and the bottom has scrolled off:\n{back}"
    );
}

#[tokio::test]
async fn the_digest_manager_arrows_follow_the_grid_and_esc_steps_back_from_a_chapter() {
    let mut app = App::new("http://127.0.0.1:8901");
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel();
    let press = |code| KeyEvent::new(code, KeyModifiers::NONE);
    let cursor = |app: &App| match &app.screen {
        Screen::Digest(v) => v.cursor,
        other => panic!("{other:?}"),
    };

    app.screen = Screen::Digest(super::screen::DigestView::new((1..=30).collect()));

    // ←/→ step one chapter; ↑/↓ step a **row**, because that is what the picture
    handle_key(&mut app, press(KeyCode::Right), &http, &job_tx).await;
    assert_eq!(cursor(&app), 1, "→ is one chapter");
    handle_key(&mut app, press(KeyCode::Left), &http, &job_tx).await;
    assert_eq!(cursor(&app), 0, "← is one chapter back");
    handle_key(&mut app, press(KeyCode::Down), &http, &job_tx).await;
    assert_eq!(
        cursor(&app),
        super::screen::DIGEST_COLS,
        "↓ is a whole row, not one chapter"
    );
    handle_key(&mut app, press(KeyCode::Up), &http, &job_tx).await;
    assert_eq!(cursor(&app), 0, "↑ is a row back");
    // Clamped at the end rather than wrapping round to the top.
    for _ in 0..40 {
        handle_key(&mut app, press(KeyCode::Right), &http, &job_tx).await;
    }
    assert_eq!(cursor(&app), 29, "the last chapter, not a wrap");

    // **The regression.** Esc inside a chapter returns to the list. It used to do
    if let Screen::Digest(v) = &mut app.screen {
        v.open = Some(super::screen::DigestChapter {
            n: 7,
            round: bm_core::digest::Round::Attribution,
            prompt: "a prompt".into(),
            cast: None,
            part: None,
            note: String::new(),
            done: false,
        });
    }
    handle_key(&mut app, press(KeyCode::Esc), &http, &job_tx).await;
    match &app.screen {
        Screen::Digest(v) => assert!(v.open.is_none(), "Esc steps back to the list"),
        other => panic!("Esc from a chapter must not close the whole screen: {other:?}"),
    }

    // A second Esc, now from the list, closes the manager.
    handle_key(&mut app, press(KeyCode::Esc), &http, &job_tx).await;
    assert!(
        matches!(app.screen, Screen::Normal),
        "Esc from the list closes it"
    );

    // `x` and `s` are the cluster-wide switch, on the screen it belongs to, the
    app.machines = vec![named_machine("192.168.2.2", "box-1")];
    app.screen = Screen::Digest(super::screen::DigestView::new(vec![1, 2]));
    handle_key(&mut app, press(KeyCode::Char('x')), &http, &job_tx).await;
    match job_rx.try_recv().expect("x dispatches").bare() {
        Job::DigestPolicy { restore, .. } => assert!(!restore, "x is off"),
        other => panic!("{other:?}"),
    }
    // `s` restores, which needs a snapshot; whether this machine has one is the
    handle_key(&mut app, press(KeyCode::Char('s')), &http, &job_tx).await;
    assert!(
        matches!(app.screen, Screen::Digest(_)),
        "the manager stays open through the switch"
    );
}

#[tokio::test]
async fn digest_off_snapshots_every_machine_and_on_refuses_without_a_snapshot() {
    // Two halves of one feature: `:off` must carry *every* machine's policy into
    let mut app = App::new("http://127.0.0.1:8901");
    app.machines = vec![
        named_machine("192.168.2.2", "box-1"),
        named_machine("10.0.0.5", "box-2"),
    ];
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel();

    assert!(
        command_key("off").is_some() && command_key("on").is_some(),
        "both words exist, so :help lists them"
    );
    do_command(&mut app, Command::DigestOff, &http, &job_tx);
    match job_rx.try_recv().expect(":off dispatches").bare() {
        Job::DigestPolicy {
            restore, machines, ..
        } => {
            assert!(!restore, ":off takes the snapshot");
            assert_eq!(machines.len(), 2, "every machine is carried into it");
            assert_eq!(machines[0].0, "192.168.2.2");
        }
        other => panic!("{other:?}"),
    }

    // The refusal, tested against a path that certainly has no snapshot. It has
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("digest-suspend.json");
    let err = super::super::jobs::digest_restore(&missing, "http://127.0.0.1:9", &http, &[])
        .await
        .expect_err("nothing to restore");
    assert!(err.contains("no snapshot"), "{err}");
    assert!(
        err.contains("policy editor"),
        "and names the way to do it by hand instead: {err}"
    );
}

#[tokio::test]
async fn the_policy_panel_toggles_and_reorders_a_machine() {
    let mut app = App::new("http://127.0.0.1:8901");
    app.machines = vec![named_machine("192.168.2.2", "box-1")];
    app.selected = 0;
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel();
    let press = |code| KeyEvent::new(code, KeyModifiers::NONE);

    handle_key(&mut app, press(KeyCode::Char('P')), &http, &job_tx).await;
    match &app.screen {
        Screen::Policy(v) => {
            assert_eq!(v.prefs[0].stage, Stage::Merge, "default leads with merge");
            assert!(v.prefs.iter().all(|p| p.enabled), "all on by default");
        }
        other => panic!("P must open the policy panel: {other:?}"),
    }

    // Enter toggles the highlighted stage off and saves it.
    handle_key(&mut app, press(KeyCode::Enter), &http, &job_tx).await;
    match &app.screen {
        Screen::Policy(v) => assert!(!v.prefs[0].enabled, "merge toggled off"),
        other => panic!("{other:?}"),
    }
    let saved = job_rx.try_recv().expect("a save was dispatched");
    assert!(
        matches!(saved.bare(), Job::SaveTaskPolicy { .. }),
        "the toggle persists: {saved:?}"
    );

    // Space grabs, Down carries merge under render and saves again.
    handle_key(&mut app, press(KeyCode::Char(' ')), &http, &job_tx).await;
    handle_key(&mut app, press(KeyCode::Down), &http, &job_tx).await;
    match &app.screen {
        Screen::Policy(v) => {
            assert_eq!(v.prefs[0].stage, Stage::Render);
            assert_eq!(v.prefs[1].stage, Stage::Merge);
        }
        other => panic!("{other:?}"),
    }

    // Esc leaves the editor; the dashboard returns.
    handle_key(&mut app, press(KeyCode::Esc), &http, &job_tx).await;
    assert!(matches!(app.screen, Screen::Normal));
}

#[test]
fn the_progress_bar_wears_the_task_colour_and_leaves_its_track_dim() {
    // `render_text` can only see glyphs, so this is the one thing it cannot
    fn render_styled(app: &mut App, w: u16, h: u16) -> Vec<Vec<(String, Color)>> {
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| {
                        let cell = &buf[(x, y)];
                        (
                            cell.symbol().to_string(),
                            cell.style().fg.unwrap_or(Color::Reset),
                        )
                    })
                    .collect()
            })
            .collect()
    }

    let mut app = App::new("http://127.0.0.1:8901");
    let mut b = beat("thang-w", "52.2.2.2", 2, "marmot");
    b.stage = Some(Stage::Digest);
    b.chapter = Some(12);
    b.progress = 0.5;
    app.beats = vec![b];

    fn text(row: &[(String, Color)]) -> String {
        row.iter().map(|(s, _)| s.as_str()).collect()
    }
    let rows = render_styled(&mut app, 120, 44);
    // A buffer cell is one glyph, so the row is found by its text and the
    let row = rows
        .iter()
        .find(|r| text(r).contains("digest"))
        .unwrap_or_else(|| {
            panic!(
                "the workers pane lists the stage:\n{}",
                rows.iter().map(|r| text(r)).collect::<Vec<_>>().join("\n")
            )
        });
    let glyphs = |g: &str| -> Vec<Color> {
        row.iter()
            .filter(|(s, _)| s == g)
            .map(|(_, c)| *c)
            .collect()
    };
    let done = glyphs("█");
    let track = glyphs("░");
    assert_eq!(done.len(), 6, "half of a 12-wide bar: {}", text(row));
    assert_eq!(track.len(), 6, "and half of it still to go: {}", text(row));
    let want = themed(stage_color("digest"));
    for c in &done {
        assert_eq!(*c, want, "the work done wears the task's own colour");
    }
    for c in &track {
        assert_eq!(*c, themed(Color::DarkGray), "the empty track stays dim");
    }
}

#[test]
fn the_bar_uses_partial_blocks_and_stays_exact_at_the_ends() {
    // The bar is drawn in two colours now — the work done in the task's hue and
    fn bar(frac: f32, width: usize) -> String {
        let (done, track) = bar_parts(frac, width);
        format!("{done}{track}")
    }

    assert_eq!(bar(0.0, 10), "░".repeat(10));
    assert_eq!(bar(1.0, 10), "█".repeat(10));
    assert_eq!(bar(0.5, 10), "█████░░░░░");
    // A third of one cell in the last slot: the old bar could not show it.
    assert_eq!(bar(0.93, 10), "█████████▎");
    // Width is always exactly what was asked for, split or not.
    for frac in [0.0f32, 0.01, 0.05, 0.33, 0.5, 0.87, 0.99, 1.0] {
        for w in [1usize, 4, 10, 17] {
            assert_eq!(bar(frac, w).chars().count(), w, "bar({frac}, {w})");
            let (done, track) = bar_parts(frac, w);
            assert_eq!(done.chars().count() + track.chars().count(), w);
            // Only the track is ever the light shade, so the tinted half can
            assert!(
                !done.contains('░'),
                "the tinted half must be work done only: {done:?} at {frac}/{w}"
            );
        }
    }
    // Nothing done, nothing left: the ends are each one colour, never both.
    assert_eq!(bar_parts(0.0, 10), (String::new(), "░".repeat(10)));
    assert_eq!(bar_parts(1.0, 10), ("█".repeat(10), String::new()));
}
