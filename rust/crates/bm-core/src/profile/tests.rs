use super::*;

fn live_fixture(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bm-profile-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    install_fixture(&dir).unwrap();
    dir
}

#[test]
fn concurrent_hashing_matches_hashing_one_at_a_time() {
    // The whole safety argument for the parallel path: the pointer on every
    // machine was computed sequentially, so a different map is a different
    // hash — a false "profile drift" on every box at once. Cross-check the
    // concurrent implementation against the sequential one, same primitive.
    let dir = live_fixture("parallel");
    let files = live_files(&dir);
    assert!(files.len() > 1, "the fixture must have something to spread");

    let concurrent = hash_files(&dir, files.clone()).unwrap();
    let mut sequential = BTreeMap::new();
    for p in &files {
        sequential.insert(
            p.strip_prefix(&dir).unwrap().display().to_string(),
            file_hash(p).unwrap(),
        );
    }
    assert_eq!(concurrent, sequential, "same files, same map");
    // And the fold over it is what the pointer holds.
    assert_eq!(manifest_hash(&concurrent), manifest_hash(&sequential));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn verify_passes_on_a_fresh_tree_and_adopts_drift() {
    let dir = live_fixture("verify");
    let hash = manifest_hash(&hash_live(&dir).unwrap());
    write_pointer(
        &dir,
        &Pointer {
            name: "fixture".into(),
            hash,
            version: String::new(),
        },
    )
    .unwrap();
    assert_eq!(verify(&dir).unwrap().name, "fixture");

    // A hand edit (e.g. a `:sound` retune) is adopted, not refused: the
    // pointer is re-stamped so the next run is clean.
    std::fs::write(dir.join("prompts/analyze.txt"), "tampered").unwrap();
    let adopted = verify(&dir).unwrap();
    assert_eq!(adopted.name, "fixture");
    assert_eq!(adopted, read_pointer(&dir).unwrap());
    // And the adopted pointer verifies cleanly afterwards.
    assert_eq!(verify(&dir).unwrap(), adopted);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn verify_without_a_pointer_names_the_load() {
    let dir = std::env::temp_dir().join("bm-profile-nopointer");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let err = verify(&dir).unwrap_err();
    assert!(err.to_string().contains("no profile loaded"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_fixture_resolves_from_any_crate() {
    // install_fixture anchors on bm-core's manifest dir, not the
    // caller's cwd — a bm-inductor test lands in the same fixture.
    let dir = std::env::temp_dir().join("bm-profile-anchor");
    let _ = std::fs::remove_dir_all(&dir);
    install_fixture(&dir).unwrap();
    assert!(dir.join("assets/scene-map.json").is_file());
    assert!(dir.join("prompts/analyze.txt").is_file());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_pre_split_pointer_becomes_the_pack_piece() {
    let dir = live_fixture("legacy");
    let hash = manifest_hash(&hash_live(&dir).unwrap());
    std::fs::create_dir_all(dir.join(".bm")).unwrap();
    // The document as it exists on every checkout today.
    std::fs::write(
        pointer_path(&dir),
        format!(r#"{{"name":"xianxia","hash":"{hash}"}}"#),
    )
    .unwrap();

    let b = read_binding(&dir).unwrap();
    assert_eq!(b.pack.name, "xianxia");
    assert_eq!(b.pack.hash, hash);
    assert!(
        b.adapter.name.is_empty() && b.engine.name.is_empty(),
        "the other two pieces are unnamed, not guessed at"
    );
    // And the six callers that still want "the profile" keep working.
    assert_eq!(read_pointer(&dir).unwrap().name, "xianxia");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The binding in force is the workspace's, piece by piece — the same
/// merge `Layout::resolve` gives the adapter and the engine, so the
/// dashboard, `profile check` and the serve gate all answer with one voice.
#[test]
fn the_binding_in_force_merges_the_workspace_and_the_checkout_piece_by_piece() {
    let dir = live_fixture("in-force");
    write_binding(
        &dir,
        &Binding {
            pack: Pointer {
                name: "xianxia".into(),
                hash: "c".into(),
                version: String::new(),
            },
            adapter: Pointer {
                name: "vi-VN".into(),
                hash: "a".into(),
                version: String::new(),
            },
            engine: Pointer::default(),
        },
    )
    .unwrap();
    // The workspace names its own pack and engine and leaves the adapter to
    // the checkout.
    let work = dir.join("workspaces/book");
    std::fs::create_dir_all(&work).unwrap();
    let settings = crate::config::Settings {
        profile: Binding {
            pack: Pointer {
                name: "apothecary".into(),
                hash: "w".into(),
                version: String::new(),
            },
            adapter: Pointer::default(),
            engine: Pointer {
                name: "pocket".into(),
                hash: String::new(),
                version: String::new(),
            },
        },
        ..crate::config::Settings::default()
    };
    settings.save(&work.join("settings.json")).unwrap();
    let layout = crate::paths::Layout {
        root: dir.clone(),
        work: work.clone(),
        ..crate::paths::Layout::new(dir.clone())
    };
    let b = in_force(&layout).unwrap();
    assert_eq!(b.pack.name, "apothecary");
    assert_eq!(
        b.adapter.name, "vi-VN",
        "the checkout's language fills the gap"
    );
    assert_eq!(b.engine.name, "pocket");
    // The implicit root workspace still reads the pointer alone.
    let root_layout = crate::paths::Layout::new(dir.clone());
    assert_eq!(in_force(&root_layout).unwrap(), read_binding(&dir).unwrap());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Running a workspace verifies *its* binding and writes neither side: the
/// workspace's `settings.json` is the ledger's stamp, so re-stamping it at
/// load would make the next `serve` refuse a consistent book; and the
/// checkout's `.bm/profile` is never this workspace's to write.
#[test]
fn verify_layout_reads_the_workspace_binding_and_writes_neither_side() {
    let dir = live_fixture("verify-ws");
    let before = Binding {
        pack: Pointer {
            name: "xianxia".into(),
            hash: "checkout".into(),
            version: String::new(),
        },
        adapter: Pointer {
            name: "vi-VN".into(),
            hash: "a".into(),
            version: String::new(),
        },
        engine: Pointer::default(),
    };
    write_binding(&dir, &before).unwrap();

    let work = dir.join("workspaces/book");
    std::fs::create_dir_all(work.join("assets")).unwrap();
    std::fs::write(work.join("assets/world.json"), "{}").unwrap();
    let settings = crate::config::Settings {
        profile: Binding {
            pack: Pointer {
                name: "book".into(),
                hash: String::new(),
                version: String::new(),
            },
            adapter: Pointer::default(),
            engine: Pointer {
                name: "pocket".into(),
                hash: String::new(),
                version: String::new(),
            },
        },
        ..crate::config::Settings::default()
    };
    settings.save(&work.join("settings.json")).unwrap();

    let layout = crate::paths::Layout {
        root: dir.clone(),
        work: work.clone(),
        ..crate::paths::Layout::new(dir.clone())
    };
    let b = verify_layout(&layout, Some("pocket")).unwrap();
    assert_eq!(b.pack.name, "book");
    assert_eq!(
        b.adapter.name, "vi-VN",
        "the checkout's language fills the gap"
    );
    assert_eq!(b.engine.name, "pocket", "named from settings");
    let stamped = crate::config::Settings::load(&work.join("settings.json")).profile;
    assert_eq!(
        stamped.pack.hash, "",
        "the workspace binding was not re-stamped"
    );
    assert_eq!(
        read_binding(&dir).unwrap(),
        before,
        "the checkout pointer is untouched"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_binding_round_trips_and_names_all_three_pieces() {
    let dir = live_fixture("binding");
    let b = Binding {
        pack: Pointer {
            name: "xianxia".into(),
            hash: "p".into(),
            version: String::new(),
        },
        adapter: Pointer {
            name: "vi-VN".into(),
            hash: "a".into(),
            version: String::new(),
        },
        engine: Pointer {
            name: "vieneu".into(),
            hash: "e".into(),
            version: String::new(),
        },
    };
    write_binding(&dir, &b).unwrap();
    assert_eq!(read_binding(&dir).unwrap(), b);
    assert!(!b.is_unset());
    // The legacy view still resolves, and re-stamping the pack keeps the
    // adapter and engine exactly as they were.
    write_pointer(
        &dir,
        &Pointer {
            name: "xianxia".into(),
            hash: "p2".into(),
            version: String::new(),
        },
    )
    .unwrap();
    let after = read_binding(&dir).unwrap();
    assert_eq!(after.pack.hash, "p2");
    assert_eq!(after.adapter.name, "vi-VN");
    assert_eq!(after.engine.name, "vieneu");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn verify_hashes_each_tree_separately_and_leaves_the_other_alone() {
    let dir = live_fixture("split");
    write_binding(
        &dir,
        &Binding {
            pack: Pointer {
                name: "xianxia".into(),
                hash: String::new(),
                version: String::new(),
            },
            adapter: Pointer {
                name: "vi-VN".into(),
                hash: String::new(),
                version: String::new(),
            },
            engine: Pointer::default(),
        },
    )
    .unwrap();

    let before = verify_binding(&dir, Some("vieneu")).unwrap();
    assert!(!before.pack.hash.is_empty(), "the pack was hashed");
    assert!(!before.adapter.hash.is_empty(), "so was the adapter");
    assert_eq!(before.engine.name, "vieneu", "named from settings");

    // The claim the split exists for: one tree moving does not move the
    // other, and the piece that moved is the one that gets re-stamped.
    std::fs::write(dir.join("prompts/analyze.txt"), "tampered").unwrap();
    let after = verify_binding(&dir, Some("vieneu")).unwrap();
    assert_eq!(after.pack.hash, before.pack.hash, "the pack did not move");
    assert_ne!(after.adapter.hash, before.adapter.hash, "the adapter did");
    assert_eq!(after, read_binding(&dir).unwrap(), "and it was re-stamped");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The point of keying a release's manifest by *where it unpacks to*: the
/// bundle and the live tree it came from hash to the same number, so loading
/// one never re-stamps a hash it just changed. A language release is only its
/// own two trees — nothing of the pack's rides along, which is the split.
#[test]
fn a_pieces_manifest_hashes_to_what_the_binding_holds() {
    let dir = live_fixture("manifest");
    let layout = crate::paths::Layout::new(&dir);
    write_binding(
        &dir,
        &Binding {
            pack: Pointer {
                name: "xianxia".into(),
                hash: String::new(),
                version: String::new(),
            },
            adapter: Pointer {
                name: "vi-VN".into(),
                hash: String::new(),
                version: String::new(),
            },
            engine: Pointer::default(),
        },
    )
    .unwrap();
    let verified = verify_binding(&dir, Some("vieneu")).unwrap();

    let pack = compute_manifest(&layout, Piece::Pack, "xianxia", "1").unwrap();
    assert_eq!(pack.piece, "pack");
    assert_eq!(manifest_hash(&pack.files), verified.pack.hash);
    assert!(pack.deps.is_empty(), "the fixture was built on nothing");
    assert!(pack.files.keys().any(|k| k.starts_with("assets/")));

    let language = compute_manifest(&layout, Piece::Adapter, "vi-VN", "1").unwrap();
    assert_eq!(language.piece, "adapter");
    assert_eq!(manifest_hash(&language.files), verified.adapter.hash);
    assert!(
        language
            .files
            .keys()
            .all(|k| k.starts_with("prompts/") || k.starts_with("crawl/")),
        "a language release carries its own two trees and nothing else: {:?}",
        language.files.keys().take(3).collect::<Vec<_>>()
    );
    assert!(language.deps.is_empty(), "a language is built on nothing");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A workspace that composes its own `assets/` is the tree a pack manifest
/// hashes — not the checkout's.
///
/// The manifest is keyed by the paths `push_pack` rsyncs and the receipt a
/// box diffs against, so a manifest rooted at the checkout while the tree
/// in force is the workspace's would describe bytes that never travelled:
/// the pack release gate would compare a box's receipt to the wrong book
/// and re-push (or skip) forever. This is the same split `sources.rs`
/// enforces for the bundle, one layer up.
#[test]
fn a_workspace_owned_pack_manifests_the_workspace_tree_not_the_checkouts() {
    let root = live_fixture("ws-pack-manifest");
    // The checkout keeps its own `assets/`, so a root-layout manifest is
    // still buildable — the assertion is that the workspace's is not it.
    let workspace = root.join("workspaces/book");
    std::fs::create_dir_all(workspace.join("assets")).unwrap();
    std::fs::write(workspace.join("assets/scene-map.json"), r#"{"scenes":[]}"#).unwrap();
    std::fs::write(workspace.join("assets/only-here.json"), "{}").unwrap();
    let layout = crate::paths::Layout {
        root: root.clone(),
        work: workspace.clone(),
        adapter: crate::paths::DEFAULT_ADAPTER.into(),
        engine: crate::paths::DEFAULT_ENGINE.into(),
    };
    assert!(layout.owns_assets(), "the fixture is the case under test");

    let pack = compute_manifest(&layout, Piece::Pack, "book", "1").unwrap();
    assert!(
        pack.files.keys().any(|k| k == "assets/only-here.json"),
        "the manifest must name the workspace's own file: {:?}",
        pack.files.keys().take(5).collect::<Vec<_>>()
    );
    let checkout =
        compute_manifest(&crate::paths::Layout::new(&root), Piece::Pack, "book", "1").unwrap();
    assert_ne!(
        manifest_hash(&pack.files),
        manifest_hash(&checkout.files),
        "the workspace's own tree is a different pack from the checkout's"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// One file per piece, in a directory per piece — so `xianxia` the pack and
/// `xianxia` the language cannot shadow each other — and the engine, which
/// is not a bundle at all.
#[test]
fn a_release_is_one_file_per_piece_and_the_engine_is_not_one() {
    let root = Path::new("/repo");
    assert!(release_path(root, Piece::Pack, "xianxia").ends_with("profiles/pack/xianxia.tar.zst"));
    assert!(
        release_path(root, Piece::Adapter, "xianxia").ends_with("profiles/adapter/xianxia.tar.zst")
    );
    assert_ne!(
        release_path(root, Piece::Pack, "xianxia"),
        release_path(root, Piece::Adapter, "xianxia"),
        "the same name on two axes is two releases"
    );
    assert_eq!(Piece::from_noun("adapter"), Some(Piece::Adapter));
    assert_eq!(Piece::from_noun("engine"), Some(Piece::Engine));
    assert_eq!(
        Piece::from_noun("language"),
        None,
        "no piece answers to that"
    );

    let dir = live_fixture("engine-release");
    let layout = crate::paths::Layout::new(&dir);
    let err = compute_manifest(&layout, Piece::Engine, "vieneu", "1")
        .unwrap_err()
        .to_string();
    assert!(err.contains("models release"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Editing a parent is what makes a child stale, and the check *names* the
/// parent: the composition record holds the hash each dependency was folded
/// in at, so nothing has to be guessed at or re-hashed to answer it.
#[test]
fn a_release_is_stale_when_a_dependency_it_was_built_on_has_moved() {
    let dir = live_fixture("stale-deps");
    let layout = crate::paths::Layout::new(&dir);
    assert!(stale_dependencies(&layout).unwrap().is_empty(), "no deps");

    let assets = layout.assets();
    std::fs::write(assets.join("pack.json"), r#"{"deps":["common"]}"#).unwrap();
    let dep = assets.join("_extends/common");
    std::fs::create_dir_all(&dep).unwrap();
    let pool = |extra: &str| {
        format!(r#"{{"wind":{{"tags":["wind"],"files":["effects/wind-1.mp3"]}}{extra}}}"#)
    };
    std::fs::write(dep.join("effect-pool.json"), pool("")).unwrap();
    // The record is written by a *resolve*; until then there is nothing to
    // compare against, and a first sighting is not staleness.
    crate::compose::resolve(&assets, false).unwrap();
    assert!(stale_dependencies(&layout).unwrap().is_empty());

    // The parent gains a sound, so this tree is built on something that has
    // moved — and stays so until it is resolved again.
    std::fs::write(
        dep.join("effect-pool.json"),
        pool(r#","rain":{"tags":["rain"],"files":["effects/rain-1.mp3"]}"#),
    )
    .unwrap();
    assert_eq!(stale_dependencies(&layout).unwrap(), vec!["common"]);
    crate::compose::resolve(&assets, false).unwrap();
    assert!(stale_dependencies(&layout).unwrap().is_empty(), "resolved");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A composition input is not this pack's own content. `assets/_extends/`
/// holds other assets' whole trees, so hashing them would double the digest
/// and make a dependency's edit read as this pack's own — while the
/// *resolved* result, which is what every reader and every worker sees, is
/// hashed as it always was.
#[test]
fn a_dependency_tree_is_not_part_of_the_packs_own_digest() {
    let dir = live_fixture("extends");
    let before = hash_live(&dir).unwrap();
    assert!(!before.keys().any(|k| k.contains("_extends")));

    std::fs::create_dir_all(dir.join("assets/_extends/common/effects")).unwrap();
    std::fs::write(
        dir.join("assets/_extends/common/effects/wind-1.mp3"),
        b"clip",
    )
    .unwrap();
    assert_eq!(
        hash_live(&dir).unwrap(),
        before,
        "an unpacked dependency is an input, not a change to the pack"
    );

    // The resolved result, on the other hand, *is* the pack.
    std::fs::create_dir_all(dir.join("assets/effects")).unwrap();
    std::fs::write(dir.join("assets/effects/wind-1.mp3"), b"clip").unwrap();
    assert_ne!(hash_live(&dir).unwrap(), before);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The sanitized dependency release: what a root pack publishes. The
/// manifest keys are the unpack paths, so `manifest_hash` over them equals
/// `tree_hash` over the dependency — the exact number the composition
/// record carries — minus the bookkeeping, which is not content.
#[test]
fn a_dependency_release_unpacks_to_the_paths_the_record_hashes() {
    let dir = live_fixture("dep-release");
    let layout = crate::paths::Layout::new(&dir);
    let dep_dir = dir.join("assets/_extends/common");
    std::fs::create_dir_all(dep_dir.join("effects")).unwrap();
    std::fs::write(dep_dir.join("effects/wind-1.mp3"), b"clip").unwrap();
    std::fs::write(
        dep_dir.join("effect-pool.json"),
        r#"{ "_note": "the world's", "wind": { "files": ["effects/wind-1.mp3"] } }"#,
    )
    .unwrap();
    // The dependency's own bookkeeping, which a resolve would refuse to
    // inherit and a release must not carry either. (A `pack.json` naming
    // deps is the composed case below, so this one names none.)
    std::fs::write(
        dep_dir.join("pack.json"),
        r#"{ "_note": "authored elsewhere", "deps": [] }"#,
    )
    .unwrap();
    std::fs::write(
        dep_dir.join("_extends.json"),
        r#"{ "deps": [], "keys": {}, "files": {} }"#,
    )
    .unwrap();

    let m = compute_dep_manifest(&layout, "common", "0.1.0").unwrap();
    assert_eq!(m.name, "common");
    assert_eq!(m.version, "0.1.0");
    assert_eq!(m.piece, "pack");
    assert!(m.deps.is_empty(), "a root is built on nothing");
    assert_eq!(
        m.files.keys().cloned().collect::<Vec<_>>(),
        vec![
            "assets/effect-pool.json".to_string(),
            "assets/effects/wind-1.mp3".to_string(),
        ]
    );
    // The identity agrees with the composition record's number: the same
    // fold over the same file set, computed straight off the release's own
    // unpack keys (manifest_hash is order-stable, so a refold is a no-op).
    let from_disk = crate::compose::tree_hash(&dep_dir).unwrap();
    let released = manifest_hash(&m.files);
    let mut refold = BTreeMap::new();
    refold.extend(m.files.iter().map(|(k, v)| (k.clone(), v.clone())));
    assert_eq!(manifest_hash(&refold), released, "the fold is the manifest");
    // `tree_hash` reads the tree *with* its bookkeeping; the release drops
    // it, so the two numbers must differ.
    assert_ne!(released, from_disk, "bookkeeping changes the tree's hash");

    // A tree that is itself composed is refused: a dependency release is
    // one pack, and a dependent names a composition's roots itself.
    std::fs::write(
        dep_dir.join("pack.json"),
        r#"{ "_note": "a preset, not a root", "deps": ["weapons"] }"#,
    )
    .unwrap();
    let err = compute_dep_manifest(&layout, "common", "0.1.0").unwrap_err();
    assert!(err.to_string().contains("composed"), "{err}");

    // And a tree nobody unpacked is an error that says where it looked.
    let err = compute_dep_manifest(&layout, "guns", "0.1.0").unwrap_err();
    assert!(err.to_string().contains("_extends"), "{err}");

    let _ = std::fs::remove_dir_all(&dir);
}
