use super::*;

#[test]
fn a_completed_activity_without_a_task_is_reported_as_idle() {
    let p = Progress {
        task_id: None,
        stage: Some("digest".into()),
        chapter: Some(7),
        frac: 1.0,
        activity: "digest ch7 done (12 segments)".into(),
        pending: None,
    };
    let who = WorkerIdentity {
        worker_id: "w1".into(),
        addr: "127.0.0.1".into(),
        hostname: "box".into(),
        alias: "owl".into(),
        // No root: no bundle, so the beat says "no opinion" about stages.
        root: PathBuf::new(),
    };
    let mut probe = LoadProbe::new();
    let beat = heartbeat_now(&p, &who, &mut probe, true, None);
    assert_eq!(beat.task_id, None);
    assert_eq!(beat.stage, None);
    assert_eq!(beat.chapter, None);
    assert_eq!(beat.progress, 0.0);
    assert_eq!(beat.activity, "idle");
}

#[test]
fn load_probe_reports_nothing_then_sane_values() {
    // First sample primes the CPU delta (a 0.0 would read as idle,
    let mut probe = LoadProbe::new();
    let (cpu, mem, gb, sidecars, sidecar_gb) = probe.sample();
    assert_eq!(
        (cpu, mem, gb),
        (None, None, None),
        "the first beat has no delta yet"
    );
    // A census, not a delta: the count is real on the very first beat.
    let sidecars = sidecars.expect("the count is always reported");
    assert!(sidecar_gb.expect("rss always measures") >= 0.0);
    assert!(
        sidecars == 0 || sidecar_gb.unwrap() > 0.0,
        "a live sidecar holds memory"
    );
    std::thread::sleep(std::time::Duration::from_millis(300));
    let (cpu, mem, gb, _, _) = probe.sample();
    let cpu = cpu.expect("second sample measures");
    let mem = mem.expect("memory always measures");
    let gb = gb.expect("memory always measures");
    assert!((0.0..=100.0).contains(&cpu), "cpu pct: {cpu}");
    assert!((0.0..=100.0).contains(&mem), "mem pct: {mem}");
    assert!(gb >= 0.0, "mem gib: {gb}");
}

#[test]
fn the_sidecar_census_asks_for_memory_and_never_for_tasks() {
    // The 8× regression, pinned where it cannot come back quietly. On
    let kind = census_refresh_kind();
    assert!(
        !kind.tasks(),
        "the census must not enumerate tasks: sysinfo's `Default` sets \
         `tasks: true`, and on Linux that is one entry per thread, each \
         reporting the whole process's RSS"
    );
    assert!(
        kind.memory(),
        "the census is a memory reading; without `with_memory()` every \
         process reports 0 bytes and the guard's primary trigger is dead"
    );
}

#[tokio::test]
async fn reaping_the_sidecar_reaches_one_we_did_not_spawn() {
    // The provision-started server is nobody's child, so `stop`, a signal
    use std::sync::atomic::{AtomicU32, Ordering};
    let hits = std::sync::Arc::new(AtomicU32::new(0));
    let (counter, app) = (hits.clone(), {
        let hits = hits.clone();
        axum::Router::new()
            // Answering 503 is what a server mid-load does, and it is
            .route(
                "/health",
                axum::routing::get(|| async { axum::http::StatusCode::SERVICE_UNAVAILABLE }),
            )
            .route(
                "/shutdown",
                axum::routing::post(move || {
                    let hits = hits.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        axum::Json(serde_json::json!({"ok": true}))
                    }
                }),
            )
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let mut s = Sidecar::new(&format!("http://{addr}"));
    assert!(!s.is_running(), "nothing here is our child");
    // Cut the wait short: a fake server cannot stop listening, and the
    let _ = tokio::time::timeout(std::time::Duration::from_millis(700), s.reap_all()).await;
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "a server this worker did not spawn must still be asked to exit"
    );
}

// -----------------------------------------------------------------

/// A box with 8 GiB and a sidecar at `rss_mib`, long enough lived that the
const BOX_8GIB: u64 = 8 * 1024 * 1024 * 1024;

/// The shipped thresholds, spelled out rather than resolved.
fn shipped() -> Budget {
    Budget::default()
}

#[test]
fn a_freshly_loaded_model_is_nowhere_near_the_budget() {
    // The floor has to be far below the cap or the guard is a reload loop:
    assert_eq!(
        budget_verdict(2_918 * 1_048_576, BOX_8GIB, 0, Some(10_000), &shipped()),
        None,
        "2.85 GB resident is the model doing its job, not a leak"
    );
}

#[test]
fn a_sidecar_that_has_grown_past_half_the_box_is_recycled() {
    // The leak this exists for: RSS climbing across a long run of
    // 8 GiB box, and the reason line has to name the numbers, "over
    // budget" with no figures is a guard an operator can only guess at.
    let why = budget_verdict(4 * 1024 * 1_048_576, BOX_8GIB, 0, Some(10_000), &shipped())
        .expect("at the budget is over the budget");
    assert!(why.contains("4096 MiB"), "{why}");
    assert!(
        why.contains("half this box's RAM"),
        "and says where the number came from: {why}"
    );

    // A bigger box gets a bigger budget, the point of a fraction. The same
    assert_eq!(
        budget_verdict(
            4 * 1024 * 1_048_576,
            32 * 1024 * 1024 * 1024,
            0,
            Some(10_000),
            &shipped()
        ),
        None,
        "the budget scales with the machine, so a big box is not reloading for nothing"
    );
}

#[test]
fn the_render_count_fires_without_any_memory_reading() {
    // The second trigger exists for exactly this case: a platform that
    let why = budget_verdict(0, 0, SIDECAR_MAX_RENDERS, Some(10_000), &shipped())
        .expect("the count cannot be unavailable");
    assert!(why.contains("200 renders"), "{why}");
    assert_eq!(
        budget_verdict(0, 0, SIDECAR_MAX_RENDERS - 1, Some(10_000), &shipped()),
        None,
        "one short of the budget is under it"
    );
}

#[test]
fn the_cooldown_bounds_what_a_wrong_budget_costs() {
    // The failure this prevents is worse than the leak: a model whose
    let over = 8 * 1024 * 1_048_576;
    assert_eq!(
        budget_verdict(over, BOX_8GIB, 0, Some(0), &shipped()),
        None,
        "just recycled — the next boundary must not recycle again"
    );
    assert_eq!(
        budget_verdict(
            over,
            BOX_8GIB,
            0,
            Some(SIDECAR_MIN_LIFETIME_SECS - 1),
            &shipped()
        ),
        None,
        "one second short of the floor"
    );
    assert!(
        budget_verdict(
            over,
            BOX_8GIB,
            0,
            Some(SIDECAR_MIN_LIFETIME_SECS),
            &shipped()
        )
        .is_some(),
        "and past the floor the guard acts"
    );
    // The cooldown must not hold a genuinely stuck count back for ever
    assert!(budget_verdict(0, 0, SIDECAR_MAX_RENDERS, None, &shipped()).is_some());
}

#[test]
fn a_per_box_override_replaces_the_fraction_and_says_so() {
    // Why the override exists: the fraction is a judgement, and the only
    let tuned = Budget {
        rss_cap_mib: Some(2048.0),
        ..Budget::default()
    };
    assert_eq!(tuned.cap_mib(BOX_8GIB), Some(2048.0));
    assert_eq!(
        tuned.cap_mib(32 * 1024 * 1024 * 1024),
        Some(2048.0),
        "absolute means absolute, on any box"
    );
    let why = budget_verdict(2048 * 1_048_576, BOX_8GIB, 0, Some(10_000), &tuned)
        .expect("2048 MiB is at the 2048 MiB cap");
    assert!(why.contains("BM_TTS_MAX_RSS_MB"), "names the knob: {why}");

    // The count and the cooldown move with the same override.
    let strict = Budget {
        max_renders: 5,
        min_lifetime_secs: 1,
        ..Budget::default()
    };
    assert!(budget_verdict(0, 0, 5, Some(1), &strict).is_some());
    assert_eq!(
        budget_verdict(0, 0, 4, Some(1), &strict),
        None,
        "four renders is under a budget of five"
    );

    // And the shipped default is untouched by any of that.
    assert_eq!(shipped().cap_mib(BOX_8GIB), Some(4096.0));
    assert_eq!(shipped().max_renders, SIDECAR_MAX_RENDERS);
    assert_eq!(shipped().min_lifetime_secs, SIDECAR_MIN_LIFETIME_SECS);
}

#[test]
fn the_startup_line_names_the_budget_that_will_be_applied() {
    // A log full of recycles says nothing unless it also says which budget
    let default = shipped().describe(BOX_8GIB);
    assert!(default.contains("4096 MiB"), "{default}");
    assert!(default.contains("50% of this box"), "{default}");
    assert!(default.contains("200 renders"), "{default}");
    assert!(default.contains("300s"), "{default}");

    let tuned = Budget {
        rss_cap_mib: Some(1536.0),
        ..Budget::default()
    }
    .describe(BOX_8GIB);
    assert!(tuned.contains("1536 MiB"), "{tuned}");
    assert!(tuned.contains("BM_TTS_MAX_RSS_MB"), "{tuned}");

    // A box that reports no memory has no denominator, so the line says the
    let unknown = shipped().describe(0);
    assert!(unknown.contains("no RSS cap"), "{unknown}");
}

#[test]
fn renders_are_counted_against_the_process_that_spoke_them() {
    // `served` describes a model, not a worker: a spawn or a reap resets it,
    let mut s = Sidecar::new("http://127.0.0.1:8818");
    assert_eq!(s.served, 0);
    s.note_renders(0);
    assert_eq!(s.served, 0, "an offer that spoke nothing is not work");
    assert!(s.serving_since.is_none(), "and does not start the clock");
    s.note_renders(10);
    assert_eq!(s.served, 10);
    assert!(
        s.serving_since.is_some(),
        "the clock starts at the first render"
    );
    s.note_renders(5);
    assert_eq!(s.served, 15, "a batch adds its takes, not one per offer");

    s.forget_work();
    assert_eq!((s.served, s.serving_since), (0, None));
    s.stop();
    assert_eq!(
        s.served, 0,
        "stop is a reap: the count goes with the process"
    );
}

#[tokio::test]
async fn a_sidecar_under_budget_is_left_alone() {
    // The other half of the guard: it must not recycle a healthy model.
    let mut s = Sidecar::new("http://127.0.0.1:1");
    s.note_renders(3);
    s.serving_since = Some(0);
    assert_eq!(s.over_budget(), None);
}

#[test]
fn segment_manifest_wire_format_round_trips() {
    // The inductor parses this JSON; the shape is the contract. Content
    let root = std::env::temp_dir().join(format!("bmseg{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let dir = root.join("data/audio/segments-vieneu-7");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("0000_Adam.wav"), b"RIFF-fake").unwrap();

    let text = serde_json::to_string(&segment_manifest(&root.join("data/audio"))).unwrap();
    let back: Vec<bm_core::segments::SegmentEntry> = serde_json::from_str(&text).unwrap();
    assert_eq!(back.len(), 1);
    assert_eq!(
        (back[0].chapter, back[0].name.as_str()),
        (7, "0000_Adam.wav")
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn render_action_pins_the_empty_offer_to_noop() {
    use bm_proto::RenderUnitSpec;
    // Zero units means "report ok/0 at once", never a fall-through into
    assert_eq!(render_action(None), RenderAction::Legacy);
    assert_eq!(render_action(Some(&[])), RenderAction::Noop);
    let one = vec![RenderUnitSpec {
        tag: "0000".into(),
        name: "t-0123456789abcdef.wav".into(),
        speaker: "A".into(),
        voice: "Adam".into(),
        text: "hi".into(),
        temperature: 0.8,
        silence_p: 0.15,
        take_key: "0123456789abcdef".into(),
        mp3_kbps: 0,
    }];
    assert_eq!(render_action(Some(&one)), RenderAction::Units);

    // There is no sweep to assert any more: a rendered chapter's segment
    assert_eq!(render_action(Some(&[])), RenderAction::Noop);
}

#[test]
fn pending_units_skips_what_this_box_already_holds() {
    // Why the offer carries every unit: the inductor cannot see this disk,
    use bm_proto::RenderUnitSpec;
    let root = std::env::temp_dir().join(format!("bmpend{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let spec = |name: &str| RenderUnitSpec {
        tag: "0000".into(),
        name: name.into(),
        speaker: "A".into(),
        voice: "Adam".into(),
        text: "hi".into(),
        temperature: 0.8,
        silence_p: 0.15,
        take_key: String::new(),
        mp3_kbps: 0,
    };
    // Held, held but truncated, absent.
    std::fs::write(root.join("0000_Adam.wav"), vec![0u8; 2000]).unwrap();
    std::fs::write(root.join("0001_Adam.wav"), vec![0u8; 500]).unwrap();
    let offered = vec![
        spec("0000_Adam.wav"),
        spec("0001_Adam.wav"),
        spec("0002_Adam.wav"),
    ];

    let todo: Vec<&str> = pending_units(&offered, &[], &root)
        .into_iter()
        .map(|u| u.name.as_str())
        .collect();
    assert_eq!(
        todo,
        vec!["0001_Adam.wav", "0002_Adam.wav"],
        "a truncated file is not done, and order is preserved"
    );

    // Everything present → nothing to speak. This is the case the inductor
    std::fs::write(root.join("0001_Adam.wav"), vec![0u8; 2000]).unwrap();
    std::fs::write(root.join("0002_Adam.wav"), vec![0u8; 2000]).unwrap();
    assert!(pending_units(&offered, &[], &root).is_empty());

    // Forced names render even when held: a same-named file with stale
    let todo: Vec<&str> = pending_units(&offered, &["0000_Adam.wav".to_string()], &root)
        .into_iter()
        .map(|u| u.name.as_str())
        .collect();
    assert_eq!(todo, vec!["0000_Adam.wav"]);
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_batched_offer_renders_every_take_it_carries() {
    // The worker half of batching, end to end. `render_offered_units` was
    // already written for a list, so the claim "no worker change was
    // needed" has to be *shown*, not asserted: a regression that spoke the
    use bm_proto::RenderUnitSpec;
    use std::sync::atomic::{AtomicU32, Ordering};
    let root = std::env::temp_dir().join(format!("bmbatch{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let layout = Layout::new(&root);
    layout.ensure().unwrap();

    let calls = std::sync::Arc::new(AtomicU32::new(0));
    let app = {
        let calls = calls.clone();
        axum::Router::new()
            .route("/health", axum::routing::get(|| async { "ok" }))
            .route(
                "/policy",
                axum::routing::get(|| async {
                    axum::Json(serde_json::json!({"allowed_voices": []}))
                }),
            )
            .route(
                "/infer",
                axum::routing::post(move || {
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        // `Tts::infer` refuses a body under 1000 bytes, so
                        vec![0u8; 4096]
                    }
                }),
            )
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    // Ten takes of one chapter, exactly what `Settings::render_batch`
    let units: Vec<RenderUnitSpec> = (0..10)
        .map(|i| RenderUnitSpec {
            tag: format!("000{i}"),
            name: format!("t-{i:016}.wav"),
            speaker: "A".into(),
            voice: "Adam".into(),
            text: format!("line {i}"),
            temperature: 0.8,
            silence_p: 0.15,
            take_key: format!("{i:016}"),
            mp3_kbps: 0,
        })
        .collect();

    let offer = TaskOffer {
        task_id: "render:7:0".into(),
        chapter: 7,
        stage: bm_proto::Stage::Render,
        root: root.display().to_string(),
        url: None,
        crawl: None,
        attempt: 1,
        tts_url: Some(format!("http://{addr}")),
        adapter: "vi-VN".into(),
        pack: "xianxia".into(),
        engine: "vieneu".into(),
        model_order: vec![],
        analyzer: "local".into(),
        analyzer_settings: bm_proto::AnalyzerSettings::default(),
        credentials: bm_proto::Credentials::default(),
        bible: None,
        script: None,
        cast: None,
        text: None,
        gap_ms: 300,
        speed: 1.0,
        ambience: false,
        music: false,
        effect_volume: 1.0,
        music_volume: 1.0,
        inject_volume: 1.0,
        render_units: Some(units.clone()),
        render_force: vec![],
        cast_hash: "cast".into(),
        merge_takes: vec![],
        local_node: false,
    };
    let shared: Shared = Arc::new(Mutex::new(Progress::default()));
    let mut sidecar = Sidecar::new(&format!("http://{addr}"));

    let res = run_offer(
        &layout,
        &Settings::default(),
        &offer,
        &shared,
        &mut sidecar,
        None,
        true,
        None,
    )
    .await
    .expect("a batch renders");
    assert!(res.ok);
    assert_eq!(res.units, 10, "ten takes spoken, ten reported");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        10,
        "ten requests to the sidecar"
    );
    assert_eq!(
        sidecar.served, 10,
        "and all ten counted against the model, which is what the guard reads"
    );
    // **Under the offer's adapter, not this box's own.** The root this
    let seg = layout
        .rebind(&offer.adapter, &offer.engine)
        .seg_dir(&offer.engine, 7);
    for u in &units {
        let p = seg.join(&u.name);
        let len = p.metadata().map(|m| m.len()).unwrap_or(0);
        assert!(len >= 1000, "{} landed ({len} bytes)", u.name);
    }

    // The second half of the contract: a take this box already holds is
    std::fs::remove_file(seg.join(&units[4].name)).unwrap();
    calls.store(0, Ordering::SeqCst);
    let res = run_offer(
        &layout,
        &Settings::default(),
        &offer,
        &shared,
        &mut sidecar,
        None,
        true,
        None,
    )
    .await
    .expect("the retry renders");
    assert_eq!(res.units, 1, "only the missing take is re-spoken");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(seg.join(&units[4].name).is_file(), "and it landed");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn shutdown_is_read_off_the_heartbeat_answer() {
    // New inductor, command set and clear.
    assert!(wants_shutdown(br#"{"ok":true,"shutdown":true}"#));
    assert!(!wants_shutdown(br#"{"ok":true,"shutdown":false}"#));
    // Old inductor: no `shutdown` key at all, the default keeps us
    assert!(!wants_shutdown(br#"{"ok": true}"#));
    // Garbage is ignored, never acted on.
    assert!(!wants_shutdown(b"not json"));
    assert!(!wants_shutdown(b""));
}

#[test]
fn default_worker_id_is_stable_per_root_not_per_process() {
    // The naming fix: restarts must keep their identity, or the
    let root = std::env::temp_dir().join(format!("bmid{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let first = default_worker_id(&root);
    assert!(first.starts_with(&format!("{}-", hostname_simple())));
    assert_eq!(
        default_worker_id(&root),
        first,
        "a restart keeps its id (the alias file persists it)"
    );
    assert!(root.join("worker.alias").is_file());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn worker_alias_is_drawn_once_then_kept() {
    let root = std::env::temp_dir().join(format!("bmalias{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let first = worker_alias_for(&root);
    assert!(
        ALIAS_POOL.contains(&first.as_str()),
        "drawn from the pool: {first}"
    );
    assert_eq!(worker_alias_for(&root), first, "a restart keeps its name");
    assert!(
        root.join("worker.alias").is_file(),
        "persisted in the worker root"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_sidecar_argv_points_at_this_box_s_own_tree() {
    // Was `sidecar_python_prefers_the_managed_venv`. The venv order is gone,
    let root = std::env::temp_dir().join(format!("bmtts{}", std::process::id()));
    let layout = Layout::new(&root);
    let engine = layout.engine_dir();
    let models = engine.join("models");
    let (bin, args) = layout.sidecar_command(8818, 0);

    assert_eq!(bin, engine.join("bm-tts"));
    assert_eq!(args[0], "--models");
    assert_eq!(args[1], models.display().to_string());

    let value_of = |flag: &str| -> Option<String> {
        args.windows(2).find(|w| w[0] == flag).map(|w| w[1].clone())
    };
    // `0` means "let the sidecar pick" (half the cores, capped at 8), so
    assert_eq!(value_of("--threads"), None);
    // The dictionary and the voice store live inside the model directory,
    assert_eq!(value_of("--codec"), Some(models.display().to_string()));
    assert_eq!(
        value_of("--dict"),
        Some(models.join("sea_g2p.bin").display().to_string())
    );
    assert_eq!(
        value_of("--voices"),
        Some(models.join("voices.json").display().to_string())
    );
    assert_eq!(value_of("--port"), Some("8818".into()));
    // Loopback: the agent is the only caller, and the port is not
    assert_eq!(value_of("--bind"), Some("127.0.0.1".into()));

    // A per-box override rides the argv the box asked for: `BM_TTS_THREADS`
    let (_, threaded) = layout.sidecar_command(8818, 8);
    assert!(threaded
        .windows(2)
        .any(|w| w[0] == "--threads" && w[1] == "8"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn offered_credentials_replace_the_workers_own_and_empty_ones_do_not() {
    // The outage this closes: provisioning never copies `.bm/`, so a
    std::env::remove_var("GEMINI_API_KEY");
    let names = install_credentials(&bm_proto::Credentials {
        gemini_api_key: "from-inductor".into(),
        openrouter_api_key: String::new(),
    });
    assert_eq!(
        names,
        vec!["GEMINI_API_KEY"],
        "the log gets names, never values"
    );
    assert_eq!(std::env::var("GEMINI_API_KEY").unwrap(), "from-inductor");

    // The inductor is the single source of truth: what it sends beats what
    std::env::set_var("GEMINI_API_KEY", "stale-local");
    install_credentials(&bm_proto::Credentials {
        gemini_api_key: "from-inductor".into(),
        openrouter_api_key: String::new(),
    });
    assert_eq!(std::env::var("GEMINI_API_KEY").unwrap(), "from-inductor");

    // An unset key is skipped, never blanked: an old inductor's empty
    install_credentials(&bm_proto::Credentials::default());
    assert_eq!(
        std::env::var("GEMINI_API_KEY").unwrap(),
        "from-inductor",
        "an absent value must not erase a present one"
    );
    std::env::remove_var("GEMINI_API_KEY");
}

/// A one-shot HTTP fixture on loopback: records each request body, answers
/// The repo's own rule for this shape (ROADMAP §1.4: "pin the request shape
/// against a local fixture server, no real API keys in tests"), and the
fn fixture_server(response_body: &'static str) -> (String, Arc<Mutex<Vec<String>>>) {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback bind");
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    std::thread::spawn(move || {
        // A fixed budget rather than `incoming()`: with the fix removed no
        for stream in listener.incoming().take(4) {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let mut len = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
                if line.trim().is_empty() {
                    break;
                }
            }
            let mut body = vec![0u8; len];
            let _ = reader.read_exact(&mut body);
            sink.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&body).into_owned());
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        }
    });
    (url, seen)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_digest_runs_on_the_inductors_analyzer_settings_not_the_boxes_own() {
    // The last mile of the analyzer-settings fix, and the only part a unit
    let (url, seen) = fixture_server(r#"{"message":{"content":"{}"}}"#);
    let dir = std::env::temp_dir().join(format!("bm-digest-settings-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let layout = Layout::new(&dir);
    std::fs::create_dir_all(layout.chapters()).unwrap();
    std::fs::create_dir_all(dir.join("prompts")).unwrap();
    std::fs::write(
        layout.prompt(),
        "bible={bible_json}\nchapter={chapter_text}\n",
    )
    .unwrap();
    std::fs::write(
        layout.script_prompt(),
        "bible={bible_json}\ncast={cast_json}\nmusic={music_palette}\neffects={effect_tags}\ninjects={inject_sounds}\nchapter={chapter_text}\n",
    )
    .unwrap();
    // The digest reads the music palette out of the scene map, it is the
    std::fs::create_dir_all(layout.assets()).unwrap();
    std::fs::write(
        layout.assets().join("scene-map.json"),
        r#"{"music_palette":{"quiet":{"tags":["soft"]},"none":{"tags":[]}}}"#,
    )
    .unwrap();

    // This box's own copy. On a real provisioned worker there is no
    let box_settings = Settings {
        ollama_url: "http://127.0.0.1:9".into(),
        local_model: "box-model".into(),
        ..Settings::default()
    };
    let offer = TaskOffer {
        task_id: "digest:1".into(),
        chapter: 1,
        stage: bm_proto::Stage::Digest,
        root: dir.display().to_string(),
        url: None,
        crawl: None,
        attempt: 1,
        tts_url: None,
        adapter: "vi-VN".into(),
        pack: "xianxia".into(),
        engine: "vieneu".into(),
        model_order: vec![],
        analyzer: "local".into(),
        analyzer_settings: bm_proto::AnalyzerSettings {
            ollama_url: url.clone(),
            local_model: "offer-model".into(),
            ..Default::default()
        },
        credentials: bm_proto::Credentials::default(),
        bible: None,
        script: None,
        cast: None,
        text: Some("Chương 1\n\nCó một người đi qua cầu.\n".into()),
        gap_ms: 300,
        speed: 1.0,
        ambience: false,
        music: false,
        effect_volume: 1.0,
        music_volume: 1.0,
        inject_volume: 1.0,
        render_units: None,
        render_force: vec![],
        cast_hash: String::new(),
        merge_takes: vec![],
        local_node: false,
    };
    let shared: Shared = Arc::new(Mutex::new(Progress::default()));
    let mut sidecar = Sidecar::new("http://127.0.0.1:8818");

    // The digest itself is *expected* to fail, the fixture answers `{}`,
    let _ = run_offer(
        &layout,
        &box_settings,
        &offer,
        &shared,
        &mut sidecar,
        None,
        true,
        None,
    )
    .await;

    let bodies = seen.lock().unwrap().clone();
    assert!(
        !bodies.is_empty(),
        "the offer's endpoint was never called — the digest ran on this \
         box's own settings"
    );
    assert!(
        bodies.iter().any(|b| b.contains("offer-model")),
        "the offered model must be the one requested: {bodies:?}"
    );
    assert!(
        !bodies.iter().any(|b| b.contains("box-model")),
        "this box's own model must not be used: {bodies:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
