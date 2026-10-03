use super::*;

// --- the EC2 half -------------------------------------------------------

#[test]
fn command_keys_are_unique_and_operators_stay_off_the_keyboard() {
    // There was no key-uniqueness test at all, so a new binding could quietly
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
    let dir = tempfile::tempdir().unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Ev>();
    super::super::jobs::job_aws_pool(
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
    // Two claims batching could have broken, one held, one did not.
    let now = bm_proto::now_secs();
    let beat: Heartbeat = serde_json::from_value(serde_json::json!({
        "worker_id": "w1", "addr": "172.31.1.5", "stage": "render",
        "progress": 0.4, "activity": "render ch42", "ts": now
    }))
    .unwrap();
    // One chapter's batch, all on one box. The head names the group; the
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
