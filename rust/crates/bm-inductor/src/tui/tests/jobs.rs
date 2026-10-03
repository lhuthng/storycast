use super::*;

#[tokio::test]
async fn tracked_jobs_queue_only_behind_a_resource_they_need() {
    use super::super::jobs::run_jobs_with;
    use super::input::dispatch;
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
    // Both name `Res::Cluster`, so the stop still waits for the start, the
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
    use super::super::jobs::run_jobs_with;
    use super::input::dispatch;
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
    super::super::jobs::run_jobs_with(job_rx, tx, |job, _tx| async move {
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
    super::super::jobs::job_start_backend(
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
