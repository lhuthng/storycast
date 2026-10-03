use super::*;

/// A throwaway repo root holding only the files the stamp looks at.
fn stamp_fixture(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("bm-stamp-{name}"));
    let _ = std::fs::remove_dir_all(&root);
    for d in ["prompts", "refs", "python", "data", "assets"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    std::fs::write(root.join("prompts/digest.md"), "prompt v1").unwrap();
    std::fs::write(root.join("python/requirements.txt"), "torch\n").unwrap();
    std::fs::write(
        root.join("data/cast-default-vieneu.json"),
        r#"{"Narrator":"Đức Trí"}"#,
    )
    .unwrap();
    std::fs::write(root.join("voices.json"), r#"{"Narrator":"refs/n.wav"}"#).unwrap();
    std::fs::write(root.join("refs/n.wav"), vec![1u8; 64]).unwrap();
    root
}

/// Stand-in for the cross-built agent binary the stamp hashes.
fn agent_bin(root: &std::path::Path) -> std::path::PathBuf {
    let p = root.join("bm-agent");
    if !p.exists() {
        std::fs::write(&p, b"agent-bytes-v1").unwrap();
    }
    p
}

/// The stage list the fixture is stamped for: the two whose files it holds.
const STAGES: [bm_proto::Stage; 2] = [bm_proto::Stage::Digest, bm_proto::Stage::Merge];

/// The stamp for a plain root. The plan is built through the same call the
fn stamp(root: &std::path::Path) -> ProvisionStamp {
    compute_provision_stamp(
        &crate::Layout::new(root),
        &STAGES,
        "0.2.0",
        &agent_bin(root),
        None,
    )
    .expect("the fixture plan must build")
}

fn stamp_v(root: &std::path::Path, version: &str) -> ProvisionStamp {
    compute_provision_stamp(
        &crate::Layout::new(root),
        &STAGES,
        version,
        &agent_bin(root),
        None,
    )
    .expect("the fixture plan must build")
}

/// The stamp with an explicit agent binary, for the rebuild cases that swap
fn stamp_bin(root: &std::path::Path, bin: &std::path::Path) -> ProvisionStamp {
    compute_provision_stamp(&crate::Layout::new(root), &STAGES, "0.2.0", bin, None)
        .expect("the fixture plan must build")
}

fn stamp_for(root: &std::path::Path, stages: &[bm_proto::Stage]) -> ProvisionStamp {
    compute_provision_stamp(
        &crate::Layout::new(root),
        stages,
        "0.2.0",
        &agent_bin(root),
        None,
    )
    .expect("the fixture plan must build")
}

/// The layout a real provision uses on a machine with a workspace selected:
fn workspace_layout(root: &std::path::Path, name: &str) -> crate::Layout {
    crate::Layout {
        root: root.to_path_buf(),
        work: root.join("workspaces").join(name),
        adapter: crate::paths::DEFAULT_ADAPTER.into(),
        engine: crate::paths::DEFAULT_ENGINE.into(),
    }
}

#[test]
fn a_stamp_is_stable_and_content_addressed() {
    let root = stamp_fixture("stable");
    let a = stamp(&root);
    let b = stamp(&root);
    assert_eq!(a, b, "nothing changed, so the stamp must not either");
    assert_eq!(a.sources_hash.len(), 64, "sha-256 hex is 64 chars");
    assert_eq!(a.voices_hash.len(), 64);
    assert!(a.sources_in_sync(&b) && a.voices_in_sync(&b));

    // A cast edit is a source change and nothing else.
    std::fs::write(
        root.join("data/cast-default-vieneu.json"),
        r#"{"Narrator":"Adam"}"#,
    )
    .unwrap();
    let c = stamp(&root);
    assert_ne!(
        a.sources_hash, c.sources_hash,
        "a cast edit must resync sources"
    );
    assert_eq!(
        a.voices_hash, c.voices_hash,
        "…and must not re-enroll voices"
    );

    // A version bump redeploys the agent even when every file is identical.
    let d = stamp_v(&root, "0.3.0");
    assert!(!a.sources_in_sync(&d), "a new agent build must redeploy");
    assert!(
        a.voices_in_sync(&d),
        "the agent version says nothing about voices"
    );
}

#[test]
fn a_workspace_cast_swap_drifts_the_stamp() {
    // The swap writes the workspace cast, not the repo-root one — if the
    let root = stamp_fixture("wscast");
    std::fs::create_dir_all(root.join(".bm")).unwrap();
    std::fs::write(root.join(".bm/active-workspace"), "book\n").unwrap();
    std::fs::create_dir_all(root.join("workspaces/book/data")).unwrap();
    std::fs::write(
        root.join("workspaces/book/data/cast-default-vieneu.json"),
        r#"{"A":"Đức Trí"}"#,
    )
    .unwrap();
    let a = compute_provision_stamp(
        &workspace_layout(&root, "book"),
        &STAGES,
        "0.2.0",
        &agent_bin(&root),
        None,
    )
    .unwrap();
    std::fs::write(
        root.join("workspaces/book/data/cast-default-vieneu.json"),
        r#"{"A":"Quang Sơn"}"#,
    )
    .unwrap();
    let b = compute_provision_stamp(
        &workspace_layout(&root, "book"),
        &STAGES,
        "0.2.0",
        &agent_bin(&root),
        None,
    )
    .unwrap();
    assert_ne!(
        a.sources_hash, b.sources_hash,
        "a workspace voice swap must force a resync"
    );
    assert_eq!(a.voices_hash, b.voices_hash, "…and must not re-enroll");
    let _ = std::fs::remove_dir_all(&root);
}

/// The clone manifest travels in the bundle; a reference clip does not
#[test]
fn the_clone_manifest_is_a_source_and_a_reference_clip_is_not() {
    let root = stamp_fixture("voices");
    let base = stamp(&root);

    // A rename in voices.json has to reach the worker's copy, so it is a
    std::fs::write(root.join("voices.json"), r#"{"Storyteller":"refs/n.wav"}"#).unwrap();
    let renamed = stamp(&root);
    assert!(
        !base.sources_in_sync(&renamed),
        "a rename must resync the manifest"
    );
    assert!(
        base.voices_in_sync(&renamed),
        "…and must not look like a voice-store change"
    );
    assert!(base.tts_in_sync(&renamed), "…nor re-send 668 MB of weights");

    // A new reference clip changes nothing a box holds: it is this machine's
    std::fs::write(root.join("refs/m.wav"), vec![2u8; 64]).unwrap();
    let added = stamp(&root);
    assert!(
        renamed.sources_in_sync(&added),
        "a clip no cell of the manifest names must not resync anything"
    );
    assert!(renamed.tts_in_sync(&added) && renamed.voices_in_sync(&added));
}

/// Widening a box's policy is drift, because the slot list is part of the
#[test]
fn a_wider_policy_is_sources_drift() {
    let root = stamp_fixture("policy");
    std::fs::create_dir_all(root.join("assets/effects")).unwrap();
    std::fs::write(
        root.join("assets/effect-pool.json"),
        r#"{"wind":{"tags":["wind"],"files":["effects/wind-1.mp3"]}}"#,
    )
    .unwrap();
    std::fs::write(root.join("assets/effects/wind-1.mp3"), vec![1u8; 32]).unwrap();

    let render_only = stamp_for(&root, &[bm_proto::Stage::Render]);
    let with_merge = stamp_for(&root, &[bm_proto::Stage::Render, bm_proto::Stage::Merge]);
    assert!(
        !render_only.sources_in_sync(&with_merge),
        "a box given merge must be re-provisioned for the clips it now needs"
    );
    assert_eq!(
        render_only.sources_stages,
        vec!["render@default".to_string()]
    );
    assert_eq!(
        with_merge.sources_stages,
        vec!["render@default".to_string(), "merge@default".to_string()],
        "the slots travel in canonical order, not the policy's own"
    );
    // The order the operator happens to list them in is not content.
    let reordered = stamp_for(&root, &[bm_proto::Stage::Merge, bm_proto::Stage::Render]);
    assert_eq!(with_merge.sources_hash, reordered.sources_hash);
    assert_eq!(with_merge.sources_stages, reordered.sources_stages);
}

/// A new adapter home is drift too, and it is the *second* dimension of the
#[test]
fn a_second_adapter_home_is_sources_drift_for_every_box() {
    let root = stamp_fixture("adapter-drift");
    let before = stamp_for(&root, &[bm_proto::Stage::Digest]);
    assert_eq!(before.sources_stages, vec!["digest@default".to_string()]);

    let home = root.join(crate::paths::ADAPTERS_DIR).join("xianxia-en-US");
    std::fs::create_dir_all(home.join("prompts")).unwrap();
    std::fs::write(home.join("prompts/analyze.txt"), "english").unwrap();
    let after = stamp_for(&root, &[bm_proto::Stage::Digest]);

    assert_eq!(
        after.sources_stages,
        vec!["digest@xianxia-en-US".to_string()]
    );
    assert!(
        !before.sources_in_sync(&after),
        "a language nothing has been pushed yet has to reach the boxes"
    );
    // …and, as with every source change, the weights are not re-sent.
    assert!(before.tts_in_sync(&after) && before.voices_in_sync(&after));
}

/// The store the sidecar loads is its own gate: a newly enrolled voice must
#[test]
fn the_baked_voice_store_is_its_own_gate_and_not_a_model_change() {
    let root = stamp_fixture("store");
    let models = crate::Layout::new(&root).models_dir();
    std::fs::create_dir_all(&models).unwrap();
    std::fs::write(models.join("manifest.json"), r#"{"files":{}}"#).unwrap();
    std::fs::write(models.join("sea_g2p.bin"), vec![1u8; 64]).unwrap();
    std::fs::write(models.join("voices.json"), r#"{"presets":{"A":{}}}"#).unwrap();
    let base = stamp(&root);

    // Enrolling a voice rewrites the store and nothing else.
    std::fs::write(models.join("voices.json"), r#"{"presets":{"A":{},"B":{}}}"#).unwrap();
    let enrolled = stamp(&root);
    assert!(
        !base.voices_in_sync(&enrolled),
        "an enrollment must reach the box"
    );
    assert!(
        base.tts_in_sync(&enrolled),
        "…and must not re-send 668 MB of weights for a 492 KB file"
    );
    assert!(base.sources_in_sync(&enrolled));

    // The weights themselves still drift `tts_hash`, so excluding the store
    std::fs::write(models.join("sea_g2p.bin"), vec![2u8; 128]).unwrap();
    let swapped = stamp(&root);
    assert!(
        !enrolled.tts_in_sync(&swapped),
        "a swapped weight must still resync the store"
    );
    assert!(
        enrolled.voices_in_sync(&swapped),
        "…without looking like an enrollment"
    );
}

/// A rebuilt sidecar reaches a box that already has the right models.
#[test]
fn a_rebuilt_sidecar_drifts_only_the_binary_hash() {
    let root = stamp_fixture("sidecar-drift");
    let bin = root.join("rust/target/x86_64-unknown-linux-gnu/release/bm-tts");
    std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
    std::fs::write(&bin, b"sidecar-bytes-v1").unwrap();
    let base = stamp(&root);
    assert!(!base.tts_bin_hash.is_empty());
    assert!(base.tts_bin_in_sync(&stamp(&root)));

    std::fs::write(&bin, b"sidecar-bytes-v2").unwrap();
    let rebuilt = stamp(&root);
    assert!(
        !base.tts_bin_in_sync(&rebuilt),
        "a rebuilt sidecar must redeploy"
    );
    assert!(
        base.sources_in_sync(&rebuilt) && base.tts_in_sync(&rebuilt),
        "…and must not drag the sources or the weights along"
    );
    assert!(base.agent_in_sync(&rebuilt), "…nor the agent");

    // The cross path is the one that actually gets pushed, and the list
    assert!(
        !rebuilt.tts_bin_hash.is_empty(),
        "the cross build is what provisioning pushes; it must be hashed"
    );

    // No sidecar on this host means no opinion, never a push of nothing.
    let none = compute_provision_stamp(
        &crate::Layout::new(root.join("elsewhere")),
        &STAGES,
        "0.2.0",
        &root.join("a"),
        None,
    )
    .unwrap();
    assert!(base.tts_bin_in_sync(&none));
}

/// A registered clip is a source; a clip nobody registers is not.
#[test]
fn the_clip_pools_are_sources_and_an_unregistered_clip_is_not() {
    let root = stamp_fixture("pools");
    std::fs::create_dir_all(root.join("assets/music")).unwrap();
    std::fs::write(root.join("assets/music/soft-1.mp3"), vec![1u8; 32]).unwrap();
    std::fs::write(
        root.join("assets/music-pool.json"),
        r#"{"soft-1":{"tags":["soft"],"files":["music/soft-1.mp3"]}}"#,
    )
    .unwrap();
    let base = stamp(&root);

    // Re-running with nothing touched must not resync — otherwise every
    assert!(base.sources_in_sync(&stamp(&root)));

    // The registry is a manifest: editing it changes what a scene means,
    std::fs::write(
        root.join("assets/music-pool.json"),
        r#"{"soft-1":{"tags":["calm"],"files":["music/soft-1.mp3"]}}"#,
    )
    .unwrap();
    let edited = stamp(&root);
    assert!(
        !base.sources_in_sync(&edited),
        "a pool edit must resync sources"
    );

    // Replacing the bytes of a clip a registry names resyncs too: the
    std::fs::write(root.join("assets/music/soft-1.mp3"), vec![2u8; 32]).unwrap();
    let swapped = stamp(&root);
    assert!(
        !edited.sources_in_sync(&swapped),
        "a replaced clip must resync sources"
    );

    // …and a clip nothing registers neither travels nor drifts a box.
    std::fs::write(root.join("assets/music/leftover.mp3"), vec![3u8; 32]).unwrap();
    assert!(swapped.sources_in_sync(&stamp(&root)));
}

/// The crawlers a crawl box is given are sources too: an edit there must
#[test]
fn a_workspace_crawler_edit_drifts_the_stamp() {
    let root = stamp_fixture("wscrawl");
    let crawl = [bm_proto::Stage::Crawl];
    let at = |ws: &str| {
        compute_provision_stamp(
            &workspace_layout(&root, ws),
            &crawl,
            "0.2.0",
            &agent_bin(&root),
            None,
        )
        .unwrap()
    };

    std::fs::create_dir_all(root.join("workspaces/book/crawl")).unwrap();
    std::fs::write(root.join("workspaces/book/crawl/site.lua"), "v1").unwrap();
    let a = at("book");

    // Editing the workspace crawler is a source change.
    std::fs::write(root.join("workspaces/book/crawl/site.lua"), "v2").unwrap();
    let b = at("book");
    assert_ne!(
        a.sources_hash, b.sources_hash,
        "a workspace crawler edit must resync sources"
    );

    // A different book is a different set: the crawler that is pushed is
    std::fs::create_dir_all(root.join("workspaces/other/crawl")).unwrap();
    std::fs::write(root.join("workspaces/other/crawl/site.lua"), "other").unwrap();
    let c = at("other");
    assert_ne!(
        b.sources_hash, c.sources_hash,
        "switching workspaces must resync sources"
    );
    // …and a crawl box holds the profile's templates either way: they are
    assert_eq!(c.sources_stages, vec!["crawl@default".to_string()]);
    let _ = std::fs::remove_dir_all(&root);
}

/// A rebuild under the same version redeploys the agent and nothing else.
#[test]
fn a_rebuild_under_the_same_version_redeploys_the_agent_only() {
    let root = stamp_fixture("agent-drift");
    let bin = agent_bin(&root);
    let base = stamp_bin(&root, &bin);
    assert!(base.agent_in_sync(&stamp_bin(&root, &bin)));

    // Same version string, different bytes: drift.
    std::fs::write(&bin, b"agent-bytes-v2").unwrap();
    let rebuilt = stamp_bin(&root, &bin);
    assert!(
        !base.agent_in_sync(&rebuilt),
        "a rebuild must redeploy the agent"
    );
    assert!(
        base.sources_in_sync(&rebuilt),
        "…but must not resync sources"
    );
    assert!(base.tts_in_sync(&rebuilt), "…or touch the sidecar");

    // No local binary to hash means no opinion, never a reinstall loop.
    let nobin = stamp_bin(&root, &root.join("no-such-binary"));
    assert!(nobin.agent_hash.is_empty());
    assert!(base.agent_in_sync(&nobin));
}

/// A stamp written before `tts_hash` existed must still parse. Forcing the
#[test]
fn an_older_stamp_without_a_tts_hash_still_parses() {
    let old = r#"{"agent_version":"0.2.0","sources_hash":"aa","voices_hash":"bb"}"#;
    let s = parse_stamp(old).expect("an older payload must not read as garbage");
    assert_eq!(s.agent_version, "0.2.0");
    assert_eq!(s.tts_hash, "", "an absent field means 'this box has none'");
    assert_eq!(
        s.agent_hash, "",
        "ditto: drift once, then the fresh stamp records it"
    );
    assert_eq!(
        s.tts_bin_hash, "",
        "ditto for the sidecar: one redeploy, then the hash is recorded"
    );
}

/// The Rust sidecar's artifacts are their own hash: a box on the Python path
#[test]
fn the_tts_artifacts_are_tracked_separately_from_the_sources() {
    let root = stamp_fixture("tts");
    let without = stamp(&root);
    assert_eq!(without.tts_hash.len(), 64);
    assert!(
        without.sources_in_sync(&without),
        "a stamp is always in sync with itself"
    );

    // Baking the models moves only the TTS hash.
    let models = crate::Layout::new(&root).models_dir();
    std::fs::create_dir_all(&models).unwrap();
    std::fs::write(models.join("manifest.json"), r#"{"files":{}}"#).unwrap();
    let baked = stamp(&root);
    assert!(
        without.sources_in_sync(&baked),
        "baking models must not resync the Python path's sources"
    );
    assert!(
        !without.tts_in_sync(&baked),
        "baking models must be visible to a box that uses them"
    );

    // …and swapping a model file under an unchanged manifest is drift too.
    std::fs::write(models.join("vieneu_prefill.onnx"), vec![1u8; 64]).unwrap();
    let swapped = stamp(&root);
    assert!(!baked.tts_in_sync(&swapped), "a swapped model must resync");
    assert!(
        baked.sources_in_sync(&swapped),
        "…and must not touch sources"
    );
}

/// A released pack is its own gate, and the sources push is *not* dragged
#[test]
fn a_released_pack_is_its_own_gate() {
    let root = stamp_fixture("pack-gate");
    std::fs::create_dir_all(root.join("assets/effects")).unwrap();
    std::fs::write(
        root.join("assets/effect-pool.json"),
        r#"{"wind":{"tags":["wind"],"files":["effects/wind-1.mp3"]}}"#,
    )
    .unwrap();
    std::fs::write(root.join("assets/effects/wind-1.mp3"), vec![1u8; 32]).unwrap();
    let release = |hash: &str| {
        crate::artifact::PackRelease::for_repo("lhuthng/storycast", "xianxia", "0.1.0", hash)
            .unwrap()
    };
    let at = |pack: Option<&crate::artifact::PackRelease>| {
        compute_provision_stamp(
            &crate::Layout::new(&root),
            &STAGES,
            "0.2.0",
            &agent_bin(&root),
            pack,
        )
        .unwrap()
    };

    let pushed = at(None);
    assert_eq!(
        pushed.pack_release, "",
        "no release means the pack is in the push"
    );
    let first = at(Some(&release("a".repeat(64).as_str())));
    let other = at(Some(&release("b".repeat(64).as_str())));

    assert!(first.pack_in_sync(&first));
    assert!(
        !first.pack_in_sync(&other),
        "a re-pointed pack must be visible, since the bundle is identical either way"
    );
    assert_eq!(
        first.sources_hash, other.sources_hash,
        "…and the pack must not be in the bundle, which is the whole saving"
    );
    assert!(
        !pushed.pack_in_sync(&first),
        "a box pushed the profile in the bundle is not in sync with a released one"
    );
    // …and the other direction: nothing about a pack re-point may re-push
    assert!(first.sources_in_sync(&other));
    assert!(first.voices_in_sync(&other) && first.tts_in_sync(&other));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_stamp_payload_parses_and_garbage_does_not() {
    let s = ProvisionStamp {
        agent_version: "0.2.0".into(),
        sources_hash: "a".repeat(64),
        voices_hash: "b".repeat(64),
        tts_hash: "c".repeat(64),
        agent_hash: "d".repeat(64),
        tts_bin_hash: "e".repeat(64),
        sources_stages: vec!["digest".into(), "merge".into()],
        pack_release: "f".repeat(64),
    };
    let text = serde_json::to_string(&s).unwrap();
    assert_eq!(parse_stamp(&text).unwrap(), s, "a real payload round-trips");
    assert!(parse_stamp("").is_none());
    assert!(
        parse_stamp("not json").is_none(),
        "a truncated file is a cache miss, never a crash"
    );
    // The failed-round-trip half, through the function both readers use
    assert_eq!(
        stamp_from(0, &text).unwrap(),
        s,
        "exit 0 and a real payload is a stamp"
    );
    assert!(
        stamp_from(1, &text).is_none(),
        "a failed `cat` is a cache miss even when it printed a valid payload: \
         stdout of a non-zero exit is whatever the failure wrote"
    );
}
