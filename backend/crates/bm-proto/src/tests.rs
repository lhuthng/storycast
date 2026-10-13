use super::*;

/// "Nothing enabled" and "no policy stored" are different answers, and the
#[test]
fn a_nothing_policy_is_not_an_absent_one() {
    let mut m = Machine::new("10.0.0.5", "thang", 22, None, "worker");
    assert!(
        m.effective_task_policy().iter().all(|p| p.enabled),
        "no stored policy is the default, all four"
    );
    m.task_policy = Some(TaskPref::nothing());
    assert!(
        m.effective_task_policy().iter().all(|p| !p.enabled),
        "a stored nothing-list has to survive the fallback"
    );
    assert!(
        !TaskPref::nothing().is_empty(),
        "an empty list is read as 'no policy', which is the opposite"
    );
    // And a box from a file that predates the field keeps the default.
    let old: Machine = serde_json::from_str(
        r#"{"id":"10.0.0.5","addr":"10.0.0.5","name":"","ssh_user":"thang",
            "ssh_port":22,"ssh_key":null,"role":"worker","state":"unknown",
            "state_since":0,"last_seen":0,"capabilities":[],"note":""}"#,
    )
    .unwrap();
    assert!(old.task_policy.is_none());
    assert!(old.effective_task_policy().iter().all(|p| p.enabled));
}

#[test]
fn stage_roundtrips_through_strings() {
    for st in Stage::ALL {
        assert_eq!(Stage::parse(st.as_str()), Some(st));
    }
    assert_eq!(Stage::parse("nope"), None);
}

/// Every variant, so a new one cannot be added without joining the families
const ALL: [MachineState; 9] = [
    MachineState::Unknown,
    MachineState::AwaitingIp,
    MachineState::Initializing,
    MachineState::Probing,
    MachineState::Configured,
    MachineState::Provisioning,
    MachineState::Online,
    MachineState::Offline,
    MachineState::Error,
];

#[test]
fn every_machine_state_roundtrips_through_its_wire_string() {
    // `as_str` is what the TUI posts to `/api/machines/state` and what the
    for s in ALL {
        let wire = serde_json::to_string(&s).unwrap();
        assert_eq!(wire, format!("\"{}\"", s.as_str()), "serde vs as_str");
        assert_eq!(serde_json::from_str::<MachineState>(&wire).unwrap(), s);
        // One word: the pane's state column is fixed-width, and a word that
        assert!(
            !s.as_str().contains('_') && !s.as_str().contains(' '),
            "{s:?} is one word"
        );
    }
    // The match in `as_str` is exhaustive, so a new variant cannot compile
    let mut names: Vec<&str> = ALL.iter().map(|s| s.as_str()).collect();
    names.sort_unstable();
    let before = names.len();
    names.dedup();
    assert_eq!(names.len(), before, "two states share a wire name");
}

#[test]
fn only_online_accepts_work() {
    assert!(MachineState::Online.accepts_work());
    for s in [
        MachineState::Initializing,
        MachineState::Probing,
        MachineState::Configured,
        MachineState::Provisioning,
        MachineState::Offline,
        MachineState::Error,
    ] {
        assert!(!s.accepts_work(), "{s:?} must not be handed work");
    }
    // `Unknown` is "no opinion formed", not "ready", the offer gate
    assert!(!MachineState::Unknown.accepts_work());
}

#[test]
fn only_an_addressless_box_is_undialable() {
    // The gate on dialing is exact: every other state is *tried*, because a
    for s in ALL {
        assert_eq!(s.dialable(), s != MachineState::AwaitingIp, "{s:?}");
    }
    // And the addressless wait is a wait, not a verdict: it may not be
    assert!(MachineState::AwaitingIp.coming_up());
}

#[test]
fn coming_up_covers_every_state_that_is_not_a_verdict() {
    // The dispatcher stamps `Offline` on any box that fails to answer
    for s in [
        MachineState::AwaitingIp,
        MachineState::Initializing,
        MachineState::Probing,
        MachineState::Provisioning,
        MachineState::Configured,
    ] {
        assert!(s.coming_up(), "{s:?} is on its way up");
        assert!(!s.accepts_work(), "{s:?} is not ready for work");
    }
    // A verdict is not "coming up": silence about these is real news.
    for s in [
        MachineState::Online,
        MachineState::Offline,
        MachineState::Error,
        MachineState::Unknown,
    ] {
        assert!(!s.coming_up(), "{s:?} is a verdict, not a wait");
    }
    // `ALL` and the four lists above must partition it: every variant is
    for s in ALL {
        assert_eq!(
            s.coming_up(),
            !matches!(
                s,
                MachineState::Online
                    | MachineState::Offline
                    | MachineState::Error
                    | MachineState::Unknown
            ),
            "{s:?} is in neither family or both"
        );
    }
}

#[test]
fn set_state_stamps_only_real_transitions() {
    let mut m = Machine::new("10.0.0.5", "ubuntu", 22, None, "worker");
    assert_eq!(m.state, MachineState::Unknown);
    assert_eq!(m.state_since, 0, "a fresh record is never stamped");

    m.set_state(MachineState::Initializing);
    let born = m.state_since;
    assert!(born > 0, "a transition stamps the clock");

    // Re-stating the same state must not move the stamp, or "initializing
    // for 4 minutes" would reset on every poll and never reach a deadline.
    m.state_since = born.saturating_sub(60);
    let aged = m.state_since;
    m.set_state(MachineState::Initializing);
    assert_eq!(m.state_since, aged, "same state, same clock");

    // A real transition does move it.
    m.set_state(MachineState::Online);
    assert!(m.state_since >= aged);
}

#[test]
fn upstream_chain_is_a_prefix_of_all() {
    for st in Stage::ALL {
        let idx = Stage::ALL.iter().position(|s| *s == st).unwrap();
        assert_eq!(st.upstream(), &Stage::ALL[..idx]);
    }
}

#[test]
fn only_render_needs_tts() {
    assert!(Stage::Render.needs_tts());
    assert!(!Stage::Crawl.needs_tts());
    assert!(!Stage::Merge.needs_tts());
}

#[test]
fn task_ids_are_stable_and_unique_per_stage() {
    let a = Task::new(7, Stage::Render);
    let b = Task::new(7, Stage::Merge);
    assert_eq!(a.id(), "render:7");
    assert_ne!(a.id(), b.id());
}

#[test]
fn ssh_target_joins_user_and_addr() {
    let m = Machine::new("10.0.0.5", "pi", 22, None, "worker");
    assert_eq!(m.ssh_target(), "pi@10.0.0.5");
}

#[test]
fn ops_roundtrip_through_kebab_case() {
    for op in [
        Op::Translate,
        Op::CrawlSetup,
        Op::Import,
        Op::Voices,
        Op::SwapVoice,
        Op::PreviewVoice,
        Op::Segment,
        Op::Eta,
        Op::Requeue,
        Op::Retry,
        Op::RetryTask,
        Op::Release,
        Op::Reconcile,
        Op::Retag,
        Op::Recast,
        Op::FixSpeaker,
        Op::Merge,
        Op::Remix,
        Op::SoundChanged,
        Op::Rerender,
        Op::Remerge,
    ] {
        assert_eq!(Op::parse(op.as_str()), Some(op));
    }
    assert_eq!(Op::parse("nope"), None);
}

#[test]
fn audition_carries_text_in_and_audio_bytes_out() {
    // The audition path: literal text in, rendered wav out.
    let req: OpRequest =
        serde_json::from_str(r#"{"op":"preview-voice","voice":"Đức Trí","text":"Ừm!"}"#).unwrap();
    assert_eq!(req.text.as_deref(), Some("Ừm!"));
    assert_eq!(req.voice.as_deref(), Some("Đức Trí"));

    // Absent text is not an error: it means the sidecar's fixed sample, which
    let bare: OpRequest = serde_json::from_str(r#"{"op":"preview-voice","voice":"X"}"#).unwrap();
    assert_eq!(bare.text, None);

    // An op that rendered no audio still parses, which is what an *older
    let res: OpResult = serde_json::from_str(r#"{"ok":true,"message":"m"}"#).unwrap();
    assert_eq!(res.audio_b64, None);
    let with: OpResult =
        serde_json::from_str(r#"{"ok":true,"message":"m","audio_b64":"UklGRg=="}"#).unwrap();
    assert_eq!(with.audio_b64.as_deref(), Some("UklGRg=="));

    // The constructors are the single place `ok` and `audio_b64` are paired,
    assert!(OpResult::ok("m").ok && OpResult::ok("m").audio_b64.is_none());
    assert!(!OpResult::fail("m").ok);
    assert_eq!(
        OpResult::ok("m")
            .with_audio_b64("UklGRg==")
            .audio_b64
            .as_deref(),
        Some("UklGRg==")
    );
}

#[test]
fn offer_without_analyzer_means_no_active_provider() {
    // An old inductor never sent `analyzer`; its offers still parse, and
    let o: TaskOffer = serde_json::from_str(
        r#"{"task_id":"digest:1","chapter":1,"stage":"digest","root":"/r",
            "engine":"vieneu","gap_ms":300,"speed":1.25,"ambience":true}"#,
    )
    .unwrap();
    assert!(o.analyzer.is_empty());
}

#[test]
fn an_offer_without_music_means_no_music_layer() {
    // The rollout: an inductor that predates the music layer sends no
    let o: TaskOffer = serde_json::from_str(
        r#"{"task_id":"merge:1","chapter":1,"stage":"merge","root":"/r",
            "engine":"vieneu","gap_ms":300,"speed":1.25,"ambience":true}"#,
    )
    .unwrap();
    assert!(o.ambience, "the old field still means what it always did");
    assert!(!o.music);
    assert_eq!(o.effect_volume, 1.0);
    assert_eq!(o.music_volume, 1.0);
    assert_eq!(o.inject_volume, 1.0);
}

#[test]
fn remix_inject_volume_is_optional_and_roundtrips() {
    let mut req: OpRequest = serde_json::from_str(
        r#"{"op":"remix","speed":1.25,"effect_volume":0.5,"music_volume":0.0}"#,
    )
    .unwrap();
    assert_eq!(req.inject_volume, None);
    for volume in [None, Some(0.0), Some(0.25), Some(2.0)] {
        req.inject_volume = volume;
        let back: OpRequest = serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert_eq!(back.inject_volume, volume);
    }
}

#[test]
fn roster_deserialises_with_optional_flags_absent() {
    // The picker must survive a payload from an older inductor that never
    let r: Roster = serde_json::from_str(
        r#"{"engine":"vieneu","source":"offline","voices":[
             {"name":"Đức Trí","gender":"male","accent":"Central/South",
              "language":"vi-VN","style":"đọc truyện"}],
           "cast":{"Narrator":"Đức Trí"},"characters":["Narrator"],
           "policy_note":"Central/South only"}"#,
    )
    .unwrap();
    assert_eq!(r.voices[0].name, "Đức Trí");
    assert!(!r.voices[0].enrolled);
    // A payload from an inductor that predates `key` must still parse, and
    assert_eq!(r.voices[0].key, "");
    assert_eq!(r.cast["Narrator"], "Đức Trí");
}

#[test]
fn task_state_names_match_the_wire_form() {
    // The TUI filters and colours tasks by `as_str`; a divergence from
    for st in TaskState::ALL {
        let wire = serde_json::to_value(st).unwrap();
        assert_eq!(wire.as_str(), Some(st.as_str()), "{st:?}");
    }
    assert_eq!(TaskState::Shelved.as_str(), "shelved");
    assert_eq!(TaskState::Running.as_str(), "running");
}

#[test]
fn retry_task_requests_carry_stage_chapter_and_force() {
    let req: OpRequest =
        serde_json::from_str(r#"{"op":"retry-task","stage":"digest","chapter":7,"force":true}"#)
            .unwrap();
    assert_eq!(req.op, Op::RetryTask);
    assert_eq!(req.op.as_str(), "retry-task");
    assert_eq!(req.stage, Some(Stage::Digest));
    assert_eq!(req.chapter, Some(7));
    assert_eq!(req.force, Some(true));
    // Everything else stays None: a retry must not smuggle a voice or range.
    assert!(req.start.is_none() && req.voice.is_none() && req.character.is_none());

    // An old caller that knows nothing of the new fields still parses, and
    let bare: OpRequest = serde_json::from_str(r#"{"op":"retry-task"}"#).unwrap();
    assert_eq!(bare.stage, None);
    assert_eq!(bare.chapter, None);
    assert_eq!(bare.force, None);

    // `retry` with a chapter is the narrow form of the blanket retry.
    let narrow: OpRequest =
        serde_json::from_str(r#"{"op":"retry","stage":"render","chapter":3}"#).unwrap();
    assert_eq!(narrow.op, Op::Retry);
    assert_eq!(narrow.stage, Some(Stage::Render));
    assert_eq!(narrow.chapter, Some(3));
    assert_eq!(narrow.force, None, "absent force means plain retry");
}

fn both_keys() -> Credentials {
    Credentials {
        gemini_api_key: "g-key".into(),
        openrouter_api_key: "o-key".into(),
    }
}

#[test]
fn credentials_travel_only_to_the_stage_that_reads_them() {
    // The digest lane takes its slot's key, and only that one. The slot
    // (`gemini` | `openai`) is resolved by the inductor from the entry's
    // `kind` — no provider id reaches this function.
    assert_eq!(
        both_keys().for_stage(Stage::Digest, "gemini", "vieneu"),
        Credentials {
            gemini_api_key: "g-key".into(),
            openrouter_api_key: String::new(),
        }
    );
    assert_eq!(
        both_keys().for_stage(Stage::Digest, "openai", "vieneu"),
        Credentials {
            gemini_api_key: String::new(),
            openrouter_api_key: "o-key".into(),
        }
    );
    // The ollama slot reads no key, so it gets none — like an empty
    // backend from an inductor with nothing active.
    for backend in ["", "ollama"] {
        assert!(
            both_keys()
                .for_stage(Stage::Digest, backend, "vieneu")
                .is_empty(),
            "{backend} reads no key"
        );
    }
    // A gemini TTS render hands the key to the sidecar the worker spawns.
    assert_eq!(
        both_keys()
            .for_stage(Stage::Render, "gemini", "gemini")
            .gemini_api_key,
        "g-key"
    );
    assert!(both_keys()
        .for_stage(Stage::Render, "gemini", "vieneu")
        .is_empty());
    // Crawl and merge touch no provider at all, a crawl offer that
    // carried a key would be shipping a secret to a box that fetches a URL.
    for stage in [Stage::Crawl, Stage::Merge] {
        assert!(
            both_keys().for_stage(stage, "gemini", "gemini").is_empty(),
            "{stage} reads no key"
        );
    }
}

#[test]
fn credential_pairs_name_the_variables_the_backends_read() {
    // These two strings are the contract with `bm-core/src/digest/llm.rs`
    // and `python/tts_router.py`: they read the environment by exactly
    // these names, so a rename here would install nothing.
    assert_eq!(
        both_keys().pairs(),
        vec![("GEMINI_API_KEY", "g-key"), ("OPENROUTER_API_KEY", "o-key")]
    );
    assert_eq!(
        both_keys().names(),
        vec!["GEMINI_API_KEY", "OPENROUTER_API_KEY"]
    );
    // An unset key is absent, never an empty assignment: the worker must
    // leave a box's own environment alone.
    let half = Credentials {
        gemini_api_key: String::new(),
        openrouter_api_key: "o-key".into(),
    };
    assert_eq!(half.pairs(), vec![("OPENROUTER_API_KEY", "o-key")]);
    assert!(!half.is_empty());
    assert!(Credentials::default().pairs().is_empty());
}

#[test]
fn a_batched_render_offer_survives_the_wire_intact() {
    // The whole change rests on one asymmetry: the **units** travel on the
    // wire, the **grouping** does not.
    //
    // `render_units` was already a `Vec`, which is what makes a batched
    // offer parse on a worker that predates batching, so the two sides can
    // be upgraded independently. The grouping is recorded on the ledger row
    // (`Task::batch`) and never serialised into an offer, so a worker can
    // neither see nor depend on a scheduling decision it has no business
    // knowing about.
    //
    // Both halves are easy to undo by accident, a `render_units` that
    // became a single struct would break the rollout, and a `batch` threaded
    // onto the offer would silently make the worker's behaviour depend on
    // the inductor's batch size, so both are pinned here.
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
            mp3_kbps: 192,
        })
        .collect();
    let offer = TaskOffer {
        task_id: "render:7:0".into(),
        chapter: 7,
        stage: Stage::Render,
        root: "/r".into(),
        url: None,
        crawl: None,
        attempt: 1,
        tts_url: Some("http://127.0.0.1:8818".into()),
        adapter: "vi-VN".into(),
        pack: "xianxia".into(),
        engine: "vieneu".into(),
        model_order: vec![],
        analyzer: "gemini".into(),
        analyzer_settings: AnalyzerSettings::default(),
        credentials: Credentials::default(),
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
        cast_hash: "abc123".into(),
        merge_takes: vec![],
        local_node: false,
    };

    let json = serde_json::to_string(&offer).unwrap();
    let back: TaskOffer = serde_json::from_str(&json).unwrap();
    let got = back.render_units.as_deref().expect("planned, not legacy");
    assert_eq!(got.len(), 10, "every take the offer carried");
    assert_eq!(
        got.iter().map(|u| u.name.clone()).collect::<Vec<_>>(),
        units.iter().map(|u| u.name.clone()).collect::<Vec<_>>(),
        "in order — a render speaks its chapter front to back"
    );
    assert_eq!(
        got[9].take_key, units[9].take_key,
        "with each take's own key"
    );
    assert_eq!(got[9].voice, "Adam");
    assert_eq!(back.cast_hash, "abc123", "and the chapter's cast hash");

    let as_value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(
        as_value.get("batch").is_none(),
        "the grouping is the ledger's, not the wire's: {json}"
    );

    // An offer that names no units is `Some([])`, not absent, the
    // distinction the worker reads as "report ok with zero units" against
    // "an old inductor, plan it yourself". A missing field must stay the
    // second of those.
    let old: TaskOffer =
        serde_json::from_str(&json.replace("\"render_units\"", "\"not_render_units\"")).unwrap();
    assert!(
        old.render_units.is_none(),
        "absent means an old inductor, and must not read as an empty chapter"
    );

    // The ledger side: the grouping is a *row's*, and round-trips there.
    let mut row = Task::new_take(7, 0);
    row.batch = vec!["render:7:1".into(), "render:7:2".into()];
    let row_back: Task = serde_json::from_str(&serde_json::to_string(&row).unwrap()).unwrap();
    assert_eq!(row_back.batch, row.batch);
    assert_eq!(row_back.take, Some(0));
    // And a row written before the field existed reads as "no grouping",
    let old_row: Task = serde_json::from_str(
        r#"{"chapter":7,"stage":"render","state":"done","attempts":0,
            "assigned_to":null,"lease_until":null,"detail":"","updated":1,"take":3}"#,
    )
    .unwrap();
    assert!(old_row.batch.is_empty(), "absent means no grouping");
    assert_eq!(old_row.take, Some(3));
}

#[test]
fn debug_never_prints_a_key() {
    // `TaskOffer` is `Debug` and every offer is a candidate for a log
    let shown = format!("{:?}", both_keys());
    assert!(!shown.contains("g-key"), "{shown}");
    assert!(!shown.contains("o-key"), "{shown}");
    assert!(shown.contains("set"), "{shown}");
    assert!(format!("{:?}", Credentials::default()).contains("unset"));
    // And through the struct that actually gets printed.
    let offer = TaskOffer {
        task_id: "digest:1".into(),
        chapter: 1,
        stage: Stage::Digest,
        root: "/r".into(),
        url: None,
        crawl: None,
        attempt: 1,
        tts_url: None,
        adapter: "vi-VN".into(),
        pack: "xianxia".into(),
        engine: "vieneu".into(),
        model_order: vec![],
        analyzer: "gemini".into(),
        analyzer_settings: AnalyzerSettings::default(),
        credentials: both_keys(),
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
        render_units: None,
        render_force: vec![],
        cast_hash: String::new(),
        merge_takes: vec![],
        local_node: false,
    };
    assert!(!format!("{offer:?}").contains("g-key"));
}

#[test]
fn an_old_inductors_offer_carries_no_credentials() {
    // The staged rollout: an inductor that predates this field sends none,
    let o: TaskOffer = serde_json::from_str(
        r#"{"task_id":"digest:1","chapter":1,"stage":"digest","root":"/r",
            "engine":"vieneu","gap_ms":300,"speed":1.25,"ambience":true}"#,
    )
    .unwrap();
    assert!(o.credentials.is_empty());
    assert!(o.credentials.pairs().is_empty());
    // And it carries no analyzer configuration either, the worker's own
    assert!(o.analyzer_settings.analyze_models.is_none());
    // And an old *worker* ignores the fields entirely, the serializer
    let round: TaskOffer = serde_json::from_str(&serde_json::to_string(&o).unwrap()).unwrap();
    assert_eq!(round.credentials, o.credentials);
    assert_eq!(round.analyzer_settings, o.analyzer_settings);
}

#[test]
fn an_empty_chain_is_not_the_same_as_saying_nothing() {
    let stated: AnalyzerSettings = serde_json::from_str(r#"{"analyze_models":[]}"#).unwrap();
    assert_eq!(stated.analyze_models, Some(vec![]));
    let silent: AnalyzerSettings = serde_json::from_str(r#"{}"#).unwrap();
    assert_eq!(silent.analyze_models, None);
    // And both survive a round trip, which is what the worker sees.
    for block in [stated, silent] {
        let back: AnalyzerSettings =
            serde_json::from_str(&serde_json::to_string(&block).unwrap()).unwrap();
        assert_eq!(back, block);
    }
}

#[test]
fn an_old_inductors_heartbeat_answer_means_stay() {
    // The shutdown latch rides the heartbeat answer. An inductor that
    let old: HeartbeatAck = serde_json::from_str(r#"{"ok": true}"#).unwrap();
    assert!(old.ok && !old.shutdown);
    let told: HeartbeatAck = serde_json::from_str(r#"{"ok":true,"shutdown":true}"#).unwrap();
    assert!(told.shutdown);
    assert_eq!(Op::parse("shutdown-workers"), Some(Op::ShutdownWorkers));
    assert_eq!(Op::ShutdownWorkers.as_str(), "shutdown-workers");
}
