use super::*;

use std::path::PathBuf;

/// One checkout with a workspace, and the temp dir holding both.
struct Fixture {
    dir: PathBuf,
    layout: Layout,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

/// A published pack tree: `voices/<name>/preset.json` beside its clones.
fn pack(root: &Path, slots: &str) -> PathBuf {
    let tree = root.join(".bm/packs/voices/demo/2026-10-19");
    write(
        &tree.join("voices/demo/preset.json"),
        &format!(r#"{{"label":"Demo","voices":{slots}}}"#),
    );
    tree
}

fn fixture() -> Fixture {
    // One directory per test: these run in parallel and a shared root would
    // have one test's pack overwrite another's.
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("bm-voice-packs-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let root = dir.join("repo");
    let work = root.join("workspaces/book");
    std::fs::create_dir_all(&work).unwrap();
    let mut layout = Layout::new(&root);
    layout.work = work;
    Fixture { dir, layout }
}

/// A clone inside the pack, so its take can differ from the checkout's.
fn pack_clone(tree: &Path, slug: &str, name: &str, take: &[u8]) {
    let dir = tree.join("voices/clones").join(slug);
    write(
        &dir.join("voice.json"),
        &format!(r#"{{"name":{name:?},"slug":{slug:?}}}"#),
    );
    std::fs::write(dir.join(TAKE), take).unwrap();
}

fn manifest(layout: &Layout) -> BTreeMap<String, String> {
    serde_json::from_str(&std::fs::read_to_string(layout.voices_manifest()).unwrap()).unwrap()
}

fn take(layout: &Layout, slot: &str) -> Vec<u8> {
    std::fs::read(
        layout
            .voices_dir()
            .join("demo/clones")
            .join(slot)
            .join(TAKE),
    )
    .unwrap()
}

#[test]
fn a_named_pack_becomes_positional_ids_and_ships_its_own_takes() {
    let f = fixture();
    let tree = pack(
        &f.layout.root,
        r#"[{"voice":"Alpha","tags":["old","male"]},
            {"voice":"Beta","tags":["young","female"]}]"#,
    );
    pack_clone(&tree, "alpha", "Alpha", b"pack-alpha");
    pack_clone(&tree, "beta", "Beta", b"pack-beta");

    let report = install_tree(&f.layout, "demo", &tree, &Origin::checkout()).unwrap();

    assert_eq!(report.pairs.len(), 2);
    assert!(report.unfilled.is_empty(), "{:?}", report.unfilled);
    let got = manifest(&f.layout);
    assert_eq!(
        got["demo/unnamed-1"],
        "voices/demo/clones/unnamed-1/vieneu-clean.wav"
    );
    assert_eq!(
        got["demo/unnamed-2"],
        "voices/demo/clones/unnamed-2/vieneu-clean.wav"
    );
    // The pack's audio, never the checkout's copy of the same clone.
    assert_eq!(take(&f.layout, "unnamed-1"), b"pack-alpha");
    let pool = crate::pool::load_pool(&f.layout.voice_pool());
    assert_eq!(pool["demo/unnamed-1"].tags, ["old", "male"]);
    assert_eq!(pool["demo/unnamed-1"].label, "Alpha");
}

#[test]
fn a_named_pack_retargets_the_cast_by_clone_name_and_slug() {
    let f = fixture();
    let tree = pack(
        &f.layout.root,
        r#"[{"voice":"Alpha","tags":["old","male"]},
            {"voice":"Beta","tags":["young","female"]}]"#,
    );
    pack_clone(&tree, "alpha", "Alpha", b"pack-alpha");
    pack_clone(&tree, "beta", "Beta", b"pack-beta");
    write(
        &f.layout.data().join("cast-vi-VN-vieneu.json"),
        r#"{"A":"Alpha","B":"beta","C":"Narrator"}"#,
    );

    let report = install_tree(&f.layout, "demo", &tree, &Origin::checkout()).unwrap();

    assert_eq!(report.retargeted, 1);
    let cast: BTreeMap<String, String> = serde_json::from_str(
        &std::fs::read_to_string(f.layout.data().join("cast-vi-VN-vieneu.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(cast["A"], "demo/unnamed-1", "a name is retargeted by id");
    assert_eq!(cast["B"], "demo/unnamed-2", "so is a slug");
    assert_eq!(cast["C"], "Narrator", "and nothing else is touched");
}

#[test]
fn a_slot_no_clone_answers_stays_unfilled() {
    let f = fixture();
    let tree = pack(
        &f.layout.root,
        r#"[{"voice":"Alpha","tags":["old","male"]},
            {"voice":"Beta","tags":["young","female"]}]"#,
    );
    pack_clone(&tree, "beta", "Beta", b"pack-beta");

    let report = install_tree(&f.layout, "demo", &tree, &Origin::checkout()).unwrap();

    assert_eq!(report.unfilled, ["unnamed-1"]);
    let got = manifest(&f.layout);
    assert!(!got.contains_key("demo/unnamed-1"));
    assert!(got.contains_key("demo/unnamed-2"));
    let pool = crate::pool::load_pool(&f.layout.voice_pool());
    assert!(!pool.contains_key("demo/unnamed-1"));
}

#[test]
fn a_reinstall_drops_slots_the_pack_no_longer_declares() {
    let f = fixture();
    let two = pack(
        &f.layout.root,
        r#"[{"voice":"Alpha","tags":["old","male"]},
            {"voice":"Beta","tags":["young","female"]}]"#,
    );
    pack_clone(&two, "alpha", "Alpha", b"a");
    pack_clone(&two, "beta", "Beta", b"b");
    install_tree(&f.layout, "demo", &two, &Origin::checkout()).unwrap();
    let mut pool = crate::pool::load_pool(&f.layout.voice_pool());
    pool.insert(
        "Hand Made".into(),
        crate::pool::PoolEntry {
            file: "refs/hand-made.mp3".into(),
            ..Default::default()
        },
    );
    std::fs::write(
        f.layout.voice_pool(),
        serde_json::to_string_pretty(&pool).unwrap(),
    )
    .unwrap();

    let one = pack(&f.layout.root, r#"[{"voice":"Alpha","tags":["old","male"]}]"#);
    pack_clone(&one, "alpha", "Alpha", b"a");
    let origin = Origin::published("2026-10-20", "sha", "manifest");
    install_tree(&f.layout, "demo", &one, &origin).unwrap();

    let got = manifest(&f.layout);
    assert_eq!(got.keys().filter(|k| !k.starts_with('_')).count(), 1);
    let pool = crate::pool::load_pool(&f.layout.voice_pool());
    assert!(
        pool.contains_key("Hand Made"),
        "a manual entry is not a pack's"
    );
    let pinned = &Packs::load(&f.layout).packs[0];
    assert_eq!(pinned.version, "2026-10-20");
    assert_eq!(pinned.sha256, "sha");
    assert_eq!(pinned.manifest_hash, "manifest");
    assert_eq!(pinned.source, PUBLISHED);
}

#[test]
fn voices_read_back_with_their_tags_label_and_take() {
    let f = fixture();
    let tree = pack(&f.layout.root, r#"[{"voice":"Alpha","tags":["old","male"]}]"#);
    pack_clone(&tree, "alpha", "Alpha", b"pack-alpha");
    install_tree(&f.layout, "demo", &tree, &Origin::checkout()).unwrap();

    let got = voices(&f.layout);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].id, "demo/unnamed-1");
    assert_eq!(got[0].tags, ["old", "male"]);
    assert_eq!(got[0].label, "Alpha");
    assert_eq!(
        got[0].file,
        "voices/demo/clones/unnamed-1/vieneu-clean.wav"
    );
}

#[test]
fn a_pack_tree_that_does_not_exist_is_refused_by_name() {
    let f = fixture();
    let opts = Options {
        tree: Some(f.layout.root.join("nowhere")),
        ..Default::default()
    };
    let err = install(&f.layout, "demo", &opts).unwrap_err().to_string();
    assert!(err.contains("nowhere"), "{err}");
}

#[test]
fn an_unpublished_pack_is_not_read_from_the_checkout() {
    let f = fixture();
    let err = install(&f.layout, "demo", &Options::default())
        .unwrap_err()
        .to_string();
    assert!(err.contains("packs_release"), "{err}");
}
