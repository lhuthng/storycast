use super::*;

#[test]
fn live_log_streams_a_copy_and_keeps_the_lines() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut log = LiveLog::new(Some(tx));
    log.push("first".into());
    log.push("second".into());
    assert_eq!(log.lines, vec!["first", "second"]);
    assert_eq!(rx.try_recv().unwrap(), "first");
    assert_eq!(rx.try_recv().unwrap(), "second");
    // Collect-only mode: no sender, no panic, lines still kept.
    let mut quiet = LiveLog::new(None);
    quiet.push("only".into());
    assert_eq!(quiet.lines, vec!["only"]);
}

#[test]
fn configured_requires_a_matching_agent_and_the_tts_sidecar() {
    let mut p = Probe {
        reachable: true,
        agent_version: Some("0.2.0".into()),
        tts_bin_present: true,
        models_present: true,
        ..Default::default()
    };
    assert!(p.configured("0.2.0"));
    assert!(
        !p.configured("0.3.0"),
        "stale agent must trigger a redeploy"
    );
    p.reachable = false;
    assert!(!p.configured("0.2.0"));

    // A Python virtualenv is no longer a reason to call a box ready. This
    p.reachable = true;
    p.tts_bin_present = false;
    p.models_present = false;
    p.python_present = true;
    assert!(!p.configured("0.2.0"), "a venv cannot render");
}

#[test]
fn a_new_voice_forces_a_model_store_push_on_an_already_configured_box() {
    let local = ProvisionStamp {
        tts_hash: "weights-v1".into(),
        voices_hash: "store-v1".into(),
        ..Default::default()
    };
    let same = local.clone();
    assert!(!models_need_push(Some(&same), &local, false));
    assert!(models_need_push(None, &local, false));
    assert!(models_need_push(Some(&same), &local, true));

    // A re-bake: the weights moved, the store did not.
    let rebaked = ProvisionStamp {
        tts_hash: "weights-v2".into(),
        ..local.clone()
    };
    assert!(
        models_need_push(Some(&rebaked), &local, false),
        "a re-bake must resync"
    );

    // An enrollment: the store moved, the weights did not. This is the case
    let enrolled = ProvisionStamp {
        voices_hash: "store-v2".into(),
        ..local.clone()
    };
    assert!(
        models_need_push(Some(&enrolled), &local, false),
        "an enrollment must reach the box even though the weights are unchanged"
    );
}

/// The verify list is the weights, and never the one mutable file among them.
#[test]
fn the_checksum_list_covers_the_weights_and_not_the_voice_store() {
    let dir = std::env::temp_dir().join("bm-model-checksums");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("manifest.json"),
        r#"{"files":{"sea_g2p.bin":{"bytes":1,"sha256":"aa11"},"voices.json":{"bytes":2,"sha256":"bb22"},"config.json":{"bytes":3,"sha256":"cc33"}}}"#,
    )
    .unwrap();
    assert_eq!(
        model_checksums(&dir),
        vec![
            "aa11  sea_g2p.bin".to_string(),
            "cc33  config.json".to_string()
        ],
        "sorted, both weights kept, and the store dropped"
    );

    // Absent or unparseable means "nothing to verify", not a failure:
    std::fs::remove_file(dir.join("manifest.json")).unwrap();
    assert!(model_checksums(&dir).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_matching_stamp_cannot_hide_a_missing_voice() {
    let manifest = [("Narrator".to_string(), "refs/narrator.wav".to_string())]
        .into_iter()
        .collect();
    assert!(voice_store_covers(
        &["Narrator".into(), "Đức Trí".into()],
        &manifest
    ));
    assert!(!voice_store_covers(&["Đức Trí".into()], &manifest));
}

#[test]
fn only_a_fresh_or_forced_provision_may_install() {
    // The decision the two `ensure_*` call sites read. A configured box
    assert!(may_install(false, false), "a fresh box installs");
    assert!(may_install(false, true), "force on a fresh box installs");
    assert!(may_install(true, true), "force re-installs on purpose");
    assert!(
        !may_install(true, false),
        "a box that already has everything is checked, not reinstalled"
    );
}

#[test]
fn a_configured_box_is_checked_but_never_re_installed() {
    // The waste this exists to stop: `ensure_opencode` can spend ten
    for script in [
        opencode_script(false),
        ffmpeg_script(false),
        sox_script(false),
    ] {
        assert!(
            script.contains("command -v"),
            "the check must survive: {script}"
        );
        assert!(
            !script.contains("npm i"),
            "a configured box must not re-run npm: {script}"
        );
        assert!(
            !script.contains("install -y"),
            "a configured box must not chase the package manager: {script}"
        );
        assert!(
            script.contains("force a re-provision"),
            "and it must name the way out: {script}"
        );
    }
    // The full path keeps both halves: a fresh box still gets them.
    assert!(opencode_script(true).contains("npm i -g"));
    assert!(ffmpeg_script(true).contains("install -y ffmpeg"));
    assert!(sox_script(true).contains("install -y sox"));

    // zstd is the deliberate exception to that rule: one second, ~1 MB, and
    let zstd = zstd_script();
    assert!(
        zstd.contains(
            r#"command -v zstd >/dev/null 2>&1 && { echo "ZSTD-OK (present)"; exit 0; }"#
        ),
        "a box that has it must pay one command: {zstd}"
    );
    assert!(
        zstd.contains("install -y zstd"),
        "and one that does not must be able to get it: {zstd}"
    );
    assert!(
        !zstd.contains("force a re-provision"),
        "the exception is that this one does not wait for a forced box: {zstd}"
    );
    // A box that already has the tool short-circuits in *both* flavours
    for script in [opencode_script(true), opencode_script(false)] {
        assert!(script.contains(
            r#"command -v opencode >/dev/null 2>&1 && { echo "OPENCODE-OK (present)"; exit 0; }"#
        ));
    }
}

/// The exit code is the whole contract between the box and this function,
#[test]
fn a_fetch_exit_code_says_whether_the_push_may_try_again() {
    let tag = "models-vdda4efee13df";
    let ok = classify_fetch(
        EXIT_LANDED,
        "FETCH-OK (16 files, 667 MiB)",
        "",
        tag,
        "models",
    );
    assert!(ok.unwrap().contains("models-vdda4efee13df"));

    // Absence: the artifact is not published, the network is down, the URL
    let absent = classify_fetch(
        EXIT_UNREACHABLE,
        "",
        "FETCH-UNREACHABLE (HTTP 404)",
        tag,
        "models",
    );
    assert!(matches!(absent, Err(FetchOutcome::Unreachable(_))));

    // Wrong bytes: the one answer that must stop, because pushing the same
    let wrong = classify_fetch(
        EXIT_CORRUPT,
        "",
        "FETCH-CORRUPT (tts.onnx: sha256 …)",
        tag,
        "models",
    );
    match wrong {
        Err(FetchOutcome::Corrupt(m)) => assert!(m.contains("tts.onnx"), "{m}"),
        other => panic!("corruption must not be a fallback: {other:?}"),
    }

    // Anything else is absence, not corruption: a box too old to have the
    for code in [1_i32, 2, 126, 127, 255] {
        match classify_fetch(code, "", "", tag, "models") {
            Err(FetchOutcome::Unreachable(m)) => assert!(m.contains(&code.to_string()), "{m}"),
            other => panic!("exit {code} must be absence: {other:?}"),
        }
    }
}

/// A pack fetch is the same contract as a models fetch, and the reason it
#[test]
fn a_pack_is_fetched_the_same_way_and_read_the_same_way() {
    let release = crate::artifact::PackRelease::for_repo(
        "lhuthng/storycast",
        "xianxia",
        "0.1.0",
        "25e7ed5b07955cd15c97897b41bc4353cea2aa344da514f51ec585ac81897821",
    )
    .unwrap();
    assert_eq!(release.tag, "xianxia-pack-v0.1.0");
    let script = pack_fetch_script(&release);
    // The prefix and the destination are the same word on purpose: the
    assert!(script.contains("--strip-prefix assets"), "{script}");
    assert!(script.contains("~/bm-worker/assets"), "{script}");
    assert!(script.contains(&release.hash), "{script}");
    assert!(script.contains(&shell_quote(&release.url)), "{script}");

    let ok = classify_fetch(
        EXIT_LANDED,
        "FETCH-OK (41 files, 62 MiB)",
        "",
        &release.tag,
        "pack",
    );
    let line = ok.unwrap();
    assert!(
        line.starts_with("pack from the release xianxia-pack-v0.1.0"),
        "{line}"
    );

    // A pack that does not verify still classifies as corrupt — the caller
    assert!(matches!(
        classify_fetch(
            EXIT_CORRUPT,
            "",
            "FETCH-CORRUPT (the bundle is a different pack)",
            &release.tag,
            "pack",
        ),
        Err(FetchOutcome::Corrupt(_))
    ));
    // …and an unreachable one is still a push.
    assert!(matches!(
        classify_fetch(
            EXIT_UNREACHABLE,
            "",
            "FETCH-UNREACHABLE (HTTP 404)",
            &release.tag,
            "pack",
        ),
        Err(FetchOutcome::Unreachable(_))
    ));
}

/// A pointer stamped before versions existed must not fetch anything, and
#[test]
fn a_pack_release_only_resolves_from_a_versioned_pointer() {
    let root = std::env::temp_dir().join("bm-packrelease-pointer");
    let _ = std::fs::remove_dir_all(&root);

    // No pointer at all.
    assert!(crate::artifact::PackRelease::resolve(&root, "lhuthng/storycast").is_none());
    // No repo: the push, which is what every box has today.
    crate::profile::write_pointer(
        &root,
        &crate::profile::Pointer {
            name: "xianxia".into(),
            hash: "aa".into(),
            version: "0.1.0".into(),
        },
    )
    .unwrap();
    assert!(crate::artifact::PackRelease::resolve(&root, "  ").is_none());

    let resolved = crate::artifact::PackRelease::resolve(&root, "lhuthng/storycast").unwrap();
    assert_eq!(resolved.tag, "xianxia-pack-v0.1.0");
    assert_eq!(
        resolved.url,
        "https://github.com/lhuthng/storycast/releases/download/xianxia-pack-v0.1.0/xianxia.tar.zst"
    );

    // The pointer written before this field existed: no version, so no
    crate::profile::write_pointer(
        &root,
        &crate::profile::Pointer {
            name: "xianxia".into(),
            hash: "aa".into(),
            version: String::new(),
        },
    )
    .unwrap();
    assert!(
        crate::artifact::PackRelease::resolve(&root, "lhuthng/storycast").is_none(),
        "an unversioned pointer must fall back to the push, not guess a tag"
    );

    // The name and the version go into a URL and a git tag, so they are
    for (name, version) in [
        ("", "0.1.0"),
        ("xianxia", ""),
        ("../etc", "0.1.0"),
        ("a/b", "0.1.0"),
    ] {
        assert!(
            crate::artifact::PackRelease::for_repo("o/n", name, version, "aa").is_err(),
            "`{name}`/`{version}` was accepted"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// The line the box runs, and the one thing that could make it do something
#[test]
fn the_fetch_line_asks_for_the_tagged_bundle_and_quotes_the_url() {
    let hash = "dda4efee13df0eb2b30ef45eb548741b5af633f6d55712e30f4da574b357c552";
    let r = crate::artifact::ModelsRelease::for_repo("lhuthng/storycast", hash).unwrap();
    let script = fetch_script(&r, "vieneu");
    assert!(
        script.contains("~/bm-worker/bm-agent fetch-artifact"),
        "{script}"
    );
    assert!(script.contains(&format!("--expect {hash}")), "{script}");
    assert!(
        script.contains("~/bm-worker/engines/vieneu/models"),
        "the destination is the engine's own models dir: {script}"
    );
    // A second engine fetches into its own tree, never over VieNeu's.
    assert!(
        fetch_script(&r, "gemini").contains("~/bm-worker/engines/gemini/models"),
        "the engine name has to be in the fetch destination"
    );
    // Quoted, because the URL is a string in a `sh -c`. A repo the parser
    assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    assert_eq!(shell_quote("plain"), "'plain'");
}

/// The push must not carry the bundle it is standing in for.
#[test]
fn the_models_push_leaves_the_transfer_artifact_behind() {
    assert_eq!(
        MODELS_PUSH_EXCLUDES,
        &["/models.tar.zst"],
        "the bundle is 380 MB of the same 668 MB, and no box reads it"
    );
    assert_eq!(
        MODELS_PUSH_EXCLUDES[0].trim_start_matches('/'),
        crate::artifact::BUNDLE_NAME,
        "and it is the one file `tools/models.sh pack` writes"
    );
}

#[test]
fn undeclared_voices_names_only_the_strays() {
    // The Suneo outage in one assertion: a hand-enrolled clone the
    let manifest: std::collections::HashMap<String, String> =
        [("Học Trò".to_string(), "refs/hoc-tro.mp3".to_string())]
            .into_iter()
            .collect();
    let mut pool = crate::pool::Pool::new();
    pool.insert(
        "Pool Sample".to_string(),
        crate::pool::PoolEntry {
            file: "refs/pool.wav".into(),
            tags: vec![],
        },
    );
    let catalogue = vec!["Thái Sơn".to_string(), "Adam".to_string()];
    // Exactly what the sidecar sends, and the difference is the whole test:
    let store = vec![
        "Thái Sơn — Nam · Trung · Kể chuyện".to_string(),
        "Adam — Nam · Nam · Giọng đọc tự nhiên".to_string(),
        "Học Trò".to_string(),
        "Pool Sample".to_string(),
        "Suneo".to_string(),
        "Suneo".to_string(),
        "_note".to_string(),
    ];
    assert_eq!(
        undeclared_voices(&store, &manifest, &pool, &catalogue),
        vec!["Suneo"],
        "one hand-enrolled clone, named by its name and not by a label"
    );
    assert!(undeclared_voices(&[], &manifest, &pool, &catalogue).is_empty());
}

/// The false positive this shape caused, in the numbers it produced.
#[test]
fn a_shipped_preset_is_never_reported_as_undeclared() {
    let catalogue: Vec<String> = crate::voices::offline_voices("vieneu")
        .into_iter()
        .map(|v| v.name)
        .collect();
    assert_eq!(catalogue.len(), 23, "the shipped ViNeu roster");
    // Built the way the sidecar builds them: name + description, verbatim.
    let store: Vec<String> = catalogue
        .iter()
        .map(|n| format!("{n} — Nam · Bắc · Kể chuyện"))
        .chain(["Suneo".to_string()])
        .collect();
    assert_eq!(
        undeclared_voices(
            &store,
            &std::collections::HashMap::new(),
            &crate::pool::Pool::new(),
            &catalogue,
        ),
        vec!["Suneo"],
        "23 declared presets, 1 genuine stray"
    );
    // And the same answer whatever the description says, or whether the
    let spelled = vec![
        "Thái Sơn – Nam · Trung".to_string(),
        "Adam - Nam".to_string(),
    ];
    assert!(undeclared_voices(
        &spelled,
        &std::collections::HashMap::new(),
        &crate::pool::Pool::new(),
        &catalogue
    )
    .is_empty());
}

#[test]
fn probe_summary_counts_voices_instead_of_listing_them() {
    // Fifty enrolled names wrapped the Logs pane for screens. The count
    let p = Probe {
        reachable: true,
        hostname: "box".into(),
        voices: vec!["Adam".into(), "Suneo".into()],
        ..Default::default()
    };
    let s = p.summary();
    assert!(s.contains("2 voices"), "{s}");
    assert!(!s.contains("Suneo"), "names stay out of the summary: {s}");
}

#[test]
fn a_box_without_ffmpeg_says_so_before_a_merge_fails() {
    // The failure this prevents: a box provisions cleanly, is offered a
    let present = Probe {
        reachable: true,
        hostname: "box".into(),
        ffmpeg_present: true,
        sox_present: true,
        ..Default::default()
    };
    assert!(
        !present.summary().contains("FFMPEG") && !present.summary().contains("SOX"),
        "{}",
        present.summary()
    );
    let absent = Probe {
        ffmpeg_present: false,
        ..present.clone()
    };
    assert!(
        absent.summary().contains("NO FFMPEG"),
        "{}",
        absent.summary()
    );
    // SoX is the second engine and a hard gate on merge, so it gets its
    let no_sox = Probe {
        sox_present: false,
        ..present.clone()
    };
    assert!(no_sox.summary().contains("NO SOX"), "{}", no_sox.summary());
    let neither = Probe {
        ffmpeg_present: false,
        sox_present: false,
        ..present.clone()
    };
    assert!(
        neither.summary().contains("NO FFMPEG/SOX"),
        "{}",
        neither.summary()
    );
}

#[test]
fn probe_normalizes_uname_spellings_to_rust_platforms() {
    // `arm64` (macOS) and `aarch64` (Linux) are the same chip; `Darwin`
    assert_eq!(super::normalize_arch("arm64"), "aarch64");
    assert_eq!(super::normalize_arch("aarch64"), "aarch64");
    assert_eq!(super::normalize_arch("amd64"), "x86_64");
    assert_eq!(super::normalize_arch("riscv64"), "riscv64");
    assert_eq!(super::normalize_os("Darwin"), "macos");
    assert_eq!(super::normalize_os("Linux"), "linux");
    let p = Probe {
        reachable: true,
        hostname: "mac".into(),
        os: "macos".into(),
        arch: "aarch64".into(),
        ..Default::default()
    };
    assert!(p.summary().contains("macos/aarch64"), "{}", p.summary());
}

#[test]
fn unreachable_probe_explains_itself() {
    let p = Probe {
        reachable: false,
        note: "ssh exit 255".into(),
        ..Default::default()
    };
    assert!(p.summary().contains("unreachable"));
    assert!(p.summary().contains("ssh exit 255"));
}

/// The sidecar needs *both* the binary and its weights, a binary with no
#[test]
fn the_tts_sidecar_needs_the_binary_and_the_weights() {
    let mut p = Probe {
        reachable: true,
        agent_version: Some("0.2.0".into()),
        ..Default::default()
    };
    assert!(!p.configured("0.2.0"), "nothing installed is not ready");
    assert_eq!(p.sidecar(), "none");

    p.tts_bin_present = true;
    assert!(!p.rust_ready(), "a binary with no models cannot render");
    assert!(!p.configured("0.2.0"));

    p.models_present = true;
    assert!(p.configured("0.2.0"));
    assert_eq!(p.sidecar(), "rust");

    assert!(!p.configured("0.3.0"), "a stale agent is never configured");
}

#[test]
fn a_linux_binary_without_its_runtime_is_not_ready() {
    // Wolf's box: binary + models present, libonnxruntime never landed.
    let mut p = Probe {
        reachable: true,
        agent_version: Some("0.2.0".into()),
        tts_bin_present: true,
        models_present: true,
        os: "linux".into(),
        ..Default::default()
    };
    assert!(!p.tts_runtime_ok());
    assert!(!p.rust_ready());
    assert!(!p.configured("0.2.0"));
    assert_eq!(p.sidecar(), "none");
    p.tts_lib_present = true;
    assert!(p.configured("0.2.0"));
    // macOS links statically: no lib, still ready.
    p.os = "macos".into();
    p.tts_lib_present = false;
    assert!(p.configured("0.2.0"));
}

/// The two flags are independent, so a box can report both without either
#[test]
fn both_sidecars_can_be_present_at_once() {
    let p = Probe {
        reachable: true,
        agent_version: Some("0.2.0".into()),
        python_present: true,
        tts_bin_present: true,
        models_present: true,
        ..Default::default()
    };
    assert!(p.configured("0.2.0"));
    // Rust wins the summary when it is ready, because that is what a switch
    assert_eq!(p.sidecar(), "rust");
}
