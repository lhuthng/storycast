use super::*;

#[test]
fn profile_check_says_whether_this_checkout_can_cook_its_language() {
    // The question `Inner::voice_gate` answers for the scheduler, asked
    let root = std::env::temp_dir().join(format!("bm-profile-check{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("adapters/xianxia-en-US")).unwrap();
    std::fs::create_dir_all(root.join(".bm")).unwrap();
    std::fs::write(
        root.join("adapters/xianxia-en-US/adapter.json"),
        r#"{"pack":"xianxia","language":"en-US"}"#,
    )
    .unwrap();
    let mut binding = bm_core::profile::Binding::default();
    binding.pack.name = "xianxia".into();
    binding.adapter.name = "xianxia-en-US".into();
    bm_core::profile::write_binding(&root, &binding).unwrap();
    let layout = Layout {
        adapter: "xianxia-en-US".into(),
        ..Layout::new(&root)
    };

    // VieNeu declares `vi-VN`, so this checkout cannot cook its own book.
    let settings = Settings::default();
    assert_eq!(
        settings.engine, "vieneu",
        "the default engine is the local one"
    );
    let (lines, ok) = profile_check(&layout, &settings).unwrap();
    assert!(!ok, "{lines:?}");
    let text = lines.join("\n");
    assert!(text.contains("xianxia · xianxia-en-US"), "{text}");
    assert!(text.contains("declares language en-US"), "{text}");
    assert!(text.contains("cannot voice"), "{text}");
    assert!(text.contains("problem"), "{text}");
    assert!(!text.contains("verdict   ok"), "{text}");

    // The engine that declares it: the same checkout is fine, and the
    let settings = Settings {
        engine: "gemini".into(),
        ..Settings::default()
    };
    let (lines, ok) = profile_check(&layout, &settings).unwrap();
    assert!(ok, "{lines:?}");
    let text = lines.join("\n");
    assert!(text.contains("language  en-US (declared)"), "{text}");
    assert!(text.contains("declares vi-VN, en-US"), "{text}");
    assert!(text.contains("verdict   ok"), "{text}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_backup_without_an_api_flag_leaves_the_providers_endpoint_alone() {
    // The flag's *absence* is the fact worth pinning. With a
    let cli =
        Cli::try_parse_from(["bm-inductor", "backup"]).expect("`backup` needs no required flags");
    let Cmd::Backup { api, .. } = cli.cmd else {
        panic!("parsed as another subcommand");
    };
    assert_eq!(api, None, "an unsaid `--api` must not become a URL");

    // Given, it is kept verbatim: the slot that reads it appends its own
    let cli = Cli::try_parse_from(["bm-inductor", "backup", "--api", "https://gw.example/v1"])
        .expect("`--api` is accepted");
    let Cmd::Backup { api, .. } = cli.cmd else {
        panic!("parsed as another subcommand");
    };
    assert_eq!(api.as_deref(), Some("https://gw.example/v1"));
}

#[test]
fn excerpts_defaults_to_chapter_one_and_no_forced_rewrite() {
    // The start of the book is a sane default: a backfill is a recovery
    let cli = Cli::try_parse_from(["bm-inductor", "excerpts"])
        .expect("`excerpts` needs no required flags");
    let Cmd::Excerpts {
        start,
        through,
        api,
        force,
        retries,
        ..
    } = cli.cmd
    else {
        panic!("parsed as another subcommand");
    };
    assert_eq!(start, 1);
    assert_eq!(through, None, "unset means the end of the book on disk");
    assert_eq!(api, None, "an unsaid `--api` must not become a URL");
    assert!(!force, "an existing excerpt is kept unless asked again");
    assert_eq!(retries, 1, "the digest's own one-repair budget");
}

#[test]
fn workspace_new_use_list_roundtrip() {
    let dir = std::env::temp_dir().join(format!("bm-workspace{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    // Create switches to it, stamping the loaded profile (none here).
    workspace_cmd(
        &dir,
        WorkspaceCmd::New {
            name: "demo".into(),
            profile: None,
            crawler: None,
        },
    )
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(Layout::active_workspace_file(&dir)).unwrap(),
        "demo\n"
    );
    assert!(dir.join("workspaces/demo/settings.json").is_file());
    // Creating twice is an error, not a wipe.
    assert!(workspace_cmd(
        &dir,
        WorkspaceCmd::New {
            name: "demo".into(),
            profile: None,
            crawler: None,
        }
    )
    .is_err());
    // Selecting a missing workspace is an error, not a creation.
    assert!(workspace_cmd(
        &dir,
        WorkspaceCmd::Use {
            name: "gone".into()
        }
    )
    .is_err());
    workspace_cmd(
        &dir,
        WorkspaceCmd::Use {
            name: "demo".into(),
        },
    )
    .unwrap();
    let listed = workspace_cmd(&dir, WorkspaceCmd::List).unwrap();
    assert!(listed.iter().any(|l| l == "* demo"), "{listed:?}");
    // A loaded profile stamps new workspaces at creation.
    std::fs::create_dir_all(dir.join(".bm")).unwrap();
    std::fs::write(dir.join(".bm/profile"), r#"{"name":"xianxia","hash":"h1"}"#).unwrap();
    workspace_cmd(
        &dir,
        WorkspaceCmd::New {
            name: "second".into(),
            profile: None,
            crawler: None,
        },
    )
    .unwrap();
    let settings: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join("workspaces/second/settings.json")).unwrap(),
    )
    .unwrap();
    // A bundle loaded before the split stamps as the pack, and the other
    assert_eq!(settings["profile"]["pack"]["name"], "xianxia");
    assert_eq!(settings["profile"]["adapter"]["name"], "");
    assert_eq!(settings["profile"]["engine"]["name"], "");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The guided create flow's crawler: the script is copied into the book's
#[test]
fn a_custom_crawler_setup_is_copied_and_wired_into_the_new_workspace() {
    let dir = std::env::temp_dir().join(format!("bm-ws-crawler{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("profiles")).unwrap();
    std::fs::write(
        dir.join("profiles/presets.json"),
        r#"{"jnovel-en": {"label": "JNovel", "pack": "xianxia", "adapter": "vi-VN", "engine": "vieneu"}}"#,
    )
    .unwrap();
    // An arbitrary local source for the guided flow's custom copy — not a
    std::fs::create_dir_all(dir.join("local")).unwrap();
    std::fs::write(dir.join("local/mysite.lua"), "-- crawl").unwrap();

    let mut params = serde_json::Map::new();
    params.insert(
        "epub".into(),
        serde_json::Value::String("tmp/book.epub".into()),
    );
    let crawler = bm_core::preset::CrawlerSetup {
        source: dir.join("local/mysite.lua"),
        url_template: "https://example.test/{book}/chuong-{n}".into(),
        params,
        ..Default::default()
    };
    workspace_cmd(
        &dir,
        WorkspaceCmd::New {
            name: "book".into(),
            profile: Some("jnovel-en".into()),
            crawler: Some(crawler),
        },
    )
    .unwrap();

    assert!(
        dir.join("workspaces/book/crawl/mysite.lua").is_file(),
        "the script is the book's own copy"
    );
    let s: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join("workspaces/book/settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(s["crawl"]["mode"], "script");
    assert_eq!(s["crawl"]["script"], "crawl/mysite.lua");
    assert_eq!(s["crawl"]["params"]["epub"], "tmp/book.epub");
    assert_eq!(s["url_template"], "https://example.test/{book}/chuong-{n}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The guided **Local file (EPUB)** choice: the operator named a book and it
#[test]
fn an_epub_crawler_setup_copies_the_book_into_the_new_workspace() {
    let dir = std::env::temp_dir().join(format!("bm-ws-epub{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("profiles")).unwrap();
    std::fs::write(
        dir.join("profiles/presets.json"),
        r#"{"jnovel-en": {"label": "JNovel", "pack": "xianxia", "adapter": "vi-VN", "engine": "pocket"}}"#,
    )
    .unwrap();
    let src = dir.join("somewhere/apothecary.epub");
    std::fs::create_dir_all(src.parent().unwrap()).unwrap();
    std::fs::write(&src, b"PK\x03\x04 not really a zip, but bytes").unwrap();

    let mut params = serde_json::Map::new();
    params.insert(
        "epub".into(),
        serde_json::Value::String("tmp/book.epub".into()),
    );
    let crawler = bm_core::preset::CrawlerSetup {
        script: "crawlers/examples/epub.lua".into(),
        params,
        book: src.clone(),
        ..Default::default()
    };
    workspace_cmd(
        &dir,
        WorkspaceCmd::New {
            name: "book".into(),
            profile: Some("jnovel-en".into()),
            crawler: Some(crawler),
        },
    )
    .unwrap();

    let dest = dir.join("workspaces/book/tmp/book.epub");
    assert!(dest.is_file(), "the book lands in the workspace");
    assert_eq!(std::fs::read(&dest).unwrap(), std::fs::read(&src).unwrap());
    let s: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join("workspaces/book/settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(s["crawl"]["mode"], "script");
    assert_eq!(s["crawl"]["script"], "crawlers/examples/epub.lua");
    assert_eq!(s["crawl"]["params"]["epub"], "tmp/book.epub");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The guided flow's **folder of volumes** shape: the operator named a
#[test]
fn a_books_directory_is_copied_into_the_new_workspace() {
    let dir = std::env::temp_dir().join(format!("bm-ws-books{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("profiles")).unwrap();
    std::fs::write(
        dir.join("profiles/presets.json"),
        r#"{"jnovel-en": {"label": "JNovel", "pack": "xianxia", "adapter": "vi-VN", "engine": "pocket"}}"#,
    )
    .unwrap();
    let shelf = dir.join("somewhere/volumes");
    std::fs::create_dir_all(&shelf).unwrap();
    std::fs::write(shelf.join("vol-01.epub"), b"volume one").unwrap();
    std::fs::write(shelf.join("vol-02.epub"), b"volume two").unwrap();
    std::fs::write(shelf.join("cover.jpg"), b"not a book").unwrap();

    let mut params = serde_json::Map::new();
    params.insert("books".into(), serde_json::Value::String("books".into()));
    let crawler = bm_core::preset::CrawlerSetup {
        script: "crawlers/examples/epub.lua".into(),
        params,
        books: shelf.clone(),
        ..Default::default()
    };
    workspace_cmd(
        &dir,
        WorkspaceCmd::New {
            name: "book".into(),
            profile: Some("jnovel-en".into()),
            crawler: Some(crawler),
        },
    )
    .unwrap();

    let dest = dir.join("workspaces/book/books");
    assert_eq!(
        std::fs::read(dest.join("vol-01.epub")).unwrap(),
        b"volume one"
    );
    assert!(dest.join("vol-02.epub").is_file());
    assert!(
        !dest.join("cover.jpg").exists(),
        "only .epub volumes are copied"
    );
    let s: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join("workspaces/book/settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(s["crawl"]["script"], "crawlers/examples/epub.lua");
    assert_eq!(s["crawl"]["params"]["books"], "books");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A preset that names a **known** site wires the *global* crawler into the
#[test]
fn a_preset_that_names_a_known_site_wires_the_global_crawler() {
    let dir = std::env::temp_dir().join(format!("bm-ws-known{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("profiles")).unwrap();
    std::fs::create_dir_all(dir.join("crawlers/known")).unwrap();
    std::fs::write(dir.join("crawlers/known/storya.lua"), "-- global").unwrap();
    std::fs::write(
        dir.join("profiles/presets.json"),
        r#"{"xianxia-vi": {"label": "Xianxia", "pack": "xianxia", "adapter": "vi-VN", "engine": "vieneu", "crawler": {"type": "known", "file": "storya.click"}}}"#,
    )
    .unwrap();
    workspace_cmd(
        &dir,
        WorkspaceCmd::New {
            name: "book".into(),
            profile: Some("xianxia-vi".into()),
            crawler: None,
        },
    )
    .unwrap();
    let s: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join("workspaces/book/settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(s["crawl"]["mode"], "script");
    assert_eq!(
        s["crawl"]["script"], "crawlers/known/storya.lua",
        "the registry path, from the checkout root"
    );
    assert_eq!(
        s["url_template"],
        "https://storya.click/truyen/nguoi-tren-van-nguoi/chuong-{n}"
    );
    assert!(
        !dir.join("workspaces/book/crawl/storya.lua").exists(),
        "the global crawler is referenced, not copied in"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn provisioning_localhost_syncs_nothing_and_reports_ready() {
    // The local worker runs in place from the repo, so there is no
    let dir = std::env::temp_dir().join(format!("bm-local-prov{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let layout = bm_core::Layout::new(&dir);
    for addr in ["127.0.0.1", "localhost", "::1"] {
        let out = provision_machine(&layout, addr, "thang", 22, None, false, None, None);
        assert!(out.ready, "{addr} must always be ready");
        assert!(out.reachable, "{addr} is local — ssh is never involved");
        assert_eq!(out.lines.len(), 1, "{:?}", out.lines);
        assert!(
            out.lines[0].contains("nothing to provision"),
            "{}",
            out.lines[0]
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn reprovision_keeps_the_stored_work_policy() {
    // A fresh `Machine` carries `task_policy: None`, and both
    let dir = std::env::temp_dir().join(format!("bm-policy{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let layout = bm_core::Layout::new(&dir);
    let policy = vec![
        bm_proto::TaskPref {
            stage: bm_proto::Stage::Merge,
            enabled: true,
        },
        bm_proto::TaskPref {
            stage: bm_proto::Stage::Digest,
            enabled: true,
        },
        bm_proto::TaskPref {
            stage: bm_proto::Stage::Crawl,
            enabled: true,
        },
        bm_proto::TaskPref {
            stage: bm_proto::Stage::Render,
            enabled: true,
        },
    ];
    bm_core::provision::save_box(
        &layout.machines(),
        &bm_core::provision::LinkedBox {
            name: "box-1".into(),
            addr: "192.0.2.1".into(),
            user: "fixture".into(),
            port: 2222,
            key: None,
            role: "worker".into(),
            task_policy: Some(policy.clone()),
            accepting_work: true,
            tts_threads: None,
        },
    )
    .unwrap();
    let mut m = Machine::new("192.0.2.1", "fixture", 2222, None, "worker");
    carry_task_policy(&mut m, &layout);
    assert_eq!(m.task_policy, Some(policy));
    // Unknown box: nothing to carry, stays default.
    let mut fresh = Machine::new("192.0.2.2", "fixture", 2222, None, "worker");
    carry_task_policy(&mut fresh, &layout);
    assert_eq!(fresh.task_policy, None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn binary_routing_serves_each_platform_its_own_build() {
    // A fixture root holding one binary per platform: routing must pick
    let dir = std::env::temp_dir().join(format!("bm-routing{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let layout = bm_core::Layout::new(&dir);
    let touch = |rel: &str| {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, b"fake").unwrap();
        p
    };
    let cross_agent = touch("rust/target/x86_64-unknown-linux-gnu/debug/bm-agent");
    assert_eq!(
        agent_binary_for("linux", "x86_64", &layout).unwrap(),
        cross_agent
    );
    assert_eq!(
        tts_runtime_dir("linux", "x86_64", &layout).unwrap(),
        dir.join("rust/target/ort-linux-x64")
    );
    // Nothing staged for linux/arm64: a build error naming the platform,
    let err = agent_binary_staged("linux", "aarch64", &layout).unwrap_err();
    assert!(err.to_string().contains("linux/aarch64"), "{err}");
    // The on-demand build only ever targets cross candidates: the native
    let native = layout.root.join("rust/target/debug/bm-agent");
    assert!(
        !buildable_agent_candidates(std::env::consts::OS, std::env::consts::ARCH, &layout)
            .contains(&native)
    );
    // A foreign platform always has cross candidates to build.
    let foreign_arch = if std::env::consts::ARCH == "x86_64" {
        "aarch64"
    } else {
        "x86_64"
    };
    assert!(
        !buildable_agent_candidates("linux", foreign_arch, &layout).is_empty(),
        "a foreign linux target must have cross candidates to build"
    );
    // This host's own platform falls back to the native build, asserted
    let native = touch("rust/target/debug/bm-agent");
    assert_eq!(
        agent_candidates(std::env::consts::OS, std::env::consts::ARCH, &layout)
            .last()
            .unwrap(),
        &native
    );
    // The macOS sidecar is self-contained: no runtime travels with it.
    assert!(tts_runtime_dir("macos", "aarch64", &layout).is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_sidecar_is_picked_from_disk_or_named_in_the_error() {
    let dir = std::env::temp_dir().join(format!("bm-tts-stage{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let layout = bm_core::Layout::new(&dir);
    // Nothing staged: the error has to name the platform and the way to
    let err = tts_binary_staged("linux", "x86_64", &layout).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("linux/x86_64"), "{msg}");
    assert!(msg.contains("make tts"), "{msg}");
    // Staged: picked, and only for the platform that built it.
    let cross = dir.join("rust/target/x86_64-unknown-linux-gnu/release/bm-tts");
    std::fs::create_dir_all(cross.parent().unwrap()).unwrap();
    std::fs::write(&cross, b"fake").unwrap();
    assert_eq!(
        tts_binary_staged("linux", "x86_64", &layout).unwrap(),
        cross
    );
    assert!(tts_binary_staged("linux", "aarch64", &layout).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn only_linux_x86_64_gets_an_on_demand_sidecar_build() {
    // The build stages its ONNX Runtime through `make runtime`, which
    let dir = std::env::temp_dir().join(format!("bm-tts-build{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let layout = bm_core::Layout::new(&dir);
    let cands = buildable_tts_candidates("linux", "x86_64", &layout);
    assert_eq!(
        cands,
        vec![dir.join("rust/target/x86_64-unknown-linux-gnu/release/bm-tts")]
    );
    for (os, arch) in [
        ("linux", "aarch64"),
        ("macos", "aarch64"),
        ("windows", "x86_64"),
    ] {
        assert!(
            buildable_tts_candidates(os, arch, &layout).is_empty(),
            "{os}/{arch} must not be offered a build this host cannot finish"
        );
    }
    // Same on a linux/x86_64 host: the native `target/release/bm-tts` is
    assert!(!cands.contains(&dir.join("rust/target/release/bm-tts")));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_old_sidecar_is_reported_but_never_silently_rebuilt() {
    // The agent rebuilds a stale staged binary because the version gate
    let dir = std::env::temp_dir().join(format!("bm-tts-stale{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let layout = bm_core::Layout::new(&dir);
    let src = dir.join("rust/crates/bm-tts/src/lib.rs");
    std::fs::create_dir_all(src.parent().unwrap()).unwrap();
    std::fs::write(&src, b"fn main() {}").unwrap();
    let bin = dir.join("rust/target/x86_64-unknown-linux-gnu/release/bm-tts");
    std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(&bin, b"fake").unwrap();
    assert!(
        !tts_is_stale(&bin, &layout),
        "built after the source: fresh"
    );
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(&src, b"fn main() { /* newer */ }").unwrap();
    assert!(tts_is_stale(&bin, &layout), "older than the source: warn");
    // A source the sidecar does not build from is not a reason to call it
    let _ = std::fs::remove_dir_all(src.parent().unwrap());
    std::fs::create_dir_all(dir.join("rust/crates/bm-agent/src")).unwrap();
    std::fs::write(
        dir.join("rust/crates/bm-agent/src/main.rs"),
        b"fn main() { /* newer */ }",
    )
    .unwrap();
    assert!(!tts_is_stale(&bin, &layout));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_stopped_run_carries_why_it_stopped() {
    // The pane used to print a bare "provision INCOMPLETE" for every
    let mut log = bm_core::provision::LiveLog::new(None);
    let out = stopped(
        &mut log,
        "10.0.0.1",
        "no TTS sidecar binary for linux/x86_64",
        true,
    );
    assert!(!out.ready);
    assert!(out.reachable);
    assert_eq!(
        out.stop.as_deref(),
        Some("no TTS sidecar binary for linux/x86_64")
    );
    assert_eq!(out.lines.len(), 1);
    assert_eq!(
        out.lines[0],
        "[10.0.0.1] no TTS sidecar binary for linux/x86_64"
    );
}

#[test]
fn a_staged_onnx_runtime_skips_the_make() {
    // The fixture root has no Makefile, so `make -C <root> runtime` can
    let dir = std::env::temp_dir().join(format!("bm-ort{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let layout = bm_core::Layout::new(&dir);
    let ort = dir.join("rust/target/ort-linux-x64");
    std::fs::create_dir_all(&ort).unwrap();
    std::fs::write(ort.join("libonnxruntime.so"), b"x").unwrap();
    assert!(
        stage_onnx_runtime(&layout, &ort).is_err(),
        "one of the two names is not a staged runtime"
    );
    std::fs::write(ort.join("libonnxruntime.so.1"), b"x").unwrap();
    assert!(stage_onnx_runtime(&layout, &ort).is_ok());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_staged_agent_older_than_its_sources_rebuilds() {
    // 0.2.3 on disk while 0.2.4 is demanded: a stale staged file reads as
    let dir = std::env::temp_dir().join(format!("bm-staged-fresh{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let layout = bm_core::Layout::new(&dir);
    let src = dir.join("rust/crates/bm-agent/src/main.rs");
    std::fs::create_dir_all(src.parent().unwrap()).unwrap();
    let bin = dir.join("rust/target/x86_64-unknown-linux-gnu/debug/bm-agent");
    std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
    std::fs::write(&bin, b"fake").unwrap();
    // The source must land strictly after the binary: one sleep so the
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(&src, b"fake newer").unwrap();
    assert!(
        !staged_is_fresh(&bin, &layout),
        "older-than-sources must rebuild"
    );
    assert!(agent_binary_staged("linux", "x86_64", &layout).is_err());
    // No sources at all (a bare fixture, like the routing test above)
    let _ = std::fs::remove_dir_all(dir.join("rust/crates"));
    assert!(staged_is_fresh(&bin, &layout));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cross_build_infers_target_and_workspace_from_the_candidate_path() {
    // The target triple is the candidate's grandparent (`…/<triple>/debug`)
    let cand = std::path::Path::new("/repo/rust/target/x86_64-unknown-linux-gnu/debug/bm-agent");
    assert_eq!(cross_target_of(cand).unwrap(), "x86_64-unknown-linux-gnu");
    assert_eq!(
        workspace_dir_above_target(cand).unwrap(),
        std::path::Path::new("/repo/rust")
    );
}
