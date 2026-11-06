use super::*;
use bm_core::config::Settings;

/// A checkout with a models tree and a bound profile, and nothing published.
fn checkout() -> (tempfile::TempDir, Layout) {
    let d = tempfile::tempdir().unwrap();
    let layout = Layout::new(d.path());
    layout.ensure().unwrap();
    let engine = layout.models_dir().join("vieneu");
    std::fs::create_dir_all(&engine).unwrap();
    std::fs::write(engine.join("model.onnx"), b"weights").unwrap();
    std::fs::write(layout.bible(), r#"{"characters":[]}"#).unwrap();
    bm_core::profile::write_pointer(
        &layout.root,
        &bm_core::profile::Pointer {
            name: "wuxia".into(),
            hash: "abc".into(),
            version: "3".into(),
        },
    )
    .unwrap();
    (d, layout)
}

fn settings(models: &str, packs: &str) -> Settings {
    Settings {
        models_release: models.into(),
        packs_release: packs.into(),
        ..Settings::default()
    }
}

#[test]
fn an_unpublished_checkout_says_which_plane_is_missing() {
    let (_d, layout) = checkout();
    let gaps = gaps(&layout, &settings("", ""));
    assert_eq!(
        gaps.len(),
        2,
        "{:?}",
        gaps.iter().map(Gap::line).collect::<Vec<_>>()
    );
    assert!(
        gaps[0].what.contains("models release"),
        "{}",
        gaps[0].line()
    );
    assert!(gaps[0].fix.contains(":release"), "{}", gaps[0].line());
    assert!(gaps[1].what.contains("packs release"), "{}", gaps[1].line());
    assert!(gaps[1].fix.contains(":packrelease"), "{}", gaps[1].line());
}

#[test]
fn a_models_repo_that_does_not_name_this_bake_is_a_gap() {
    let (_d, layout) = checkout();
    // An empty models tree has no bake hash, so no release can be this one.
    std::fs::remove_dir_all(layout.models_dir().join("vieneu")).unwrap();
    let gaps = gaps(&layout, &settings("team/models", ""));
    assert!(
        gaps.iter().any(|g| g.what.contains("not the bake")),
        "{:?}",
        gaps.iter().map(Gap::line).collect::<Vec<_>>()
    );
}

#[test]
fn a_preset_bucket_is_read_as_a_bucket_not_as_an_owner_name_repo() {
    // Both settings name a bucket here, which is how this checkout is set up.
    // A URL is not a GitHub repo: the gate has to resolve it the way
    // provisioning does, or it refuses a launch whose files are all published.
    let (_d, layout) = checkout();
    // Port 9 refuses at once, so no external network and no wait.
    let dead = "http://127.0.0.1:9/nowhere";
    let gaps = gaps(&layout, &settings(dead, dead));
    let lines: Vec<String> = gaps.iter().map(Gap::line).collect();
    assert!(
        !gaps.iter().any(|g| g.what.contains("not the bake")),
        "a bucket URL is not a repo: {lines:?}"
    );
    let models = gaps
        .iter()
        .find(|g| g.what.contains("weights"))
        .unwrap_or_else(|| panic!("no models gap: {lines:?}"));
    assert!(
        models.what.contains("could not be read"),
        "{}",
        models.line()
    );
    assert!(models.fix.contains("--from-machine"), "{}", models.line());
    let pack = gaps
        .iter()
        .find(|g| g.what.contains("assets/"))
        .unwrap_or_else(|| panic!("no packs gap: {lines:?}"));
    assert!(pack.what.contains("could not be read"), "{}", pack.line());
}

#[test]
fn a_profile_that_names_no_version_is_named_with_its_own_fix() {
    let (_d, layout) = checkout();
    bm_core::profile::write_pointer(
        &layout.root,
        &bm_core::profile::Pointer {
            name: "wuxia".into(),
            hash: String::new(),
            version: String::new(),
        },
    )
    .unwrap();
    let gaps = gaps(&layout, &settings("team/models", "team/packs"));
    assert!(
        gaps.iter().any(|g| g.what.contains("no versioned pack")),
        "{:?}",
        gaps.iter().map(Gap::line).collect::<Vec<_>>()
    );
}

#[test]
fn a_checkout_only_voice_pack_is_listed_by_name() {
    let (_d, layout) = checkout();
    bm_core::voices::packs::Packs {
        version: 1,
        packs: vec![bm_core::voices::packs::PackRef {
            name: "local-voices".into(),
            version: String::new(),
            sha256: String::new(),
            manifest_hash: String::new(),
            source: bm_core::voices::packs::CHECKOUT.into(),
        }],
    }
    .save(&layout.voice_packs())
    .unwrap();

    let gaps = gaps(&layout, &settings("team/models", "team/packs"));
    let line = gaps
        .iter()
        .find(|g| g.what.contains("voice pack"))
        .map(Gap::line)
        .unwrap_or_else(|| {
            panic!(
                "no gap: {:?}",
                gaps.iter().map(Gap::line).collect::<Vec<_>>()
            )
        });
    assert!(line.contains("local-voices"), "{line}");
}

#[test]
fn a_published_pin_is_not_a_gap() {
    let (_d, layout) = checkout();
    bm_core::voices::packs::Packs {
        version: 1,
        packs: vec![bm_core::voices::packs::PackRef {
            name: "pocket".into(),
            version: "2".into(),
            sha256: "deadbeef".into(),
            manifest_hash: "cafe".into(),
            source: bm_core::voices::packs::PUBLISHED.into(),
        }],
    }
    .save(&layout.voice_packs())
    .unwrap();
    let gaps = gaps(&layout, &settings("team/models", "team/packs"));
    assert!(
        !gaps.iter().any(|g| g.what.contains("voice pack")),
        "{:?}",
        gaps.iter().map(Gap::line).collect::<Vec<_>>()
    );
}

#[test]
fn a_workspace_that_composes_its_own_assets_needs_no_pack_release() {
    // `owns_assets()` is `work != root` with an assets/ tree of its own.
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("checkout");
    let work = d.path().join("book");
    std::fs::create_dir_all(work.join("assets")).unwrap();
    std::fs::write(work.join("bible.json"), r#"{"characters":[]}"#).unwrap();
    let mut layout = Layout::new(&root);
    layout.ensure().unwrap();
    std::fs::write(layout.bible(), r#"{"characters":[]}"#).unwrap();
    layout.work = work;
    assert!(layout.owns_assets());

    let gaps = gaps(&layout, &settings("team/models", ""));
    assert!(
        !gaps.iter().any(|g| g.what.contains("packs release")),
        "{:?}",
        gaps.iter().map(Gap::line).collect::<Vec<_>>()
    );
}
