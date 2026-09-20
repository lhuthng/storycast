//! Key and render tests, moved as one file.
use super::app::App;
use super::audio::Player;
use super::audition::AuditionLine;
use super::draw::draw;
use super::input::command::{command_key, do_command, Command};
use super::input::runconfig::{
    parse_mix_config, parse_run_config, run_preview, save_run_config, save_ssh_setting,
};
use super::input::submit::submit_text;
use super::input::{handle_key, op_key, urlencode};
use super::jobs::{job_segment, run_job, set_machine_state, DoneKind, Ev, Job};
use super::layout::{
    cols, size_class, width_of, Size, COMPACT_EVENTS_MIN_H, COMPACT_FOOTER_H, COMPACT_MACHINES_H,
    COMPACT_MACHINE_COLS, COMPACT_WORKERS_H, COMPACT_WORKER_COLS, FULL_EVENTS_MIN_H, FULL_FOOTER_H,
    FULL_H, FULL_MACHINES_H, FULL_TASKS_H, FULL_W, FULL_WORKERS_H, KEYS_COMPACT, KEYS_FULL, MIN_H,
    MIN_W,
};
use super::model::*;
use super::screen::*;
use super::sound::{self, SoundView};
use super::style::*;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use bm_proto::{
    Heartbeat, Machine, MachineState, Op, OpRequest, Roster, Stage, Task, TaskState, VoiceInfo,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Color;
use std::collections::BTreeMap;

#[tokio::test]
async fn tracked_jobs_keep_lifecycle_serial_and_commands_live() {
    use super::input::dispatch;
    use super::jobs::run_jobs_with;
    use std::sync::Arc;
    use std::time::Duration;
    let gate = Arc::new(tokio::sync::Notify::new());
    let (job_tx, job_rx) = tokio::sync::mpsc::unbounded_channel();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let blocked = gate.clone();
    let worker = tokio::spawn(run_jobs_with(job_rx, tx, move |job, tx| {
        let blocked = blocked.clone();
        async move {
            if matches!(job, Job::StartBackend { .. }) {
                blocked.notified().await;
            }
            let _ = tx.send(Ev::Log(LogLine {
                level: Level::Info,
                wall: 0,
                text: job.label(),
            }));
            let _ = tx.send(Ev::Done(job.fallback_done()));
            let _ = tx.send(Ev::Done(DoneKind::Other));
        }
    }));
    let mut app = App::new("http://unused");
    let start = Job::StartBackend {
        layout_root: Default::default(),
        api: "unused".into(),
        api_up: false,
        start: 1,
        count: 1,
        enqueue: false,
        machines: vec![],
        cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        settings_key: None,
    };
    assert!(dispatch(&mut app, &job_tx, start));
    assert!(dispatch(
        &mut app,
        &job_tx,
        Job::StopBackend {
            layout_root: Default::default(),
            machines: vec![],
            api: "unused".into(),
            settings_key: None,
        }
    ));
    assert!(dispatch(
        &mut app,
        &job_tx,
        Job::LoadLines {
            layout_root: Default::default()
        }
    ));
    assert_eq!(
        app.background_jobs.iter().map(|j| j.id).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert_eq!(app.background_jobs[0].name, "start backend");
    assert!(app.background_jobs.iter().all(|j| j.started.is_none()));
    assert!(app.background_jobs[0].queued <= std::time::Instant::now());
    let mut finished = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let ev = rx.recv().await.unwrap();
            assert!(!matches!(ev, Ev::JobStarted(2)));
            let end = matches!(ev, Ev::JobFinished(3));
            if let Ev::JobFinished(id) = &ev {
                finished.push(*id);
            }
            app.apply(ev);
            if end {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(app.pending, 2);
    assert_eq!(app.background_jobs.len(), 2);
    assert!(app.background_jobs[1].started.is_none());
    gate.notify_one();
    drop(job_tx);
    tokio::time::timeout(Duration::from_secs(2), worker)
        .await
        .unwrap()
        .unwrap();
    while let Some(ev) = rx.recv().await {
        if let Ev::JobFinished(id) = &ev {
            finished.push(*id);
        }
        app.apply(ev);
    }
    assert_eq!(finished, vec![3, 1, 2]);
    assert_eq!(app.pending, 0);
    assert!(app.background_jobs.is_empty());
}

#[test]
fn tracked_activity_and_cleanup_use_identity_not_queue_order() {
    let mut app = App::new("http://unused");
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    for _ in 0..2 {
        super::input::dispatch(
            &mut app,
            &tx,
            Job::LoadLines {
                layout_root: Default::default(),
            },
        );
    }
    app.apply(Ev::JobStarted(2));
    app.apply(Ev::JobProgress {
        id: 2,
        text: "machine b: waiting".into(),
    });
    assert!(app.background_jobs[0].started.is_none());
    assert!(app.background_jobs[1].started.is_some());
    assert_eq!(app.background_jobs[1].activity, "machine b: waiting");
    app.apply(Ev::Done(DoneKind::Other));
    assert_eq!(app.background_jobs.len(), 2);
    app.apply(Ev::JobFinished(2));
    app.apply(Ev::JobFinished(2));
    app.apply(Ev::JobProgress {
        id: 2,
        text: "late".into(),
    });
    assert_eq!(app.background_jobs[0].id, 1);
    assert_eq!(app.background_jobs[0].activity, "queued");
    assert_eq!(app.pending, 1);
}

#[tokio::test]
async fn tracked_crashes_and_missing_done_release_markers() {
    let mut app = App::new("http://unused");
    let (job_tx, job_rx) = tokio::sync::mpsc::unbounded_channel();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let http = reqwest::Client::new();
    assert!(super::input::dispatch_op(
        &mut app,
        &job_tx,
        &http,
        OpRequest {
            op: Op::PreviewVoice,
            voice: Some("private voice".into()),
            ..Default::default()
        }
    ));
    app.audition = Some("private voice".into());
    app.ensure_lines(&job_tx);
    app.load_roster(&job_tx, &http);
    app.backend_start_outstanding = true;
    super::input::dispatch(
        &mut app,
        &job_tx,
        Job::StartBackend {
            layout_root: Default::default(),
            api: "unused".into(),
            api_up: false,
            start: 1,
            count: 1,
            enqueue: false,
            machines: vec![],
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            settings_key: None,
        },
    );
    drop(job_tx);
    super::jobs::run_jobs_with(job_rx, tx, |job, _tx| async move {
        if !matches!(job, Job::LoadLines { .. }) {
            panic!("simulated crash");
        }
    })
    .await;
    while let Some(ev) = rx.recv().await {
        app.apply(ev);
    }
    assert_eq!(app.pending, 0);
    assert!(app.background_jobs.is_empty());
    assert!(app.inflight.is_empty());
    assert!(app.audition.is_none());
    assert!(!app.lines_loading);
    assert!(!app.roster_loading);
    assert!(!app.backend_start_outstanding);
}

#[test]
fn failed_dispatch_and_id_exhaustion_leave_no_markers() {
    let mut app = App::new("http://unused");
    let http = reqwest::Client::new();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    drop(rx);
    app.pending = 7;
    app.audition = Some("voice".into());
    assert!(!super::input::dispatch_op(
        &mut app,
        &tx,
        &http,
        OpRequest {
            op: Op::PreviewVoice,
            ..Default::default()
        }
    ));
    app.ensure_lines(&tx);
    app.load_roster(&tx, &http);
    assert!(!app.lines_loading && !app.roster_loading);
    assert!(app.audition.is_none() && app.inflight.is_empty());
    assert_eq!(app.pending, 7);
    assert_eq!(app.next_job_id, 0);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    app.next_job_id = u64::MAX;
    assert!(!super::input::dispatch_op(
        &mut app,
        &tx,
        &http,
        OpRequest::default()
    ));
    assert!(rx.try_recv().is_err());
    assert!(app.background_jobs.is_empty() && app.inflight.is_empty());
    assert_eq!(app.pending, 7);
    assert_eq!(app.next_job_id, u64::MAX);
}

#[tokio::test]
async fn cancelled_start_never_touches_backend() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    super::jobs::job_start_backend(
        tx,
        Default::default(),
        "unused".into(),
        false,
        1,
        1,
        true,
        vec![],
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        None,
    )
    .await;
    let mut cancelled = false;
    let mut done = 0;
    while let Some(ev) = rx.recv().await {
        match ev {
            Ev::Log(line) => {
                cancelled |= line.text.contains("cancelled");
                assert!(!line.text.contains("all machines caught up"));
            }
            Ev::Done(DoneKind::StartDone) => done += 1,
            Ev::BackendLive { .. } => panic!("cancelled start became live"),
            _ => {}
        }
    }
    assert!(cancelled);
    assert_eq!(done, 1);
}

#[test]
fn accents_are_folded_so_filters_ignore_diacritics() {
    assert_eq!(fold("Thái Sơn"), "thai son");
    assert_eq!(fold("Đức Trí"), "duc tri");
    assert_eq!(fold("Thục Đoan"), "thuc doan");
    assert_eq!(fold("Lạc Lan Tuyết"), "lac lan tuyet");
    assert!(matches("thai son", "Thái Sơn"));
    assert!(matches("duc", "Đức Trí"));
    assert!(
        matches("", "anything"),
        "an empty filter matches everything"
    );
    assert!(!matches("adam", "Thái Sơn"));
}

#[test]
fn neutral_does_not_fold_to_a_female_marker() {
    // Guards the Python/Rust twin of the same bug.
    assert_eq!(fold("neutral"), "neutral");
}

#[test]
fn text_prompt_edits_by_character_not_byte() {
    let mut p = TextPrompt::new(TextKind::AddMachine, "t", "h", "Đức");
    // Three characters, six bytes: a byte-indexed cursor would land inside
    // 'ứ' and panic on the next edit.
    assert_eq!(p.len(), 3);
    assert_eq!(p.cursor, 3);
    p.left();
    assert_eq!(p.cursor, 2, "cursor 2 sits before the third character");
    p.insert('x');
    assert_eq!(p.buf, "Đứxc");
    assert_eq!(p.cursor, 3);
    p.backspace();
    assert_eq!(p.buf, "Đức");
    assert_eq!(p.cursor, 2);
    p.home();
    p.delete();
    assert_eq!(p.buf, "ức");
    p.kill_to_start();
    assert_eq!(p.buf, "ức", "cursor is already at 0, so nothing is cut");
    p.end();
    assert_eq!(p.cursor, 2);
    p.kill_word();
    assert_eq!(p.buf, "");
}

#[test]
fn kill_word_stops_at_a_space() {
    let mut p = TextPrompt::new(TextKind::Translate, "t", "h", "21 80");
    p.kill_word();
    assert_eq!(p.buf, "21 ");
    p.kill_word();
    assert_eq!(p.buf, "");
}

#[test]
fn translate_prompt_rejects_garbage_instead_of_defaulting() {
    let mut app = App::new("http://x");
    let p = TextPrompt::new(TextKind::Translate, "t", "h", "abc 80");
    let err = submit_text(&mut app, &p).unwrap_err();
    assert!(err.contains("not a chapter number"), "{err}");

    let p = TextPrompt::new(TextKind::Translate, "t", "h", "21");
    assert!(submit_text(&mut app, &p).unwrap_err().contains("expected"));

    let p = TextPrompt::new(TextKind::Translate, "t", "h", "21 0");
    assert!(submit_text(&mut app, &p)
        .unwrap_err()
        .contains("at least 1"));

    let p = TextPrompt::new(TextKind::Translate, "t", "h", "21 80");
    assert!(submit_text(&mut app, &p).is_ok());
}

#[test]
fn run_config_parses_range_analyzer_and_models() {
    let (s, c, a, m) = parse_run_config("1 1", "opencode").unwrap();
    assert_eq!((s, c), (1, 1));
    assert_eq!(a, "opencode");
    assert!(m.is_none(), "omitted models stay out of the file");
    let (_, _, a, m) = parse_run_config("2 5 gemini 3.8-flash, 3.7-flash", "opencode").unwrap();
    assert_eq!(a, "gemini");
    assert_eq!(m.unwrap(), vec!["3.8-flash", "3.7-flash"]);
    assert!(parse_run_config("abc 80", "opencode")
        .unwrap_err()
        .contains("not a chapter number"));
    assert!(parse_run_config("1 1 watson", "opencode")
        .unwrap_err()
        .contains("unknown"));
    assert!(
        parse_run_config("1 1 gemini 3.8-flash 3.7-flash", "opencode")
            .unwrap_err()
            .contains("comma-separated")
    );
    assert!(parse_run_config("1 1 gemini ,", "opencode")
        .unwrap_err()
        .contains("empty"));
}

#[test]
fn run_config_save_persists_everything_it_parsed() {
    let dir = std::env::temp_dir().join("bm-runconfig-save");
    let _ = std::fs::remove_dir_all(&dir);
    let mut app = App::new("http://x");
    app.layout_root = dir.clone();

    let msg = save_run_config(&app, "1 1 gemini 3.8-flash,3.7-flash").unwrap();
    assert!(msg.contains("ch1"), "{msg}");
    let saved: bm_core::config::Settings =
        bm_core::read_json(&bm_core::Layout::new(&dir).settings()).unwrap();
    assert_eq!((saved.start, saved.count), (1, 1));
    assert_eq!(saved.analyzer, "gemini");
    assert_eq!(saved.analyze_models, vec!["3.8-flash", "3.7-flash"]);

    // Omitted models keep the saved chain — a blank field must not wipe it.
    save_run_config(&app, "1 1 gemini").unwrap();
    let saved: bm_core::config::Settings =
        bm_core::read_json(&bm_core::Layout::new(&dir).settings()).unwrap();
    assert_eq!(saved.analyze_models, vec!["3.8-flash", "3.7-flash"]);

    assert!(save_run_config(&app, "1 1 watson")
        .unwrap_err()
        .contains("unknown"));
}

#[test]
fn run_preview_prefers_live_api_then_file_then_defaults() {
    // Live backend: its boot-time settings, labeled as such.
    let mut app = App::new("http://x");
    app.settings = Some(serde_json::json!({
        "start": 5, "count": 2, "analyzer": "gemini",
        "analyze_models": ["3.8-flash"], "engine": "vieneu",
    }));
    let cfg = run_preview(&app);
    assert!(cfg.live);
    assert_eq!((cfg.start, cfg.count), (5, 2));
    assert_eq!(cfg.analyzer, "gemini");
    assert_eq!(cfg.models, vec!["3.8-flash"]);
    app.settings.as_mut().unwrap()["inject_volume"] = serde_json::json!(0.25);
    assert_eq!(run_preview(&app).inject_volume, 0.25);
    assert_eq!(super::input::runconfig::mix_prefill(&app), "1.25 1 1 0.25");
    assert_eq!(
        (
            cfg.speed,
            cfg.effect_volume,
            cfg.music_volume,
            cfg.inject_volume
        ),
        (1.25, 1.0, 1.0, 1.0)
    );

    // Down backend: the saved file is what the next boot will use.
    let dir = std::env::temp_dir().join("bm-runconfig-preview");
    let _ = std::fs::remove_dir_all(&dir);
    let settings = bm_core::config::Settings {
        start: 1,
        count: 1,
        ..bm_core::config::Settings::default()
    };
    settings
        .save(&bm_core::Layout::new(&dir).settings())
        .unwrap();
    let mut app = App::new("http://x");
    app.layout_root = dir;
    let cfg = run_preview(&app);
    assert!(!cfg.live);
    assert!(cfg.saved, "a settings file exists");
    assert_eq!((cfg.start, cfg.count), (1, 1));

    // Neither: honest defaults, labeled as nobody's choice.
    let app = App::new("http://x");
    let cfg = run_preview(&app);
    assert!(!cfg.live);
    assert!(!cfg.saved);
    assert_eq!((cfg.start, cfg.count), (1, 1));
    assert_eq!(
        (
            cfg.speed,
            cfg.effect_volume,
            cfg.music_volume,
            cfg.inject_volume
        ),
        (1.25, 1.0, 1.0, 1.0)
    );
}

#[test]
fn mix_config_parses_ranges_and_rejects_garbage() {
    // The prompt validates; the op itself saves, so a typo keeps the prompt
    // open and never dispatches.
    assert_eq!(
        parse_mix_config("1.25 1.0 1.0 0.5").unwrap(),
        (1.25, 1.0, 1.0, Some(0.5))
    );
    assert_eq!(
        parse_mix_config("0.5 0 2 1").unwrap(),
        (0.5, 0.0, 2.0, Some(1.0))
    );
    assert!(parse_mix_config("1.25 1.0")
        .unwrap_err()
        .contains("expected"));
    assert_eq!(
        parse_mix_config("1.25 1.0 1.0").unwrap(),
        (1.25, 1.0, 1.0, None)
    );
    for input in ["1 1 1 -0.1", "1 1 1 NaN", "1 1 1 inf"] {
        assert!(parse_mix_config(input).unwrap_err().contains("inject"));
    }
    assert!(parse_mix_config("1 1 1 1 1")
        .unwrap_err()
        .contains("expected"));
    assert!(parse_mix_config("0.4 1 1 1").unwrap_err().contains("speed"));
    assert!(parse_mix_config("2.1 1 1 1").unwrap_err().contains("speed"));
    assert!(parse_mix_config("1 3 1 1").unwrap_err().contains("fx"));
    assert!(parse_mix_config("1 1 -0.1 1")
        .unwrap_err()
        .contains("music"));
    assert!(parse_mix_config("1 1 1 3").unwrap_err().contains("inject"));
    assert!(parse_mix_config("1 x 1 1")
        .unwrap_err()
        .contains("not a number"));
}

#[tokio::test]
async fn run_screen_enters_and_launches_with_previewed_values() {
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);

    // `e` opens the config editor prefilled from the preview.
    let mut app = App::new("http://x");
    app.settings = Some(serde_json::json!({
        "start": 1, "count": 1, "analyzer": "opencode", "engine": "vieneu",
    }));
    app.screen = Screen::Run;
    handle_key(&mut app, key(KeyCode::Char('e')), &http, &job_tx).await;
    match &app.screen {
        Screen::Text(p) => assert_eq!(p.buf, "1 1 opencode"),
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

    // `Esc` just closes.
    app.screen = Screen::Run;
    handle_key(&mut app, key(KeyCode::Esc), &http, &job_tx).await;
    assert!(matches!(app.screen, Screen::Normal));
}

#[tokio::test]
async fn machine_state_falls_back_to_the_ledger_file_while_down() {
    // Nothing answers on port 9 (discard): the API post fails fast and
    // the ledger patch carries the phase instead.
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path().join(".bm")).unwrap();
    std::fs::write(
            d.path().join(".bm/ledger.json"),
            r#"{"tasks": [], "machines": [
                {"id": "a", "addr": "a", "ssh_user": "u", "ssh_port": 22, "role": "worker", "state": "unknown", "last_seen": 0, "note": ""}
            ]}"#,
        )
        .unwrap();
    set_machine_state(
        "http://127.0.0.1:9",
        d.path(),
        "a",
        MachineState::Provisioning,
        "catching up",
    )
    .await;
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(d.path().join(".bm/ledger.json")).unwrap())
            .unwrap();
    assert_eq!(doc["machines"][0]["state"], "provisioning");
    assert_eq!(doc["machines"][0]["note"], "catching up");
}

#[test]
fn machine_targets_fall_back_to_the_ledger_file() {
    // The trap: fresh TUI + dead inductor leaves app.machines empty, and
    // B used to default to local-only, silently dropping remotes. The
    // registry file (addr + ssh credentials) stands in instead.
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path().join(".bm")).unwrap();
    std::fs::write(
            d.path().join(".bm").join("ledger.json"),
            r#"{"tasks": [], "machines": [
                {"id": "192.168.2.2", "addr": "192.168.2.2", "ssh_user": "thang", "ssh_port": 22, "ssh_key": "/k", "role": "worker", "state": "unknown", "last_seen": 0, "note": ""},
                {"id": "127.0.0.1", "addr": "127.0.0.1", "ssh_user": "local", "ssh_port": 22, "role": "worker", "state": "unknown", "last_seen": 0, "note": ""}
            ]}"#,
        )
        .unwrap();
    let found = registry_machines(d.path());
    assert_eq!(found.len(), 2);
    assert_eq!(found[1].addr, "192.168.2.2");
    assert_eq!(
        found[1].ssh_key.as_deref(),
        Some("/k"),
        "credentials ride along"
    );

    let mut app = App::new("http://x");
    app.layout_root = d.path().to_path_buf();
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
        registry_machines(nowhere).is_empty(),
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

#[tokio::test]
async fn a_cold_start_names_the_fix_instead_of_reqwest_prose() {
    // The complaint this answers: starting the TUI with no inductor up
    // logged `inductor unreachable at …: error sending request for url …`
    // as an ERROR. A refused connection is the normal cold start, so the
    // poll verdict names `:B` and fits on one line.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let api = format!("http://127.0.0.1:{port}");
    let http = reqwest::Client::new();
    let err = super::jobs::fetch_state(&http, &api)
        .await
        .expect_err("nothing listens there");
    assert!(err.contains("inductor is down"), "names the state: {err}");
    assert!(err.contains(":B"), "names the fix: {err}");
    assert!(
        !err.contains("error sending request"),
        "no reqwest prose: {err}"
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

#[test]
fn crawl_template_requires_the_chapter_placeholder() {
    let mut app = App::new("http://x");
    let p = TextPrompt::new(TextKind::CrawlTemplate, "t", "h", "https://x/chuong");
    assert!(submit_text(&mut app, &p).unwrap_err().contains("{n}"));
    let p = TextPrompt::new(TextKind::CrawlTemplate, "t", "h", "https://x/chuong-{n}");
    assert!(submit_text(&mut app, &p).is_ok());
    let p = TextPrompt::new(TextKind::CrawlTemplate, "t", "h", "   ");
    assert!(submit_text(&mut app, &p).unwrap_err().contains("empty"));
}

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
            layout_root: dir,
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

    // Failure refreshes nothing — the roster it would fetch is unchanged.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Ev>();
    run_job(
        Job::AddSample {
            layout_root: std::env::temp_dir().join("bm-addsample-reload"),
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

#[test]
fn add_machine_rejects_whitespace_addresses() {
    // The bind prompt is a tuple now (`addr [user [port [key]]]`), so a
    // second token is a user, not an error — only the address itself is
    // validated.
    let mut app = App::new("http://x");
    let p = TextPrompt::new(TextKind::AddMachine, "t", "h", "192.168.2.7 extra");
    match submit_text(&mut app, &p) {
        Ok(Job::AddMachine { m, .. }) => {
            assert_eq!(m.addr, "192.168.2.7");
            assert_eq!(m.ssh_user, "extra");
        }
        other => panic!("second token is the user now, got {other:?}"),
    }
    let p = TextPrompt::new(TextKind::AddMachine, "t", "h", "  ");
    assert!(submit_text(&mut app, &p).unwrap_err().contains("empty"));
}

#[test]
fn scroll_clamping_keeps_the_cursor_visible() {
    let mut scroll = 0;
    clamp_scroll(0, &mut scroll, 100, 10);
    assert_eq!(scroll, 0);
    clamp_scroll(15, &mut scroll, 100, 10);
    assert_eq!(
        scroll, 8,
        "cursor 15 keeps two lookahead rows in a 10-row window"
    );
    clamp_scroll(2, &mut scroll, 100, 10);
    assert_eq!(scroll, 2);
    // A short list must not scroll past its end.
    let mut s2 = 5;
    clamp_scroll(0, &mut s2, 3, 10);
    assert_eq!(s2, 0);
    // Near the end the padding collapses: there is nothing below to show.
    let mut s3 = 0;
    clamp_scroll(99, &mut s3, 100, 10);
    assert_eq!(s3, 90);
}

#[test]
fn seen_label_says_never_rather_than_a_fifty_year_uptime() {
    let mut m = Machine::new("10.0.0.5", "u", 22, None, "worker");
    assert_eq!(seen_label(&m), "never");
    m.last_seen = bm_proto::now_secs().saturating_sub(5);
    assert_eq!(seen_label(&m), "5s");
    m.last_seen = bm_proto::now_secs().saturating_sub(120);
    assert_eq!(seen_label(&m), "2m");
    m.last_seen = bm_proto::now_secs().saturating_sub(7200);
    assert_eq!(seen_label(&m), "2h");
}

#[test]
fn stages_and_states_have_distinct_palettes() {
    // The old build coloured the Workers stage column with the task-state
    // palette, which no stage name matched.
    assert_eq!(stage_color("render"), Color::Cyan);
    assert_eq!(state_color("online"), Color::Green);
    assert_ne!(stage_color("render"), state_color("render"));
}

#[test]
fn users_of_lists_every_character_on_a_voice() {
    let mut cast = BTreeMap::new();
    cast.insert("Narrator".to_string(), "Đức Trí".to_string());
    cast.insert("A".to_string(), "Đức Trí".to_string());
    cast.insert("B".to_string(), "Adam".to_string());
    assert_eq!(users_of(&cast, "Đức Trí").len(), 2);
    assert_eq!(users_of(&cast, "Adam"), vec!["B".to_string()]);
    assert!(users_of(&cast, "Nobody").is_empty());
}

#[test]
fn urlencode_leaves_hostnames_alone_and_escapes_the_rest() {
    assert_eq!(urlencode("192.168.2.7"), "192.168.2.7");
    assert_eq!(urlencode("host name"), "host%20name");
}

#[test]
fn wall_clock_stamps_read_as_local_hh_mm_ss() {
    // Shape, not value: the machine's timezone is whatever it is.
    for epoch in [1u64, 1_789_485_796u64] {
        let s = wall_hms(epoch);
        assert_eq!(s.len(), 8, "{s}");
        assert_eq!(&s[2..3], ":");
        assert_eq!(&s[5..6], ":");
        assert!(
            s.chars().filter(|c| *c != ':').all(|c| c.is_ascii_digit()),
            "{s}"
        );
    }
}

#[test]
fn log_heads_alias_machines_and_workers_but_not_sentences() {
    assert_eq!(log_head("[192.168.2.2] enrolled x"), Some("192.168.2.2"));
    assert_eq!(
        log_head("localhost-4578: render done"),
        Some("localhost-4578")
    );
    assert_eq!(
        log_head("DESKTOP-V1JNVB0-18150: digest done"),
        Some("DESKTOP-V1JNVB0-18150")
    );
    assert_eq!(log_head("reconcile: nothing to fold"), None);
    assert_eq!(log_head("render:52 done"), None);
    assert_eq!(log_head("backend starting"), None);
    assert_eq!(log_head("[broken"), None);
}

#[test]
fn command_line_maps_keys_and_words() {
    assert_eq!(command_key("m"), Some(Command::Reconcile));
    assert_eq!(command_key("B"), Some(Command::Backend));
    assert_eq!(command_key("?"), Some(Command::Key(KeyCode::Char('?'))));
    assert_eq!(
        command_key("u"),
        Some(Command::Retry),
        "single chars are commands"
    );
    assert_eq!(command_key("r"), Some(Command::Key(KeyCode::Char('r'))));
    assert_eq!(command_key("reconcile"), Some(Command::Reconcile));
    assert_eq!(command_key("rerender"), Some(Command::Rerender));
    assert_eq!(command_key("remerge"), Some(Command::Remerge));
    assert_eq!(command_key("backend"), Some(Command::Backend));
    assert_eq!(command_key("stop"), Some(Command::Stop));
    assert_eq!(command_key("quit"), Some(Command::Key(KeyCode::Char('q'))));
    assert_eq!(command_key("exit"), Some(Command::Key(KeyCode::Char('q'))));
    assert_eq!(command_key("q"), Some(Command::Key(KeyCode::Char('q'))));
    assert_eq!(command_key("prov"), Some(Command::Provision { force: false }));
    assert_eq!(command_key("reprov"), Some(Command::Provision { force: true }));
    assert_eq!(command_key("remove"), Some(Command::DropMachine));
    assert_eq!(command_key("ADD"), Some(Command::AddMachine));
    assert_eq!(command_key("current"), Some(Command::AuditionCurrent));
    assert_eq!(command_key("cur"), Some(Command::AuditionCurrent));
    assert_eq!(command_key("try"), Some(Command::AuditionTry));
    assert_eq!(command_key("test"), Some(Command::AuditionTry));
    assert_eq!(command_key("another"), Some(Command::AuditionAnother));
    assert_eq!(command_key("change"), Some(Command::AuditionAnother));
    assert_eq!(command_key("next"), Some(Command::AuditionAnother));
    assert_eq!(command_key("drain"), Some(Command::ShutdownWhenIdle));
    assert_eq!(
        command_key("colour"),
        Some(Command::Key(KeyCode::Char('C')))
    );
    assert_eq!(command_key("color"), Some(Command::Key(KeyCode::Char('C'))));
    assert_eq!(command_key(":"), None, "a bare colon reopens nothing");
    assert_eq!(command_key("frobnicate"), None);
    assert_eq!(command_key(""), None);
}

#[test]
fn app_starts_on_normal_with_a_hint_not_a_blank_status() {
    let app = App::new("http://127.0.0.1:8901/");
    assert_eq!(
        app.api, "http://127.0.0.1:8901",
        "trailing slash is trimmed"
    );
    assert!(matches!(app.screen, Screen::Normal));
    assert!(!app.status.text.is_empty());
    assert_eq!(app.pending, 0);
    assert!(app.colour);
}

// --- responsive layout --------------------------------------------------

#[test]
fn size_class_picks_a_tier_per_axis() {
    assert_eq!(size_class(120, 40), Size::Full);
    assert_eq!(size_class(FULL_W, FULL_H), Size::Full);
    assert_eq!(
        size_class(80, 24),
        Size::Compact,
        "the common default terminal"
    );
    assert_eq!(
        size_class(MIN_W, MIN_H),
        Size::Compact,
        "the floor is still usable"
    );
    assert_eq!(size_class(60, 24), Size::TooSmall, "too narrow");
    assert_eq!(size_class(120, 10), Size::TooSmall, "too short");
    assert_eq!(
        size_class(0, 0),
        Size::TooSmall,
        "a degenerate area must not divide by zero"
    );
}

#[test]
fn compact_columns_fit_a_minimum_width_terminal() {
    // The same totals the compile-time guards prove; asserted here too so a
    // failure names the pane instead of just refusing to compile.
    let machines = cols(&COMPACT_MACHINE_COLS);
    let workers = cols(&COMPACT_WORKER_COLS);
    assert!(
        machines + 2 <= MIN_W,
        "machines needs {machines}+2 columns, terminal floor is {MIN_W}"
    );
    assert!(
        workers + 2 <= MIN_W,
        "workers needs {workers}+2 columns, terminal floor is {MIN_W}"
    );
}

#[test]
fn compact_layout_fits_the_hard_minimum() {
    let panes = COMPACT_MACHINES_H + COMPACT_WORKERS_H + COMPACT_EVENTS_MIN_H + COMPACT_FOOTER_H;
    assert!(
        panes <= MIN_H,
        "compact panes need {panes} rows, floor is {MIN_H}"
    );
    // The full tier must not be tighter than the compact one.
    let full = FULL_MACHINES_H + FULL_WORKERS_H + FULL_TASKS_H + FULL_EVENTS_MIN_H + FULL_FOOTER_H;
    assert!(
        full <= FULL_H,
        "full panes need {full} rows, threshold is {FULL_H}"
    );
}

#[test]
fn key_hints_fit_their_tier_without_clipping() {
    // The single 161-character line this replaced was clipped on every
    // terminal, and the lost tail held the least guessable keys.
    for k in KEYS_FULL {
        assert!(
            width_of(k) <= FULL_W as usize,
            "{k} is {} columns",
            width_of(k)
        );
    }
    for k in KEYS_COMPACT {
        assert!(
            width_of(k) <= MIN_W as usize,
            "{k} is {} columns",
            width_of(k)
        );
    }
}

#[test]
fn the_footer_advertises_the_cast_key_in_both_tiers() {
    // Regression guard: at 80 columns `S cast` fell off the clipped tail of
    // the old one-line hint, so the feature was undiscoverable exactly
    // where the terminal was most cramped.
    assert!(
        KEYS_FULL.iter().any(|k| k.contains("S cast")),
        "{KEYS_FULL:?}"
    );
    assert!(
        KEYS_COMPACT.iter().any(|k| k.contains("S cast")),
        "{KEYS_COMPACT:?}"
    );
}

#[test]
fn task_rollup_survives_a_missing_or_empty_counts_object() {
    let text = |v: &serde_json::Value| -> String {
        task_rollup(v, false)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect()
    };
    assert!(text(&serde_json::Value::Null).contains("waiting"));
    assert!(text(&serde_json::json!({})).contains("none queued"));
}

#[test]
fn task_rollup_totals_every_stage_and_flags_shelved() {
    let counts = serde_json::json!({
        "crawl":  {"done": 3, "failed": 1},
        "render": {"done": 1, "shelved": 2},
    });
    let text: String = task_rollup(&counts, false)
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert!(text.contains("4/7 done"), "{text}");
    assert!(text.contains("1 open"), "{text}");
    assert!(text.contains("1 failed"), "{text}");
    assert!(text.contains("2 shelved"), "{text}");
}

#[test]
fn task_rollup_hides_zero_failure_and_shelved_counters() {
    let counts = serde_json::json!({"crawl": {"done": 2, "failed": 0, "shelved": 0}});
    let text: String = task_rollup(&counts, false)
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert!(text.contains("2/2 done"), "{text}");
    assert!(!text.contains("failed"), "a zero counter is noise: {text}");
    assert!(!text.contains("shelved"), "a zero counter is noise: {text}");
}

// --- cast overview ------------------------------------------------------

fn roster_fixture() -> Roster {
    let voice = |name: &str, gender: &str, accent: &str, allowed: bool, enrolled: bool| {
        VoiceInfo {
            // Keys come from the real catalogue, so the fixture cannot drift
            // from what the picker actually receives — and a name the
            // catalogue does not declare keeps an empty key, as a clone does.
            key: bm_core::voices::key_for_name("vieneu", name).unwrap_or_default(),
            name: name.to_string(),
            gender: gender.to_string(),
            accent: accent.to_string(),
            language: "vi-VN".to_string(),
            style: "tin tức".to_string(),
            enrolled,
            allowed,
        }
    };
    Roster {
        engine: "vieneu".into(),
        source: "live".into(),
        voices: vec![
            voice("Đức Trí", "male", "South", true, false),
            voice("Adam", "male", "unknown", false, true),
            voice("Bắc Kỳ", "male", "Northern", false, false),
        ],
        cast: BTreeMap::from([
            ("Narrator".to_string(), "Đức Trí".to_string()),
            ("Kiên".to_string(), "Adam".to_string()),
            ("Vũ".to_string(), "Adam".to_string()),
            ("Lâm".to_string(), "Bắc Kỳ".to_string()),
            ("Hà".to_string(), "Đã Biến Mất".to_string()),
        ]),
        characters: vec![
            "Narrator".into(),
            "Kiên".into(),
            "Vũ".into(),
            "Lâm".into(),
            "Hà".into(),
            "Mới".into(),
        ],
        policy_note: "Central/South only".into(),
    }
}

#[test]
fn cast_rows_put_narrator_first_and_keep_unassigned_speakers() {
    let rows = cast_rows(&roster_fixture());
    assert_eq!(
        rows[0].character, "Narrator",
        "Narrator is the fallback voice"
    );
    assert_eq!(rows.len(), 6, "every speaker appears exactly once");
    let moi = rows.iter().find(|r| r.character == "Mới").unwrap();
    assert!(moi.unassigned());
    assert_eq!(moi.verdict(), Verdict::Unassigned);
}

#[test]
fn cast_rows_flag_shared_voices_from_both_sides() {
    let rows = cast_rows(&roster_fixture());
    let by = |n: &str| rows.iter().find(|r| r.character == n).unwrap().clone();
    assert_eq!(by("Kiên").shared_with, vec!["Vũ".to_string()]);
    assert_eq!(by("Vũ").shared_with, vec!["Kiên".to_string()]);
    assert!(by("Kiên").shared());
    assert!(
        !by("Narrator").shared(),
        "a sole user of a voice is not flagged"
    );
}

#[test]
fn cast_rows_separate_blocked_from_unknown_and_accept_enrolled_clones() {
    let rows = cast_rows(&roster_fixture());
    let by = |n: &str| rows.iter().find(|r| r.character == n).unwrap().verdict();
    assert_eq!(
        by("Lâm"),
        Verdict::Blocked,
        "listed, and the policy rejects it"
    );
    assert_eq!(
        by("Hà"),
        Verdict::Unknown,
        "the roster has never heard of it"
    );
    assert_eq!(by("Kiên"), Verdict::Ok, "enrolled clones bypass the policy");
    assert_eq!(by("Narrator"), Verdict::Ok);
}

#[test]
fn unassigned_speakers_do_not_count_as_sharing_the_empty_voice() {
    let mut r = roster_fixture();
    r.cast.retain(|k, _| k == "Kiên");
    let rows = cast_rows(&r);
    let unassigned: Vec<&CastRow> = rows.iter().filter(|x| x.unassigned()).collect();
    assert!(
        unassigned.len() > 1,
        "the fixture must have several unassigned speakers"
    );
    assert!(unassigned.iter().all(|x| x.shared_with.is_empty()));
}

#[test]
fn cast_rows_filter_by_speaker_voice_or_style_ignoring_diacritics() {
    let rows = cast_rows(&roster_fixture());
    assert_eq!(
        filtered_cast_rows(&rows, "duc tri").len(),
        1,
        "matches the voice"
    );
    assert_eq!(
        filtered_cast_rows(&rows, "adam").len(),
        2,
        "both speakers on Adam"
    );
    assert_eq!(
        filtered_cast_rows(&rows, "kien").len(),
        1,
        "matches the speaker"
    );
    assert_eq!(
        filtered_cast_rows(&rows, "tin tuc").len(),
        4,
        "the style is searchable too, and without diacritics"
    );
    assert_eq!(
        filtered_cast_rows(&rows, "   ").len(),
        rows.len(),
        "a blank filter keeps all"
    );
    assert!(filtered_cast_rows(&rows, "nobody").is_empty());
}

#[test]
fn cast_filter_preserves_row_order_and_never_invents_rows() {
    let rows = cast_rows(&roster_fixture());
    let filtered = filtered_cast_rows(&rows, "adam");
    let order: Vec<&String> = filtered.iter().map(|r| &r.character).collect();
    assert_eq!(order, vec!["Kiên", "Vũ"]);
}

#[test]
fn cast_rows_carry_the_voice_metadata_through() {
    let rows = cast_rows(&roster_fixture());
    let narrator = rows.iter().find(|r| r.character == "Narrator").unwrap();
    assert_eq!(narrator.gender, "male");
    assert_eq!(narrator.accent, "South");
    assert!(narrator.in_roster);
    assert!(narrator.allowed && !narrator.enrolled);
    // A voice the roster does not list carries no metadata at all, so the
    // table renders dashes rather than a fabricated accent.
    let ha = rows.iter().find(|r| r.character == "Hà").unwrap();
    assert!(!ha.in_roster);
    assert!(ha.accent.is_empty() && ha.gender.is_empty());
}

// --- rendering ----------------------------------------------------------

/// Render one frame into an in-memory terminal and flatten it to text.
///
/// The responsive tiers are pure layout, so they can be checked without a
/// real terminal — which is also the only way to prove the size guard does
/// not panic on a degenerate area.
fn render_text(app: &mut App, w: u16, h: u16) -> String {
    let backend = ratatui::backend::TestBackend::new(w, h);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|f| draw(f, app)).unwrap();
    let buf = terminal.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

#[test]
fn the_log_pane_has_one_title_aliases_and_local_time() {
    let mut app = App::new("http://127.0.0.1:8901");
    app.log_at(Level::Ok, "[192.168.2.2] enrolled x");
    app.log_at(Level::Error, "localhost-99: render failed: boom");
    app.log_at(Level::Info, "reconcile: nothing certain to fold");
    let text = render_text(&mut app, 140, 44);
    assert!(text.contains("Logs"), "pane renamed:\n{text}");
    assert!(!text.contains("Events"), "no stale title anywhere:\n{text}");
    // Address heads are box-level lines: shown as written, never hashed
    // into a phantom worker name.
    assert!(
        text.contains("[192.168.2.2]"),
        "machine line verbatim:\n{text}"
    );
    assert!(
        text.contains("[localhost-99]"),
        "unknown worker id verbatim:\n{text}"
    );
    assert!(
        text.contains("reconcile: nothing certain"),
        "plain lines pass through:\n{text}"
    );
}

#[test]
fn log_lines_use_reported_aliases_when_beats_carry_them() {
    // The mismatch: the Workers pane said `marmot` while the log line
    // said `[hare] [thang-29486]` — the log hashed the raw id instead of
    // asking the beats.
    let mut app = App::new("http://127.0.0.1:8901");
    app.beats = vec![beat("thang-29486", "192.168.2.2", 2, "marmot")];
    app.log_at(Level::Ok, "[thang-29486] render:23 done in 255.3s");
    let text = render_text(&mut app, 140, 44);
    assert!(
        text.contains("[marmot]"),
        "the reported alias wins:\n{text}"
    );
    // ...but an id no beat knows stays itself. Hashing it once minted
    // `[hawk]` for the address `192.168.2.2` — a worker that never
    // existed, hunted across every pane.
    app.log_at(Level::Ok, "[ghost-1] render:24 done");
    let text = render_text(&mut app, 140, 44);
    assert!(
        text.contains("[ghost-1]"),
        "unknown ids stay verbatim:\n{text}"
    );
}

#[test]
fn an_address_head_never_becomes_a_phantom_worker() {
    // The exact confusion: provision lines are tagged with the box
    // address while its worker beats as `thang-marmot`. The log must
    // show the address, not hash it into a third name.
    let mut app = App::new("http://127.0.0.1:8901");
    app.beats = vec![beat("thang-marmot", "192.168.2.2", 2, "marmot")];
    app.log_at(Level::Ok, "[192.168.2.2] already configured (agent 0.2.3 + tts sidecar)");
    let text = render_text(&mut app, 140, 44);
    assert!(
        text.contains("[192.168.2.2]"),
        "the address stays an address:\n{text}"
    );
    assert!(
        !text.contains("[hawk]"),
        "no phantom worker is minted:\n{text}"
    );
}

#[test]
fn the_size_guard_replaces_the_dashboard_below_the_floor() {
    let mut app = App::new("http://127.0.0.1:8901");
    let text = render_text(&mut app, 60, 16);
    assert!(text.contains("too small"), "{text}");
    assert!(
        !text.contains("Machines"),
        "no clipped panes behind the notice:\n{text}"
    );
    assert!(
        text.contains("60×16"),
        "the notice names the actual size:\n{text}"
    );
    assert!(text.contains("76×20"), "and the requirement:\n{text}");
}

#[test]
fn the_size_guard_does_not_panic_on_a_degenerate_area() {
    let mut app = App::new("http://127.0.0.1:8901");
    // Only the first has room for the full notice; the slivers must simply
    // not panic, and must never leak a clipped dashboard.
    for (w, h) in [(60u16, 16u16), (1, 1), (0, 0), (200, 3), (3, 200)] {
        let text = render_text(&mut app, w, h);
        assert!(
            !text.contains("Machines"),
            "{w}x{h} rendered panes:\n{text}"
        );
        assert!(!text.contains("Workers"), "{w}x{h} rendered panes:\n{text}");
    }
}

#[test]
fn the_compact_tier_folds_the_tasks_pane_into_the_footer() {
    let mut app = App::new("http://127.0.0.1:8901");
    let text = render_text(&mut app, 80, 24);
    assert!(text.contains("Machines"), "{text}");
    assert!(text.contains("Workers"), "{text}");
    assert!(text.contains("Logs"), "Logs keeps its pane:\n{text}");
    assert!(
        !text.contains("┌Tasks"),
        "the Tasks pane is collapsed:\n{text}"
    );
    assert!(
        text.contains("tasks:"),
        "its roll-up takes its place:\n{text}"
    );
}

#[test]
fn the_full_tier_shows_every_pane_and_the_new_key() {
    let mut app = App::new("http://127.0.0.1:8901");
    let text = render_text(&mut app, 140, 44);
    for pane in ["Machines", "Workers", "Tasks", "Logs"] {
        assert!(text.contains(pane), "{pane} is missing:\n{text}");
    }
    assert!(
        text.contains("S cast"),
        "the cast key is advertised:\n{text}"
    );
}

#[test]
fn an_empty_cluster_says_what_to_do_in_every_tier() {
    let mut app = App::new("http://127.0.0.1:8901");
    for (w, h) in [(80u16, 24u16), (140, 44)] {
        let text = render_text(&mut app, w, h);
        assert!(
            text.contains("no machines in the cluster"),
            "{w}x{h}:\n{text}"
        );
        assert!(text.contains("no workers connected"), "{w}x{h}:\n{text}");
        assert!(
            text.contains("nothing has happened yet"),
            "{w}x{h}:\n{text}"
        );
    }
}

/// A beat with the fields the panes read, fresh unless told otherwise.
/// Hostname never echoes the addr: the workers pane falls back to addr
/// when it is empty, which would muddy addr-counting assertions.
fn beat(id: &str, addr: &str, age_secs: u64, alias: &str) -> Heartbeat {
    Heartbeat {
        worker_id: id.into(),
        addr: addr.into(),
        task_id: None,
        stage: None,
        chapter: None,
        progress: 0.0,
        activity: "idle".into(),
        eta_secs: None,
        ts: bm_proto::now_secs().saturating_sub(age_secs),
        hostname: format!("host-{id}"),
        alias: alias.into(),
        cpu_pct: None,
        mem_pct: None,
        mem_gb: None,
    }
}

#[test]
fn workers_pane_hides_stale_beats_and_shows_reported_aliases() {
    // The "two hares": a dead worker rendered as an idle row next to the
    // live one. Only fresh beats may draw; the name shown is the worker's
    // own (kept across restarts), with the id hash as fallback for older
    // agents that report none.
    let mut app = App::new("http://127.0.0.1:8901");
    app.beats = vec![
        beat("thang-1", "192.168.2.2", 2, "quokka"),
        beat("thang-0", "192.168.2.2", 900, "quokka"),
        beat("localhost-9", "127.0.0.1", 3, ""),
    ];
    let text = render_text(&mut app, 140, 44);
    assert_eq!(
        text.matches("quokka").count(),
        2,
        "live once per pane (Workers, Stats), stale never:\n{text}"
    );
    let (fallback, _) = worker_alias("localhost-9");
    assert!(
        text.contains(fallback),
        "old agents keep the hash name:\n{text}"
    );
    assert!(
        !text.contains("no live workers"),
        "a live row draws:\n{text}"
    );
}

#[test]
fn workers_pane_says_so_when_every_beat_is_stale() {
    let mut app = App::new("http://127.0.0.1:8901");
    app.beats = vec![beat("thang-0", "192.168.2.2", 900, "quokka")];
    let text = render_text(&mut app, 140, 44);
    assert!(
        text.contains("no live workers"),
        "stale is not idle:\n{text}"
    );
    assert!(!text.contains("quokka"), "stale rows never draw:\n{text}");
}

fn named_machine(addr: &str, name: &str) -> Machine {
    let mut m = Machine::new(addr, "thang", 22, None, "worker");
    m.name = name.into();
    m
}

#[test]
fn machine_name_prefers_the_registry_handle() {
    use super::model::machine_name;
    let machines = vec![named_machine("192.168.2.2", "hawk")];
    // Known box: the handle the provision log used, not the OS hostname.
    let b = beat("thang-marmot", "192.168.2.2", 2, "marmot");
    assert_eq!(machine_name(&machines, &b), "hawk");
    // Unnamed box: the reported hostname, then the address.
    let machines = vec![Machine::new("192.168.2.2", "thang", 22, None, "worker")];
    assert_eq!(machine_name(&machines, &b), "host-thang-marmot");
    let mut nohost = b.clone();
    nohost.hostname.clear();
    assert_eq!(machine_name(&machines, &nohost), "192.168.2.2");
    // Unknown box entirely: same fallbacks, never empty.
    assert_eq!(machine_name(&[], &b), "host-thang-marmot");
    assert_eq!(machine_name(&[], &nohost), "192.168.2.2");
}

#[test]
fn both_panes_call_a_known_box_by_its_handle() {
    // The hawk hunt: provision says one name, panes must agree with it.
    let mut app = App::new("http://127.0.0.1:8901");
    app.machines = vec![named_machine("192.168.2.2", "hawk")];
    app.beats = vec![beat("thang-marmot", "192.168.2.2", 2, "marmot")];
    let text = render_text(&mut app, 140, 44);
    assert!(text.contains("marmot"), "the worker keeps its alias:\n{text}");
    assert!(text.contains("hawk"), "both panes use the handle:\n{text}");
    assert!(
        !text.contains("host-thang-marmot"),
        "the OS hostname steps aside where a handle exists:\n{text}"
    );
}

fn stats_app() -> App {
    // One worker mid-render (half done), one idle; history says a render
    // task takes 100s, a digest 40s.
    let mut app = App::new("http://127.0.0.1:8901");
    app.machines = vec![named_machine("192.168.2.2", "hawk")];
    let mut busy = beat("thang-marmot", "192.168.2.2", 2, "marmot");
    busy.stage = Some(Stage::Render);
    busy.chapter = Some(7);
    busy.progress = 0.5;
    busy.cpu_pct = Some(25.0);
    busy.mem_pct = Some(40.0);
    busy.mem_gb = Some(4.5);
    let idle = beat("localhost-caracal", "127.0.0.1", 2, "caracal");
    app.beats = vec![busy, idle];
    app.stats = super::model::parse_stats(Some(&serde_json::json!({
        "counts": {"thang-marmot": {"render": 3, "digest": 1}},
        "avg_task_secs": {"render": 100.0, "digest": 40.0},
    })));
    app
}

#[test]
fn stats_pane_counts_completions_and_estimates_the_remainder() {
    let mut app = stats_app();
    let text = render_text(&mut app, 140, 44);
    assert!(text.contains("Stats"), "the panel exists:\n{text}");
    assert!(text.contains("marmot"), "rows are workers:\n{text}");
    // Half of a 100s render remains: the TUI-side ETA, not a report.
    assert!(text.contains("50s"), "remainder from history × progress:\n{text}");
    // Idle with no active stage estimates nothing; unknown load dashes.
    assert!(text.contains("caracal"), "idle workers list too:\n{text}");
    assert!(text.contains("—"), "dashes where nothing is known:\n{text}");
    // The 100-column floor still fits the split row, not just wide terms.
    let narrow = render_text(&mut app, 100, 32);
    assert!(narrow.contains("Stats"), "panel survives the floor:\n{narrow}");
    assert!(narrow.contains("50s"), "eta survives the floor:\n{narrow}");
}

#[test]
fn workers_pane_shows_load_where_measured() {
    let mut app = stats_app();
    let text = render_text(&mut app, 140, 44);
    assert!(text.contains("25.0%"), "cpu pct:\n{text}");
    assert!(text.contains("40% 4.5G"), "mem pct + gib:\n{text}");
}

#[test]
fn task_eta_scales_history_by_the_unworked_fraction() {
    use super::model::task_eta;
    assert_eq!(task_eta(Some(100.0), 0.5), Some(50));
    assert_eq!(task_eta(Some(100.0), 0.0), Some(100));
    assert_eq!(task_eta(Some(100.0), 1.0), Some(0));
    assert_eq!(task_eta(None, 0.5), None, "no history, no number");
    assert_eq!(task_eta(Some(0.0), 0.5), None, "no zero-duration average");
    assert_eq!(task_eta(Some(100.0), 9.9), Some(0), "clamped progress");
}

#[test]
fn machines_pane_counts_live_workers_instead_of_repeating_the_addr() {
    // `id` was the addr by construction, so the column only echoed its
    // neighbour. The useful number is how many live workers each box has.
    let mut app = App::new("http://127.0.0.1:8901");
    app.machines = vec![
        Machine::new("127.0.0.1", "local", 22, None, "worker"),
        Machine::new("192.168.2.2", "thang", 22, None, "worker"),
    ];
    app.beats = vec![
        beat("localhost-9", "127.0.0.1", 2, "quokka"),
        beat("thang-1", "192.168.2.2", 3, "wombat"),
        beat("thang-0", "192.168.2.2", 900, "wombat"),
    ];
    let text = render_text(&mut app, 140, 44);
    assert!(
        text.contains("workers"),
        "the count column is headed:\n{text}"
    );
    assert_eq!(
        text.matches("192.168.2.2").count(),
        1,
        "the addr appears once, never echoed:\n{text}"
    );
    let now = bm_proto::now_secs();
    assert_eq!(
        live_workers(&app.beats, "192.168.2.2", now),
        1,
        "stale excluded"
    );
    assert_eq!(live_workers(&app.beats, "127.0.0.1", now), 1);
    assert_eq!(live_workers(&app.beats, "10.0.0.9", now), 0, "unknown box");
}

#[test]
fn the_cast_overview_renders_every_speaker_and_flags_shared_voices() {
    let mut app = App::new("http://127.0.0.1:8901");
    app.roster = Some(roster_fixture());
    app.screen = Screen::Cast(CastView::new());
    let text = render_text(&mut app, 140, 44);
    for speaker in ["Narrator", "Kiên", "Vũ", "Lâm", "Hà", "Mới"] {
        assert!(text.contains(speaker), "{speaker} is missing:\n{text}");
    }
    assert!(
        text.contains("6 speakers"),
        "the summary counts them:\n{text}"
    );
    assert!(text.contains("4 voices in use"), "{text}");
    assert!(text.contains("1 shared"), "only Adam is shared:\n{text}");
    assert!(text.contains("2 to fix"), "Lâm and Hà:\n{text}");
    assert!(text.contains("1 unassigned"), "Mới:\n{text}");
    assert!(text.contains("shared with 1 other"), "{text}");
    assert!(
        text.contains("accent policy concern"),
        "Lâm is flagged:\n{text}"
    );
    assert!(
        text.contains("unknown voice — stale cast?"),
        "Hà is flagged:\n{text}"
    );
    assert!(
        text.contains("unassigned — v fills gaps"),
        "Mới is flagged:\n{text}"
    );
}

#[test]
fn the_cast_overview_without_a_roster_offers_the_retry_key() {
    let mut app = App::new("http://127.0.0.1:8901");
    app.screen = Screen::Cast(CastView::new());
    let text = render_text(&mut app, 120, 32);
    assert!(text.contains("roster not loaded — press R"), "{text}");
}

/// A small ledger: a shelved digest carrying a real failure reason, a
/// render mid-flight, and a finished crawl. Sorted as a snapshot would be.
fn tasks_app() -> App {
    let mut app = App::new("http://127.0.0.1:8901");
    let mut shelved = Task::new(3, Stage::Digest);
    shelved.state = TaskState::Shelved;
    shelved.attempts = 3;
    shelved.assigned_to = Some("w2".into());
    shelved.detail =
        "opencode exited 1: model 'claude' unavailable\nsecond line of the report".into();
    let mut running = Task::new(3, Stage::Render);
    running.state = TaskState::Running;
    running.assigned_to = Some("w1".into());
    running.lease_until = Some(bm_proto::now_secs() + 120);
    running.detail = "rendering segment 12/40".into();
    let mut done = Task::new(4, Stage::Crawl);
    done.state = TaskState::Done;
    done.detail = "ok".into();
    app.tasks = vec![shelved, running, done];
    app.tasks.sort_by_key(|t| (t.chapter, t.stage));
    app
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

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
    assert_eq!(filtered_tasks(all, "").len(), 3, "no filter, everything");
    assert_eq!(
        filtered_tasks(all, "   ").len(),
        3,
        "whitespace is not a filter"
    );
    assert_eq!(filtered_tasks(all, "shelved").len(), 1);
    assert_eq!(
        filtered_tasks(all, "  SHELVED ").len(),
        1,
        "case and space insensitive"
    );
    assert_eq!(filtered_tasks(all, "render")[0].chapter, 3);
    assert_eq!(filtered_tasks(all, "digest:3").len(), 1);
    assert_eq!(
        filtered_tasks(all, "4").len(),
        1,
        "a chapter number matches"
    );
    assert!(filtered_tasks(all, "merge").is_empty());
    assert!(
        filtered_tasks(all, "shel").len() == 1,
        "a partial state name still matches"
    );
}

#[test]
fn an_empty_ledger_says_what_to_do_instead_of_drawing_nothing() {
    let mut app = App::new("http://127.0.0.1:8901");
    app.screen = Screen::Tasks(TasksView::new());
    let text = render_text(&mut app, 140, 44);
    assert!(text.contains("no tasks in the ledger yet"), "{text}");
    assert!(text.contains("press t to enqueue"), "{text}");
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

#[test]
fn the_footer_names_the_ledger_when_work_is_shelved() {
    let mut app = tasks_app();
    // Wide enough that the footer is not clipped: the point is that the
    // count and the key are *there*, not that they survive an 80-column
    // terminal (the compact tier keeps them, minus the em-dash detail).
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
    assert!(text.contains("3 of 3 before it is shelved"), "{text}");
    assert!(
        text.contains(worker_alias("w2").0),
        "the worker that failed:\n{text}"
    );
    assert!(text.contains("lease"), "the lease is on the page:\n{text}");

    // Esc returns to the list — and to the same view of it.
    handle_key(&mut app, key(KeyCode::Esc), &http, &job_tx).await;
    assert!(matches!(app.screen, Screen::Tasks(_)), "{:?}", app.screen);
}

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

// --- auditioning --------------------------------------------------------

/// Long enough to clear `MIN_LINE_CHARS`, so the chooser prefers it over
/// anything shorter a fixture might also offer.
fn audition_line(tag: &str) -> String {
    format!("{tag} — một câu đủ dài để làm mẫu thử giọng đọc cho nhân vật này nhé")
}

/// A picker at step 2 for Narrator (cast to Đức Trí), filtered to a single
/// candidate so "the highlighted voice" means one thing.
///
/// The filter is load-bearing: `filtered_voices` returns roster order, not
/// relevance order, so an empty filter would highlight whoever happens to be
/// first in the catalogue rather than the voice the test names.
///
/// The index is pre-set rather than loaded, because `ensure_lines` is what the
/// screens call and a test should not need a `data/` directory.
fn audition_app() -> App {
    let mut app = App::new("http://127.0.0.1:8901");
    app.conn = Conn::Up;
    app.roster = Some(roster_fixture());
    let mut p = Picker::new();
    p.stage = PickStage::Voice;
    p.character = "Narrator".into();
    p.filter = "adam".into();
    app.screen = Screen::Pick(p);
    app.lines = Some(std::collections::HashMap::from([(
        "Narrator".to_string(),
        vec![audition_line("một"), audition_line("hai")],
    )]));
    app
}

/// Pull the `OpRequest` a keypress dispatched, if it dispatched one.
fn last_op(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Job>) -> Option<OpRequest> {
    match rx.try_recv().ok()?.into_bare() {
        Job::Op { req, .. } => Some(req),
        other => panic!("expected an Op job, got {other:?}"),
    }
}

/// Release the in-flight audition slot the way a completed op would.
///
/// Deliberately carries no audio: a helper that shipped a wav would start a
/// real player in every test that calls it, and `cargo test` must not make
/// noise. The path where audio *does* arrive is covered by
/// `a_completed_audition_writes_the_sample_next_to_the_speaker`, which
/// installs a silent player first.
fn finish_audition(app: &mut App, voice: &str) {
    app.apply(Ev::Done(DoneKind::Op {
        op: Op::PreviewVoice,
        key: op_key(&OpRequest {
            op: Op::PreviewVoice,
            ..Default::default()
        }),
        ok: true,
        voice: Some(voice.to_string()),
        audio_b64: None,
        line_speaker: None,
        line_text: None,
    }));
}

#[tokio::test]
async fn current_word_tests_the_current_voice_on_the_shown_line() {
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = audition_app();

    // `:current`: the current voice on the shown line, from cache only. The
    // cursor sits on Adam, but it never follows it — that is what `:try`
    // (render) and Enter (pick) are for.
    do_command(&mut app, Command::AuditionCurrent, &http, &job_tx);
    let req = last_op(&mut job_rx).expect(":current must dispatch a segment fetch");
    assert_eq!(req.op, Op::Segment);
    assert_eq!(
        req.voice.as_deref(),
        Some("Đức Trí"),
        "the current voice, not the pointed one"
    );
    assert_eq!(req.character.as_deref(), Some("Narrator"));
    let text = req.text.clone().expect(":current always names the shown line");
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

    // `:another`: the pointed voice again, re-rolled — a render, never a
    // cache-only segment fetch.
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
    // The synthesis itself is not run here (model weights, minutes) — the
    // dispatch shape is the contract, and the job reports honestly alone.
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = audition_app();
    app.conn = Conn::Unknown;
    app.layout_root = std::path::PathBuf::from("/tmp/bm-offline-audition");

    do_command(&mut app, Command::AuditionTry, &http, &job_tx);
    match job_rx.try_recv().expect(":try dispatches offline").into_bare() {
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
    // the filter alone. (`^T` auditions and `^R` focuses — both dispatch
    // or mark, so they are covered by the focus tests, not here.)
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
    // a refused dispatch sends no `Done` — so the screen sat behind a render
    // that never started, and every later key was refused for the same reason.
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
    // Bare letters outside t/T focus the filter and type — o and n stand
    // in for all of them here (t/T audition in audition focus).
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
    // types everything too, now that auditioning is `:words`).
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
    // focuses the filter and types.
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
    // Focused, `t` types like every other letter — no audition.
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
    // exists here, so the word plays what the speaker already has.
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

fn cast_app() -> App {
    let mut app = App::new("http://127.0.0.1:8901");
    app.conn = Conn::Up;
    app.roster = Some(roster_fixture());
    app.lines = Some(std::collections::HashMap::from([(
        "Narrator".to_string(),
        vec![audition_line("n")],
    )]));
    app.screen = Screen::Cast(CastView::new());
    app
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
    // shows it with an empty voice. Filtering to it is deterministic; walking
    // to the last row is not, because the table's order is not the cast's.
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
        "no data/script-*.json under /r — run :translate first".into(),
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
    // *dispatched*, and it used to be replaced only when the op returned
    // *and* had audio. Against an inductor older than the TUI — which has no
    // audio field at all — the bar kept claiming a render was in flight
    // after it had finished, and nothing else ever clears that line.
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
    // stop saying a render is in flight.
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
    // machine with the speaker, so the inductor's disk stays untouched. The
    // stand-in player is `true` — it spawns for real and makes no sound.
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
    // `Ev::Done`, so `App::pending` was +1 from the first screen that
    // auditions until the process exited — a footer reading "1 job(s)
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

fn bind_app() -> App {
    let mut app = App::new("http://x");
    app.settings = Some(serde_json::json!({"ssh": {"user": "op", "port": 2222}}));
    app
}

fn bind_machine(app: &mut App, buf: &str) -> Machine {
    let p = TextPrompt::new(TextKind::AddMachine, "t", "h", buf);
    match submit_text(app, &p) {
        Ok(Job::AddMachine { m, .. }) => m,
        other => panic!("bind {buf:?} must dispatch AddMachine, got {other:?}"),
    }
}

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
    // instead of dispatching a box that can never provision.
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
    app.layout_root = dir.clone();
    let settings_path = dir.join(".bm").join("settings.json");
    let load = || bm_core::config::Settings::load(&settings_path);

    let key = dir.join("id_def");
    std::fs::write(&key, "k").unwrap();
    let msg = save_ssh_setting(&app, TextKind::SshKey, key.to_str().unwrap()).unwrap();
    assert!(msg.contains("saved"), "{msg}");
    assert_eq!(load().ssh.key.as_deref(), Some(key.to_str().unwrap()));
    // Clearing is a real answer: ssh decides per machine afterwards.
    save_ssh_setting(&app, TextKind::SshKey, "  ").unwrap();
    assert_eq!(load().ssh.key, None);
    // A missing file keeps the prompt open, it never saves garbage.
    let err = save_ssh_setting(&app, TextKind::SshKey, "/nonexistent/k").unwrap_err();
    assert!(err.contains("/nonexistent/k"), "{err}");
    assert_eq!(load().ssh.key, None);

    save_ssh_setting(&app, TextKind::SshUser, "worker").unwrap();
    assert_eq!(load().ssh.user, "worker");
    assert!(save_ssh_setting(&app, TextKind::SshUser, "  ")
        .unwrap_err()
        .contains("empty"));

    assert!(save_ssh_setting(&app, TextKind::SshPort, "abc")
        .unwrap_err()
        .contains("not a number"));
    save_ssh_setting(&app, TextKind::SshPort, "2222").unwrap();
    assert_eq!(load().ssh.port, 2222);
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
    // screen that can dispatch work.
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
async fn quit_word_quits_from_the_picker_command_line() {
    // The filter owns every letter on picker/cast, so a bare `q` types —
    // but `:quit` must still quit from there, not type another letter.
    let http = reqwest::Client::new();
    let (job_tx, _job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = audition_app();
    app.pending = 0;
    let pick = app.screen.clone();
    app.command_return = Some(pick);
    app.screen = Screen::Text(TextPrompt::new(
        TextKind::Command,
        ":",
        "",
        "quit",
    ));
    let quit = handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    assert!(quit, ":quit from the picker must quit");
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

// ---------------------------------------------------------------------------
// the sound-design editor
// ---------------------------------------------------------------------------

/// A checkout holding the fixture scene map and registries, with every clip
/// they name present as an empty file.
///
/// The fixture mirrors production shapes, so the editor guards behave as they
/// do live; the live tree itself is ignored and may be absent. The clips are
/// placeholders — nothing in the editor reads their contents, only whether
/// they are there.
fn sound_layout(tag: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let layout = bm_core::Layout::new(dir.path());
    bm_core::profile::install_fixture(dir.path()).expect("fixture profile");
    for kind in bm_core::audio_pool::PoolKind::ALL {
        let pool = bm_core::audio_pool::load_pool(&layout.pool(kind));
        assert!(!pool.is_empty(), "{} empty", kind.registry());
        for sound in pool.values() {
            for f in &sound.files {
                let p = layout.assets().join(f);
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(&p, b"").unwrap();
            }
        }
    }
    let _ = tag;
    (dir, layout.root.clone())
}

/// An app parked on the sound editor with the pools loaded, as the screen
/// finds them.
fn sound_app(root: &std::path::Path) -> App {
    let mut app = App::new("http://unused");
    app.layout_root = root.to_path_buf();
    app.sound = Some(sound::load(root).expect("the fixture loads"));
    app.screen = Screen::Sound(SoundView::new());
    app
}

#[test]
fn the_sound_editor_marks_remove_unavailable_where_it_is() {
    let (_d, root) = sound_layout("render");
    let mut app = sound_app(&root);
    let text = render_text(&mut app, 120, 40);
    // The three tabs, with their sizes.
    assert!(text.contains("effects 11"), "tabs missing:\n{text}");
    assert!(text.contains("music 9"), "{text}");
    assert!(text.contains("injects 39"), "{text}");
    // Every shipped effect sound answers a shipped rule, so the first row is
    // in use and the remove key is drawn as unavailable, with the reason.
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
/// has to survive every tier — including the 76-column one where the overlay
/// has no margin to give. The compile-time guard in `layout.rs` predicts this;
/// this is the render that proves it.
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
    // room for it — at the minimum width the counts win.
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
    let text = render_text(&mut app, 120, 40);
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
    // And it is a `:` command only — no single key reaches it.
    let text = render_text(&mut app, 120, 40);
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
    // removes exactly that one.
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
/// file can change. Re-checking on Enter is what keeps the guard from being a
/// keystroke's opinion rather than a fact about the mix.
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
    std::fs::create_dir_all(root.join("data")).unwrap();
    std::fs::write(
        root.join("data/script-04.json"),
        format!(r#"{{"segments":[{{"sound":"{name}"}}]}}"#),
    )
    .unwrap();
    app.sound = Some(sound::load(&root).unwrap());

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
    // reflow the prose or the entries that were already there.
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
    let (job_tx, _job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
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
    let after = sound::rows(
        app.sound.as_ref().unwrap(),
        bm_core::audio_pool::PoolKind::Inject,
    );
    let after = after.iter().find(|r| r.name == "cooking").unwrap();
    assert_eq!(after.sound.level, Some(0.35));
    // Every other field is where it was — a retune is not a rewrite.
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
    let text = render_text(&mut app, 120, 40);
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
/// the paths that read the local cache instead of the API.
/// An entry whose clip has gone missing must stay editable: it is already
/// flagged in red, and refusing a tag change because somebody moved a file is
/// how an entry becomes unfixable from the screen. A *new* take still has to
/// exist — that is the typo the check is for.
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

/// A checkout with one chapter's script and one rendered segment in it, for
/// the paths that read the local cache instead of the API.
fn local_cache_layout() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let layout = bm_core::Layout::new(dir.path());
    std::fs::create_dir_all(layout.data()).unwrap();
    std::fs::write(
        layout.script(1),
        serde_json::json!({"segments": [
            {"speaker": "Narrator", "text": "Nar nói."},
        ]})
        .to_string(),
    )
    .unwrap();
    let seg = layout.seg_dir("vieneu", 1);
    std::fs::create_dir_all(&seg).unwrap();
    std::fs::write(seg.join("0000_Đức Trí.wav"), b"RIFF-fake-local").unwrap();
    (dir, layout.root.clone())
}

#[tokio::test]
async fn an_unrendered_held_line_falls_back_to_one_of_hers() {
    // Fresh swap, rendered chapter by chapter: the held line misses in her
    // voice, but her voice exists in the cache — play one of hers, still
    // zero synthesis, and hold it so T compares on the same sentence.
    let (_dir, root) = local_cache_layout();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Ev>();
    job_segment(
        tx,
        root,
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
    // instead of the API. Listening needs no :B.
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let (_dir, root) = local_cache_layout();
    let mut app = audition_app();
    app.conn = Conn::Down("inductor down".into());
    app.layout_root = root.clone();
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
    // playback cannot tell the paths apart.
    let (tx2, mut rx2) = tokio::sync::mpsc::unbounded_channel::<Ev>();
    job_segment(
        tx2,
        root,
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
    app.layout_root = std::path::PathBuf::new();
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
    // silently select nothing.
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = audition_app();
    handle_key(&mut app, key(KeyCode::Down), &http, &job_tx).await;
    match &app.screen {
        Screen::Pick(p) => assert_eq!(p.cursor, 0, "one row in the filter, nowhere to go"),
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
