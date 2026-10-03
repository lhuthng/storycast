use super::*;

fn fixture_root(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bm-layout-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    // The repo marker discover() walks up to (tracked, always present).
    std::fs::create_dir_all(dir.join("rust")).unwrap();
    std::fs::write(dir.join("rust/Cargo.toml"), "[workspace]").unwrap();
    dir
}

#[test]
fn the_workspace_list_says_which_directory_is_a_book() {
    // A list that offers a directory with no settings.json — or with one
    let root = fixture_root("ws-inventory");
    let book = root.join("workspaces/book-a");
    std::fs::create_dir_all(book.join("data/chapters")).unwrap();
    std::fs::create_dir_all(book.join("data/script")).unwrap();
    // Exactly what `workspace new` stamps.
    crate::config::Settings::default()
        .save(&book.join("settings.json"))
        .unwrap();
    std::fs::write(book.join("data/chapters/ch01.txt"), "x").unwrap();
    std::fs::write(book.join("data/script/01.json"), "{}").unwrap();
    // A directory whose settings do not parse, and a file under
    std::fs::create_dir_all(root.join("workspaces/book-b/data/chapters")).unwrap();
    std::fs::write(root.join("workspaces/book-b/data/chapters/ch01.txt"), "x").unwrap();
    std::fs::write(root.join("workspaces/book-b/settings.json"), "{ not json").unwrap();
    std::fs::write(root.join("workspaces/notes.txt"), "x").unwrap();
    std::fs::create_dir_all(root.join(".bm")).unwrap();
    std::fs::write(Layout::active_workspace_file(&root), "book-a\n").unwrap();

    let found = workspaces(&root);
    assert_eq!(
        found.iter().map(|w| w.name.as_str()).collect::<Vec<_>>(),
        vec!["book-a", "book-b"],
        "a plain file under workspaces/ is not a workspace"
    );
    assert_eq!(found[0].config, WorkspaceConfig::Valid);
    assert!(found[0].active, "the pointer marks the row it names");
    assert_eq!((found[0].chapters, found[0].scripts), (1, 1));
    assert_eq!(found[1].config, WorkspaceConfig::Broken);
    assert_eq!(
        (found[1].chapters, found[1].scripts),
        (1, 0),
        "the counts come from the tree whatever the config says"
    );
    assert!(!found[1].active);

    // No pointer is not an error: the root is the implicit default, and a
    std::fs::remove_file(Layout::active_workspace_file(&root)).unwrap();
    assert!(workspaces(&root).iter().all(|w| !w.active));
}

#[test]
fn title_mode_default_pins_the_crawled_headline() {
    // A digest is a fresh model call, so its `title` can differ every time
    let root = fixture_root("title-mode");
    let l = Layout::new(&root);
    std::fs::create_dir_all(l.chapters()).unwrap();
    std::fs::write(l.chapter_txt(3), "Chapter 3: Maomao\n\nbody\n").unwrap();
    std::fs::create_dir_all(l.script(3).parent().unwrap()).unwrap();
    std::fs::write(
        l.script(3),
        r#"{"title":"The Digest Renamed This","segments":[]}"#,
    )
    .unwrap();
    assert_eq!(
        l.chapter_title(3),
        "The Digest Renamed This",
        "auto is the default and prefers the digest's title"
    );
    let mut s = crate::config::Settings::load(&l.settings());
    s.title_mode = "default".into();
    std::fs::create_dir_all(l.settings().parent().unwrap()).unwrap();
    s.save(&l.settings()).unwrap();
    assert_eq!(
        l.chapter_title(3),
        "Maomao",
        "default takes the headline, so a re-digest cannot move the title"
    );
}

#[test]
fn a_workspace_rooted_layout_keeps_its_adapter_and_its_settings() {
    // Resolved from the *book*, this layout used to report the adapter as
    let root = fixture_root("ws-rooted");
    let book = root.join("workspaces/book");
    std::fs::create_dir_all(book.join("adapters/jnovel-en-US")).unwrap();
    std::fs::write(
        book.join("adapters/jnovel-en-US/adapter.json"),
        r#"{"pack":"","language":"en-US","engine":""}"#,
    )
    .unwrap();
    let mut s = crate::config::Settings::default();
    s.profile.adapter.name = "jnovel-en-US".into();
    s.profile.engine.name = "pocket".into();
    s.save(&book.join("settings.json")).unwrap();

    let l = Layout::resolve(&book).unwrap();
    assert_eq!(l.adapter, "jnovel-en-US");
    assert_eq!(l.engine, "pocket");
    assert_eq!(
        l.settings(),
        book.join("settings.json"),
        "a root carrying settings.json is a workspace, not a legacy .bm/"
    );
    assert_eq!(
        crate::adapter::in_force(&l).unwrap().unwrap().language,
        "en-US"
    );
}

#[test]
fn the_sidecar_prefers_the_provisioned_copy_then_the_workspace_build() {
    // Moved with the fallback itself: the local worker runs from the
    let root = std::env::temp_dir().join(format!("bm-sidecar-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let layout = Layout::new(&root);
    assert_eq!(
        layout.sidecar_binary(),
        layout.tts_binary(),
        "absent everywhere reports the canonical path"
    );
    assert_eq!(
        layout.tts_binary(),
        root.join("engines/vieneu/bm-tts"),
        "the sidecar is the engine's, inside its own tree"
    );
    let debug = root.join("rust/target/debug/bm-tts");
    std::fs::create_dir_all(debug.parent().unwrap()).unwrap();
    std::fs::write(&debug, b"fake").unwrap();
    assert_eq!(layout.sidecar_binary(), debug, "debug is the last resort");
    let release = root.join("rust/target/release/bm-tts");
    std::fs::create_dir_all(release.parent().unwrap()).unwrap();
    std::fs::write(&release, b"fake").unwrap();
    assert_eq!(layout.sidecar_binary(), release, "release beats debug");
    let provisioned = layout.tts_binary();
    std::fs::create_dir_all(provisioned.parent().unwrap()).unwrap();
    std::fs::write(&provisioned, b"fake").unwrap();
    assert_eq!(layout.sidecar_binary(), provisioned);
    let (bin, args) = layout.sidecar_command(8818, 0);
    assert_eq!(bin, provisioned);
    assert!(args.windows(2).any(|w| w[0] == "--port" && w[1] == "8818"));
    // VieNeu declares a lexicon, so it is handed one — from its own tree.
    let dict_at = args.iter().position(|a| a == "--dict").expect("--dict");
    assert_eq!(
        args[dict_at + 1],
        layout.tts_dict().unwrap().display().to_string()
    );
    assert!(args[dict_at + 1].contains("engines/vieneu/models/sea_g2p.bin"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn paths_are_adapter_and_engine_scoped() {
    let l = Layout::new("/repo");
    // A checkout with no adapter bundle keys under `default`.
    assert!(l.cast("vieneu").ends_with("cast-default-vieneu.json"));
    assert!(l
        .seg_dir("vieneu", 7)
        .ends_with("data/audio/segments-default-vieneu-07"));
    // `gemini` keeps its historical on-disk spelling, so renaming nothing
    assert!(l
        .seg_dir("gemini", 7)
        .ends_with("data/audio/segments-default-gemini-v2-07"));
    assert!(l
        .seg_dir("neutts-air", 7)
        .ends_with("data/audio/segments-default-neutts-air-07"));
    assert_ne!(
        l.seg_dir("neutts-air", 7),
        l.seg_dir("gemini", 7),
        "a third engine must not land in another engine's cache"
    );
    // A named adapter is its own namespace: vi-VN and en-US of one book are
    let named = Layout {
        adapter: "en-US".into(),
        ..Layout::new("/repo")
    };
    assert!(named.cast("vieneu").ends_with("cast-en-US-vieneu.json"));
    assert_ne!(named.seg_dir("vieneu", 7), l.seg_dir("vieneu", 7));
}

#[test]
fn a_pre_split_cache_is_renamed_into_the_new_shape() {
    let root = fixture_root("cache-migrate");
    let l = Layout::new(&root);
    let data = l.data();
    std::fs::create_dir_all(data.join("audio/segments-vieneu-07")).unwrap();
    std::fs::write(data.join("cast-vieneu.json"), "{\"Narrator\":\"Adam\"}").unwrap();

    let moved = l.migrate_cache_keys("vieneu").unwrap();
    assert_eq!(moved.len(), 2, "the cast and one chapter: {moved:?}");
    assert!(l.cast("vieneu").is_file(), "the cast was carried over");
    assert!(l.seg_dir("vieneu", 7).is_dir(), "and the segments with it");
    assert!(
        !data.join("cast-vieneu.json").exists(),
        "the old name is gone, not duplicated"
    );
    // Idempotent: nothing left to move, and nothing overwritten.
    assert!(l.migrate_cache_keys("vieneu").unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

/// A language that was flat at the root takes its own home, and the pointer
#[test]
fn a_pre_adapter_home_checkout_moves_the_prompts_into_the_languages_home() {
    let root = fixture_root("adapter-migrate");
    // The flat language, as it sat before adapters were a directory: the
    std::fs::create_dir_all(root.join("prompts")).unwrap();
    std::fs::write(root.join("prompts/analyze.txt"), "vi-VN").unwrap();
    std::fs::create_dir_all(root.join("assets/crawl/templates")).unwrap();
    std::fs::write(root.join("assets/crawl/templates/storya.lua"), "-- crawl").unwrap();
    std::fs::create_dir_all(root.join("assets/music")).unwrap();
    std::fs::write(root.join("assets/music/day-1.mp3"), b"bed").unwrap();
    std::fs::create_dir_all(root.join(".bm")).unwrap();
    std::fs::write(
        root.join(".bm/profile"),
        r#"{"name":"xianxia","hash":"deadbeef"}"#,
    )
    .unwrap();

    let l = Layout::new(&root);
    assert_eq!(l.adapter, DEFAULT_ADAPTER, "nothing has named the language");
    assert_eq!(l.migrate_adapter_tree().unwrap().as_deref(), Some("vi-VN"));

    let home = root.join("adapters/vi-VN");
    assert!(home.join("prompts/analyze.txt").is_file());
    assert!(!root.join("prompts").exists(), "moved, not copied");
    assert!(
        root.join("assets/crawl/templates/storya.lua").is_file(),
        "the old pack crawlers are left where they are — the global tree is the live one"
    );
    assert!(
        root.join("assets/music/day-1.mp3").is_file(),
        "the art stays"
    );

    // The name is the pointer's, and it is what the layout now resolves
    let after = Layout::resolve(&root).unwrap();
    assert_eq!(after.adapter, "vi-VN");
    assert_eq!(after.adapter_home(), Some(home.clone()));
    assert_eq!(after.prompts_base(), home);
    assert_eq!(after.crawl_scripts(), root.join("crawlers"));

    // Idempotent: a bundle exists, so there is nothing left to move.
    assert_eq!(after.migrate_adapter_tree().unwrap(), None);
    let _ = std::fs::remove_dir_all(&root);
}

/// A cache written before the language had a name holds the right bytes
#[test]
fn default_keyed_caches_are_re_keyed_for_the_language_that_now_has_a_name() {
    let root = fixture_root("adapter-cache");
    let l = Layout {
        adapter: "vi-VN".into(),
        ..Layout::new(&root)
    };
    let data = l.data();
    std::fs::create_dir_all(data.join("audio/segments-default-vieneu-07")).unwrap();
    std::fs::create_dir_all(data.join("audio/segments-default-gemini-v2-07")).unwrap();
    std::fs::write(data.join("cast-default-vieneu.json"), "{}").unwrap();
    std::fs::write(data.join("cast-default-gemini-v2.json"), "{}").unwrap();

    assert_eq!(l.migrate_cache_keys("vieneu").unwrap().len(), 4);
    assert!(data.join("cast-vi-VN-vieneu.json").is_file());
    assert!(data.join("cast-vi-VN-gemini-v2.json").is_file());
    assert!(data.join("audio/segments-vi-VN-vieneu-07").is_dir());
    assert!(data.join("audio/segments-vi-VN-gemini-v2-07").is_dir());
    assert!(
        !data.join("cast-default-vieneu.json").exists(),
        "the name-less key is gone, not duplicated"
    );
    // Idempotent, and the `default` spelling is left alone once named.
    assert!(l.migrate_cache_keys("vieneu").unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

/// The engine's own files have an identity now: one tree per engine, and the
#[test]
fn engine_files_live_under_a_named_tree_and_the_dictionary_is_declared() {
    let l = Layout::new("/repo");
    assert_eq!(l.engine, "vieneu", "no binding means the local engine");
    assert_eq!(l.engine_dir(), Path::new("/repo/engines/vieneu"));
    assert!(l.models_dir().ends_with("engines/vieneu/models"));
    assert!(l
        .tts_voices()
        .ends_with("engines/vieneu/models/voices.json"));
    assert_eq!(
        l.tts_lib_dir(),
        l.engine_dir(),
        "the SONAME sits by the binary"
    );
    assert!(l
        .tts_dict()
        .unwrap()
        .ends_with("engines/vieneu/models/sea_g2p.bin"));

    // A second engine gets its own space and its own answer to the question
    let cloud = Layout {
        engine: "gemini".into(),
        ..Layout::new("/repo")
    };
    assert_eq!(cloud.tts_dict(), None);
    assert_ne!(cloud.models_dir(), l.models_dir());
    assert_ne!(cloud.tts_binary(), l.tts_binary());
    // And a name nobody declared has no dictionary either, rather than
    let unknown = Layout {
        engine: "neutts-air".into(),
        ..Layout::new("/repo")
    };
    assert_eq!(unknown.tts_dict(), None);
}

/// The one-time move into the engine tree: rename-only, never overwriting,
#[test]
fn a_pre_engine_tree_checkout_is_renamed_into_the_engine_tree() {
    let root = fixture_root("engine-migrate");
    let l = Layout::new(&root);
    let bm = l.bm_state();
    // The flat tree, as it sat before engines had one — VieNeu's, always.
    std::fs::create_dir_all(root.join("models")).unwrap();
    std::fs::write(root.join("models/manifest.json"), "{}").unwrap();
    std::fs::write(root.join("models/sea_g2p.bin"), b"lexicon").unwrap();
    std::fs::write(root.join("bm-tts"), b"binary").unwrap();
    std::fs::write(root.join("libonnxruntime.so.1"), b"soname").unwrap();
    std::fs::create_dir_all(bm.join("voices/refs")).unwrap();
    std::fs::write(bm.join("voices/refs/narrator.mp3"), b"clip").unwrap();

    let moved = l.migrate_engine_tree().unwrap();
    assert_eq!(moved.len(), 4, "weights, binary, lib and refs: {moved:?}");
    assert!(l.models_dir().join("sea_g2p.bin").is_file());
    assert!(l.tts_binary().is_file());
    assert!(l.tts_lib_dir().join("libonnxruntime.so.1").is_file());
    assert!(l.voice_refs().join("narrator.mp3").is_file());
    assert!(
        !root.join("models").exists(),
        "the old name is gone, not duplicated"
    );
    assert!(!root.join("bm-tts").exists());
    assert!(
        !bm.join("voices").exists(),
        "the emptied scaffolding goes too"
    );

    // Idempotent: nothing left to move, so a second start is silent.
    assert!(l.migrate_engine_tree().unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

/// The migration never overwrites: a target that already exists wins, so a
#[test]
fn the_engine_tree_migration_never_overwrites_what_is_already_there() {
    let root = fixture_root("engine-migrate-keep");
    let l = Layout::new(&root);
    std::fs::write(root.join("bm-tts"), b"the old flat binary").unwrap();
    std::fs::create_dir_all(l.engine_dir()).unwrap();
    std::fs::write(l.tts_binary(), b"the engine's own binary").unwrap();

    assert!(l.migrate_engine_tree().unwrap().is_empty());
    assert_eq!(
        std::fs::read(l.tts_binary()).unwrap(),
        b"the engine's own binary"
    );
    // …and the loser is left exactly where it was rather than deleted.
    assert!(root.join("bm-tts").is_file());
    let _ = std::fs::remove_dir_all(&root);
}

/// The prompts come from the adapter: a workspace that carries a `prompts/`
#[test]
fn prompts_come_from_the_workspace_adapter_and_fall_back_to_the_checkout() {
    let root = fixture_root("adapter-prompts");
    std::fs::create_dir_all(root.join("prompts")).unwrap();
    std::fs::write(root.join("prompts/analyze.txt"), "checkout").unwrap();

    // No tree of its own: the checkout answers, exactly as before.
    let bare = Layout::new(&root);
    assert_eq!(bare.prompts_base(), root);
    assert_eq!(bare.prompt(), root.join("prompts/analyze.txt"));

    // Now the workspace carries one, and *both* prompts move with it: a
    let book = root.join("workspaces/book");
    std::fs::create_dir_all(book.join("prompts")).unwrap();
    std::fs::write(book.join("prompts/analyze.txt"), "xianxia-en-US").unwrap();
    std::fs::write(book.join("prompts/script.txt"), "xianxia-en-US").unwrap();
    let l = Layout {
        root: root.clone(),
        work: book.clone(),
        adapter: "xianxia-en-US".into(),
        engine: DEFAULT_ENGINE.into(),
    };
    assert_eq!(l.prompts_base(), book, "and the bundle is cut from there");
    assert_eq!(l.prompt(), book.join("prompts/analyze.txt"));
    assert_eq!(l.script_prompt(), book.join("prompts/script.txt"));
    assert_ne!(l.prompt(), bare.prompt());
    let _ = std::fs::remove_dir_all(&root);
}

/// The pack is work-scoped the way the prompts are: a workspace with its
#[test]
fn the_pack_comes_from_the_workspace_and_falls_back_to_the_checkout() {
    let root = fixture_root("workspace-assets");
    std::fs::create_dir_all(root.join("assets")).unwrap();
    std::fs::write(root.join("assets/scene-map.json"), "{}").unwrap();

    // No tree of its own: the checkout answers, exactly as before.
    let bare = Layout::new(&root);
    assert_eq!(bare.assets(), root.join("assets"));
    assert_eq!(bare.scene_map(), root.join("assets/scene-map.json"));

    // The workspace's own composition wins whole — scene map, pools,
    let book = root.join("workspaces/book");
    std::fs::create_dir_all(book.join("assets")).unwrap();
    std::fs::write(book.join("assets/scene-map.json"), "{}").unwrap();
    std::fs::write(book.join("assets/music-pool.json"), "{}").unwrap();
    let l = Layout {
        root: root.clone(),
        work: book.clone(),
        adapter: DEFAULT_ADAPTER.into(),
        engine: DEFAULT_ENGINE.into(),
    };
    assert_eq!(l.assets(), book.join("assets"));
    assert_eq!(l.scene_map(), book.join("assets/scene-map.json"));
    assert_ne!(l.assets(), bare.assets());
    let _ = std::fs::remove_dir_all(&root);
}

/// Voice material is the workspace's own and is **not** the checkout's:
#[test]
fn voice_material_is_the_workspaces_own_and_not_the_checkouts() {
    let root = fixture_root("workspace-voices");
    std::fs::create_dir_all(root.join("refs")).unwrap();
    std::fs::write(root.join("voices.json"), r#"{"Narrator":"refs/n.wav"}"#).unwrap();
    std::fs::write(
        root.join("voice-pool.json"),
        r#"{"a":{"file":"refs/a.wav","tags":[]}}"#,
    )
    .unwrap();

    // The checkout root owns its own material by definition.
    let bare = Layout::new(&root);
    assert_eq!(bare.voices_manifest(), root.join("voices.json"));
    assert_eq!(bare.voice_pool(), root.join("voice-pool.json"));
    assert_eq!(bare.refs(), root.join("refs"));

    // A workspace with none of its own reads none — never the root's.
    let book = root.join("workspaces/book");
    let l = Layout {
        root: root.clone(),
        work: book.clone(),
        adapter: DEFAULT_ADAPTER.into(),
        engine: DEFAULT_ENGINE.into(),
    };
    assert_eq!(l.voices_manifest(), book.join("voices.json"));
    assert_eq!(l.voice_pool(), book.join("voice-pool.json"));
    assert_eq!(l.refs(), book.join("refs"));
    assert!(
        !l.voices_manifest().exists() && !l.voice_pool().exists(),
        "the checkout's manifests must not answer for the workspace"
    );
    assert!(
        crate::pool::load_manifest(&l.voices_manifest()).is_empty(),
        "a book with no voices casts from none, not from another book's"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The adapter is a *home* now for **prompts**: a checkout carrying
#[test]
fn an_adapter_bundle_owns_its_prompts_and_the_crawlers_are_global() {
    let root = fixture_root("adapter-home");
    std::fs::create_dir_all(root.join("prompts")).unwrap();
    std::fs::write(root.join("prompts/analyze.txt"), "flat").unwrap();

    let flat = Layout {
        adapter: "vi-VN".into(),
        ..Layout::new(&root)
    };
    assert_eq!(flat.adapter_home(), None, "no bundle, no home");
    assert_eq!(
        flat.prompts_base(),
        root,
        "so the prompts are the flat ones"
    );
    assert_eq!(flat.crawl_scripts(), root.join("crawlers"));

    // With the bundle, the prompts answer from one directory.
    let home = root.join("adapters/vi-VN");
    std::fs::create_dir_all(home.join("prompts")).unwrap();
    std::fs::write(home.join("prompts/analyze.txt"), "vi-VN").unwrap();
    let l = Layout {
        adapter: "vi-VN".into(),
        ..Layout::new(&root)
    };
    assert_eq!(l.adapter_home(), Some(home.clone()));
    assert_eq!(l.prompts_base(), home);
    assert_eq!(l.prompt(), home.join("prompts/analyze.txt"));
    // …while the crawlers do not move: they are the global tree either way.
    assert_eq!(l.crawl_scripts(), root.join("crawlers"));

    // A workspace's own prompts are nearer than the checkout's — the same
    let book = root.join("workspaces/book");
    std::fs::create_dir_all(book.join("adapters/vi-VN/prompts")).unwrap();
    let scoped = Layout {
        root: root.clone(),
        work: book.clone(),
        adapter: "vi-VN".into(),
        engine: DEFAULT_ENGINE.into(),
    };
    assert_eq!(scoped.prompts_base(), book.join("adapters/vi-VN"));
    assert_eq!(scoped.crawl_scripts(), root.join("crawlers"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn chapter_title_prefers_subtitle_and_scrubs_illegal_chars() {
    let root = fixture_root("title");
    let l = Layout::new(&root);
    std::fs::create_dir_all(l.chapters()).unwrap();
    std::fs::write(
        l.chapter_txt(3),
        "Chương 3: Kiếm khí xung thiên...\n\nbody\n",
    )
    .unwrap();
    assert_eq!(l.chapter_title(3), "Kiếm khí xung thiên");
}

/// The crawled headline is a word-for-word machine translation of the
#[test]
fn chapter_title_prefers_the_digests_own_title_over_the_mt_headline() {
    let root = fixture_root("title-script");
    let l = Layout::new(&root);
    std::fs::create_dir_all(l.chapters()).unwrap();
    std::fs::create_dir_all(l.script_dir()).unwrap();
    std::fs::write(
        l.chapter_txt(9),
        "Chương 9: Tê! Thật là khủng khiếp dao phay\n\nbody\n",
    )
    .unwrap();
    // No script yet: the headline is all there is.
    assert_eq!(l.chapter_title(9), "Tê! Thật là khủng khiếp dao phay");
    // A script with a title: it wins, and the headline is not consulted.
    std::fs::write(
        l.script(9),
        r#"{"title":"Bí Ẩn Dao Phay Trong Phòng Bếp","segments":[]}"#,
    )
    .unwrap();
    assert_eq!(l.chapter_title(9), "Bí Ẩn Dao Phay Trong Phòng Bếp");
    // An empty or whitespace title falls back rather than naming the file "".
    std::fs::write(l.script(9), r#"{"title":"   ","segments":[]}"#).unwrap();
    assert_eq!(l.chapter_title(9), "Tê! Thật là khủng khiếp dao phay");
    // And a script that predates the field does too.
    std::fs::write(l.script(9), r#"{"segments":[]}"#).unwrap();
    assert_eq!(l.chapter_title(9), "Tê! Thật là khủng khiếp dao phay");
}

#[test]
fn chapter_title_falls_back_when_no_text() {
    let root = fixture_root("missing");
    let l = Layout::new(&root);
    assert_eq!(l.chapter_title(9), "Chapter 9");
}

#[test]
fn scratch_shares_the_output_root_so_publish_can_rename() {
    let l = Layout::new("/repo");
    assert!(l.scratch().ends_with("tmp"));
    assert!(l.scratch_ch(7).ends_with("tmp/ch07"));
    assert_eq!(l.scratch_ch(7).parent().unwrap(), l.scratch());
    // `publish` renames scratch -> output. Both must hang off the same
    assert!(l.scratch().starts_with(&l.work));
    assert!(l.output().starts_with(&l.work));
}

#[test]
fn resolve_pins_the_active_workspace_and_new_stays_legacy() {
    // No pointer: this root is its own workspace, exactly `new()` —
    let l = Layout::resolve("/repo").unwrap();
    assert_eq!(l.work, Path::new("/repo"));
    // ...through the old `.bm/` state paths.
    assert_eq!(
        l.settings(),
        Path::new("/repo/.bm/settings.json"),
        "default workspace keeps its paths"
    );
    assert!(l.scratch().ends_with(".bm/tmp"));
    // A pointer names a directory under workspaces/.
    let dir = std::env::temp_dir().join(format!("bm-resolve{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".bm")).unwrap();
    std::fs::create_dir_all(dir.join("workspaces/beyond-myriads")).unwrap();
    std::fs::write(dir.join(".bm/active-workspace"), "beyond-myriads\n").unwrap();
    let l = Layout::resolve(&dir).unwrap();
    assert_eq!(l.work, dir.join("workspaces/beyond-myriads"));
    assert_eq!(
        l.settings(),
        dir.join("workspaces/beyond-myriads/settings.json")
    );
    assert_eq!(
        l.ledger(),
        dir.join("workspaces/beyond-myriads/ledger.json")
    );
    // Machine-global files stay at the root.
    assert_eq!(l.machines(), dir.join(".bm/machines.json"));
    // A pointer at a missing directory is stale, not a fallback.
    std::fs::write(dir.join(".bm/active-workspace"), "gone\n").unwrap();
    let err = Layout::resolve(&dir).unwrap_err();
    assert!(err.to_string().contains("gone"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Scripts come back in chapter order, and **chapter order is the number**,
#[test]
fn scripts_come_back_in_chapter_order_past_ninety_nine() {
    let dir = std::env::temp_dir().join(format!("bm-script-order{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let layout = Layout::new(&dir);
    std::fs::create_dir_all(layout.script_dir()).unwrap();
    for n in [1u32, 2, 9, 10, 11, 99, 100, 101, 132] {
        std::fs::write(layout.script(n), "{}").unwrap();
    }
    assert_eq!(
        layout.script_chapters(),
        vec![1, 2, 9, 10, 11, 99, 100, 101, 132],
        "100 must sit after 99, not between 10 and 11"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The workspace's own binding is what it reads: a book stamped with
#[test]
fn the_active_workspaces_binding_chooses_its_adapter_and_engine() {
    let dir = std::env::temp_dir().join(format!("bm-resolve-bind{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".bm")).unwrap();
    std::fs::create_dir_all(dir.join("workspaces/book")).unwrap();
    std::fs::write(dir.join(".bm/active-workspace"), "book\n").unwrap();
    std::fs::write(
        dir.join(".bm/profile"),
        r#"{"pack":{"name":"xianxia","hash":"p"},"adapter":{"name":"vi-VN","hash":"a"},"engine":{"name":"vieneu","hash":""}}"#,
    )
    .unwrap();

    // No workspace settings: the pointer answers, as always.
    let l = Layout::resolve(&dir).unwrap();
    assert_eq!(l.adapter, "vi-VN");
    assert_eq!(l.engine, "vieneu");

    // The workspace stamps its own triple: it wins, piece by piece.
    std::fs::write(
        dir.join("workspaces/book/settings.json"),
        r#"{"profile":{"pack":{"name":"apothecary","hash":"q"},"adapter":{"name":"jnovel-en-US","hash":"b"},"engine":{"name":"pocket","hash":""}}}"#,
    )
    .unwrap();
    let l = Layout::resolve(&dir).unwrap();
    assert_eq!(l.adapter, "jnovel-en-US", "the book's adapter");
    assert_eq!(l.engine, "pocket", "the book's engine");

    // A binding that names only some pieces: the named ones win, the
    std::fs::write(
        dir.join("workspaces/book/settings.json"),
        r#"{"profile":{"pack":{"name":"xianxia","hash":"p"},"adapter":{"name":"","hash":""},"engine":{"name":"gemini","hash":""}}}"#,
    )
    .unwrap();
    let l = Layout::resolve(&dir).unwrap();
    assert_eq!(l.adapter, "vi-VN", "unnamed stays the pointer's");
    assert_eq!(l.engine, "gemini", "named wins");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_catalogue_is_tracked() {
    let l = Layout::new("/repo");
    // The catalogue is repo content: a fresh clone has to render with no
    assert_eq!(l.roster_default(), Path::new("/repo/voices.default.json"));
    // Everything engine-owned lives under the engine's own tree, which
    for p in [l.voice_refs(), l.voice_samples()] {
        assert!(
            p.starts_with(l.engine_dir()),
            "{} escaped the engine tree",
            p.display()
        );
    }
    assert!(l.voice_refs().ends_with("engines/vieneu/refs"));
    assert!(l.voice_samples().ends_with("engines/vieneu/samples"));
    // refs and samples are different things and stay separable, so
    assert_ne!(l.voice_refs(), l.voice_samples());
}

/// `discover()` reads the *process* cwd, so tests that move it have to take
fn in_dir<T>(dir: &Path, f: impl FnOnce() -> T) -> T {
    static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir).unwrap();
    let out = f();
    std::env::set_current_dir(prev).unwrap();
    out
}

#[test]
fn resolve_or_root_hands_back_the_pointer_it_could_not_follow() {
    // The management plane opens on a broken pointer; the error rides
    let dir = std::env::temp_dir().join(format!("bm-lenient{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".bm")).unwrap();
    std::fs::write(dir.join(".bm/active-workspace"), "gone\n").unwrap();
    let (layout, err) = Layout::resolve_or_root(&dir);
    assert_eq!(layout.work, dir, "falls back to the root");
    assert!(err.unwrap().contains("gone"));
    // And with no pointer at all there is nothing to report.
    std::fs::remove_file(dir.join(".bm/active-workspace")).unwrap();
    let (layout, err) = Layout::resolve_or_root(&dir);
    assert_eq!(layout.work, dir);
    assert!(err.is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn discover_finds_repo_root_from_a_subdir() {
    let root = fixture_root("discover");
    let sub = root.join("data/audio");
    std::fs::create_dir_all(&sub).unwrap();
    let found = in_dir(&sub, Layout::discover).unwrap();
    assert_eq!(
        found.root.canonicalize().unwrap(),
        root.canonicalize().unwrap()
    );
}

/// A worker root is a flat mirror: prompts, assets, models, binaries — and
#[test]
fn discover_finds_a_provisioned_worker_root() {
    let root = std::env::temp_dir().join(format!("bm-worker-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join(".bm")).unwrap();
    std::fs::write(
        root.join(".bm/profile"),
        r#"{"name":"fixture","hash":"00"}"#,
    )
    .unwrap();
    let found = in_dir(&root, Layout::discover).unwrap();
    // Canonicalize both sides: macOS reports `/private/var/...` for the
    assert_eq!(
        found.root.canonicalize().unwrap(),
        root.canonicalize().unwrap()
    );
    // No workspace pointer means the worker root *is* the workspace.
    assert_eq!(found.work, found.root);
    let _ = std::fs::remove_dir_all(&root);
}

/// **The divergence the offer's binding closes.** A worker's root is a flat
#[test]
fn rebind_moves_the_caches_to_the_adapter_the_offer_is_for() {
    let root = fixture_root("rebind");
    let worker = Layout::resolve(&root).unwrap();
    assert_eq!(worker.adapter, DEFAULT_ADAPTER, "no pointer, no language");
    let audio = worker.data().join("audio");
    assert_eq!(
        worker.seg_dir(DEFAULT_ENGINE, 7),
        audio.join("segments-default-vieneu-07")
    );

    let bound = worker.rebind("vi-VN", "gemini");
    assert_eq!(bound.adapter, "vi-VN");
    assert_eq!(
        bound.seg_dir("gemini", 7),
        audio.join("segments-vi-VN-gemini-v2-07"),
        "the engine is the argument, the adapter is the binding"
    );
    assert_eq!(
        bound.cast("gemini"),
        worker.data().join("cast-vi-VN-gemini-v2.json"),
        "and the cast moves with it, under its historical spelling"
    );
    // A name, not a tree: the root and the workspace are untouched.
    assert_eq!(&bound.root, &worker.root);
    assert_eq!(&bound.work, &worker.work);

    // An empty name is no opinion — an inductor from before the binding
    let kept = worker.rebind("", "   ");
    assert_eq!(kept.adapter, DEFAULT_ADAPTER);
    assert_eq!(kept.engine, DEFAULT_ENGINE);
}

/// Every home in every scope, sorted, with a name found in both resolving
/// one in force, so "the tree the bundle ships" and "the tree this run
/// reads" cannot disagree about which scope an adapter lives in.
#[test]
fn adapter_homes_walks_both_scopes_and_the_workspace_wins() {
    let root = fixture_root("homes");
    let book = root.join("workspaces/book");
    std::fs::create_dir_all(&book).unwrap();
    for (scope, name) in [
        (&root, "vi-VN"),
        (&root, "en-US"),
        (&book, "vi-VN"),
        (&book, "ja-JP"),
    ] {
        std::fs::create_dir_all(scope.join(ADAPTERS_DIR).join(name).join("prompts")).unwrap();
    }
    let layout = Layout {
        root: root.clone(),
        work: book.clone(),
        adapter: "vi-VN".into(),
        engine: DEFAULT_ENGINE.into(),
    };
    assert_eq!(
        layout.adapter_homes(),
        vec![
            ("ja-JP".to_string(), book.clone()),
            ("vi-VN".to_string(), book.clone()),
            ("en-US".to_string(), root.clone()),
        ],
        "workspace scope first, each scope sorted, a shadowed name once"
    );
    assert_eq!(
        layout.adapter_home(),
        Some(book.join(ADAPTERS_DIR).join("vi-VN")),
        "and the one in force resolves in that same scope"
    );

    // No `adapters/` anywhere is the pre-split shape, and it is an empty
    let bare = Layout::new(fixture_root("homes-bare"));
    assert!(bare.adapter_homes().is_empty());
}

/// A script knows its chapter; recovering the workspace from the script's
#[test]
fn a_script_path_resolves_the_workspace_behind_it() {
    let l = Layout::new(fixture_root("of-script"));
    l.ensure().unwrap();
    std::fs::write(l.chapter_txt(9), "Chương 9: Tiêu đề\n\nbody\n").unwrap();
    std::fs::write(l.script(9), r#"{"segments":[]}"#).unwrap();

    let (back, chapter) = Layout::of_script(&l.script(9)).expect("the script is in a script dir");
    assert_eq!(chapter, 9, "the chapter is the file's own name");
    assert_eq!(
        back.chapter_txt(9),
        l.chapter_txt(9),
        "and the layout is rooted at the workspace, not at data/"
    );
    assert_eq!(
        back.script(9),
        l.script(9),
        "so it also agrees about the script"
    );

    // A path that is not in a script folder is refused rather than guessed
    assert!(Layout::of_script(&l.chapter_txt(9)).is_none());
    assert!(Layout::of_script(&l.data().join("bible.json")).is_none());
}

/// A layout reached through a script path keeps the adapter that names the
#[test]
fn a_script_path_keeps_the_adapter_that_declares_the_language() {
    // A book-rooted workspace: its own `settings.json` is the authority.
    let book = Layout::new(fixture_root("of-script-book"));
    book.ensure().unwrap();
    std::fs::create_dir_all(book.root.join("adapters/en-US")).unwrap();
    std::fs::write(
        book.root.join("adapters/en-US/adapter.json"),
        r#"{"pack":"","language":"en-US","engine":""}"#,
    )
    .unwrap();
    let mut settings = crate::config::Settings::default();
    settings.profile.adapter.name = "en-US".into();
    settings.save(&book.root.join("settings.json")).unwrap();
    std::fs::write(book.script(3), r#"{"segments":[]}"#).unwrap();

    let (from_book, chapter) = Layout::of_script(&book.script(3)).expect("a script resolves");
    assert_eq!(chapter, 3);
    assert_eq!(
        from_book.adapter, "en-US",
        "the book's own settings name it"
    );
    assert_eq!(
        crate::adapter::in_force(&from_book)
            .expect("readable")
            .expect("a manifest")
            .language,
        "en-US",
        "so the language the heading word is chosen from is the real one"
    );

    // A provisioned box: no `settings.json` and no workspace pointer, so
    let boxy = Layout::new(fixture_root("of-script-box"));
    boxy.ensure().unwrap();
    std::fs::create_dir_all(boxy.root.join("adapters/en-US")).unwrap();
    std::fs::write(
        boxy.root.join("adapters/en-US/adapter.json"),
        r#"{"pack":"","language":"en-US","engine":""}"#,
    )
    .unwrap();
    std::fs::create_dir_all(boxy.root.join(".bm")).unwrap();
    std::fs::write(
        boxy.root.join(".bm/profile"),
        r#"{"pack":{"name":"b","hash":"","version":""},"adapter":{"name":"en-US","hash":"","version":""},"engine":{"name":"pocket","hash":"","version":""}}"#,
    )
    .unwrap();
    std::fs::write(boxy.script(4), r#"{"segments":[]}"#).unwrap();

    let (from_box, chapter) = Layout::of_script(&boxy.script(4)).expect("a script resolves");
    assert_eq!(chapter, 4);
    assert_eq!(
        from_box.adapter, "en-US",
        "a box's pushed binding names its adapter, not `default`"
    );
}
