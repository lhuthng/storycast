use super::*;

// ---------------------------------------------------------------------------

#[test]
fn the_sound_editor_marks_remove_unavailable_where_it_is() {
    let (_d, root) = sound_layout("render");
    let mut app = sound_app(&root);
    let text = render_text(&mut app, 120, 60);
    // The three tabs, with their sizes.
    assert!(text.contains("effects 11"), "tabs missing:\n{text}");
    assert!(text.contains("music 9"), "{text}");
    assert!(text.contains("injects 39"), "{text}");
    // Every shipped effect sound answers a shipped rule, so the first row is
    assert!(text.contains("in use"), "{text}");
    assert!(
        text.contains("remove ✗ in use"),
        "the dead key is not marked:\n{text}"
    );
    assert!(text.contains("0 removable"), "{text}");
    // The gain chain, so a pool level is read in context.
    assert!(text.contains("layers.effect.trim"), "{text}");

    // And the inject tab, where nothing places anything yet, offers removal.
    let mut inject = sound_app(&root);
    inject.screen = Screen::Sound(SoundView {
        layer: bm_core::audio_pool::PoolKind::Inject,
        cursor: 0,
        scroll: 0,
    });
    let text = render_text(&mut inject, 120, 40);
    assert!(text.contains("39 removable"), "{text}");
    assert!(
        text.contains(" remove · "),
        "the live key is not offered:\n{text}"
    );
    assert!(!text.contains("remove ✗"), "{text}");
}

/// The bar that says `d` is dead is the screen's whole warning surface, so it
#[test]
fn the_editor_renders_at_every_tier_with_the_warning_intact() {
    let (_d, root) = sound_layout("tiers");
    let mut app = sound_app(&root);
    for (w, h) in [(76, 20), (76, 24), (100, 32), (160, 50)] {
        let text = render_text(&mut app, w, h);
        assert!(
            text.contains("remove ✗ in use"),
            "{w}x{h}: the dead remove key was clipped:\n{text}"
        );
        assert!(text.contains("effects"), "{w}x{h}: no tab line:\n{text}");
        assert!(
            text.contains("in use by"),
            "{w}x{h}: the status column lost its verdict:\n{text}"
        );
    }
    // The gain chain is context, not a warning, and only shows where there is
    assert!(!render_text(&mut app, 76, 24).contains("layers.effect.trim"));
    assert!(render_text(&mut app, 140, 44).contains("layers.effect.trim"));
}

#[test]
fn a_missing_clip_is_called_out_before_the_chapter_is_run() {
    let (_d, root) = sound_layout("broken");
    let layout = bm_core::Layout::new(&root);
    std::fs::remove_file(layout.assets().join("effects/rain-1.mp3")).unwrap();
    let mut app = sound_app(&root);
    // `rain` is the second row of the effect pool in name order.
    let rows = sound::rows(
        app.sound.as_ref().unwrap(),
        bm_core::audio_pool::PoolKind::Effect,
    );
    let at = rows.iter().position(|r| r.name == "rain").unwrap();
    app.screen = Screen::Sound(SoundView {
        layer: bm_core::audio_pool::PoolKind::Effect,
        cursor: at,
        scroll: 0,
    });
    let text = render_text(&mut app, 120, 60);
    assert!(text.contains("1 clip(s) missing"), "{text}");
    assert!(text.contains("MISSING CLIP: effects/rain-1.mp3"), "{text}");
}

#[tokio::test]
async fn the_command_line_opens_the_editor_and_loads_the_pools() {
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = App::new("http://unused");
    app.screen = Screen::Text(TextPrompt::new(TextKind::Command, ":", "", "sound"));
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    assert!(matches!(app.screen, Screen::Sound(_)), "{:?}", app.screen);
    assert!(app.sound_loading, "the pools must be loaded, not assumed");
    // `:pools` and `:sounds` are the same command.
    for word in ["pools", "sounds"] {
        assert_eq!(
            command_key(word),
            Some(Command::Sound),
            "{word} does not route"
        );
    }
    // And it is a `:` command only, no single key reaches it.
    let text = render_text(&mut app, 120, 60);
    assert!(text.contains("loading the three pools"), "{text}");
    let _ = job_rx.try_recv();
}

#[tokio::test]
async fn remove_is_refused_by_name_for_an_entry_still_in_use() {
    let http = reqwest::Client::new();
    let (job_tx, _job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let (_d, root) = sound_layout("guard");
    let mut app = sound_app(&root);
    // `wind` is reached by the mountain tag and by no name at all.
    let rows = sound::rows(
        app.sound.as_ref().unwrap(),
        bm_core::audio_pool::PoolKind::Effect,
    );
    let at = rows.iter().position(|r| r.name == "wind").unwrap();
    app.screen = Screen::Sound(SoundView {
        layer: bm_core::audio_pool::PoolKind::Effect,
        cursor: at,
        scroll: 0,
    });
    handle_key(&mut app, key(KeyCode::Char('d')), &http, &job_tx).await;
    // No dialog: the refusal is stated, with the reason, and nothing was armed.
    assert!(matches!(app.screen, Screen::Sound(_)), "{:?}", app.screen);
    assert!(matches!(app.status.level, Level::Warn), "{:?}", app.status);
    assert!(
        app.status.text.contains("cannot be removed") && app.status.text.contains("mountain"),
        "{:?}",
        app.status
    );

    // An entry nothing reaches does open the confirmation, and answering it
    let mut free = sound_app(&root);
    free.screen = Screen::Sound(SoundView {
        layer: bm_core::audio_pool::PoolKind::Inject,
        cursor: 0,
        scroll: 0,
    });
    let name = sound::rows(
        free.sound.as_ref().unwrap(),
        bm_core::audio_pool::PoolKind::Inject,
    )[0]
    .name
    .clone();
    handle_key(&mut free, key(KeyCode::Char('d')), &http, &job_tx).await;
    let Screen::Confirm(c) = free.screen.clone() else {
        panic!("expected a confirmation, got {:?}", free.screen);
    };
    assert!(matches!(c.action, ConfirmAction::SoundRemove { .. }));
    assert!(c.danger);
    handle_key(&mut free, key(KeyCode::Enter), &http, &job_tx).await;
    assert!(matches!(free.screen, Screen::Sound(_)), "{:?}", free.screen);
    let left = sound::rows(
        free.sound.as_ref().unwrap(),
        bm_core::audio_pool::PoolKind::Inject,
    );
    assert_eq!(left.len(), 38);
    assert!(
        !left.iter().any(|r| r.name == name),
        "{name} is still there"
    );
    // On disk too, not only in the screen.
    let back = bm_core::audio_pool::load_pool(
        &bm_core::Layout::new(&root).pool(bm_core::audio_pool::PoolKind::Inject),
    );
    assert!(!back.contains_key(&name));
}

/// The guard is answered by the *file*, and the dialog is a window in which the
#[tokio::test]
async fn the_guard_is_re_read_when_the_confirmation_is_answered() {
    let http = reqwest::Client::new();
    let (job_tx, _job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let (_d, root) = sound_layout("recheck");
    let mut app = sound_app(&root);
    app.screen = Screen::Sound(SoundView {
        layer: bm_core::audio_pool::PoolKind::Inject,
        cursor: 0,
        scroll: 0,
    });
    let name = sound::rows(
        app.sound.as_ref().unwrap(),
        bm_core::audio_pool::PoolKind::Inject,
    )[0]
    .name
    .clone();
    handle_key(&mut app, key(KeyCode::Char('d')), &http, &job_tx).await;
    assert!(matches!(app.screen, Screen::Confirm(_)), "{:?}", app.screen);

    // While the dialog is open, a script starts placing that sound.
    let layout = bm_core::Layout::new(&root);
    layout.ensure().unwrap();
    std::fs::write(
        layout.script(4),
        format!(r#"{{"segments":[{{"sound":"{name}"}}]}}"#),
    )
    .unwrap();
    app.sound = Some(sound::load(&layout).unwrap());

    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    assert!(
        app.status.text.contains("in use"),
        "a stale dialog removed a sound a script now places: {:?}",
        app.status
    );
    let back = bm_core::audio_pool::load_pool(
        &bm_core::Layout::new(&root).pool(bm_core::audio_pool::PoolKind::Inject),
    );
    assert!(back.contains_key(&name), "{name} was removed anyway");
}

#[tokio::test]
async fn adding_an_entry_writes_the_registry_and_leaves_the_rest_of_it_alone() {
    let http = reqwest::Client::new();
    let (job_tx, _job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let (_d, root) = sound_layout("add");
    let layout = bm_core::Layout::new(&root);
    let registry = layout.pool(bm_core::audio_pool::PoolKind::Inject);
    let before = std::fs::read_to_string(&registry).unwrap();
    // A clip the operator just copied in.
    std::fs::write(layout.assets().join("injects/kettle-1.mp3"), b"").unwrap();

    let mut app = sound_app(&root);
    app.screen = Screen::Sound(SoundView {
        layer: bm_core::audio_pool::PoolKind::Inject,
        cursor: 0,
        scroll: 0,
    });
    handle_key(&mut app, key(KeyCode::Char('a')), &http, &job_tx).await;
    let Screen::Text(prompt) = app.screen.clone() else {
        panic!("expected the add prompt, got {:?}", app.screen);
    };
    assert_eq!(
        prompt.kind,
        TextKind::SoundAdd(bm_core::audio_pool::PoolKind::Inject)
    );
    assert!(
        prompt.hint.contains("mode=hit|overlap|trail"),
        "{}",
        prompt.hint
    );
    app.screen = Screen::Text(TextPrompt::new(
        TextKind::SoundAdd(bm_core::audio_pool::PoolKind::Inject),
        "t",
        "h",
        "name=kettle files=injects/kettle-1.mp3 tags=kettle,whistle mode=hit",
    ));
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    assert!(matches!(app.screen, Screen::Sound(_)), "{:?}", app.screen);
    assert!(matches!(app.status.level, Level::Ok), "{:?}", app.status);

    let after = std::fs::read_to_string(&registry).unwrap();
    assert!(after.contains("\"kettle\""), "{after}");
    // The registry is hand-formatted prose plus entries: adding one must not
    let note_before = before.lines().find(|l| l.contains("\"_note\"")).unwrap();
    assert!(after.contains(note_before), "the note was rewritten");
    assert!(after.starts_with("{\n  \"_note\":"), "the note moved");
    assert!(
        after.contains("  \"boiling-water\": {\n    \"tags\": [\n"),
        "an untouched entry was reflowed:\n{after}"
    );
    // And the screen's own copy agrees with the file.
    assert!(sound::rows(
        app.sound.as_ref().unwrap(),
        bm_core::audio_pool::PoolKind::Inject
    )
    .iter()
    .any(|r| r.name == "kettle"));
}

#[tokio::test]
async fn a_rejected_entry_keeps_the_prompt_open_and_writes_nothing() {
    let http = reqwest::Client::new();
    let (job_tx, _job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let (_d, root) = sound_layout("reject");
    let registry = bm_core::Layout::new(&root).pool(bm_core::audio_pool::PoolKind::Effect);
    let before = std::fs::read_to_string(&registry).unwrap();
    let mut app = sound_app(&root);
    // A clip that is not there: the merge would only warn about it.
    app.screen = Screen::Text(TextPrompt::new(
        TextKind::SoundAdd(bm_core::audio_pool::PoolKind::Effect),
        "t",
        "h",
        "name=ghost files=effects/ghost-1.mp3 tags=ghost",
    ));
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    assert!(
        matches!(app.screen, Screen::Text(_)),
        "the prompt closed on an error"
    );
    assert!(matches!(app.status.level, Level::Error), "{:?}", app.status);
    assert!(app.status.text.contains("no such clip"), "{:?}", app.status);
    assert_eq!(std::fs::read_to_string(&registry).unwrap(), before);
}

#[tokio::test]
async fn a_level_edit_touches_only_the_level() {
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let (_d, root) = sound_layout("level");
    let mut app = sound_app(&root);
    let rows = sound::rows(
        app.sound.as_ref().unwrap(),
        bm_core::audio_pool::PoolKind::Inject,
    );
    let at = rows.iter().position(|r| r.name == "cooking").unwrap();
    let before = rows[at].sound.clone();
    app.screen = Screen::Sound(SoundView {
        layer: bm_core::audio_pool::PoolKind::Inject,
        cursor: at,
        scroll: 0,
    });
    handle_key(&mut app, key(KeyCode::Char('l')), &http, &job_tx).await;
    let Screen::Text(prompt) = app.screen.clone() else {
        panic!("expected the level prompt, got {:?}", app.screen);
    };
    assert_eq!(prompt.buf, "0.8", "prefilled with the level in force");
    app.screen = Screen::Text(TextPrompt::new(
        TextKind::SoundLevel(bm_core::audio_pool::PoolKind::Inject, "cooking".into()),
        "t",
        "h",
        "0.35",
    ));
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    // A retune is a sound-design change, and this is the one write the
    match job_rx.try_recv().map(Job::into_bare) {
        Ok(Job::Op { req, .. }) => assert_eq!(req.op, Op::SoundChanged),
        other => panic!("expected a sound-changed op, got {other:?}"),
    }
    let after = sound::rows(
        app.sound.as_ref().unwrap(),
        bm_core::audio_pool::PoolKind::Inject,
    );
    let after = after.iter().find(|r| r.name == "cooking").unwrap();
    assert_eq!(after.sound.level, Some(0.35));
    // Every other field is where it was, a retune is not a rewrite.
    assert_eq!(after.sound.tags, before.tags);
    assert_eq!(after.sound.files, before.files);
    assert_eq!(after.sound.mode, before.mode);
    assert_eq!(after.sound.looped, before.looped);
    // And clearing it puts the sound back at unity.
    app.screen = Screen::Text(TextPrompt::new(
        TextKind::SoundLevel(bm_core::audio_pool::PoolKind::Inject, "cooking".into()),
        "t",
        "h",
        "",
    ));
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    // Clearing is an edit too: unity is a value, not the absence of one.
    match job_rx.try_recv().map(Job::into_bare) {
        Ok(Job::Op { req, .. }) => assert_eq!(req.op, Op::SoundChanged),
        other => panic!("expected a sound-changed op, got {other:?}"),
    }
    let cleared = sound::rows(
        app.sound.as_ref().unwrap(),
        bm_core::audio_pool::PoolKind::Inject,
    );
    assert_eq!(
        cleared
            .iter()
            .find(|r| r.name == "cooking")
            .unwrap()
            .sound
            .level,
        None
    );
}

#[tokio::test]
async fn switching_layers_lands_on_the_top_row_of_the_new_pool() {
    let http = reqwest::Client::new();
    let (job_tx, _job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let (_d, root) = sound_layout("tabs");
    let mut app = sound_app(&root);
    app.screen = Screen::Sound(SoundView {
        layer: bm_core::audio_pool::PoolKind::Effect,
        cursor: 9,
        scroll: 4,
    });
    handle_key(&mut app, key(KeyCode::Tab), &http, &job_tx).await;
    let Screen::Sound(v) = app.screen.clone() else {
        panic!("{:?}", app.screen);
    };
    assert_eq!(v.layer, bm_core::audio_pool::PoolKind::Music);
    assert_eq!((v.cursor, v.scroll), (0, 0), "the cursor came along");
    // And it wraps both ways.
    handle_key(&mut app, key(KeyCode::BackTab), &http, &job_tx).await;
    handle_key(&mut app, key(KeyCode::BackTab), &http, &job_tx).await;
    let Screen::Sound(v) = app.screen.clone() else {
        panic!("{:?}", app.screen);
    };
    assert_eq!(v.layer, bm_core::audio_pool::PoolKind::Inject);
}

#[test]
fn the_editor_says_why_it_shows_nothing_rather_than_showing_an_empty_pool() {
    let mut app = App::new("http://unused");
    app.sound_error = Some("no assets/ — the clip pools live beside the scene map".into());
    app.screen = Screen::Sound(SoundView::new());
    let text = render_text(&mut app, 120, 60);
    assert!(text.contains("could not be read"), "{text}");
    assert!(text.contains("no assets/"), "{text}");
    assert!(
        text.contains("R retries"),
        "the way out is not offered:\n{text}"
    );
    // The loading state is a different sentence, not the same empty table.
    let mut loading = App::new("http://unused");
    loading.screen = Screen::Sound(SoundView::new());
    let text = render_text(&mut loading, 120, 40);
    assert!(text.contains("loading the three pools"), "{text}");
    assert!(!text.contains("could not be read"), "{text}");
}

/// A checkout with one chapter's script and one rendered segment in it, for
#[tokio::test]
async fn an_entry_whose_clip_is_gone_can_still_be_retagged() {
    let http = reqwest::Client::new();
    let (job_tx, _job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let (_d, root) = sound_layout("gone");
    let layout = bm_core::Layout::new(&root);
    std::fs::remove_file(layout.assets().join("effects/rain-1.mp3")).unwrap();

    let mut app = sound_app(&root);
    let rows = sound::rows(
        app.sound.as_ref().unwrap(),
        bm_core::audio_pool::PoolKind::Effect,
    );
    let at = rows.iter().position(|r| r.name == "rain").unwrap();
    assert_eq!(
        rows[at].missing.len(),
        1,
        "the fixture did not break the row"
    );
    app.screen = Screen::Sound(SoundView {
        layer: bm_core::audio_pool::PoolKind::Effect,
        cursor: at,
        scroll: 0,
    });
    // Retag it, leaving the broken take where it is.
    app.screen = Screen::Text(TextPrompt::new(
        TextKind::SoundEdit(bm_core::audio_pool::PoolKind::Effect, "rain".into()),
        "t",
        "h",
        "name=rain files=effects/rain-1.mp3,effects/rain-2.mp3 tags=rain,calm,drizzle",
    ));
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    assert!(
        matches!(app.status.level, Level::Ok),
        "a retag of a broken entry was refused: {:?}",
        app.status
    );
    let after = sound::rows(
        app.sound.as_ref().unwrap(),
        bm_core::audio_pool::PoolKind::Effect,
    );
    let rain = after.iter().find(|r| r.name == "rain").unwrap();
    assert_eq!(rain.sound.tags, vec!["rain", "calm", "drizzle"]);
    assert_eq!(rain.missing.len(), 1, "the broken take is still reported");

    // A take the edit *adds* is still checked.
    app.screen = Screen::Text(TextPrompt::new(
        TextKind::SoundEdit(bm_core::audio_pool::PoolKind::Effect, "rain".into()),
        "t",
        "h",
        "name=rain files=effects/rain-1.mp3,effects/typo-9.mp3 tags=rain",
    ));
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    assert!(matches!(app.screen, Screen::Text(_)), "the prompt closed");
    assert!(app.status.text.contains("no such clip"), "{:?}", app.status);
}

#[tokio::test]
async fn an_unrendered_held_line_falls_back_to_one_of_hers() {
    // Fresh swap, rendered chapter by chapter: the held line misses in her
    let (_dir, layout) = local_cache_layout();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Ev>();
    job_segment(
        tx,
        layout,
        "Narrator".into(),
        "Đức Trí".into(),
        "a line never rendered anywhere".into(),
    )
    .await;
    let mut done: Option<DoneKind> = None;
    while let Ok(ev) = rx.try_recv() {
        if let Ev::Done(d) = ev {
            done = Some(d);
        }
    }
    match done.expect("the fallback owes exactly one Done") {
        DoneKind::Op {
            ok,
            line_text,
            audio_b64,
            ..
        } => {
            assert!(ok);
            assert_eq!(line_text.as_deref(), Some("Nar nói."));
            assert!(audio_b64.is_some(), "plays bytes, synthesizes nothing");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn current_word_while_disconnected_reads_the_local_cache() {
    // No backend: :current serves the same lookup from this checkout's files
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let (_dir, layout) = local_cache_layout();
    let mut app = audition_app();
    app.conn = Conn::Down("inductor down".into());
    app.layout = layout.clone();
    do_command(&mut app, Command::AuditionCurrent, &http, &job_tx);
    match job_rx
        .try_recv()
        .expect(":current must dispatch while disconnected")
        .into_bare()
    {
        Job::Segment {
            character,
            voice,
            text,
            ..
        } => {
            assert_eq!(character, "Narrator");
            assert_eq!(
                voice, "Đức Trí",
                ":current tests the current voice, never the pointed one"
            );
            assert!(!text.is_empty(), ":current always names the shown line");
        }
        other => panic!("offline :current must be a Segment job, got {other:?}"),
    }
    assert_eq!(app.audition.as_deref(), Some("Đức Trí"));

    // The worker reports through the same Done shape, so holding and
    let (tx2, mut rx2) = tokio::sync::mpsc::unbounded_channel::<Ev>();
    job_segment(
        tx2,
        layout,
        "Narrator".into(),
        "Đức Trí".into(),
        "Nar nói.".into(),
    )
    .await;
    let mut done: Option<DoneKind> = None;
    while let Ok(ev) = rx2.try_recv() {
        if let Ev::Done(d) = ev {
            done = Some(d);
        }
    }
    match done.expect("the job owes exactly one Done") {
        DoneKind::Op {
            ok,
            voice,
            audio_b64,
            line_speaker,
            line_text,
            ..
        } => {
            assert!(ok);
            assert_eq!(voice.as_deref(), Some("Đức Trí"));
            assert_eq!(line_speaker.as_deref(), Some("Narrator"));
            assert_eq!(line_text.as_deref(), Some("Nar nói."));
            let wav = B64.decode(audio_b64.unwrap().as_bytes()).unwrap();
            assert_eq!(wav, b"RIFF-fake-local");
        }
        other => panic!("{other:?}"),
    }
    assert!(
        app.inflight.iter().any(|k| k.starts_with("segment|")),
        "the slot frees on Done"
    );
}

#[tokio::test]
async fn current_word_with_no_backend_and_no_checkout_says_so() {
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = audition_app();
    app.conn = Conn::Down("inductor down".into());
    app.layout = bm_core::Layout::new("");
    do_command(&mut app, Command::AuditionCurrent, &http, &job_tx);
    assert!(
        job_rx.try_recv().is_err(),
        "nothing to read from, nothing dispatched"
    );
    assert!(app.audition.is_none(), "no marker without work");
    assert!(
        app.status.text.contains(":B"),
        "name the way back: {:?}",
        app.status
    );
}

#[tokio::test]
async fn down_at_the_last_row_stays_put() {
    // The highlight must never leave the list: Down past the end used to
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = audition_app();
    handle_key(&mut app, key(KeyCode::Down), &http, &job_tx).await;
    match &app.screen {
        // One voice in the filter, so Down has nowhere to go. Row 0 is its
        Screen::Pick(p) => assert_eq!(
            filtered_voices(&app, &p.filter)
                .get(p.cursor)
                .and_then(|r| r.voice())
                .map(|v| v.name.as_str()),
            Some("Adam"),
            "the highlight stays on the one voice there is"
        ),
        other => panic!("{other:?}"),
    }
    assert!(job_rx.try_recv().is_err(), "movement dispatches nothing");

    let mut app = App::new("http://127.0.0.1:8901");
    app.roster = Some(roster_fixture());
    let mut v = CastView::new();
    v.filter = "kien".into();
    app.screen = Screen::Cast(v);
    handle_key(&mut app, key(KeyCode::Down), &http, &job_tx).await;
    match &app.screen {
        Screen::Cast(v) => assert_eq!(v.cursor, 0, "one row in the filter, nowhere to go"),
        other => panic!("{other:?}"),
    }
}
