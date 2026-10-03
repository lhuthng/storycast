use super::*;

#[test]
fn add_sample_rejects_an_empty_path() {
    let mut app = App::new("http://x");
    let p = TextPrompt::new(TextKind::AddSample, "t", "h", "  ");
    assert!(submit_text(&mut app, &p).unwrap_err().contains("empty"));
    let p = TextPrompt::new(TextKind::AddSample, "t", "h", "~/dl/young-female-4.mp3");
    assert!(matches!(
        submit_text(&mut app, &p),
        Ok(Job::AddSample { .. })
    ));
}

#[test]
fn add_sample_refuses_names_and_points_at_the_named_window() {
    // One window, one job: `as` belongs to N, never smuggled through A.
    let mut app = App::new("http://x");
    let p = TextPrompt::new(
        TextKind::AddSample,
        "t",
        "h",
        "refs/narrator.mp3 as Narrator",
    );
    assert!(submit_text(&mut app, &p).unwrap_err().contains("press N"));
}

#[test]
fn add_named_requires_path_as_name_and_stays_private() {
    let mut app = App::new("http://x");
    let p = TextPrompt::new(
        TextKind::AddNamed,
        "t",
        "h",
        "refs/trien-chieu.mp3 as Triển Chiêu",
    );
    match submit_text(&mut app, &p) {
        Ok(Job::AddSample {
            path, name, tags, ..
        }) => {
            assert_eq!(path, "refs/trien-chieu.mp3");
            assert_eq!(name.as_deref(), Some("Triển Chiêu"));
            assert_eq!(tags, Some(Vec::new()), "named voices carry no pool tags");
        }
        other => panic!("expected an add-sample job, got {other:?}"),
    }
    // Half a rename keeps the window open.
    for bad in ["refs/narrator.mp3 as ", " as Narrator", "refs/narrator.mp3"] {
        let p = TextPrompt::new(TextKind::AddNamed, "t", "h", bad);
        assert!(
            submit_text(&mut app, &p).is_err(),
            "{bad:?} must not submit"
        );
    }
}

#[tokio::test]
async fn add_sample_success_reloads_the_roster() {
    let dir = std::env::temp_dir().join("bm-addsample-reload");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("young-male-9.mp3");
    std::fs::write(&src, b"fake").unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Ev>();
    run_job(
        Job::AddSample {
            layout: bm_core::Layout::new(&dir),
            path: src.display().to_string(),
            name: None,
            tags: None,
        },
        tx,
    )
    .await;
    let mut dones = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let Ev::Done(k) = ev {
            dones.push(k);
        }
    }
    assert!(
        dones.iter().any(|k| matches!(k, DoneKind::ReloadRoster)),
        "success must refresh the showing roster: {dones:?}"
    );
    assert!(
        dones.iter().all(|k| !matches!(k, DoneKind::Other)),
        "no stale Done: {dones:?}"
    );

    // Failure refreshes nothing, the roster it would fetch is unchanged.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Ev>();
    run_job(
        Job::AddSample {
            layout: bm_core::Layout::new(std::env::temp_dir().join("bm-addsample-reload")),
            path: "/nonexistent/clip.mp3".into(),
            name: None,
            tags: None,
        },
        tx,
    )
    .await;
    let mut dones = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let Ev::Done(k) = ev {
            dones.push(k);
        }
    }
    assert!(
        dones.iter().all(|k| matches!(k, DoneKind::Other)),
        "{dones:?}"
    );
}

#[tokio::test]
async fn text_prompt_closes_on_submit_or_esc_but_stays_open_on_error() {
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);

    // Esc closes without dispatching.
    let mut app = App::new("http://x");
    app.screen = Screen::Text(TextPrompt::new(TextKind::AddSample, "t", "h", "x.mp3"));
    handle_key(&mut app, key(KeyCode::Esc), &http, &job_tx).await;
    assert!(
        matches!(app.screen, Screen::Normal),
        "Esc must close the prompt"
    );
    assert!(
        job_rx.try_recv().is_err(),
        "a cancelled prompt dispatches nothing"
    );

    // A good submit closes and dispatches exactly one job.
    app.screen = Screen::Text(TextPrompt::new(TextKind::AddSample, "t", "h", "x.mp3"));
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    assert!(
        matches!(app.screen, Screen::Normal),
        "submit must close the prompt"
    );
    assert!(job_rx.try_recv().is_ok());

    // A bad submit keeps the prompt (and its text) open.
    app.screen = Screen::Text(TextPrompt::new(TextKind::AddSample, "t", "h", "   "));
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    assert!(
        matches!(app.screen, Screen::Text(_)),
        "an error must keep the prompt open"
    );
}
