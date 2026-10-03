use super::*;

#[test]
fn resolve_clip_expands_tilde_and_falls_back_to_the_root() {
    // `~` without a shell.
    let _env = crate::ENV_LOCK.lock().unwrap();
    let home = std::env::temp_dir().join("bm-clip-home");
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("young-male-8.mp3"), b"fake").unwrap();
    let saved = std::env::var("HOME").ok();
    std::env::set_var("HOME", &home);
    let got = resolve_clip(
        Path::new("/nonexistent-root"),
        Path::new("~/young-male-8.mp3"),
    );
    assert_eq!(got, home.join("young-male-8.mp3"));
    if let Some(h) = saved {
        std::env::set_var("HOME", h);
    } else {
        std::env::remove_var("HOME");
    }

    // Relative, missing from the working directory: found under the root.
    let root = std::env::temp_dir().join("bm-clip-root");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("in")).unwrap();
    std::fs::write(root.join("in/young-male-9.mp3"), b"fake").unwrap();
    let got = resolve_clip(&root, Path::new("in/young-male-9.mp3"));
    assert_eq!(got, root.join("in/young-male-9.mp3"));

    // End to end through add_sample with a root-relative path.
    let log = add_sample(
        &crate::Layout::new(&root),
        Path::new("in/young-male-9.mp3"),
        None,
        None,
    )
    .unwrap();
    assert!(log.iter().any(|l| l.contains("young-male-9")), "{log:?}");
    assert!(root.join("refs/young-male-9.mp3").is_file());

    // Missing everywhere: the error names what was typed.
    let err = add_sample(
        &crate::Layout::new(&root),
        Path::new("nope/young-male-9.mp3"),
        None,
        None,
    )
    .unwrap_err();
    assert!(err.to_string().contains("nope/young-male-9.mp3"), "{err}");
}

#[test]
fn filename_tags_drop_the_take_number() {
    assert_eq!(parse_sample_tags("young-female-1"), vec!["young", "female"]);
    assert_eq!(parse_sample_tags("old_male_12"), vec!["old", "male"]);
    assert_eq!(parse_sample_tags("shizuka"), vec!["shizuka"]);
    assert!(
        parse_sample_tags("007").is_empty(),
        "a bare number is no tag"
    );
}

#[test]
fn young_male_cannot_take_a_young_female_sample() {
    let sample = vec!["young".to_string(), "female".to_string()];
    let has = |tags: &[&str]| tags.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert!(
        compatible(&sample, &has(&["young"])),
        "shares young, clashes on nothing"
    );
    assert!(compatible(&sample, &has(&["female"])), "shares female");
    assert!(
        compatible(&sample, &has(&["young", "female"])),
        "shares both"
    );
    assert!(
        !compatible(&sample, &has(&["young", "male"])),
        "male clashes with female"
    );
    assert!(
        !compatible(&sample, &has(&["old", "female"])),
        "old clashes with young"
    );
    assert!(
        !compatible(&sample, &has(&["male"])),
        "no shared tag at all"
    );
    assert!(
        !compatible(&sample, &has(&[])),
        "untagged characters fall back to presets"
    );
}

#[test]
fn free_form_tags_match_by_equality_only() {
    let sample = vec!["sly".to_string()];
    let has = |tags: &[&str]| tags.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert!(compatible(&sample, &has(&["sly", "young"])));
    assert!(
        !compatible(&sample, &has(&["young"])),
        "unknown tags give no antonyms, only overlap"
    );
}

#[test]
fn hints_backfill_tags_for_untagged_bible_entries() {
    assert_eq!(tags_from_hint("elderly male, stern"), vec!["old", "male"]);
    assert_eq!(tags_from_hint("girl, lively"), vec!["young", "female"]);
    assert_eq!(tags_from_hint("boy, polite"), vec!["young", "male"]);
    // Female-first: "female" contains "male".
    assert_eq!(tags_from_hint("adult female, cold"), vec!["female"]);
    assert_eq!(tags_from_hint("adult male, warm"), vec!["male"]);
    assert!(tags_from_hint("ageless genderless system").is_empty());
}

#[test]
fn candidates_are_compatible_names_sorted() {
    let mut pool = Pool::new();
    pool.insert(
        "young-female-1".into(),
        PoolEntry {
            file: "refs/young-female-1.mp3".into(),
            tags: vec!["young".into(), "female".into()],
        },
    );
    pool.insert(
        "young-female-2".into(),
        PoolEntry {
            file: "refs/young-female-2.mp3".into(),
            tags: vec!["young".into(), "female".into()],
        },
    );
    pool.insert(
        "old-male-1".into(),
        PoolEntry {
            file: "refs/old-male-1.mp3".into(),
            tags: vec!["old".into(), "male".into()],
        },
    );
    let got = candidates(&pool, &["young".to_string(), "female".to_string()]);
    assert_eq!(got, vec!["young-female-1", "young-female-2"]);
    assert!(candidates(&pool, &[]).is_empty());
}

#[test]
fn a_missing_or_broken_registry_is_an_empty_pool() {
    assert!(load_pool(Path::new("/nonexistent/voice-pool.json")).is_empty());
    let d = std::env::temp_dir().join("bm-pool-broken");
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("voice-pool.json"), "{ nope").unwrap();
    assert!(load_pool(&d.join("voice-pool.json")).is_empty());
}

#[test]
fn add_sample_registers_pool_and_enrollment_together() {
    let d = std::env::temp_dir().join("bm-pool-add");
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let src = d.join("young-female-9.mp3");
    std::fs::write(&src, b"fake-audio").unwrap();

    let log = add_sample(&crate::Layout::new(&d), &src, None, None).unwrap();
    assert!(log.iter().any(|l| l.contains("young-female-9")), "{log:?}");
    assert!(d.join("refs/young-female-9.mp3").is_file());
    // No venv here: enrollment defers to provisioning, loudly, not silently.
    assert!(log.iter().any(|l| l.contains("next provision")), "{log:?}");

    let pool = load_pool(&d.join("voice-pool.json"));
    assert_eq!(pool["young-female-9"].tags, vec!["young", "female"]);
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(d.join("voices.json")).unwrap()).unwrap();
    assert_eq!(manifest["young-female-9"], "refs/young-female-9.mp3");

    // Adding the same file twice is idempotent, not an error.
    let again = add_sample(&crate::Layout::new(&d), &src, None, None).unwrap();
    assert!(
        again.iter().any(|l| l.contains("already in place")),
        "{again:?}"
    );
}

#[test]
fn venv_order_prefers_the_managed_store() {
    // Enrollment and serving must resolve the same interpreter, or a
    let d = std::env::temp_dir().join("bm-pool-venvs");
    let _ = std::fs::remove_dir_all(&d);
    let managed = d.join("python/.venv/bin/python");
    let legacy = d.join(".venv/bin/python");
    std::fs::create_dir_all(managed.parent().unwrap()).unwrap();
    std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    std::fs::write(&managed, b"x").unwrap();
    std::fs::write(&legacy, b"x").unwrap();
    let layout = crate::Layout::new(&d);
    assert_eq!(layout.venv_python().as_deref(), Some(managed.as_path()));
    std::fs::remove_file(&managed).unwrap();
    assert_eq!(layout.venv_python().as_deref(), Some(legacy.as_path()));
    std::fs::remove_file(&legacy).unwrap();
    assert_eq!(layout.venv_python(), None);
}

#[test]
fn preview_without_a_venv_says_so_instead_of_500ing() {
    let d = std::env::temp_dir().join("bm-pool-no-venv-preview");
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let err = synth_preview(&d, "Đức Trí", "xin chào", &d.join("p.wav")).unwrap_err();
    assert!(err.to_string().contains("no local voice store"), "{err}");
}

#[test]
fn enroll_without_a_venv_defers_to_provisioning() {
    let d = std::env::temp_dir().join("bm-pool-no-venv");
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    // Positive path needs model weights and minutes; the skip contract —
    let lines = enroll_local(&d, "young-female-1", "refs/young-female-1.mp3").unwrap();
    assert!(
        lines.iter().any(|l| l.contains("next provision")),
        "{lines:?}"
    );
}

#[test]
fn a_renamed_voice_is_named_not_pooled() {
    // `refs/narrator.mp3 as Narrator`: the registry and the enrollment
    let d = std::env::temp_dir().join("bm-pool-rename");
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let src = d.join("narrator.mp3");
    std::fs::write(&src, b"fake-audio").unwrap();

    let log = add_sample(&crate::Layout::new(&d), &src, None, Some("Narrator".into())).unwrap();
    assert!(log.iter().any(|l| l.contains("named voice")), "{log:?}");
    let pool = load_pool(&d.join("voice-pool.json"));
    assert!(pool["Narrator"].tags.is_empty());
    assert!(
        !pool.contains_key("narrator"),
        "the stem must not leak in as a second voice"
    );
    assert!(
        candidates(&pool, &["young".to_string(), "female".to_string()]).is_empty(),
        "a named voice rolls for nobody"
    );
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(d.join("voices.json")).unwrap()).unwrap();
    assert_eq!(manifest["Narrator"], "refs/narrator.mp3");
}

#[test]
fn explicit_tags_still_pool_a_renamed_voice() {
    // The power-user shape: custom name AND rotation tags.
    let d = std::env::temp_dir().join("bm-pool-rename-tags");
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let src = d.join("clip.mp3");
    std::fs::write(&src, b"fake-audio").unwrap();

    add_sample(
        &crate::Layout::new(&d),
        &src,
        Some(vec!["old".into(), "male".into()]),
        Some("Lão".into()),
    )
    .unwrap();
    let pool = load_pool(&d.join("voice-pool.json"));
    assert_eq!(pool["Lão"].tags, vec!["old", "male"]);
    assert_eq!(
        candidates(&pool, &["old".to_string(), "male".to_string()]),
        vec!["Lão".to_string()]
    );
}

#[test]
fn add_sample_enrolls_a_clip_store_and_leaves_vieneus_alone() {
    // `:N` on a pocket workspace: the engine's store takes the clip, and
    let d = std::env::temp_dir().join("bm-pool-add-pocket");
    let _ = std::fs::remove_dir_all(&d);
    let mut layout = crate::Layout::new(&d);
    layout.engine = "pocket".to_string();
    std::fs::create_dir_all(layout.models_dir()).unwrap();
    std::fs::create_dir_all(d.join(".venv/bin")).unwrap();
    std::fs::write(d.join(".venv/bin/python"), b"x").unwrap();
    std::fs::write(
        layout.tts_voices(),
        r#"{"presets":{"alba":{"file":"voices/alba.safetensors"}}}"#,
    )
    .unwrap();
    let src = d.join("Maomao.wav");
    crate::assemble::silent_wav(&src, 1.0, 24_000).unwrap();

    // Exactly the TUI's `:N` shape: an explicit (empty) tag list, which is
    let log = add_sample(&layout, &src, Some(Vec::new()), Some("Maomao".into())).unwrap();
    assert!(
        log.iter()
            .any(|l| l.contains("engine store: Maomao -> refs/maomao.wav")),
        "{log:?}"
    );
    assert!(
        !log.iter().any(|l| l.contains("no local voice store")),
        "the preset lane must not run for a clip store: {log:?}"
    );
    let store: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(layout.tts_voices()).unwrap()).unwrap();
    assert_eq!(store["presets"]["Maomao"]["file"], "refs/maomao.wav");
    assert!(layout.models_dir().join("refs/maomao.wav").is_file());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn bake_merges_only_manifest_voices_missing_from_the_store() {
    // Wolf's box: enrolled in the venv store, absent from the pushed
    let d = std::env::temp_dir().join("bm-pool-bake");
    let _ = std::fs::remove_dir_all(&d);
    let assets = d.join(".venv/lib/python3.12/site-packages/vieneu/assets");
    let store = crate::Layout::new(&d).tts_voices();
    std::fs::create_dir_all(&assets).unwrap();
    std::fs::create_dir_all(store.parent().unwrap()).unwrap();
    std::fs::create_dir_all(d.join(".venv/bin")).unwrap();
    std::fs::write(d.join(".venv/bin/python"), b"x").unwrap();
    std::fs::write(
        d.join("voices.json"),
        r#"{"Have":"refs/h.mp3","Want":"refs/w.mp3","Ghost":"refs/g.mp3","_note":"x"}"#,
    )
    .unwrap();
    std::fs::write(
        &store,
        r#"{"meta":{},"default_voice":"Have","presets":{"Have":{"emb":[1]}}}"#,
    )
    .unwrap();
    std::fs::write(
        assets.join("voices_v3_turbo.json"),
        r#"{"presets":{"Want":{"emb":[2]},"Else":{"emb":[3]}}}"#,
    )
    .unwrap();

    let layout = crate::Layout::new(&d);
    assert_eq!(bake_missing_voices(&layout), vec!["Want".to_string()]);
    let bake: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(layout.tts_voices()).unwrap()).unwrap();
    assert_eq!(bake["presets"]["Have"]["emb"], serde_json::json!([1]));
    assert_eq!(bake["presets"]["Want"]["emb"], serde_json::json!([2]));
    assert!(
        bake["presets"].get("Else").is_none(),
        "unmentioned presets never ride along"
    );
    assert!(
        bake["presets"].get("Ghost").is_none(),
        "enrolled nowhere stays missing for the warning"
    );
    // Idempotent: nothing missing, nothing written.
    assert!(bake_missing_voices(&layout).is_empty());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn bake_never_writes_vieneu_presets_into_another_engines_store() {
    // The pocket shape: `models/voices.json` is a `file` per voice, not a
    let d = std::env::temp_dir().join("bm-pool-bake-pocket");
    let _ = std::fs::remove_dir_all(&d);
    let assets = d.join(".venv/lib/python3.12/site-packages/vieneu/assets");
    let mut layout = crate::Layout::new(&d);
    layout.engine = "pocket".to_string();
    std::fs::create_dir_all(&assets).unwrap();
    std::fs::create_dir_all(layout.models_dir()).unwrap();
    std::fs::create_dir_all(d.join(".venv/bin")).unwrap();
    std::fs::write(d.join(".venv/bin/python"), b"x").unwrap();
    std::fs::write(layout.voices_manifest(), r#"{"Want":"refs/w.mp3"}"#).unwrap();
    std::fs::write(
        assets.join("voices_v3_turbo.json"),
        r#"{"presets":{"Want":{"emb":[2]}}}"#,
    )
    .unwrap();
    let store = r#"{"presets":{"alba":{"file":"refs/deep-voice.wav"}}}"#;
    std::fs::write(layout.tts_voices(), store).unwrap();

    // The clip `refs/w.mp3` does not exist, so there is nothing to
    assert!(bake_missing_voices(&layout).is_empty());
    let after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(layout.tts_voices()).unwrap()).unwrap();
    assert!(
        after["presets"].get("Want").is_none(),
        "no VieNeu preset may enter a clip store: {after}"
    );
    assert_eq!(after["presets"]["alba"]["file"], "refs/deep-voice.wav");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn bake_enrolls_a_clip_store_from_the_books_own_refs() {
    // The other half of the same declaration: where a preset store is
    let d = std::env::temp_dir().join("bm-pool-bake-clips");
    let _ = std::fs::remove_dir_all(&d);
    let mut layout = crate::Layout::new(&d);
    layout.engine = "pocket".to_string();
    std::fs::create_dir_all(layout.models_dir()).unwrap();
    std::fs::create_dir_all(layout.work.join("refs")).unwrap();
    // The book's own clip, already the store's house shape.
    let clip = layout.work.join("refs/Maomao.wav");
    crate::assemble::silent_wav(&clip, 1.0, 24_000).unwrap();
    std::fs::write(
        layout.voices_manifest(),
        r#"{"Maomao":"refs/Maomao.wav","Ghost":"refs/ghost.mp3","_note":"x"}"#,
    )
    .unwrap();
    std::fs::write(
        layout.tts_voices(),
        r#"{"presets":{"alba":{"file":"voices/alba.safetensors"}}}"#,
    )
    .unwrap();

    assert_eq!(bake_missing_voices(&layout), vec!["Maomao".to_string()]);
    let store: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(layout.tts_voices()).unwrap()).unwrap();
    assert_eq!(store["presets"]["Maomao"]["file"], "refs/maomao.wav");
    let copied = layout.models_dir().join("refs/maomao.wav");
    assert_eq!(
        std::fs::read(&copied).unwrap(),
        std::fs::read(&clip).unwrap(),
        "an already-house-shape clip travels byte for byte"
    );
    assert!(
        store["presets"].get("Ghost").is_none(),
        "a clip that is not there stays missing for the provision warning"
    );
    assert_eq!(store["presets"]["alba"]["file"], "voices/alba.safetensors");
    // Idempotent: the entry is held, so a second run writes nothing and
    let before = std::fs::read_to_string(layout.tts_voices()).unwrap();
    assert!(bake_missing_voices(&layout).is_empty());
    assert_eq!(
        std::fs::read_to_string(layout.tts_voices()).unwrap(),
        before
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn bake_without_a_venv_or_bake_is_a_quiet_noop() {
    let d = std::env::temp_dir().join("bm-pool-bake-none");
    let _ = std::fs::remove_dir_all(&d);
    let layout = crate::Layout::new(&d);
    std::fs::create_dir_all(layout.models_dir()).unwrap();
    std::fs::write(d.join("voices.json"), r#"{"Want":"refs/w.mp3"}"#).unwrap();
    std::fs::write(layout.tts_voices(), r#"{"presets":{"Have":{}}}"#).unwrap();
    // No venv here, so nothing can be baked — and nothing breaks.
    assert!(bake_missing_voices(&layout).is_empty());
    // No bake file at all: also nothing, not an error.
    std::fs::remove_file(layout.tts_voices()).unwrap();
    assert!(bake_missing_voices(&layout).is_empty());
    let _ = std::fs::remove_dir_all(&d);
}
