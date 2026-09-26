//! Key and render tests, moved as one file.
use super::app::App;
use super::audio::Player;
use super::audition::AuditionLine;
use super::draw::draw;
use super::input::command::{busy_summary, command_key, do_command, Command, WORDS};
use super::input::runconfig::{
    parse_mix_config, parse_render_batch, parse_run_config, run_preview, save_app_setting,
    save_render_batch, save_run_config,
};
use super::input::submit::submit_text;
use super::input::{handle_key, op_key, urlencode};
use super::jobs::{
    job_segment, run_job, set_machine_state, unreachable_verdict, verdict_after_failed_provision,
    BackgroundJob, DoneKind, Ev, Job, ProfileReq, Res, WorkspaceReq,
};
use super::layout::{
    cols, size_class, width_of, Size, COMPACT_EVENTS_MIN_H, COMPACT_FOOTER_H,
    COMPACT_MACHINES_MAX_H, COMPACT_MACHINES_MIN_H, COMPACT_MACHINE_COLS, COMPACT_TASKS_MAX_H,
    COMPACT_TASKS_MIN_H, COMPACT_WORKERS_MAX_H, COMPACT_WORKERS_MIN_H, COMPACT_WORKER_COLS,
    FULL_EVENTS_MIN_H, FULL_FOOTER_H, FULL_H, FULL_HEADER_H, FULL_MACHINES_MAX_H,
    FULL_MACHINES_MIN_H, FULL_TASKS_MAX_H, FULL_TASKS_MIN_H, FULL_W, FULL_WORKERS_MAX_H,
    FULL_WORKERS_MIN_H, KEYS_COMPACT, KEYS_FULL, MIN_H, MIN_W,
};
use super::model::*;
use super::screen::*;
use super::sound::{self, SoundView};
use super::style::*;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use bm_proto::{
    Heartbeat, Machine, MachineState, Op, OpRequest, Roster, Stage, Task, TaskPref, TaskState,
    VoiceInfo,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Color;
use std::collections::BTreeMap;

#[tokio::test]
async fn tracked_jobs_queue_only_behind_a_resource_they_need() {
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
        layout: bm_core::Layout::new(""),
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
    // Both name `Res::Cluster`, so the stop still waits for the start — the
    // pair is the one place "one cluster, one lifecycle" is literally true.
    assert!(dispatch(
        &mut app,
        &job_tx,
        Job::StopBackend {
            layout: bm_core::Layout::new(""),
            machines: vec![],
            api: "unused".into(),
            settings_key: None,
        }
    ));
    assert!(dispatch(
        &mut app,
        &job_tx,
        Job::LoadLines {
            layout: bm_core::Layout::new("")
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
fn a_queued_row_says_what_it_is_waiting_for() {
    // "causing every later job to be queued" is only actionable if the row
    // says why. A job that holds something names it; a job on the default lane
    // has nothing to contend with, and saying so is the honest answer rather
    // than inventing a reason.
    let mut app = App::new("http://unused");
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    super::input::dispatch(
        &mut app,
        &tx,
        Job::Provision {
            layout: bm_core::Layout::new(""),
            api: "unused".into(),
            machine: Machine::new("10.0.0.5", "u", 22, None, "worker"),
            force: false,
            settings_key: None,
            cancel: None,
        },
    );
    super::input::dispatch(
        &mut app,
        &tx,
        Job::LoadLines {
            layout: bm_core::Layout::new(""),
        },
    );
    assert_eq!(
        app.background_jobs[0].activity,
        "queued · needs box 10.0.0.5"
    );
    assert_eq!(app.background_jobs[1].activity, "queued");
}

/// A job holds the thing it touches and nothing else.
///
/// This is the whole difference from the two hardcoded lanes: `aws discover`
/// used to queue behind a five-minute box push because both were filed under
/// "lifecycle", and two boxes provisioned one after the other because there was
/// one queue for all of them.
#[test]
fn resources_name_what_a_job_actually_touches() {
    let box_job = |addr: &str| Job::Provision {
        layout: bm_core::Layout::new(""),
        api: "unused".into(),
        machine: Machine::new(addr, "u", 22, None, "worker"),
        force: false,
        settings_key: None,
        cancel: None,
    };
    let start = Job::StartBackend {
        layout: bm_core::Layout::new(""),
        api: "unused".into(),
        api_up: false,
        start: 1,
        count: 1,
        enqueue: false,
        machines: vec![],
        cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        settings_key: None,
    };
    assert_eq!(start.resources(), vec![Res::Cluster]);
    assert_eq!(
        Job::StopBackend {
            layout: bm_core::Layout::new(""),
            machines: vec![],
            api: "unused".into(),
            settings_key: None,
        }
        .resources(),
        vec![Res::Cluster]
    );
    assert_eq!(
        box_job("10.0.0.5").resources(),
        vec![Res::Box("10.0.0.5".into())]
    );
    // Two boxes are disjoint, which is what lets them provision at once…
    assert_ne!(
        box_job("10.0.0.5").resources(),
        box_job("10.0.0.6").resources()
    );
    // …and the same box twice is not: a second push would interleave with the
    // first, so those two do queue.
    assert_eq!(
        box_job("10.0.0.5").resources(),
        box_job("10.0.0.5").resources()
    );
    // The AWS four read-modify-write the same document.
    assert_eq!(
        Job::AwsUp {
            root: std::path::PathBuf::from("/tmp/x"),
            api: "unused".into(),
            http: reqwest::Client::new(),
            count: 1,
        }
        .resources(),
        vec![Res::Aws]
    );
    // Read-only indexes hold nothing: they must never queue behind heavy work.
    assert_eq!(
        Job::LoadLines {
            layout: bm_core::Layout::new("")
        }
        .resources(),
        Vec::<Res>::new()
    );
    assert_eq!(
        Job::LoadRoster {
            api: "unused".into(),
            http: reqwest::Client::new(),
            layout: bm_core::Layout::new("")
        }
        .resources(),
        Vec::<Res>::new()
    );
    // Everything else is the default lane, which stays serial among itself.
    assert_eq!(
        Job::Segment {
            layout: bm_core::Layout::new(""),
            character: "Vũ".into(),
            voice: "adam".into(),
            text: "Xin chào.".into(),
        }
        .resources(),
        vec![Res::Command]
    );
}

/// The regression this change exists for: a job whose resources are free starts
/// *now*, even while a long job holds something else.
#[tokio::test]
async fn a_free_job_does_not_wait_for_a_long_one() {
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
            if matches!(&job, Job::Provision { machine, .. } if machine.addr == "10.0.0.5") {
                blocked.notified().await;
            }
            let _ = tx.send(Ev::Done(DoneKind::Other));
        }
    }));
    let mut app = App::new("http://unused");
    let provision = |addr: &str| Job::Provision {
        layout: bm_core::Layout::new(""),
        api: "unused".into(),
        machine: Machine::new(addr, "u", 22, None, "worker"),
        force: false,
        settings_key: None,
        cancel: None,
    };
    assert!(dispatch(&mut app, &job_tx, provision("10.0.0.5")));
    // A different box: nothing in common with the blocked one, so it runs.
    assert!(dispatch(&mut app, &job_tx, provision("10.0.0.6")));
    // The AWS account: also nothing in common, so it runs.
    assert!(dispatch(
        &mut app,
        &job_tx,
        Job::AwsUp {
            root: std::path::PathBuf::from("/tmp/x"),
            api: "unused".into(),
            http: reqwest::Client::new(),
            count: 1,
        }
    ));
    // The same box again: this one really does contend, so it waits.
    assert!(dispatch(&mut app, &job_tx, provision("10.0.0.5")));
    let started = |ev: &Ev| match ev {
        Ev::JobStarted(id) => Some(*id),
        _ => None,
    };
    let mut seen = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), async {
        while seen.len() < 3 {
            let ev = rx.recv().await.unwrap();
            if let Some(id) = started(&ev) {
                seen.push(id);
            }
            app.apply(ev);
        }
    })
    .await
    .expect("every job whose resources are free must start at once");
    seen.sort();
    assert_eq!(
        seen,
        vec![1, 2, 3],
        "both boxes and the account started together — the single lifecycle \
         lane this replaced ran them one after another"
    );
    // Job 4 names the box job 1 still holds (job 1's runner is parked on the
    // gate, so the box is held for the whole test), and that is the one job
    // here that genuinely has to wait. Give the scheduler a beat to prove the
    // negative rather than reading an empty channel as an answer.
    tokio::time::sleep(Duration::from_millis(150)).await;
    while let Ok(ev) = rx.try_recv() {
        if let Some(id) = started(&ev) {
            seen.push(id);
        }
        app.apply(ev);
    }
    assert!(
        !seen.contains(&4),
        "a second push to the same box must queue behind the first: {seen:?}"
    );
    drop(job_tx);
    gate.notify_one();
    let _ = tokio::time::timeout(Duration::from_secs(2), worker).await;
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
                layout: bm_core::Layout::new(""),
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
            layout: bm_core::Layout::new(""),
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
        bm_core::Layout::new(""),
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
    app.layout = bm_core::Layout::new(&dir);

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
    app.layout = bm_core::Layout::new(dir);
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
    // …but the cancel flag outlives it. `StartDone` used to be the end of the
    // catch-up; it is now the moment the catch-up *starts*, and the boxes it
    // handed out are still provisioning. Clearing the flag here would leave `X`
    // with nothing to set, and a provision mid-push would launch its worker
    // anyway — a cluster that is not quiet after a stop.
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
    // queue a duplicate push at every box — exactly the queueing this change
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
    // old skip trusted those states and provisioned nothing — the backend
    // came up with no workers and only a second `:B` (fresh states, Offline)
    // brought the boxes.
    use super::jobs::split_catchup;
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
    // `<root>/.bm/ledger.json` — which is what a root-only layout did — wrote
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
    // local-only and the remotes were dropped again — the same bug, one layer
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
    // other test was handed that port in between — a race with a real window
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

#[test]
fn the_import_prompt_takes_a_number_and_a_path_but_never_guesses_the_number() {
    let mut app = App::new("http://x");
    let p = |buf: &str| TextPrompt::new(TextKind::Import, "t", "h", buf);

    // `<chapter> <path>` → the op carries both.
    match submit_text(&mut app, &p("34 /tmp/ch34.txt")).unwrap() {
        Job::Op { req, .. } => {
            assert_eq!(req.op, Op::Import);
            assert_eq!(req.chapter, Some(34));
            assert_eq!(req.paths, vec!["/tmp/ch34.txt".to_string()]);
        }
        other => panic!("{other:?}"),
    }
    // A path alone: the number comes from the filename, later, by the importer
    // (which is the one place that rule lives) — the prompt does not guess one.
    match submit_text(&mut app, &p("/tmp/ch217.txt")).unwrap() {
        Job::Op { req, .. } => {
            assert_eq!(req.chapter, None);
            assert_eq!(req.paths, vec!["/tmp/ch217.txt".to_string()]);
        }
        other => panic!("{other:?}"),
    }
    // Several files, comma-separated: every one must carry its own number or be
    // refused by the importer, so the prompt passes the number to the first.
    match submit_text(&mut app, &p("34 a.txt, b.txt")).unwrap() {
        Job::Op { req, .. } => assert_eq!(req.paths.len(), 2),
        other => panic!("{other:?}"),
    }

    // Refused, with the reason, so the prompt stays open.
    assert!(submit_text(&mut app, &p("  "))
        .unwrap_err()
        .contains("nothing to import"));
    assert!(submit_text(&mut app, &p("34"))
        .unwrap_err()
        .contains("give the path"));
    assert!(submit_text(&mut app, &p("0 /tmp/ch0.txt"))
        .unwrap_err()
        .contains("chapter 0"));
}

#[test]
fn crawl_template_takes_a_placeholder_or_nothing_at_all() {
    let mut app = App::new("http://x");
    let p = TextPrompt::new(TextKind::CrawlTemplate, "t", "h", "https://x/chuong");
    assert!(submit_text(&mut app, &p).unwrap_err().contains("{n}"));
    let p = TextPrompt::new(TextKind::CrawlTemplate, "t", "h", "https://x/chuong-{n}");
    assert!(submit_text(&mut app, &p).is_ok());
    // Empty probes without saving a template: a crawler with a `discover()` has
    // no chapter-number URL at all, and `:crawl` is the command that has to be
    // usable for it.
    let p = TextPrompt::new(TextKind::CrawlTemplate, "t", "h", "   ");
    assert!(submit_text(&mut app, &p).is_ok());
}

/// Pasting a URL we have a crawler for should say which one, and how to get
/// past the prompt that wants a `{n}`.
///
/// The shape that motivates it: ReadNovelFull's chapter URLs carry a title
/// slug, so the prompt's own rule ("must contain {n}") refuses every URL the
/// operator could possibly paste, and the refusal — left generic — is a loop.
/// Naming the crawler turns a dead end into a next step.
#[test]
fn a_known_site_url_names_its_crawler_instead_of_only_refusing() {
    let mut app = App::new("http://x");
    let p = TextPrompt::new(
        TextKind::CrawlTemplate,
        "t",
        "h",
        "https://readnovelfull.com/the-sword-god-of-the-universe.html",
    );
    let err = submit_text(&mut app, &p).unwrap_err();
    assert!(err.contains("readnovelfull.com"), "{err}");
    assert!(
        err.contains("assets/crawl/templates/readnovelfull.lua"),
        "the refusal must name the crawler, not just refuse: {err}"
    );

    // And the live note under the prompt, which is what saves the round trip.
    let note = p.known_note().expect("a recognised URL gets a note");
    assert!(note.contains("readnovelfull.com"), "{note}");
    assert!(note.contains("readnovelfull.lua"), "{note}");
    // It has to say what to do, or the note is only a label.
    assert!(note.contains("no {n} in its URLs"), "{note}");
}

/// A site on the list that is blocked must say so in the dialog, not offer a
/// crawler that cannot run.
#[test]
fn a_blocked_known_site_says_so_rather_than_offering_a_crawler() {
    let mut app = App::new("http://x");
    let p = TextPrompt::new(
        TextKind::CrawlTemplate,
        "t",
        "h",
        "https://novelfull.com/a-book/chapter-1",
    );
    let note = p.known_note().expect("a recognised URL gets a note");
    assert!(note.contains("no bundled crawler"), "{note}");
    let err = submit_text(&mut app, &p).unwrap_err();
    assert!(err.contains("no crawler for it"), "{err}");
    assert!(err.contains("Cloudflare"), "and why: {err}");
}

/// The note is a function of what is on screen, so it must not stick.
#[test]
fn the_known_site_note_tracks_the_buffer_and_only_where_it_belongs() {
    let p = TextPrompt::new(
        TextKind::CrawlTemplate,
        "t",
        "h",
        "https://storya.click/truyen/a/chuong-{n}",
    );
    // A template with `{n}` in it is already a mapping; matching it against the
    // registry would comment on a host it says nothing useful about.
    assert_eq!(p.known_site(), None);
    // An unknown site says nothing, rather than guessing.
    let p = TextPrompt::new(TextKind::CrawlTemplate, "t", "h", "https://example.com/c/1");
    assert_eq!(p.known_site(), None);
    // A prompt that is not about URLs never shows one, whatever it holds.
    let p = TextPrompt::new(
        TextKind::Import,
        "t",
        "h",
        "https://readnovelfull.com/the-sword-god.html",
    );
    assert_eq!(p.known_site(), None);
    assert_eq!(p.known_note(), None);
    // And a known one resolves from a bare host, scheme and all.
    let p = TextPrompt::new(TextKind::CrawlTemplate, "t", "h", "storya.click");
    assert_eq!(p.known_site().map(|s| s.host), Some("storya.click"));
    // Templatable, so no "no {n}" instruction — that would be a lie here.
    let note = p.known_note().unwrap();
    assert!(!note.contains("no {n}"), "{note}");
}

#[test]
fn workspace_prompt_parses_list_use_and_new() {
    // One prompt, three verbs — parsed at submit so a typo keeps the prompt
    // open with the operator's own text still in it.
    let mut app = App::new("http://x");
    let prompt = |buf: &str| TextPrompt::new(TextKind::Workspace, "t", "h", buf);

    assert!(matches!(
        submit_text(&mut app, &prompt("")),
        Ok(Job::Workspace {
            req: WorkspaceReq::List,
            ..
        })
    ));
    assert!(matches!(
        submit_text(&mut app, &prompt("  beyond-myriads  ")),
        Ok(Job::Workspace {
            req: WorkspaceReq::Use(ref n),
            ..
        }) if n == "beyond-myriads"
    ));
    assert!(matches!(
        submit_text(&mut app, &prompt("new second-book")),
        Ok(Job::Workspace {
            req: WorkspaceReq::New(ref n),
            ..
        }) if n == "second-book"
    ));
    // A name is one path segment: `../x` would escape workspaces/.
    for bad in ["../x", "a/b", "new ", "."] {
        assert!(
            submit_text(&mut app, &prompt(bad)).is_err(),
            "“{bad}” must be refused"
        );
    }
}

#[test]
fn profile_prompt_parses_list_load_and_pack() {
    let mut app = App::new("http://x");
    let prompt = |buf: &str| TextPrompt::new(TextKind::Profile, "t", "h", buf);

    assert!(matches!(
        submit_text(&mut app, &prompt("")),
        Ok(Job::Profile {
            req: ProfileReq::List,
            ..
        })
    ));
    // A bare name loads — the common case needs no verb.
    assert!(matches!(
        submit_text(&mut app, &prompt("xianxia")),
        Ok(Job::Profile {
            req: ProfileReq::Load(ref n),
            ..
        }) if n == "xianxia"
    ));
    assert!(matches!(
        submit_text(&mut app, &prompt("pack xianxia")),
        Ok(Job::Profile {
            req: ProfileReq::Pack(ref n),
            ..
        }) if n == "xianxia"
    ));
    assert!(submit_text(&mut app, &prompt("pack ")).is_err());
}

#[test]
fn the_footer_names_the_active_workspace_and_the_loaded_profile() {
    // `default` is the implicit root workspace, not a missing name: a fresh
    // clone with no pointer runs there and the footer has to say so.
    let l = bm_core::Layout::new("/repo");
    assert_eq!(super::model::workspace_label(&l), "default");
    let named = bm_core::Layout {
        root: "/repo".into(),
        work: "/repo/workspaces/beyond-myriads".into(),
    };
    assert_eq!(super::model::workspace_label(&named), "beyond-myriads");
    // No profile is the state every runner refuses to start in, so it is
    // reported plainly rather than left blank.
    assert_eq!(super::model::profile_label(None), "none");
    let p = bm_core::profile::Pointer {
        name: "xianxia".into(),
        hash: "0123456789abcdef".into(),
    };
    assert_eq!(
        super::model::profile_label(Some(&p)),
        "xianxia (0123456789ab)"
    );
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

    // Failure refreshes nothing — the roster it would fetch is unchanged.
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
fn state_age_says_unknown_rather_than_a_fifty_year_boot() {
    // The same trap `seen_label` has, one field over: `state_since == 0` means
    // the record predates the field. Formatting that as an elapsed time would
    // print "1471228h" and make every old record look permanently stuck.
    let mut m = Machine::new("10.0.0.5", "u", 22, None, "worker");
    assert_eq!(state_age_label(&m), "—", "never stamped is not 0s ago");

    m.set_state(MachineState::Initializing);
    assert_eq!(state_age_label(&m), "0s", "just launched");

    m.state_since = bm_proto::now_secs().saturating_sub(45);
    assert_eq!(state_age_label(&m), "45s");
    m.state_since = bm_proto::now_secs().saturating_sub(120);
    assert_eq!(state_age_label(&m), "2m", "a long boot reads in minutes");
    m.state_since = bm_proto::now_secs().saturating_sub(7200);
    assert_eq!(state_age_label(&m), "2h");
}

#[test]
fn a_booting_box_that_never_answered_ssh_is_not_called_broken() {
    // `:prov` seconds after `:up` is the likeliest way to meet a box whose
    // sshd is not listening yet. The probe learned nothing — it cannot even
    // tell a booting box from a dead one — so calling it `Error` is the exact
    // misreading `initializing` exists to prevent. Stay booting; the boot
    // deadline is what gives up.
    assert_eq!(
        verdict_after_failed_provision(true, false),
        MachineState::Initializing
    );
}

#[test]
fn a_box_that_answered_but_failed_a_step_is_broken_even_while_booting() {
    // The boundary that makes the rule above safe rather than a blanket
    // amnesty: ssh *answered*, so the failure is real — a missing python, a
    // full disk, a failed push. That is a fault whatever the clock says.
    assert_eq!(
        verdict_after_failed_provision(true, true),
        MachineState::Error
    );
}

#[test]
fn an_unreachable_box_we_never_thought_was_booting_is_broken() {
    // The other half of the boundary: without this, every unreachable box
    // would be forgiven once and sit in `initializing` until the deadline,
    // turning a plain wrong address into a five-minute wait.
    assert_eq!(
        verdict_after_failed_provision(false, false),
        MachineState::Error
    );
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
fn retry_scopes_narrow_by_argument_and_refuse_a_bare_stage() {
    // `:retry` is the only way to aim a requeue at one chapter from the main
    // panel, so the parser has to be exact: a mistyped scope must leave the
    // prompt open rather than quietly run the blanket retry.
    fn retry(stage: Option<Stage>, chapter: Option<u32>) -> Option<Command> {
        Some(Command::Retry { stage, chapter })
    }
    assert_eq!(
        command_key("retry"),
        retry(None, None),
        "no argument is the blanket retry"
    );
    assert_eq!(command_key("retry 24"), retry(None, Some(24)));
    assert_eq!(
        command_key("retry render 24"),
        retry(Some(Stage::Render), Some(24))
    );
    assert_eq!(
        command_key("u merge 7"),
        retry(Some(Stage::Merge), Some(7)),
        "the single-letter form takes the same arguments"
    );
    assert_eq!(
        command_key("retry RENDER 24"),
        retry(Some(Stage::Render), Some(24)),
        "stage names are case-insensitive"
    );

    // Refusals. Each would otherwise run something at the wrong scope, and the
    // blanket retry is the dangerous direction: it forgives every strike in the
    // ledger, so `:u render` must not reach it.
    assert_eq!(command_key("retry render"), None, "a bare stage is refused");
    assert_eq!(command_key("retry 0"), None, "chapter 0 is not a chapter");
    assert_eq!(command_key("retry ch24"), None, "no `ch` prefix");
    assert_eq!(command_key("retry r 24"), None, "no single-letter stages");
    assert_eq!(command_key("retry render 24 extra"), None, "one scope only");
    assert_eq!(command_key("retry boss 24"), None, "no such stage");
}

#[test]
fn command_line_maps_keys_and_words() {
    assert_eq!(command_key("m"), Some(Command::Reconcile));
    assert_eq!(command_key("B"), Some(Command::Backend));
    assert_eq!(command_key("?"), Some(Command::Key(KeyCode::Char('?'))));
    assert_eq!(
        command_key("u"),
        Some(Command::Retry {
            stage: None,
            chapter: None
        }),
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
    assert_eq!(
        command_key("prov"),
        Some(Command::Provision { force: false })
    );
    assert_eq!(
        command_key("reprov"),
        Some(Command::Provision { force: true })
    );
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
    assert!(app.colour());
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
    // The **floors**, because that is the case where every pane has nothing to
    // show and is therefore at its minimum. Longer content takes its rows out
    // of Logs, which is the pane meant to give them up.
    let panes = COMPACT_MACHINES_MIN_H
        + COMPACT_WORKERS_MIN_H
        + COMPACT_TASKS_MIN_H
        + COMPACT_EVENTS_MIN_H
        + COMPACT_FOOTER_H;
    assert!(
        panes <= MIN_H,
        "compact floors need {panes} rows, floor is {MIN_H}"
    );
    // The full tier must not be tighter than the compact one.
    let full = FULL_HEADER_H
        + FULL_MACHINES_MIN_H
        + FULL_WORKERS_MIN_H
        + FULL_TASKS_MIN_H
        + FULL_EVENTS_MIN_H
        + FULL_FOOTER_H;
    assert!(
        full <= FULL_H,
        "full floors need {full} rows, threshold is {FULL_H}"
    );
}

/// A pane's ceiling must not be so high that one busy pane crowds out the rest.
///
/// The ceilings exist so a thirty-machine cluster scrolls instead of pushing
/// Logs and the footer off the screen. This is the arithmetic behind that: at
/// the ceiling, **Logs still gets its readable floor** and the footer is never
/// squeezed out.
#[test]
fn a_busy_pane_at_its_ceiling_still_leaves_the_log_and_the_footer_room() {
    let worst = FULL_MACHINES_MAX_H + FULL_WORKERS_MAX_H + FULL_TASKS_MAX_H;
    let rest = FULL_HEADER_H + worst + FULL_EVENTS_MIN_H + FULL_FOOTER_H;
    assert!(
        rest <= FULL_H,
        "every pane at its ceiling needs {rest} rows, threshold is {FULL_H} — \
         a busy cluster would push the log off the screen"
    );
    // The compact tier is the tighter one and has the smaller ceilings, so it
    // is the one that actually has to hold.
    let compact_worst = COMPACT_MACHINES_MAX_H + COMPACT_WORKERS_MAX_H + COMPACT_TASKS_MAX_H;
    let compact_rest = compact_worst + COMPACT_EVENTS_MIN_H + COMPACT_FOOTER_H;
    assert!(
        compact_rest <= MIN_H,
        "compact ceilings need {compact_rest} rows, floor is {MIN_H}"
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
fn the_footer_advertises_jobs_on_tab_in_both_tiers() {
    // The footer is the only map of the dashboard; the key it names must be
    // the key that works, in both tiers, or the overlay is undiscoverable.
    assert!(KEYS_FULL.iter().any(|k| k.contains("Tab jobs")));
    assert!(KEYS_COMPACT.iter().any(|k| k.contains("Tab jobs")));
}

#[test]
fn every_dashboard_header_reads_in_full_at_the_100_column_floor() {
    // Regression guard for the two header crops an operator actually read:
    // the full-tier Workers pane drew `box cp` (7 glyphs in a 6-wide column)
    // and the Machines table overflowed its 98 interior columns, pushing
    // `seen` and half of `state` off the pane. Both are rendered here at the
    // exact terminal where they broke.
    let mut app = stats_app();
    let text = render_text(&mut app, 100, 32);
    for header in ["box cpu", "box ram", "workers", "activity", "seen"] {
        assert!(text.contains(header), "{header} clipped:\n{text}");
    }
}

/// A pane is sized to its content, and the content is what the terminal can
/// actually show.
///
/// This replaced a fixed row count per pane, under which a one-box cluster was
/// shown an eight-row Machines pane that was mostly border and a ten-worker
/// cluster had workers clipped with nothing saying so.
#[test]
fn every_live_worker_is_visible_on_a_terminal_that_can_hold_them() {
    let mut app = stats_app();
    let before = app.live_workers().len();
    for i in 0..8 {
        app.beats.push(beat(
            &format!("worker-{i}"),
            &format!("192.0.2.{i}"),
            2,
            &format!("worker-{i}"),
        ));
    }
    // The pane is sized from the same filtered set the renderer draws, so the
    // count that drives the layout is the count of rows on screen.
    let live = app.live_workers().len();
    assert_eq!(live, before + 8, "the fixture's own beats count too");
    let text = render_text(&mut app, 140, 44);
    for i in 0..8 {
        assert!(
            text.contains(&format!("worker-{i}")),
            "worker {i} was clipped:\n{text}"
        );
    }
}

/// Tasks and Stats are drawn in the compact tier too.
///
/// They used to be carved out of the Workers pane *only on the full tier*, so
/// on a 76x24 terminal — the default on most setups — both were simply not
/// drawn, and the footer carried a roll-up instead. A pane that cannot be seen
/// is a pane that cannot answer the question you opened the dashboard to ask.
#[test]
fn the_compact_tier_still_shows_tasks_and_stats() {
    let mut app = stats_app();
    let text = render_text(&mut app, 80, 24);
    assert!(
        text.contains("╭Tasks"),
        "no Tasks pane on a default terminal:\n{text}"
    );
    assert!(
        text.contains("╭Stats"),
        "no Stats pane on a default terminal:\n{text}"
    );
    assert!(
        text.contains("╭Logs"),
        "and the log is still there:\n{text}"
    );
    // Every pane has to fit the compact floor, or something is pushed off.
    for pane in ["╭Machines", "╭Workers", "╭Tasks", "╭Stats", "╭Logs"] {
        assert!(text.contains(pane), "{pane} missing:\n{text}");
    }
}

/// The log is the one pane that grows, because a message is the point of it.
///
/// Before this change the spare rows went to the worker list, which meant a
/// terminal with room to spare still showed a five-line log on a failing
/// cluster. Everything else now takes exactly its content, so whatever is left
/// lands here.
#[test]
fn the_log_takes_the_rows_the_other_panes_do_not_need() {
    // `stats_app` has machines and workers, so the three content panes are all
    // above their floors and the difference between these two renders is the
    // log.
    let mut app = stats_app();
    let short = render_text(&mut app, 140, 32);
    let tall = render_text(&mut app, 140, 52);

    // Measured from the rendered box itself: the number of rows between the
    // Logs top border and the footer. Counting lines that look like log lines
    // would pass whether the pane grew or not.
    let log_height = |t: &str| -> usize {
        let lines: Vec<&str> = t.lines().collect();
        let top = lines
            .iter()
            .position(|l| l.contains("╭Logs"))
            .unwrap_or_else(|| panic!("no Logs pane in:\n{t}"));
        let bottom = lines
            .iter()
            .rposition(|l| l.contains("Tab jobs") || l.contains(":add"))
            .unwrap_or_else(|| panic!("no footer in:\n{t}"));
        bottom
            .checked_sub(top)
            .expect("the log sits above the footer")
    };
    assert!(
        log_height(&short) >= FULL_EVENTS_MIN_H as usize,
        "the log lost its floor at the tier threshold:\n{short}"
    );
    assert!(
        log_height(&tall) > log_height(&short),
        "a taller terminal must grow the log, not the borders: {} -> {}",
        log_height(&short),
        log_height(&tall)
    );
}

#[test]
fn the_workers_headers_fit_their_columns() {
    // Regression guard for the `box cp` crop: the full-tier load columns
    // must be at least as wide as their headers (the `box cpu` cell carries
    // a trailing space against the edge, so its column needs 8).
    for (header, w) in [("box cpu ", 8usize), ("box ram", 10), ("progress", 19)] {
        assert!(
            width_of(header) <= w,
            "{header:?} is {} columns in a {w}-wide one",
            width_of(header)
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

/// Whether `hint` is on the rendered screen, **as a reader would see it**.
///
/// `render_text` returns the buffer row by row, so anything the overlay *wraps* is
/// split across two of them — and a hint line longer than the overlay's width
/// wraps by definition. A plain `contains` therefore misses phrases that are
/// plainly visible on screen, which is a test failing for the wrong reason. This
/// collapses the whitespace first, so the phrase is looked for as it reads.
fn hint_visible(text: &str, hint: &str) -> bool {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    flat.contains(hint)
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

#[tokio::test]
async fn paging_up_past_the_log_stops_at_the_buffers_own_top() {
    let http = reqwest::Client::new();
    let (job_tx, _job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = App::new("http://x");
    for i in 0..20 {
        app.log_at(Level::Info, format!("line {i}"));
    }
    // A page is a page: the draw publishes the pane height, so the test pins
    // it to 4 and the walk is in screenfuls, not an arbitrary 5.
    app.events_rows = 4;

    // One page back is 4 lines; a page and a half lands on 4's second press…
    handle_key(&mut app, key(KeyCode::PageUp), &http, &job_tx).await;
    assert_eq!(app.events_scroll, 4);
    handle_key(&mut app, key(KeyCode::PageUp), &http, &job_tx).await;
    assert_eq!(app.events_scroll, 8);

    // …and the walk back costs exactly what the walk out did.
    handle_key(&mut app, key(KeyCode::PageDown), &http, &job_tx).await;
    assert_eq!(app.events_scroll, 4);
    handle_key(&mut app, key(KeyCode::PageDown), &http, &job_tx).await;
    assert_eq!(
        app.events_scroll, 0,
        "a second page-down is already at newest"
    );

    // Far more pages than lines: the buffer's own top is a real edge, not a
    // number that runs to a thousand.
    for _ in 0..50 {
        handle_key(&mut app, key(KeyCode::PageUp), &http, &job_tx).await;
    }
    assert_eq!(
        app.events_scroll, 20,
        "scroll stopped at the buffer length, not 400"
    );
    // At that edge the title names the edge instead of showing a stuck count.
    let text = render_text(&mut app, 140, 44);
    assert!(
        text.contains("oldest kept line"),
        "the top of the buffer is legible:\n{text}"
    );
}

#[test]
fn a_new_line_holds_the_reading_position_instead_of_shoving_it() {
    // The scroll offset is a distance from the live tail, so an arriving line
    // grows that distance and the line under the operator's eyes stays put.
    // Pinned to newest, the tail simply follows.
    let mut app = App::new("http://x");
    for i in 0..10 {
        app.log_at(Level::Info, format!("line {i}"));
    }
    app.events_scroll = 3;
    // The top visible line is `len - scroll`; that index must survive a push.
    let top_before = app.events[app.events.len() - app.events_scroll]
        .text
        .clone();
    app.log_at(Level::Info, "arrives while reading");
    assert_eq!(app.events_scroll, 4, "the distance grew by exactly one");
    let top_after = &app.events[app.events.len() - app.events_scroll];
    assert_eq!(
        top_after.text, top_before,
        "the view is still on the same line"
    );

    // Pinned to newest, arrivals scroll by — the tail follows.
    app.events_scroll = 0;
    app.log_at(Level::Info, "tail follows");
    assert_eq!(app.events_scroll, 0);
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
    app.log_at(
        Level::Ok,
        "[192.168.2.2] already configured (agent 0.2.3 + tts sidecar)",
    );
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
        sidecars: None,
        sidecar_gb: None,
        capabilities: vec![],
        sidecar_keep: None,
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
fn workers_pane_hides_ghosts_of_offline_boxes() {
    use super::model::beat_backed;
    use bm_proto::MachineState;
    // A beat the box's Offline verdict postdates is a ghost, not a worker.
    let mut m = named_machine("192.168.2.2", "hawk");
    m.set_state(MachineState::Offline);
    let ghost = beat("thang-marmot", "192.168.2.2", 30, "marmot");
    assert!(!beat_backed(&[m.clone()], &ghost));
    // A beat newer than the verdict still counts — one slow poll flickers
    // the dot without deleting the row.
    m.state_since = m.state_since.saturating_sub(100);
    let fresh = beat("thang-marmot", "192.168.2.2", 2, "marmot");
    assert!(beat_backed(&[m.clone()], &fresh));
    // Anything but Offline backs its beats; unknown boxes back everything.
    m.set_state(MachineState::Online);
    assert!(beat_backed(&[m.clone()], &ghost));
    assert!(beat_backed(&[], &ghost));

    // And the pane agrees: no ghost rows, and the count with them.
    let mut app = App::new("http://127.0.0.1:8901");
    let mut off = named_machine("192.168.2.2", "hawk");
    off.set_state(MachineState::Offline);
    app.machines = vec![off];
    app.beats = vec![beat("thang-marmot", "192.168.2.2", 30, "marmot")];
    let text = render_text(&mut app, 140, 44);
    // The Workers block only (Stats keeps per-worker history rows, which
    // legitimately still name the box).
    let workers = text
        .split_once("Workers ·")
        .and_then(|(_, rest)| rest.split_once("╭"))
        .map(|(block, _)| block)
        .unwrap_or_default();
    assert!(
        !workers.contains("marmot"),
        "ghost rows never draw:\n{text}"
    );
    assert!(
        text.contains("0 live"),
        "the count drops with them:\n{text}"
    );
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
    assert!(
        text.contains("marmot"),
        "the worker keeps its alias:\n{text}"
    );
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
    assert!(
        text.contains("50s"),
        "remainder from history × progress:\n{text}"
    );
    // Idle with no active stage estimates nothing; unknown load dashes.
    assert!(text.contains("caracal"), "idle workers list too:\n{text}");
    assert!(text.contains("—"), "dashes where nothing is known:\n{text}");
    // The 100-column floor still fits the split row, not just wide terms.
    let narrow = render_text(&mut app, 100, 32);
    assert!(
        narrow.contains("Stats"),
        "panel survives the floor:\n{narrow}"
    );
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
        text.contains("unassigned — :v fills gaps"),
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
    // the dashboard". Tab opens; Tab closes — the same toggle shape the
    // sound editor's layer tabs already use.
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
    assert!(text.contains("3 of 15 before it is shelved"), "{text}");
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
async fn the_task_page_explains_a_batched_render_from_the_row_that_owns_it() {
    // Ten rows flipping to `assigned` on one box with nothing on screen to say
    // why is the state batching created, and the TUI is the operator's only
    // interface. The fact lives on the row the offer named — the one the
    // worker reports progress against — so that is where it is shown, and a
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

    // A row with no batch says nothing extra — the line is not decoration.
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
    app.layout = bm_core::Layout::new(&dir);
    let settings_path = dir.join(".bm").join("settings.json");
    let load = || bm_core::config::Settings::load(&settings_path);

    let key = dir.join("id_def");
    std::fs::write(&key, "k").unwrap();
    let msg = save_app_setting(&app, TextKind::SshKey, key.to_str().unwrap()).unwrap();
    assert!(msg.contains("saved"), "{msg}");
    assert_eq!(load().ssh.key.as_deref(), Some(key.to_str().unwrap()));
    // Clearing is a real answer: ssh decides per machine afterwards.
    save_app_setting(&app, TextKind::SshKey, "  ").unwrap();
    assert_eq!(load().ssh.key, None);
    // A missing file keeps the prompt open, it never saves garbage.
    let err = save_app_setting(&app, TextKind::SshKey, "/nonexistent/k").unwrap_err();
    assert!(err.contains("/nonexistent/k"), "{err}");
    assert_eq!(load().ssh.key, None);

    save_app_setting(&app, TextKind::SshUser, "worker").unwrap();
    assert_eq!(load().ssh.user, "worker");
    assert!(save_app_setting(&app, TextKind::SshUser, "  ")
        .unwrap_err()
        .contains("empty"));

    assert!(save_app_setting(&app, TextKind::SshPort, "abc")
        .unwrap_err()
        .contains("not a number"));
    save_app_setting(&app, TextKind::SshPort, "2222").unwrap();
    assert_eq!(load().ssh.port, 2222);
}

#[test]
fn render_batch_parses_its_bounds_and_saves_to_this_workspaces_settings() {
    // The knob's own rules in one place. `0` is the value that would deadlock
    // the scheduler — an offer of no takes assigns no row, so the chapter never
    // leaves Pending and nothing anywhere says why — and 64 is where a batch
    // stops being a batch and becomes a lease held on one box for hours. Both
    // are refused *while the operator's typing is still on screen*; the
    // scheduler's clamp is the last resort, not the first answer.
    let dir = std::env::temp_dir().join("bm-renderbatch-save");
    let _ = std::fs::remove_dir_all(&dir);
    let mut app = App::new("http://x");
    app.layout = bm_core::Layout::new(&dir);
    let load = || bm_core::config::Settings::load(&bm_core::Layout::new(&dir).settings());

    assert_eq!(parse_render_batch("12").unwrap(), 12);
    assert_eq!(
        parse_render_batch(" 3 ").unwrap(),
        3,
        "whitespace is not a typo"
    );
    assert!(parse_render_batch("").unwrap_err().contains("not a number"));
    assert!(parse_render_batch("ten")
        .unwrap_err()
        .contains("not a number"));
    assert!(parse_render_batch("-2")
        .unwrap_err()
        .contains("not a number"));
    assert!(parse_render_batch("0").unwrap_err().contains("at least 1"));
    let too_big = parse_render_batch("65").unwrap_err();
    assert!(too_big.contains("at most 64"), "{too_big}");

    let msg = save_render_batch(&app, "4").unwrap();
    assert!(msg.contains("4 take"), "{msg}");
    assert_eq!(
        load().render_batch,
        4,
        "the file is what the scheduler reads"
    );

    // A refused value writes nothing — a typo must not clear what is in force.
    assert!(save_render_batch(&app, "0").is_err());
    assert_eq!(
        load().render_batch,
        4,
        "and the old value survives the typo"
    );
}

#[tokio::test]
async fn the_batch_command_opens_a_prefilled_prompt_and_enter_saves_it() {
    // End to end through the key chain: the word routes, the prompt opens on
    // the value actually in force (a compiled default has to read differently
    // from a number somebody chose), and Enter writes it and closes.
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let dir = std::env::temp_dir().join("bm-renderbatch-prompt");
    let _ = std::fs::remove_dir_all(&dir);
    let mut app = App::new("http://x");
    app.layout = bm_core::Layout::new(&dir);

    assert_eq!(command_key("batch"), Some(Command::RenderBatch));
    assert_eq!(command_key("renderbatch"), Some(Command::RenderBatch));
    assert_eq!(
        command_key("b"),
        Some(Command::Key(KeyCode::Char('b'))),
        "no single-key form: `b` is not the batch"
    );

    // Nothing saved yet, so the prompt shows the compiled default.
    do_command(&mut app, Command::RenderBatch, &http, &job_tx);
    match &app.screen {
        Screen::Text(p) => {
            assert_eq!(p.kind, TextKind::RenderBatch);
            assert_eq!(
                p.buf,
                bm_core::config::DEFAULT_RENDER_BATCH.to_string(),
                "prefilled with what is in force"
            );
        }
        other => panic!("expected the batch prompt, got {other:?}"),
    }
    assert!(
        job_rx.try_recv().is_err(),
        "a save-only prompt dispatches nothing"
    );

    // A bad value keeps the prompt open with the problem stated in place.
    if let Screen::Text(p) = &mut app.screen {
        p.buf = "0".into();
    }
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    assert!(
        matches!(app.screen, Screen::Text(_)),
        "the prompt stays open: {:?}",
        app.screen
    );
    assert!(
        !bm_core::Layout::new(&dir).settings().is_file(),
        "and a refused value wrote nothing at all — no file, not an empty one"
    );

    // A good one saves and closes, and the next prompt shows the new value.
    if let Screen::Text(p) = &mut app.screen {
        p.buf = "6".into();
    }
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    assert!(matches!(app.screen, Screen::Normal), "{:?}", app.screen);
    let saved = bm_core::config::Settings::load(&bm_core::Layout::new(&dir).settings());
    assert_eq!(saved.render_batch, 6);
    assert!(
        job_rx.try_recv().is_err(),
        "and it still dispatched nothing — the scheduler reads the file"
    );

    do_command(&mut app, Command::RenderBatch, &http, &job_tx);
    match &app.screen {
        Screen::Text(p) => assert_eq!(p.buf, "6", "prefilled from the file, not the default"),
        other => panic!("expected the batch prompt, got {other:?}"),
    }
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
async fn roster_job_shows_disk_first_without_contacting_anyone() {
    use std::time::Duration;
    let d = tempfile::tempdir().unwrap();
    let layout = bm_core::Layout::new(d.path());
    std::fs::create_dir_all(layout.data()).unwrap();
    std::fs::write(layout.cast("vieneu"), r#"{"A":"Đức Trí"}"#).unwrap();
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Ev>();
    // Nothing listens on :9 — the inductor hop fails fast. The local roster
    // must already be on the channel: picking never waits for the network.
    super::jobs::job_load_roster(tx, "http://127.0.0.1:9".into(), http, layout).await;
    let first = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("the local roster arrives fast")
        .expect("channel open");
    match first {
        Ev::Roster(Ok(r)) => {
            assert_eq!(r.source, "offline");
            assert_eq!(r.cast.get("A").map(String::as_str), Some("Đức Trí"));
            assert!(
                !r.voices.is_empty(),
                "catalogue lists voices with no sidecar"
            );
        }
        Ev::Roster(Err(e)) => panic!("local roster failed: {e}"),
        _ => panic!("the first event must be the local roster"),
    }
    // ...then the job's Done (the dead inductor contributes no upgrade).
    let second = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("Done follows")
        .expect("channel open");
    assert!(matches!(second, Ev::Done(DoneKind::Other)));
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
    app.screen = Screen::Text(TextPrompt::new(TextKind::Command, ":", "", "quit"));
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
    let layout = bm_core::Layout::new(root);
    app.sound = Some(sound::load(&layout).expect("the fixture loads"));
    app.layout = layout;
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
    app.sound = Some(sound::load(&bm_core::Layout::new(&root)).unwrap());

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
    // scheduler does not make — the pool registry is edited from the screen —
    // so the inductor has to be told to look. Without this op the level moves
    // and every published chapter keeps its old mix for ever.
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
fn local_cache_layout() -> (tempfile::TempDir, bm_core::Layout) {
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
    (dir, layout)
}

#[tokio::test]
async fn an_unrendered_held_line_falls_back_to_one_of_hers() {
    // Fresh swap, rendered chapter by chapter: the held line misses in her
    // voice, but her voice exists in the cache — play one of hers, still
    // zero synthesis, and hold it so T compares on the same sentence.
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
    // instead of the API. Listening needs no :B.
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
    // playback cannot tell the paths apart.
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

// --- the EC2 half -------------------------------------------------------

#[test]
fn command_keys_are_unique_and_operators_stay_off_the_keyboard() {
    // There was no key-uniqueness test at all, so a new binding could quietly
    // shadow an existing one. Read-only navigations may share their Normal-mode
    // key (that is the point of `Command::Key`); operator commands may not —
    // they live on the `:` line, and a bare keypress must not launch or
    // terminate anything.
    let mut seen: Vec<char> = Vec::new();
    for w in WORDS {
        let Some(k) = w.key else { continue };
        assert!(
            !seen.contains(&k),
            "key {k:?} is bound twice in the command table"
        );
        seen.push(k);
    }
    // The three new cloud keys are gated, not live: a stray `w`/`o`/`l` must not
    // launch or terminate EC2 instances. (A few older operators — `B`, `X` — do
    // keep a normal-mode key on purpose; these do not.)
    for k in ['w', 'o', 'l'] {
        assert!(seen.contains(&k), "{k:?} lost its command binding");
        assert!(
            "aANpPdtcvsSeumBXwol".contains(k),
            "cloud key {k:?} is not in the normal-mode gate list"
        );
    }
}

#[test]
fn cloud_commands_parse_words_keys_and_the_up_count() {
    assert_eq!(command_key("up"), Some(Command::AwsUp { count: 1 }));
    assert_eq!(command_key("up 3"), Some(Command::AwsUp { count: 3 }));
    assert_eq!(command_key("launch"), Some(Command::AwsUp { count: 1 }));
    // A bad count is refused, not defaulted to one.
    assert_eq!(command_key("up 0"), None);
    assert_eq!(command_key("up x"), None);
    assert_eq!(command_key("pool"), Some(Command::AwsPool));
    assert_eq!(command_key("aws"), Some(Command::AwsPool));
    assert_eq!(command_key("cloud"), Some(Command::AwsPool));
    assert_eq!(command_key("down"), Some(Command::AwsDown { force: false }));
    assert_eq!(
        command_key("down force"),
        Some(Command::AwsDown { force: true })
    );
    assert_eq!(
        command_key("terminate"),
        Some(Command::AwsDown { force: false })
    );
    assert_eq!(command_key("l"), Some(Command::AwsPool));
    assert_eq!(command_key("w"), Some(Command::AwsUp { count: 1 }));
    assert_eq!(command_key("o"), Some(Command::AwsDown { force: false }));
}

#[test]
fn login_and_discover_are_words_with_no_key_of_their_own() {
    // Setup verbs: reachable from the `:` line, never a bare keypress. They are
    // one-off account work, and a stray letter must not store a credential.
    assert_eq!(command_key("login"), Some(Command::AwsLogin));
    assert_eq!(command_key("LOGIN"), Some(Command::AwsLogin));
    assert_eq!(command_key("discover"), Some(Command::AwsDiscover));
    for name in ["login", "discover"] {
        let w = WORDS
            .iter()
            .find(|w| w.names[0] == name)
            .unwrap_or_else(|| panic!("{name} is not in the word table"));
        assert!(w.key.is_none(), "{name} must not fire from a bare key");
        assert!(w.desc.is_some(), "{name} needs a `:help` line");
    }
}

#[test]
fn discover_parses_the_same_flags_the_cli_takes_and_refuses_a_typo() {
    // The dashboard parses through the CLI's own clap definition, so this is a
    // regression guard against the two front ends drifting apart: a flag the
    // CLI accepts must parse here, and an unknown one must be an error rather
    // than silently dropped.
    let toks = |s: &str| -> Vec<String> { s.split_whitespace().map(str::to_string).collect() };
    let args = crate::aws_ops::DiscoverArgs::parse_tokens(&toks(
        "--region eu-central-1 --instance-profile storycast-worker --security-group sg-abc",
    ))
    .unwrap();
    assert_eq!(args.region.as_deref(), Some("eu-central-1"));
    assert_eq!(args.instance_profile.as_deref(), Some("storycast-worker"));
    assert_eq!(args.security_group.as_deref(), Some("sg-abc"));
    assert!(!args.force);
    // Empty is a legitimate refresh: every field is kept from the pool.
    assert!(crate::aws_ops::DiscoverArgs::parse_tokens(&[]).is_ok());
    assert!(
        crate::aws_ops::DiscoverArgs::parse_tokens(&toks("--regionn eu-central-1")).is_err(),
        "a typo must not be ignored"
    );
    assert!(
        crate::aws_ops::LoginArgs::parse_tokens(&toks("--csv x.csv"))
            .unwrap()
            .csv
            .is_some()
    );
}

#[test]
fn the_login_prompt_takes_the_console_csv_and_never_a_typed_secret() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = App::new("http://x");
    app.layout.root = dir.path().to_path_buf();
    let prompt = |buf: &str| TextPrompt::new(TextKind::AwsLogin, "t", "h", buf);

    // A bare key id has no secret to go with it, and the secret must not be
    // typed on a screen — refused with that reason, not half-handled.
    let err = submit_text(&mut app, &prompt("--access-key-id AKIAEXAMPLE")).unwrap_err();
    assert!(err.contains("secret cannot be typed here"), "{err}");
    let err = submit_text(&mut app, &prompt("~/nowhere.csv")).unwrap_err();
    assert!(err.contains("no such file"), "{err}");

    // A real file is taken as-is, and a leading `~` is expanded (no shell here).
    let csv = dir.path().join("accessKeys.csv");
    std::fs::write(
        &csv,
        "Access key ID,Secret access key\nAKIAEXAMPLE,s3cr3t\n",
    )
    .unwrap();
    match submit_text(&mut app, &prompt(&csv.display().to_string())).unwrap() {
        Job::AwsLogin { csv: p, .. } => assert_eq!(p, csv),
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_discover_prompt_expands_a_tilde_pem_and_refuses_a_missing_one() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = App::new("http://x");
    app.layout.root = dir.path().to_path_buf();
    let prompt = |buf: &str| TextPrompt::new(TextKind::AwsDiscover, "t", "h", buf);

    assert!(submit_text(&mut app, &prompt("")).is_err());
    let err = submit_text(
        &mut app,
        &prompt("--region eu-central-1 --pem /no/such.pem"),
    )
    .unwrap_err();
    assert!(err.contains("no such .pem"), "{err}");

    let pem = dir.path().join("storycast.pem");
    std::fs::write(&pem, "-----BEGIN PRIVATE KEY-----").unwrap();
    match submit_text(
        &mut app,
        &prompt(&format!("--region eu-central-1 --pem {}", pem.display())),
    )
    .unwrap()
    {
        Job::AwsDiscover { args, .. } => {
            assert_eq!(args.region.as_deref(), Some("eu-central-1"));
            assert_eq!(args.pem.as_deref(), Some(pem.as_path()));
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn aws_pool_job_reports_an_unreadable_account_and_never_a_blank_one() {
    // A fresh root with no `.bm/aws.json`: the read fails on the missing region.
    // The Cloud view must get the reason — an empty account is the one wrong
    // answer here, because it reads as "nothing is running".
    let dir = tempfile::tempdir().unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Ev>();
    super::jobs::job_aws_pool(
        tx,
        dir.path().to_path_buf(),
        "http://127.0.0.1:1".into(),
        reqwest::Client::new(),
    )
    .await;
    let mut cloud = None;
    let mut done = 0usize;
    while let Ok(ev) = rx.try_recv() {
        match ev {
            Ev::Cloud(r) => cloud = Some(r),
            Ev::Done(_) => done += 1,
            _ => {}
        }
    }
    assert!(
        matches!(cloud, Some(Err(_))),
        "must report the failure, not an empty list: {cloud:?}"
    );
    assert_eq!(done, 1, "every arm owes exactly one Done");
}

#[test]
fn a_batch_is_visible_to_the_down_guard_and_bounded_in_its_dialog() {
    // Two claims batching could have broken — one held, one did not.
    //
    // (a) The guard must still see the box as busy. It reads *rows*, not the
    //     beat's `task_id`, so every take of a batch counts: batching must not
    //     be able to hide in-flight work from the one guard that exists to stop
    //     an operator killing a render.
    // (b) The dialog line did break. `Confirm`'s height is `body.len() + 5` —
    //     one entry per body line — while the paragraph *wraps*, so a single
    //     long entry costs visual lines the height never counted and pushes the
    //     `Enter / y confirm` hint out of the box. One task per box never did
    //     that; sixty-four takes on one box does.
    let now = bm_proto::now_secs();
    let beat: Heartbeat = serde_json::from_value(serde_json::json!({
        "worker_id": "w1", "addr": "172.31.1.5", "stage": "render",
        "progress": 0.4, "activity": "render ch42", "ts": now
    }))
    .unwrap();
    // One chapter's batch, all on one box. The head names the group; the
    // members carry no grouping of their own.
    let mut rows: Vec<Task> = (0..12)
        .map(|pos| {
            let mut t = Task::new_take(42, pos);
            t.state = TaskState::Running;
            t.assigned_to = Some("w1".into());
            t
        })
        .collect();
    rows[0].batch = (1..12).map(|p| format!("render:42:{p}")).collect();

    let busy = busy_on(&[beat], &rows, &["172.31.1.5".to_string()], now);
    assert_eq!(
        busy.len(),
        13,
        "all twelve takes are in flight, plus the worker row: {busy:?}"
    );
    assert!(busy.contains(&"render:42:0".to_string()), "{busy:?}");
    assert!(busy.contains(&"render:42:11".to_string()), "{busy:?}");

    // The line is bounded, and says how many it left out. The list arrives
    // sorted, so `w1` sorts last and is the one that gets elided.
    let line = busy_summary(&busy);
    assert!(line.starts_with("render:42:0"), "{line}");
    assert!(line.contains("more"), "the remainder is stated: {line}");
    assert!(!line.contains("w1"), "and the tail is elided: {line}");
    assert!(
        line.len() <= 68,
        "and it fits inside the dialog's width: {} chars — {line}",
        line.len()
    );
    // The bound is by width, not by count, so a list of *long* ids is elided
    // harder than a list of short ones — the property a fixed count gets wrong.
    let long: Vec<String> = (0..12).map(|i| format!("render:1999:{i}")).collect();
    assert!(busy_summary(&long).len() <= 68, "{}", busy_summary(&long));
    let short: Vec<String> = (0..12).map(|i| format!("m:{i}")).collect();
    let short_line = busy_summary(&short);
    assert!(short_line.len() <= 68);
    assert!(
        short_line.matches(", ").count() > busy_summary(&long).matches(", ").count(),
        "shorter ids fit more of them: {short_line} vs {}",
        busy_summary(&long)
    );
    // A short list is not elided at all: no ellipsis, no arithmetic.
    assert_eq!(busy_summary(&busy[..2]), "render:42:0, render:42:1");
}

#[test]
fn the_down_guard_sees_a_render_in_flight_on_one_of_those_boxes() {
    let now = bm_proto::now_secs();
    let skip = |json: serde_json::Value| -> Heartbeat { serde_json::from_value(json).unwrap() };
    let beats = vec![
        skip(serde_json::json!({
            "worker_id": "w1", "addr": "172.31.1.5", "stage": "render",
            "progress": 0.4, "activity": "render ch42", "ts": now
        })),
        // A worker merely online with no stage is not "in flight".
        skip(serde_json::json!({
            "worker_id": "w2", "addr": "10.0.0.9", "progress": 0.0,
            "activity": "idle", "ts": now
        })),
    ];
    let mut t = Task::new(42, Stage::Render);
    t.state = TaskState::Running;
    t.assigned_to = Some("w1".into());
    let tasks = vec![t];
    // Both the public address the registry keys on and the private one the agent
    // reports name the same box.
    let addrs = vec!["3.76.103.21".to_string(), "172.31.1.5".to_string()];
    let busy = busy_on(&beats, &tasks, &addrs, now);
    assert!(busy.contains(&"render:42".to_string()), "{busy:?}");
    assert!(busy.contains(&"w1".to_string()), "{busy:?}");
    assert!(!busy.contains(&"w2".to_string()), "{busy:?}");

    // A box that is not in the list is never the box being killed.
    let elsewhere = vec!["203.0.113.9".to_string()];
    assert!(busy_on(&beats, &tasks, &elsewhere, now).is_empty());
    // A stale heartbeat is not a live render.
    assert!(busy_on(&beats, &tasks, &addrs, now + 200).is_empty());
}

#[test]
fn cloud_listing_addresses_and_liveness_are_read_off_the_instances() {
    let live = bm_core::provision::AwsInstance {
        id: "i-09def58f197d3092c".into(),
        instance_type: "c7i.xlarge".into(),
        state: "running".into(),
        az: "eu-central-1a".into(),
        spot: true,
        public_ip: "3.76.103.21".into(),
        private_ip: "172.31.19.210".into(),
        profile: "b20f7789f510".into(),
        launch_time: "2026-09-20T18:38:56+00:00".into(),
    };
    assert!(is_live_state(&live.state));
    assert_eq!(
        instance_addresses(&[live]),
        vec!["3.76.103.21".to_string(), "172.31.19.210".to_string()]
    );
    assert!(is_live_state("running"));
    assert!(!is_live_state("terminated"));
    assert!(!is_live_state("stopped"));
}

// --- visual polish: theme, header, spinner, selection --------------------

fn http_client() -> reqwest::Client {
    reqwest::Client::new()
}

fn job_channel() -> tokio::sync::mpsc::UnboundedSender<Job> {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    tx
}

#[tokio::test]
async fn the_c_key_cycles_the_three_themes_and_mono_drops_the_hues() {
    use crate::tui::style::{theme_label, themed};
    use ratatui::style::Color;

    let mut app = App::new("http://127.0.0.1:8901");
    let http = http_client();
    let job_tx = job_channel();
    assert_eq!(theme_label(), "default");
    assert!(app.colour(), "default keeps the hues");

    // default → dim
    handle_key(&mut app, key(KeyCode::Char('C')), &http, &job_tx).await;
    assert_eq!(theme_label(), "dim");
    assert!(app.colour(), "dim is still colour");
    assert_ne!(
        themed(Color::Green),
        Color::Green,
        "dim remaps the stock hues"
    );

    // dim → mono
    handle_key(&mut app, key(KeyCode::Char('C')), &http, &job_tx).await;
    assert_eq!(theme_label(), "mono");
    assert!(!app.colour(), "mono drops the hues");
    assert_eq!(
        themed(Color::Green),
        Color::White,
        "mono reads every state hue as white"
    );

    // mono → default, closing the cycle
    handle_key(&mut app, key(KeyCode::Char('C')), &http, &job_tx).await;
    assert_eq!(theme_label(), "default");
    assert!(themed(Color::Green) == Color::Green);
}

#[tokio::test]
async fn the_theme_cycle_lands_on_the_same_theme_every_time() {
    use crate::tui::style::theme_label;
    // The thread-local must not depend on which test ran before it.
    for expected in ["dim", "mono", "default", "dim"] {
        let mut app = App::new("http://127.0.0.1:8901");
        let http = http_client();
        let job_tx = job_channel();
        handle_key(&mut app, key(KeyCode::Char('C')), &http, &job_tx).await;
        assert_eq!(theme_label(), expected);
    }
}

/// The mouse can be handed back to the terminal, so an error can be copied.
///
/// While mouse reporting is on the terminal routes every drag to the program
/// instead of treating it as a selection, which is why an error message could
/// not be highlighted and copied — the one thing anybody wants to do with an
/// error. `M` turns reporting off and back on again.
#[tokio::test]
async fn the_mouse_can_be_handed_back_to_the_terminal_to_copy_an_error() {
    let mut app = App::new("http://127.0.0.1:8901");
    let http = http_client();
    let job_tx = job_channel();
    assert!(
        app.mouse_capture,
        "reporting starts on, so panes are clickable"
    );

    handle_key(&mut app, key(KeyCode::Char('M')), &http, &job_tx).await;
    assert!(
        !app.mouse_capture,
        "M must hand the mouse back for selection"
    );
    assert!(
        app.mouse_toggle,
        "and ask the loop, which owns the terminal, to do it"
    );
    assert!(
        app.status.text.contains("select"),
        "the status line must say what M did, or it is a key nobody finds again: {}",
        app.status.text
    );

    // And back again, because click-to-select is worth having too.
    app.mouse_toggle = false;
    handle_key(&mut app, key(KeyCode::Char('M')), &http, &job_tx).await;
    assert!(app.mouse_capture);
    assert!(app.mouse_toggle);
}

/// `M` is a new key, so it must not have been somebody else's.
///
/// `m` is the documented alias for `:m` (reconcile) and is deliberately left
/// alone: two keys one letter apart doing unrelated things is exactly how a
/// dashboard grows a wrong muscle memory.
#[tokio::test]
async fn the_mouse_key_does_not_collide_with_the_reconcile_alias() {
    let (job_tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = App::new("http://127.0.0.1:8901");
    let http = http_client();
    handle_key(&mut app, key(KeyCode::Char('m')), &http, &job_tx).await;
    assert!(app.mouse_capture, "lowercase m must not toggle the mouse");
    assert!(!app.mouse_toggle);
    // The existing guarantee still holds: a bare m from Normal mode dispatches
    // nothing and opens nothing.
    assert!(matches!(app.screen, Screen::Normal));
    assert!(rx.try_recv().is_err(), "a bare m must dispatch nothing");
}

/// The crawl view answers the question the dashboard could not: what is
/// actually in force, and what will this book fetch.
#[tokio::test]
async fn the_crawl_key_answers_what_is_in_force() {
    let dir = std::env::temp_dir().join("bm-crawlview-render");
    let _ = std::fs::remove_dir_all(&dir);
    let layout = bm_core::Layout::new(&dir);
    std::fs::create_dir_all(layout.settings().parent().unwrap()).unwrap();
    std::fs::write(
        layout.settings(),
        serde_json::to_string_pretty(&serde_json::json!({
            "crawl": { "mode": "script", "script": "crawl/truyencom.lua", "pace_ms": 0 }
        }))
        .unwrap(),
    )
    .unwrap();

    let mut app = App::new("http://127.0.0.1:8901");
    app.layout = layout.clone();
    let http = http_client();
    let job_tx = job_channel();
    handle_key(&mut app, key(KeyCode::Char('c')), &http, &job_tx).await;
    assert!(
        matches!(app.screen, Screen::Crawl { .. }),
        "c opens the crawl view, not nothing"
    );

    let text = render_text(&mut app, 120, 44);
    // The three things the question is made of: the method, the crawler, and
    // the limits.
    assert!(text.contains("mode"), "{text}");
    assert!(text.contains("script"), "{text}");
    assert!(text.contains("truyencom.lua"), "{text}");
    assert!(text.contains("max_fetches"), "{text}");
    // And the two faults a hand-edited settings file causes without saying so.
    assert!(
        text.contains("NOT FOUND"),
        "a crawler that is not there must say so on screen:\n{text}"
    );
    assert!(
        text.contains("pacing off"),
        "pace 0 is a decision, so it is flagged:\n{text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Uppercase `C` is the palette cycle, so the two must not trade places.
#[tokio::test]
async fn the_crawl_key_does_not_steal_the_palette_cycle() {
    let (job_tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = App::new("http://127.0.0.1:8901");
    let http = http_client();
    let before = app.theme;
    handle_key(&mut app, key(KeyCode::Char('C')), &http, &job_tx).await;
    assert_ne!(app.theme, before, "C still cycles the palette");
    assert!(matches!(app.screen, Screen::Normal), "C opens nothing");

    app.theme = before;
    handle_key(&mut app, key(KeyCode::Char('c')), &http, &job_tx).await;
    assert_eq!(
        app.theme, before,
        "c must not change the theme — it only opens the view"
    );
    assert!(matches!(app.screen, Screen::Crawl { .. }));
    assert!(
        rx.try_recv().is_err(),
        "the view reads; it dispatches nothing"
    );
}

/// A view nobody can find is a view that does not exist.
#[test]
fn the_crawl_view_is_in_the_footer_and_the_help_screen() {
    assert!(
        KEYS_FULL.iter().any(|k| k.contains("c crawl")),
        "{KEYS_FULL:?}"
    );
    let mut help_app = App::new("http://127.0.0.1:8901");
    help_app.screen = Screen::Help { scroll: 0 };
    let help = render_text(&mut help_app, 140, 60);
    assert!(
        help.contains("crawl view"),
        "the help screen must list it:\n{help}"
    );
}

/// The key that turns the mouse off has to be findable without being told.
#[test]
fn the_mouse_key_is_in_the_footer_and_the_help_screen() {
    // One line a tier is enough — what must not happen is the key being
    // nowhere in the footer, which is how it is never found.
    for (tier, lines) in [("full", &KEYS_FULL), ("compact", &KEYS_COMPACT)] {
        assert!(
            lines.iter().any(|k| k.contains('M')),
            "the {tier} footer must name the mouse key: {lines:?}"
        );
    }
    // The help screen is where a key nobody uses daily is looked up.
    let mut help_app = App::new("http://127.0.0.1:8901");
    help_app.screen = Screen::Help { scroll: 0 };
    let help = render_text(&mut help_app, 140, 60);
    assert!(
        help.contains("select") && help.contains("copy"),
        "the help screen must explain what M is for:\n{help}"
    );
}

#[tokio::test]
async fn the_full_tier_has_a_header_strip_and_the_compact_tier_does_not() {
    let mut app = App::new("http://127.0.0.1:8901");
    let text = render_text(&mut app, 140, 44);
    assert!(
        text.contains("ws: default"),
        "the header names the book:\n{text}"
    );
    assert!(
        text.contains("profile:"),
        "the header names the profile:\n{text}"
    );
    assert!(
        text.contains("C cycles"),
        "the theme chip advertises the key:\n{text}"
    );

    // The compact tier keeps ws/profile where its footer can show them.
    let mut app = App::new("http://127.0.0.1:8901");
    let text = render_text(&mut app, 80, 24);
    assert!(
        text.contains("ws: default"),
        "compact keeps the workspace in the footer:\n{text}"
    );
}

#[tokio::test]
async fn pending_jobs_show_a_spinner_and_live_shows_a_pulse() {
    let mut app = App::new("http://127.0.0.1:8901");
    app.pending = 2;
    app.tick = 3;
    app.conn = Conn::Up;
    app.refreshed = Some(std::time::Instant::now());
    let text = render_text(&mut app, 140, 44);
    let frames: Vec<char> = "⠋⠙⠹⠸⠼⠴⠦⠇".chars().collect();
    assert!(
        text.contains(&format!("{} 2 job(s) running", frames[3])),
        "the spinner steps with the tick:\n{text}"
    );
}

#[test]
fn the_log_severity_column_is_fixed_width() {
    let mut app = App::new("http://127.0.0.1:8901");
    app.log_at(Level::Error, "boom one");
    app.log_at(Level::Ok, "fine two");
    let text = render_text(&mut app, 140, 44);
    // Both tags start their message at the same column; the old mixed-width
    // glyphs (`OK`, `ERROR`) left the text ragged.
    for tag in ["err ", " ok "] {
        assert!(text.contains(tag), "fixed-width `{tag}` tag:\n{text}");
    }
    assert!(!text.contains("ERROR "), "no wide ERROR tag:\n{text}");
}

#[test]
fn the_state_column_leads_with_a_glyph_and_the_word_stays() {
    let mut app = App::new("http://127.0.0.1:8901");
    app.machines.push(Machine {
        id: "192.168.2.2".into(),
        addr: "192.168.2.2".into(),
        name: "box-1".into(),
        ssh_user: "ubuntu".into(),
        ssh_port: 22,
        ssh_key: None,
        role: "worker".into(),
        state: MachineState::Online,
        state_since: bm_proto::now_secs(),
        last_seen: bm_proto::now_secs(),
        capabilities: Vec::new(),
        tts_url: None,
        task_port: None,
        task_policy: None,
        note: String::new(),
    });
    let text = render_text(&mut app, 140, 44);
    assert!(
        text.contains("● online"),
        "a healthy box reads at a glance:\n{text}"
    );
    assert!(
        text.contains("box-1") && text.contains("1 up"),
        "the right title counts the boxes:\n{text}"
    );
}

#[test]
fn machines_pane_shows_no_address_for_a_box_the_account_has_not_addressed_yet() {
    // The confusion this ends: a launched box keyed by an address nothing can
    // dial. `RunInstances` answers before the address exists, and the old
    // fallback printed the *private* address there — a real-looking IP for a box
    // across the internet, which the scheduler then failed to reach every two
    // seconds.
    let mut app = App::new("http://127.0.0.1:8901");
    let pending = bm_core::provision::AwsInstance {
        id: "i-0123456789abcdef0".into(),
        instance_type: "t3.large".into(),
        state: "pending".into(),
        az: "eu-central-1a".into(),
        spot: false,
        public_ip: String::new(),
        private_ip: "172.31.21.86".into(),
        profile: "p".into(),
        launch_time: String::new(),
    };
    let m = bm_core::provision::machine_from_instance(
        &pending,
        &bm_core::provision::AwsConfig::default(),
    );
    assert_eq!(m.state, MachineState::AwaitingIp);
    assert_eq!(
        super::model::addr_label(&m),
        "—",
        "the ip column says nothing rather than something undialable"
    );
    assert_eq!(
        super::model::machine_label(&m),
        "i-0123456789abcdef0",
        "but the row is named by the handle the account read repairs it by"
    );
    assert_eq!(super::model::machine_kind(&m), "aws");
    // The private address is not lost — it is in the note, for the detail panel
    // and for an operator whose inductor sits in the same VPC.
    assert!(m.note.contains("172.31.21.86"));

    let addressed = bm_core::provision::machine_from_instance(
        &bm_core::provision::AwsInstance {
            public_ip: "52.2.2.2".into(),
            ..pending.clone()
        },
        &bm_core::provision::AwsConfig::default(),
    );
    assert_eq!(addressed.state, MachineState::Initializing);
    assert_eq!(super::model::addr_label(&addressed), "52.2.2.2");

    app.machines = vec![m];
    let text = render_text(&mut app, 140, 44);
    assert!(
        text.contains("◐ awaiting-ip"),
        "the state says what is being waited for — and fits the column, which \
         `awaiting-address` did not:\n{text}"
    );
    assert!(
        text.contains("i-0123456789"),
        "and the handle names the row; the column clips it, leaving the \
         recognizable head of the id:\n{text}"
    );
    assert!(
        !text.contains("172.31.21.86"),
        "no undialable address is offered as though it were one:\n{text}"
    );
}

#[test]
fn a_box_whose_address_just_arrived_is_queued_for_onboarding_once() {
    // `:up 3` used to end with boxes nobody would ever provision: the address
    // arrives asynchronously, nothing noticed, and the operator was told to
    // `:prov` each one by hand. The marker `relink` writes is read here.
    let mut app = App::new("http://x");
    let mut newborn = named_machine("52.2.2.2", "box-1");
    newborn.set_state(MachineState::Initializing);
    newborn.note = format!(
        "EC2 i-0123456789abcdef0 (running) · {}",
        bm_core::provision::AWAITING_ONBOARD
    );
    let payload = serde_json::json!({ "machines": [newborn.clone()] });

    app.apply_state(payload.clone());
    assert_eq!(app.pending_onboard.len(), 1, "a new box is offered");
    assert_eq!(app.pending_onboard[0].addr, "52.2.2.2");

    // The dashboard records the hand-out before dispatching, so the ~800 ms poll
    // cannot queue a second provision for a box the first job has not reached
    // yet. Without this the same box would be pushed two or three times.
    app.onboarded.insert("52.2.2.2".into());
    app.apply_state(payload.clone());
    assert!(app.pending_onboard.is_empty(), "not queued twice");

    // The provision job clears the marker by rewriting the note — which is why
    // the trigger is a note and not a field: nothing has to remember to clear it.
    let mut taken = newborn.clone();
    taken.set_state(MachineState::Provisioning);
    taken.note = "provisioning (p) · EC2 i-0123456789abcdef0".into();
    app.apply_state(serde_json::json!({ "machines": [taken.clone()] }));
    assert!(app.pending_onboard.is_empty());
    assert!(
        !app.onboarded.contains("52.2.2.2"),
        "and the guard is released, so a future re-mark would be seen"
    );

    // A box that was already working carries no marker and is never offered —
    // the expensive mistake a marker written on *rotation* would cause.
    let mut working = named_machine("52.2.2.3", "box-2");
    working.set_state(MachineState::Configured);
    working.note = "EC2 i-0ffffffffffffffff (running)".into();
    app.apply_state(serde_json::json!({ "machines": [working] }));
    assert!(app.pending_onboard.is_empty());
}

#[test]
fn machines_pane_shows_every_state_word_whole() {
    // A state column that truncates the verdict it exists to show is worse than
    // one with slack: `initializing` rendered as `initializin`, and the first
    // two-word state would have hidden the noun that carried the meaning. This
    // is the test that fails when a state is added and the column is not widened
    // with it.
    for state in [
        MachineState::Unknown,
        MachineState::AwaitingIp,
        MachineState::Initializing,
        MachineState::Probing,
        MachineState::Configured,
        MachineState::Provisioning,
        MachineState::Online,
        MachineState::Offline,
        MachineState::Error,
    ] {
        let mut app = App::new("http://127.0.0.1:8901");
        let mut m = named_machine("52.2.2.2", "box-1");
        m.set_state(state);
        app.machines = vec![m];
        let text = render_text(&mut app, 140, 44);
        // The glyph is part of the cell, so this asserts the word is complete
        // *and* that the pane still renders it with its dot. The trailing space
        // is what makes it a completeness check rather than a prefix check.
        let needle = format!(" {} ", state.as_str());
        assert!(
            text.contains(&needle),
            "`{}` is clipped by the state column:\n{text}",
            state.as_str()
        );
    }
}

#[test]
fn machines_pane_names_the_kind_and_the_address() {
    // A row reads `box-1 · rmt · 192.168.2.2` — whose box, where it came from,
    // and how to reach it. The old `role` column said only "worker".
    let mut app = App::new("http://127.0.0.1:8901");
    let mut remote = named_machine("192.168.2.2", "box-1");
    remote.ssh_user = "thang".into();
    let mut aws = named_machine("52.2.2.2", "box-2");
    aws.note = "EC2 i-0123456789abcdef0 (running)".into();
    app.machines = vec![
        Machine::new("127.0.0.1", "local", 22, None, "both"),
        remote,
        aws,
    ];
    let text = render_text(&mut app, 140, 44);
    for head in ["machine", "kind", "ip"] {
        assert!(text.contains(head), "missing `{head}` column:\n{text}");
    }
    for cell in [
        "local",
        "rmt",
        "aws",
        "127.0.0.1",
        "192.168.2.2",
        "52.2.2.2",
    ] {
        assert!(text.contains(cell), "missing `{cell}`:\n{text}");
    }
    // Default policy reads at a glance, most-preferred first.
    assert!(text.contains("M>R>D>C"), "policy summary:\n{text}");
}

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
    // ledger has never heard of is not offered work it cannot report against.
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
    // overlay clipping its own grid or losing the key hints off the bottom.
    let text = render_text(&mut app, 100, 32);
    assert!(text.contains("digest manager"), "{text}");
    for n in ["7", "9", "11"] {
        assert!(text.contains(n), "ch{n} is listed:\n{text}");
    }
    assert!(text.contains("3 chapters"), "the count is stated:\n{text}");
    // The keys, not the prose: a hint that is reworded should not fail a test
    // about whether the screen *has* hints.
    for hint in ["Enter open", "f filter", "←→ chapter", "stop digest"] {
        assert!(
            hint_visible(&text, hint),
            "the {hint:?} hint is on screen:\n{text}"
        );
    }

    // `f` filters. It also has to keep the cursor *inside* the list it filters —
    // the cursor indexes the rows, so a filter that shrinks them can leave it
    // pointing past the end at nothing.
    handle_key(&mut app, press(KeyCode::Down), &http, &job_tx).await;
    handle_key(&mut app, press(KeyCode::Down), &http, &job_tx).await;
    handle_key(&mut app, press(KeyCode::Char('f')), &http, &job_tx).await;
    match &app.screen {
        Screen::Digest(v) => {
            assert!(v.hide_done, "the filter is on");
            // Asserted through `Layout::digested`, which is the question the
            // handler and the painter both ask. An earlier version of this test
            // invented its own predicate (`n == 7`) and then complained that the
            // cursor — correctly clamped against the *real* rows — was out of
            // range for the invented ones. The lesson is the reason
            // `Layout::digested` exists: three sites spelling out one question is
            // three chances to disagree.
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
    // prompt, which needs a chapter file and a bible. What this pins is the
    // *page* — that the round, the validator's words and the prompt's identity
    // are all on it, at every tier the layout supports.
    let mut app = App::new("http://127.0.0.1:8901");
    let mut v = super::screen::DigestView::new(vec![7, 9]);
    v.open = Some(super::screen::DigestChapter {
        n: 9,
        round: bm_core::digest::Round::Cast,
        prompt: "You are a Vietnamese web-novel dramaturg.".into(),
        cast: None,
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
    // *instruction* — the operator pastes it back into their model — so it is
    // deliberately shown in full, and a real one is a paragraph, not a phrase.
    // This is the same trap as the confirm dialog and the grid footer: the height
    // counts lines, the paragraph wraps, and the line that falls off the bottom is
    // whichever was drawn last.
    let mut app = App::new("http://127.0.0.1:8901");
    let mut v = super::screen::DigestView::new(vec![7]);
    v.open = Some(super::screen::DigestChapter {
        n: 7,
        round: bm_core::digest::Round::Script,
        prompt: "You are a Vietnamese web-novel dramaturg.".into(),
        cast: Some(serde_json::json!({"roster": ["Narrator"]})),
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
        // when the note overflows — and it is how the operator checks the right
        // prompt is on the clipboard before pasting anything.
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
    // selection disappears. The grid had no viewport at all, so the cursor walked
    // off the bottom of a 200-chapter book with nothing on screen to show where it
    // had got to.
    //
    // Chapters in the thousands so **absence is testable**: "1000" occurs in no
    // other number in this list, whereas "1" hides inside 100, 121, 200…
    let mut app = App::new("http://127.0.0.1:8901");
    let http = reqwest::Client::new();
    let (job_tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let press = |code| KeyEvent::new(code, KeyModifiers::NONE);
    app.screen = Screen::Digest(super::screen::DigestView::new((1000..=1200).collect()));

    // A short terminal, deliberately: on a tall one all seventeen rows fit and the
    // window would never have to move, so the test would pass against the bug.
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
    // cursor in both directions rather than only ever scrolling forward.
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
    // shows — the numbers are drawn `DIGEST_COLS` to a line. Stepping one chapter
    // on ↑ would move the highlight sideways.
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
    // nothing: the handler works on a *clone* of the view and writes it back after
    // the match, so the Esc arm clearing `open` had that undone one line later.
    // The list-level Esc was tested and passed — which is exactly why this went
    // unnoticed, since the bug only lived in the branch the test never entered.
    if let Screen::Digest(v) = &mut app.screen {
        v.open = Some(super::screen::DigestChapter {
            n: 7,
            round: bm_core::digest::Round::Cast,
            prompt: "a prompt".into(),
            cast: None,
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

    // `x` and `s` are the cluster-wide switch, on the screen it belongs to — the
    // same command the `:off`/`:on` words run, so there is one implementation and
    // two ways in.
    app.machines = vec![named_machine("192.168.2.2", "box-1")];
    app.screen = Screen::Digest(super::screen::DigestView::new(vec![1, 2]));
    handle_key(&mut app, press(KeyCode::Char('x')), &http, &job_tx).await;
    match job_rx.try_recv().expect("x dispatches").bare() {
        Job::DigestPolicy { restore, .. } => assert!(!restore, "x is off"),
        other => panic!("{other:?}"),
    }
    // `s` restores, which needs a snapshot; whether this machine has one is the
    // filesystem's business, not the test's — what the test holds is that the
    // screen survives the attempt, because the operator is still on it.
    handle_key(&mut app, press(KeyCode::Char('s')), &http, &job_tx).await;
    assert!(
        matches!(app.screen, Screen::Digest(_)),
        "the manager stays open through the switch"
    );
}

#[tokio::test]
async fn digest_off_snapshots_every_machine_and_on_refuses_without_a_snapshot() {
    // Two halves of one feature: `:off` must carry *every* machine's policy into
    // the job (it is the snapshot), and `:on` must refuse rather than guess when
    // there is nothing to restore.
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
    // to refuse *before* posting anything: an empty policy reads as the default
    // list, which is digest ON everywhere — the opposite of the ask.
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("digest-suspend.json");
    let err = super::jobs::digest_restore(&missing, "http://127.0.0.1:9", &http, &[])
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
fn the_bar_uses_partial_blocks_and_stays_exact_at_the_ends() {
    assert_eq!(bar(0.0, 10), "░".repeat(10));
    assert_eq!(bar(1.0, 10), "█".repeat(10));
    assert_eq!(bar(0.5, 10), "█████░░░░░");
    // A third of one cell in the last slot: the old bar could not show it.
    assert_eq!(bar(0.93, 10), "█████████▎");
    // Width is always exactly what was asked for.
    for frac in [0.0f32, 0.01, 0.05, 0.33, 0.5, 0.87, 0.99, 1.0] {
        for w in [1usize, 4, 10, 17] {
            assert_eq!(bar(frac, w).chars().count(), w, "bar({frac}, {w})");
        }
    }
}

// --- the hint audit: every hint a screen draws is a promise about its keys

#[test]
fn the_cast_overview_hints_name_only_keys_the_screen_handles() {
    // The cast rows used to advertise a bare `v` (a gated `:` command) and a
    // "Backspace clears it" that only popped one character. The hint must
    // name what the screen really binds: `:v`, and Backspace-widens/Ctrl-U-
    // clears, exactly like the task ledger spells it.
    let mut app = App::new("http://127.0.0.1:8901");
    app.roster = Some(roster_fixture());
    app.screen = Screen::Cast(CastView {
        filter: "zzz".into(),
        ..CastView::new()
    });
    let text = render_text(&mut app, 140, 44);
    assert!(
        text.contains("Backspace widens it, Ctrl-U clears"),
        "the empty state names the real editing keys:\n{text}"
    );
    assert!(
        !text.contains("Backspace clears it"),
        "the dead advice is gone:\n{text}"
    );
    app.screen = Screen::Cast(CastView::new());
    let text = render_text(&mut app, 140, 44);
    assert!(
        text.contains("— :v fills gaps"),
        "the gated command, with its colon:\n{text}"
    );
    assert!(
        !text.contains("— v fills gaps"),
        "no bare `v`, which types into the filter:\n{text}"
    );
}

#[test]
fn the_cast_and_picker_empty_states_point_at_the_gated_commands() {
    // `t` and `v` were removed from Normal mode; an empty speaker list that
    // said "run t or v first" sent the operator to a warning status.
    let mut app = App::new("http://127.0.0.1:8901");
    // A roster that loaded but knows no speakers: the state the empty-body
    // line is written for (a *missing* roster has its own line).
    let mut roster = roster_fixture();
    roster.characters.clear();
    roster.cast.clear();
    app.roster = Some(roster);
    app.screen = Screen::Cast(CastView::new());
    let text = render_text(&mut app, 140, 44);
    assert!(
        text.contains(":t (translate) or :v (voices) first"),
        "\n{text}"
    );
    app.screen = Screen::Pick(Picker::new());
    let text = render_text(&mut app, 140, 44);
    assert!(
        text.contains(":t (translate) or :v (voices) first"),
        "the picker says the same thing the same way:\n{text}"
    );
}

#[test]
fn the_cloud_error_names_the_real_command_words() {
    // There are no `aws login` / `aws discover` words — the setup commands
    // are `:login` and `:discover`, and the hint must send the operator to
    // the command line that actually has them.
    let mut app = App::new("http://127.0.0.1:8901");
    app.cloud_error = Some("no credentials".into());
    app.screen = Screen::Cloud(CloudView::new());
    let text = render_text(&mut app, 140, 44);
    assert!(
        text.contains("check :login / :discover, then r to retry"),
        "\n{text}"
    );
    assert!(!text.contains("aws login"), "{text}");
}

#[test]
fn screens_without_a_reload_key_do_not_advertise_one() {
    // Two overlays told the operator to press a key they do not handle:
    // the Run screen said "press R to retry" (only Enter/e/Esc are live
    // there) and the Machine screen said "P to configure" (only Esc/Enter/
    // q/i). Both now name the way back to a screen that has the key.
    let mut app = App::new("http://127.0.0.1:8901");
    app.conn = Conn::Down("boom".to_string());
    app.screen = Screen::Run;
    let text = render_text(&mut app, 100, 34);
    assert!(
        text.contains("Esc, then R on the dashboard"),
        "the run screen names its own way out:\n{text}"
    );

    let mut app = App::new("http://127.0.0.1:8901");
    let mut m = named_machine("192.168.2.2", "hawk");
    // Every stage off: the one state in which the "none enabled" line draws.
    m.task_policy = Some(vec![
        TaskPref {
            stage: Stage::Merge,
            enabled: false,
        },
        TaskPref {
            stage: Stage::Render,
            enabled: false,
        },
        TaskPref {
            stage: Stage::Digest,
            enabled: false,
        },
        TaskPref {
            stage: Stage::Crawl,
            enabled: false,
        },
    ]);
    app.machines = vec![m];
    app.screen = Screen::Machine("192.168.2.2".to_string());
    let text = render_text(&mut app, 140, 44);
    assert!(
        text.contains("Esc, then P on the dashboard"),
        "the machine screen says where P actually lives:\n{text}"
    );
}

#[test]
fn the_task_ledger_hints_the_movement_it_actually_binds() {
    // Letters type into the filter here, so `j`/`k` never moved anything —
    // and the hint said they did.
    let mut app = tasks_app();
    app.screen = Screen::Tasks(TasksView::new());
    let text = render_text(&mut app, 140, 44);
    assert!(
        text.contains("↑/↓ move"),
        "the movement the ledger really has:\n{text}"
    );
    assert!(
        !text.contains("j/k"),
        "the keys that type into the filter are not advertised:\n{text}"
    );
}
