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
    assert_eq!(
        command_key("up"),
        Some(Command::Up {
            provider: None,
            count: 1,
            ttl_minutes: None,
            strict: false,
            from_machine: false,
        })
    );
    assert_eq!(
        command_key("up 3"),
        Some(Command::Up {
            provider: None,
            count: 3,
            ttl_minutes: None,
            strict: false,
            from_machine: false,
        })
    );
    assert_eq!(
        command_key("launch"),
        Some(Command::Up {
            provider: None,
            count: 1,
            ttl_minutes: None,
            strict: false,
            from_machine: false,
        })
    );
    // A bad count is refused, not defaulted to one.
    assert_eq!(command_key("up 0"), None);
    assert_eq!(command_key("up x"), None);
    assert_eq!(command_key("pool"), Some(Command::AwsPool));
    assert_eq!(command_key("aws"), Some(Command::AwsPool));
    assert_eq!(command_key("cloud"), Some(Command::AwsPool));
    // One word after `:cloud` retries the deletes the cloud still owes.
    assert_eq!(command_key("cloud reap"), Some(Command::CloudReap));
    assert_eq!(command_key("CLOUD Reap"), Some(Command::CloudReap));
    assert_eq!(command_key("cloud reap all"), None, "one word, not two");
    assert_eq!(
        command_key("down"),
        Some(Command::Down {
            provider: None,
            force: false
        })
    );
    assert_eq!(
        command_key("down force"),
        Some(Command::Down {
            provider: None,
            force: true
        })
    );
    assert_eq!(
        command_key("terminate"),
        Some(Command::Down {
            provider: None,
            force: false
        })
    );
    assert_eq!(command_key("l"), Some(Command::AwsPool));
    assert_eq!(
        command_key("w"),
        Some(Command::Up {
            provider: None,
            count: 1,
            ttl_minutes: None,
            strict: false,
            from_machine: false,
        })
    );
    assert_eq!(
        command_key("o"),
        Some(Command::Down {
            provider: None,
            force: false
        })
    );
}

#[test]
fn up_takes_a_provider_a_ttl_and_strictness() {
    use bm_core::cloud::Provider;
    let up = |provider, count, ttl_minutes, strict| {
        Some(Command::Up {
            provider,
            count,
            ttl_minutes,
            strict,
            from_machine: false,
        })
    };
    assert_eq!(
        command_key("up aws 3"),
        up(Some(Provider::Aws), 3, None, false)
    );
    // `--from-machine` is the packs gate's escape hatch.
    assert_eq!(
        command_key("up aws 3 --from-machine"),
        Some(Command::Up {
            provider: Some(Provider::Aws),
            count: 3,
            ttl_minutes: None,
            strict: false,
            from_machine: true,
        })
    );
    assert_eq!(
        command_key("up instabox 1 --ttl 1h"),
        up(Some(Provider::InstaBox), 1, Some(60), false)
    );
    assert_eq!(
        command_key("up aws 4 --ttl 0"),
        up(Some(Provider::Aws), 4, Some(0), false),
        "zero is a deadline the operator chose: none"
    );
    assert_eq!(
        command_key("up 2 --ttl 90m --strict"),
        up(None, 2, Some(90), true)
    );
    assert_eq!(command_key("up --ttl=7h 2"), up(None, 2, Some(420), false));
    // Refused rather than guessed: two providers, two counts, a bad ttl.
    assert_eq!(command_key("up aws instabox 2"), None);
    assert_eq!(command_key("up 2 3"), None);
    assert_eq!(command_key("up --ttl soon"), None);
    assert_eq!(command_key("up --ttl"), None);
}

#[test]
fn down_takes_a_provider_and_force() {
    use bm_core::cloud::Provider;
    assert_eq!(
        command_key("down aws force"),
        Some(Command::Down {
            provider: Some(Provider::Aws),
            force: true
        })
    );
    assert_eq!(
        command_key("down instabox"),
        Some(Command::Down {
            provider: Some(Provider::InstaBox),
            force: false
        })
    );
    assert_eq!(command_key("down aws instabox"), None);
    assert_eq!(command_key("down nonsense"), None);
}

#[test]
fn login_and_discover_are_words_with_no_key_of_their_own() {
    // Setup verbs: reachable from the `:` line, never a bare keypress. They are
    assert_eq!(command_key("login"), Some(Command::AwsLogin));
    assert_eq!(command_key("LOGIN"), Some(Command::AwsLogin));
    assert_eq!(command_key("discover"), Some(Command::AwsDiscover));
    assert_eq!(command_key("ibx"), Some(Command::IbxLogin));
    assert_eq!(command_key("INSTABOX"), Some(Command::IbxLogin));
    for name in ["login", "discover", "ibx"] {
        let w = WORDS
            .iter()
            .find(|w| w.names[0] == name)
            .unwrap_or_else(|| panic!("{name} is not in the word table"));
        assert!(w.key.is_none(), "{name} must not fire from a bare key");
        assert!(w.desc.is_some(), "{name} needs a `:help` line");
    }
}

#[test]
fn the_ibx_word_takes_the_key_alone_and_refuses_a_pasted_pair() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = App::new("http://x");
    app.layout.root = dir.path().to_path_buf();
    let prompt = |buf: &str| TextPrompt::new(TextKind::IbxLogin, "t", "h", buf);

    // A line with a space is a mistyped paste, caught before the API is asked.
    let err = submit_text(&mut app, &prompt("ibk_abc def")).unwrap_err();
    assert!(err.contains("no spaces"), "{err}");
    match submit_text(&mut app, &prompt("ibk_0123456789abcdef")).unwrap() {
        Job::IbxLogin { key, .. } => assert_eq!(key, "ibk_0123456789abcdef"),
        other => panic!("{other:?}"),
    }
    // Empty is the deliberate clear, and still a job: clearing is what turns
    // the provider off, so it writes the definition rather than skipping it.
    match submit_text(&mut app, &prompt("  ")).unwrap() {
        Job::IbxLogin { key, .. } => assert!(key.is_empty()),
        other => panic!("{other:?}"),
    }

    // Neither one may be echoed into the status line after submit.
    assert!(TextKind::IbxLogin.holds_a_secret());
    assert!(TextKind::LlmKey("p".into()).holds_a_secret());
    assert!(
        !TextKind::AwsLogin.holds_a_secret(),
        "the AWS prompt carries a csv path, not a secret"
    );
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

#[test]
fn the_boxes_a_terminate_covers_are_the_ones_whose_note_names_the_instance() {
    let row = |addr: &str, note: &str| {
        let mut m = Machine::new(addr, "thang", 22, None, "worker");
        m.note = note.into();
        m
    };
    let machines = vec![
        row("10.0.0.1", "launched · EC2 i-0aaa1111aaaa1111a (running)"),
        row("10.0.0.2", "provisioned · EC2 i-0bbb2222bbbb2222b alive"),
        // Never EC2-born: nothing ties it to an instance, so a terminate
        // must leave it alone.
        row("10.0.0.3", "added by hand"),
        row("10.0.0.4", "EC2 i-0ccc3333cccc3333c (running)"),
    ];
    assert_eq!(
        instance_machines(
            &machines,
            &[
                "i-0aaa1111aaaa1111a".to_string(),
                "i-0ccc3333cccc3333c".to_string(),
            ],
        ),
        vec!["10.0.0.1".to_string(), "10.0.0.4".to_string()],
        "the two boxes those instances were, and no others"
    );
    assert!(instance_machines(&machines, &["i-0zzz4444zzzz4444z".to_string()]).is_empty());
}

#[test]
fn a_dropped_machine_leaves_the_panes_at_once() {
    let mut app = App::new("http://127.0.0.1:8901");
    app.machines = vec![
        Machine::new("10.0.0.1", "thang", 22, None, "worker"),
        Machine::new("10.0.0.2", "thang", 22, None, "worker"),
    ];
    app.beats = vec![beat("w1", "10.0.0.1", 0, "box")];
    app.selected = 1;
    app.apply(Ev::MachineDropped {
        addr: "10.0.0.1".into(),
    });
    assert_eq!(app.machines.len(), 1, "the row is gone");
    assert_eq!(app.machines[0].addr, "10.0.0.2");
    assert!(app.beats.is_empty(), "its worker row goes with it");
    assert_eq!(app.selected, 0, "the cursor still points at a row");
}

/// A row this workspace launched on a cloud that has no pool listing: the
/// registry's own provider id is what `:down` deletes by.
#[test]
fn a_box_launched_on_another_provider_is_matched_by_its_provider_id() {
    let mut ibx = Machine::new("https://m-1.instabox.sh", "instabox", 22, None, "worker");
    ibx.provider = Some("instabox".into());
    ibx.provider_id = Some("m-1".into());
    let mut aws = Machine::new("10.0.0.1", "ubuntu", 22, None, "worker");
    aws.note = "EC2 i-0aaa1111aaaa1111a (running)".into();
    let machines = vec![ibx, aws];

    assert_eq!(
        instance_machines(&machines, &["m-1".to_string()]),
        vec!["https://m-1.instabox.sh".to_string()],
        "the provider id names the box, whatever its note says"
    );
    assert_eq!(
        instance_machines(&machines, &["i-0aaa1111aaaa1111a".to_string()]),
        vec!["10.0.0.1".to_string()],
        "and an EC2 box still reads by the id in its note"
    );
}

#[tokio::test]
async fn down_on_another_provider_asks_about_what_this_workspace_launched() {
    let mut app = App::new("http://127.0.0.1:8901");
    let mut ibx = Machine::new("https://m-1.instabox.sh", "instabox", 22, None, "worker");
    ibx.provider = Some("instabox".into());
    ibx.provider_id = Some("m-1".into());
    let mut hand = Machine::new("192.168.2.9", "thang", 22, None, "worker");
    hand.provider = Some("instabox".into());
    app.machines = vec![ibx, hand];
    let http = reqwest::Client::new();
    let (job_tx, _job_rx) = tokio::sync::mpsc::unbounded_channel();

    do_command(
        &mut app,
        Command::Down {
            provider: Some(bm_core::cloud::Provider::InstaBox),
            force: false,
        },
        &http,
        &job_tx,
    );
    match &app.screen {
        Screen::Confirm(c) => match &c.action {
            ConfirmAction::AwsDown { provider, ids } => {
                assert_eq!(*provider, bm_core::cloud::Provider::InstaBox);
                assert_eq!(ids, &vec!["m-1".to_string()], "only the launched box");
            }
            other => panic!("{other:?}"),
        },
        other => panic!("no confirm for a provider teardown: {other:?}"),
    }
}
