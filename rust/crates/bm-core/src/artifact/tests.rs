use super::*;
use serde_json::json;
use std::collections::BTreeMap;

/// Pinned against the *other* implementation: this is the value
#[test]
fn the_manifest_hash_matches_the_shell_rule() {
    let doc = json!({"files": {
        "b.npz": {"sha256": "bb", "bytes": 2},
        "a.onnx": {"sha256": "aa", "bytes": 1},
        "c.bin": {"sha256": "cc", "bytes": 3}
    }});
    // sha256 over: "a.onnx\0aa\0" "b.npz\0bb\0" "c.bin\0cc\0", computed by
    assert_eq!(
        manifest_hash(&doc).unwrap(),
        "be11da56be81cc3ed33566e46257e1c7bbcb8d4072b53cbb483c6fe01ecc1da8"
    );
    // The name is a function of the content and the *order* it is read in,
    let same_reordered = json!({"files": {
        "c.bin": {"sha256": "cc", "bytes": 3},
        "a.onnx": {"sha256": "aa", "bytes": 1},
        "b.npz": {"sha256": "bb", "bytes": 2}
    }});
    assert_eq!(
        manifest_hash(&doc).unwrap(),
        manifest_hash(&same_reordered).unwrap()
    );
    let changed = json!({"files": {
        "a.onnx": {"sha256": "aa", "bytes": 1},
        "b.npz": {"sha256": "bb", "bytes": 2},
        "c.bin": {"sha256": "CHANGED", "bytes": 3}
    }});
    assert_ne!(
        manifest_hash(&doc).unwrap(),
        manifest_hash(&changed).unwrap()
    );
}

#[test]
fn the_tag_is_the_hash_the_url_names() {
    let hash = "dda4efee13df4c5e9a0b1c2d3e4f5a6b7c8d9e0f1a2b3c4d5e6f7a8b9c0d1e2f";
    let r = ModelsRelease::for_repo("lhuthng/storycast", hash).unwrap();
    assert_eq!(r.tag, "models-vdda4efee13df");
    assert_eq!(
        r.url,
        "https://github.com/lhuthng/storycast/releases/download/models-vdda4efee13df/models.tar.zst"
    );
    assert_eq!(r.hash, hash);
}

#[test]
fn a_repo_that_is_not_owner_name_is_refused() {
    for bad in [
        "",
        "storycast",
        "a/b/c",
        "owner/",
        "/name",
        "own er/name",
        "o/name?x=1",
    ] {
        assert!(
            ModelsRelease::for_repo(bad, "dda4efee13df").is_err(),
            "`{bad}` was accepted"
        );
    }
}

#[test]
fn no_repo_configured_means_no_release_not_an_error() {
    assert!(ModelsRelease::resolve(Path::new("/nonexistent"), "  ").is_none());
    // And a repo with no bake beside it resolves to nothing, rather than
    assert!(ModelsRelease::resolve(Path::new("/nonexistent"), "o/n").is_none());
}

/// The tree the box ends up with, and the two ways it can be wrong: a file
#[test]
fn a_tree_is_verified_in_both_directions() {
    let dir = tstdir("verify");
    std::fs::create_dir_all(&dir).unwrap();
    let doc = json!({"files": {
        "one.bin": {"sha256": sha_of(b"one"), "bytes": 3},
        "two.bin": {"sha256": sha_of(b"two"), "bytes": 3}
    }});
    std::fs::write(dir.join("manifest.json"), doc.to_string()).unwrap();
    std::fs::write(dir.join("one.bin"), b"one").unwrap();
    std::fs::write(dir.join("two.bin"), b"two").unwrap();
    let want = manifest_hash(&doc).unwrap();
    assert_eq!(verify_dir(&dir, &want).unwrap(), 2);

    // A weight that is not the one the bake recorded.
    std::fs::write(dir.join("two.bin"), b"tampered").unwrap();
    let err = verify_dir(&dir, &want).unwrap_err();
    assert!(
        err.contains("two.bin") && err.contains("does not match"),
        "{err}"
    );

    // A file nobody listed: the box would be checking a set it was not told.
    std::fs::write(dir.join("two.bin"), b"two").unwrap();
    std::fs::write(dir.join("smuggled.bin"), b"x").unwrap();
    let err = verify_dir(&dir, &want).unwrap_err();
    assert!(
        err.contains("smuggled.bin") && err.contains("absent from its manifest"),
        "{err}"
    );
}

/// The expectation travels from the inductor, so a bundle that verifies
#[test]
fn a_self_consistent_bundle_for_another_bake_is_still_refused() {
    let dir = tstdir("other-bake");
    std::fs::create_dir_all(&dir).unwrap();
    let doc = json!({"files": {"one.bin": {"sha256": sha_of(b"one"), "bytes": 3}}});
    std::fs::write(dir.join("manifest.json"), doc.to_string()).unwrap();
    std::fs::write(dir.join("one.bin"), b"one").unwrap();
    let its_own = manifest_hash(&doc).unwrap();
    let err = verify_dir(&dir, &sha_of(b"a different bake entirely")).unwrap_err();
    assert!(err.contains("a different bake"), "{err}");
    assert!(verify_dir(&dir, &its_own).is_ok());
}

/// The whole point of the exercise, end to end and offline: pack a bundle
#[test]
fn a_bundle_lands_and_replaces_what_was_there() {
    let root = tstdir("land");
    std::fs::create_dir_all(&root).unwrap();
    let stage = root.join("pack");
    std::fs::create_dir_all(&stage).unwrap();
    let doc = json!({"files": {
        "backbone.data": {"sha256": sha_of(b"weights"), "bytes": 7},
        "tts.onnx": {"sha256": sha_of(b"graph"), "bytes": 5}
    }});
    std::fs::write(stage.join("manifest.json"), doc.to_string()).unwrap();
    std::fs::write(stage.join("backbone.data"), b"weights").unwrap();
    std::fs::write(stage.join("tts.onnx"), b"graph").unwrap();
    let want = manifest_hash(&doc).unwrap();

    let bundle = root.join(BUNDLE_NAME);
    pack(&bundle, &stage, &doc, &[]);

    // A tree already in place, which the landing must not damage on the
    let dest = root.join("models");
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::write(dest.join("voices.json"), b"{\"live\":true}").unwrap();

    let landed = land(&bundle, &dest, &want).unwrap();
    assert_eq!(landed.files, 2);
    assert_eq!(landed.tag, tag_for(&want));
    assert_eq!(std::fs::read(dest.join("tts.onnx")).unwrap(), b"graph");
    // The bundle is the manifest plus the weights, and the voice store is
    assert!(!dest.join("voices.json").exists());
    // Nothing left lying about.
    let leftovers: Vec<String> = std::fs::read_dir(&root)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("stage") || n.contains("fetch") || n.contains("old-"))
        .collect();
    assert!(leftovers.is_empty(), "left {leftovers:?} behind");
}

/// A bundle that does not match must leave the destination exactly as it
#[test]
fn a_bundle_that_does_not_verify_leaves_the_old_tree_untouched() {
    let root = tstdir("reject");
    std::fs::create_dir_all(&root).unwrap();
    let stage = root.join("pack");
    std::fs::create_dir_all(&stage).unwrap();
    let doc = json!({"files": {"tts.onnx": {"sha256": sha_of(b"graph"), "bytes": 5}}});
    std::fs::write(stage.join("manifest.json"), doc.to_string()).unwrap();
    std::fs::write(stage.join("tts.onnx"), b"graph").unwrap();
    let bundle = root.join(BUNDLE_NAME);
    pack(&bundle, &stage, &doc, &[]);

    let dest = root.join("models");
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::write(dest.join("tts.onnx"), b"the install already here").unwrap();

    let err = land(&bundle, &dest, &sha_of(b"another bake")).unwrap_err();
    assert!(matches!(err, FetchError::Corrupt(_)), "{err:?}");
    assert!(err.to_string().contains("a different bake"), "{err}");
    assert_eq!(
        std::fs::read(dest.join("tts.onnx")).unwrap(),
        b"the install already here"
    );
    assert!(!root.join("models").join("manifest.json").exists());
}

/// A bundle carrying files its manifest never listed, which is not a
#[test]
fn a_member_nobody_listed_is_refused_by_name() {
    let root = tstdir("sidecar");
    std::fs::create_dir_all(&root).unwrap();
    let stage = root.join("pack");
    std::fs::create_dir_all(&stage).unwrap();
    let doc = json!({"files": {"tts.onnx": {"sha256": sha_of(b"graph"), "bytes": 5}}});
    std::fs::write(stage.join("manifest.json"), doc.to_string()).unwrap();
    std::fs::write(stage.join("tts.onnx"), b"graph").unwrap();
    // A stand-in for the sidecar: same shape, no xattr needed to make it.
    std::fs::write(stage.join("._tts.onnx"), b"\x00\x05\x16\x07").unwrap();
    let want = manifest_hash(&doc).unwrap();
    let bundle = root.join(BUNDLE_NAME);
    pack(&bundle, &stage, &doc, &["._tts.onnx"]);

    let dest = root.join("models");
    let err = land(&bundle, &dest, &want).unwrap_err();
    assert!(matches!(err, FetchError::Corrupt(_)), "{err:?}");
    assert!(err.to_string().contains("._tts.onnx"), "{err}");
    assert!(!dest.exists(), "nothing was put in place");
}

/// A bundle that is not a zstd frame, or not a tar, is corrupt bytes — not
#[test]
fn bytes_that_are_not_a_bundle_are_corruption() {
    let root = tstdir("not-a-bundle");
    std::fs::create_dir_all(&root).unwrap();
    let bundle = root.join(BUNDLE_NAME);
    std::fs::write(&bundle, b"<html>404: Not Found</html>").unwrap();
    let err = land(&bundle, &root.join("models"), &sha_of(b"x")).unwrap_err();
    assert!(matches!(err, FetchError::Corrupt(_)), "{err:?}");
}

fn sha_of(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex(&h.finalize())
}

// -----------------------------------------------------------------------

/// A stage holding the two-file shape every small pack has: a registry and
fn pack_stage(root: &Path, files: &[(&str, &[u8])]) -> (PathBuf, BTreeMap<String, String>) {
    // Named `src`, not `stage`: the leftovers assertions below look for
    let stage = root.join("src");
    std::fs::create_dir_all(&stage).unwrap();
    let mut map = BTreeMap::new();
    for (rel, bytes) in files {
        let at = stage.join(rel);
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::fs::write(&at, bytes).unwrap();
        map.insert((*rel).to_string(), sha_of(bytes));
    }
    (stage, map)
}

/// A pack's `manifest.json`, in the shape `bm-inductor profile manifest`
fn pack_manifest(name: &str, version: &str, files: &BTreeMap<String, String>) -> Value {
    serde_json::json!({
        "name": name, "version": version, "piece": "pack", "deps": [],
        "files": files,
    })
}

fn write_pack_manifest(stage: &Path, doc: &Value) {
    std::fs::write(
        stage.join(PACK_MANIFEST),
        serde_json::to_vec_pretty(doc).unwrap(),
    )
    .unwrap();
}

/// Tar a pack bundle the way `tools/profile.sh pack` does: the manifest
fn tar_bundle(bundle: &Path, stage: &Path, names: &[String]) {
    let mut all = vec![PACK_MANIFEST.to_string()];
    all.extend(names.iter().cloned());
    let list = bundle.with_extension("members");
    std::fs::write(&list, all.join("\n")).unwrap();
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!(
            "COPYFILE_DISABLE=1 tar -cf - -C {} -T {} | zstd -q -3 -o {}",
            stage.display(),
            list.display(),
            bundle.display()
        ))
        .status()
        .expect("tar and zstd on PATH (brew install zstd)");
    assert!(status.success(), "packing the test pack failed");
    let _ = std::fs::remove_file(&list);
}

/// The whole path, end to end and offline: a pack is packed the way
#[test]
fn a_pack_bundle_lands_its_assets_subtree_and_agrees_with_the_pointer() {
    let root = tstdir("pack-land");
    std::fs::create_dir_all(&root).unwrap();
    let names = ["assets/effect-pool.json", "assets/effects/wind-1.mp3"];
    let (stage, files) = pack_stage(&root, &[(names[0], &b"{}"[..]), (names[1], &b"a clip"[..])]);
    let doc = pack_manifest("xianxia", "0.1.0", &files);
    write_pack_manifest(&stage, &doc);
    // What the pointer holds: the same rule, over the same keys.
    let expect = crate::profile::manifest_hash(&files);
    let bundle = root.join("xianxia.tar.zst");
    let owned: Vec<String> = names.iter().map(|s| s.to_string()).collect();
    tar_bundle(&bundle, &stage, &owned);

    // A box already running a different profile.
    let dest = root.join("assets");
    std::fs::create_dir_all(dest.join("effects")).unwrap();
    std::fs::write(dest.join("effect-pool.json"), b"the old one").unwrap();
    std::fs::write(
        dest.join("effects/gone-1.mp3"),
        b"a clip the new pack drops",
    )
    .unwrap();

    let landed = land_pack(&bundle, &dest, &expect, "xianxia-pack-v0.1.0").unwrap();
    assert_eq!(landed.files, 2);
    assert_eq!(landed.tag, "xianxia-pack-v0.1.0");
    assert_eq!(std::fs::read(dest.join("effect-pool.json")).unwrap(), b"{}");
    assert_eq!(
        std::fs::read(dest.join("effects/wind-1.mp3")).unwrap(),
        b"a clip"
    );
    // **Replaced, not merged**: the clip the new pack does not carry is
    assert!(!dest.join("effects/gone-1.mp3").exists());
    // The manifest does not land inside `assets/`: it is the bundle's own
    assert!(!dest.join(PACK_MANIFEST).exists());
    assert!(
        !root.join("manifest.json").exists(),
        "the worker root is not where a pack is unpacked"
    );
    let leftovers: Vec<String> = std::fs::read_dir(&root)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("stage") || n.contains("fetch") || n.contains("old-"))
        .collect();
    assert!(leftovers.is_empty(), "left {leftovers:?} behind");
}

/// **The update path's landing: the bundle is its own witness.**
/// A box is handed the hash it must end up at; an update asked for "the
/// newest release" and has no such number, so the check is the one that
#[test]
fn an_unpinned_pack_landing_verifies_against_the_bundle_and_answers_its_hash() {
    let root = tstdir("pack-unpinned");
    std::fs::create_dir_all(&root).unwrap();
    let names = ["assets/effect-pool.json", "assets/effects/wind-1.mp3"];
    let (stage, files) = pack_stage(&root, &[(names[0], &b"{}"[..]), (names[1], &b"a clip"[..])]);
    let expect = crate::profile::manifest_hash(&files);
    write_pack_manifest(&stage, &pack_manifest("common", "0.1.0", &files));
    let bundle = root.join("common.tar.zst");
    let owned: Vec<String> = names.iter().map(|s| s.to_string()).collect();
    tar_bundle(&bundle, &stage, &owned);

    let dest = root.join("extends-common");
    let (landed, hash) = land_pack_unpinned(&bundle, &dest, "common-pack-v0.1.0").unwrap();
    assert_eq!(landed.files, 2);
    assert_eq!(hash, expect, "the record gets the number the bytes fold to");
    assert_eq!(landed.tag, "common-pack-v0.1.0");
    assert_eq!(std::fs::read(dest.join("effect-pool.json")).unwrap(), b"{}");

    // And the check is real, not skipped for want of an expectation: a
    let bad_root = tstdir("pack-unpinned-bad");
    std::fs::create_dir_all(&bad_root).unwrap();
    let (stage2, mut files2) = pack_stage(&bad_root, &[(names[0], &b"{}"[..])]);
    files2.insert(names[0].to_string(), "0".repeat(64));
    write_pack_manifest(&stage2, &pack_manifest("common", "0.1.0", &files2));
    let bad = bad_root.join("common.tar.zst");
    tar_bundle(&bad, &stage2, &[names[0].to_string()]);
    let refused = bad_root.join("extends-common");
    let err = land_pack_unpinned(&bad, &refused, "common-pack-v0.1.0").unwrap_err();
    assert!(format!("{err}").contains("sha256"), "{err}");
    assert!(
        !refused.exists(),
        "a bundle that does not verify lands nothing"
    );
}

/// The failure the update path hit on a **real published release**: a bundle
#[test]
fn a_bundle_carrying_appledouble_members_is_refused_by_name() {
    let root = tstdir("pack-appledouble");
    std::fs::create_dir_all(&root).unwrap();
    let sidecar = "assets/._effect-pool.json";
    let (stage, mut files) = pack_stage(
        &root,
        &[
            ("assets/effect-pool.json", &b"{}"[..]),
            (sidecar, &b"x"[..]),
        ],
    );
    // The manifest is written without the sidecar, which is exactly how a
    files.remove(sidecar);
    write_pack_manifest(&stage, &pack_manifest("common", "0.1.0", &files));
    let bundle = root.join("common.tar.zst");
    let owned = vec![sidecar.to_string(), "assets/effect-pool.json".to_string()];
    tar_bundle(&bundle, &stage, &owned);

    let dest = root.join("extends-common");
    let err = land_pack_unpinned(&bundle, &dest, "common-pack-v0.1.0").unwrap_err();
    let text = format!("{err}");
    assert!(text.contains("absent from its manifest"), "{text}");
    assert!(text.contains("._effect-pool.json"), "{text}");
    assert!(!dest.exists(), "and nothing was unpacked");
}

/// A release that verifies against *its own* manifest and is a different
#[test]
fn a_self_consistent_pack_for_another_profile_is_still_refused() {
    let root = tstdir("pack-other");
    std::fs::create_dir_all(&root).unwrap();
    let names = ["assets/effect-pool.json"];
    let (stage, files) = pack_stage(&root, &[(names[0], &b"{}"[..])]);
    write_pack_manifest(&stage, &pack_manifest("xianxia", "0.1.0", &files));
    let its_own = crate::profile::manifest_hash(&files);
    let bundle = root.join("xianxia.tar.zst");
    let owned: Vec<String> = names.iter().map(|s| s.to_string()).collect();
    tar_bundle(&bundle, &stage, &owned);

    let dest = root.join("assets");
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::write(dest.join("effect-pool.json"), b"the install already here").unwrap();

    let err = land_pack(
        &bundle,
        &dest,
        &sha_of(b"another profile"),
        "xianxia-pack-v0.1.0",
    )
    .unwrap_err();
    assert!(matches!(err, FetchError::Corrupt(_)), "{err:?}");
    assert!(err.to_string().contains("a different pack"), "{err}");
    assert_eq!(
        std::fs::read(dest.join("effect-pool.json")).unwrap(),
        b"the install already here",
        "nothing was put in place"
    );
    // …and the same bundle is accepted against its own hash, so the refusal
    assert!(land_pack(&bundle, &dest, &its_own, "xianxia-pack-v0.1.0").is_ok());
}

/// A bundle carrying something outside `assets/` — and a macOS `._name`
#[test]
fn a_member_outside_the_pack_directory_is_refused_by_name() {
    let root = tstdir("pack-stray");
    std::fs::create_dir_all(&root).unwrap();
    let (stage, mut files) = pack_stage(&root, &[("assets/effect-pool.json", &b"{}"[..])]);
    // A prompt smuggled in beside the profile, and a Finder sidecar for it.
    std::fs::write(stage.join("prompts.txt"), b"not part of the pack").unwrap();
    std::fs::write(stage.join("._effect-pool.json"), b"\x00\x05\x16\x07").unwrap();
    files.insert(
        "assets/._effect-pool.json".to_string(),
        sha_of(b"\x00\x05\x16\x07"),
    );
    write_pack_manifest(&stage, &pack_manifest("xianxia", "0.1.0", &files));
    let expect = crate::profile::manifest_hash(&files);
    let bundle = root.join("xianxia.tar.zst");
    tar_bundle(
        &bundle,
        &stage,
        &["assets/effect-pool.json".into(), "prompts.txt".into()],
    );

    let dest = root.join("assets");
    let err = land_pack(&bundle, &dest, &expect, "xianxia-pack-v0.1.0").unwrap_err();
    assert!(matches!(err, FetchError::Corrupt(_)), "{err:?}");
    let msg = err.to_string();
    assert!(msg.contains("prompts.txt"), "{msg}");
    assert!(!dest.exists(), "nothing was put in place");
}

/// A language release offered where a pack is expected is refused, because
#[test]
fn a_language_release_is_not_a_profile_pack() {
    let root = tstdir("pack-piece");
    std::fs::create_dir_all(&root).unwrap();
    let (stage, mut files) = pack_stage(&root, &[("assets/prompts.txt", b"vi")]);
    files.remove("assets/prompts.txt");
    let mut doc = pack_manifest("vi-VN", "1", &files);
    doc["piece"] = serde_json::json!("adapter");
    write_pack_manifest(&stage, &doc);
    let bundle = root.join("vi.tar.zst");
    tar_bundle(&bundle, &stage, &[]);
    let err = land_pack(
        &bundle,
        &root.join("assets"),
        &crate::profile::manifest_hash(&files),
        "vi-VN-adapter-v1",
    );
    assert!(err.is_err(), "a language is not a profile");
}

/// The pack half of `tools/models.sh`, through the same two binaries the
fn pack(bundle: &Path, stage: &Path, doc: &Value, extra: &[&str]) {
    let mut names = vec!["manifest.json".to_string()];
    let mut keys: Vec<&String> = doc["files"].as_object().unwrap().keys().collect();
    keys.sort();
    names.extend(keys.into_iter().cloned());
    names.extend(extra.iter().map(|s| s.to_string()));
    let list = bundle.with_extension("members");
    std::fs::write(&list, names.join("\n")).unwrap();
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!(
            "COPYFILE_DISABLE=1 tar -cf - -C {} -T {} | zstd -q -3 -o {}",
            stage.display(),
            list.display(),
            bundle.display()
        ))
        .status()
        .expect("tar and zstd on PATH (brew install zstd)");
    assert!(status.success(), "packing the test bundle failed");
    let _ = std::fs::remove_file(&list);
}

/// Named temp dirs: bm-core has no `tempfile` dev-dependency, and a test
fn tstdir(what: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bm-artifact-{what}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn files(pairs: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// The delta is the whole sync decision: changed paths travel, removed
#[test]
fn a_manifest_diff_names_only_what_moved() {
    let old = files(&[
        ("assets/scene-map.json", "aa"),
        ("assets/music/a.mp3", "bb"),
        ("assets/music/gone.mp3", "cc"),
    ]);
    let new = files(&[
        ("assets/scene-map.json", "aa"),
        ("assets/music/a.mp3", "BB"),
        ("assets/music/b.mp3", "dd"),
    ]);
    let d = diff_manifests(&old, &new);
    assert_eq!(d.changed, vec!["assets/music/a.mp3", "assets/music/b.mp3"]);
    assert_eq!(d.removed, vec!["assets/music/gone.mp3"]);
    let same = diff_manifests(&new, &new);
    assert!(same.changed.is_empty() && same.removed.is_empty());
}

/// The receipt round-trips through its one spelling: what the fetch path
#[test]
fn a_receipt_survives_its_own_spelling() {
    let dir = tstdir("receipt");
    std::fs::create_dir_all(&dir).unwrap();
    let m = crate::profile::Manifest {
        name: "xianxia".into(),
        version: "0.2.0".into(),
        piece: "pack".into(),
        files: files(&[("assets/scene-map.json", "aa")]),
        deps: vec![],
    };
    let at = dir.join(PACK_RECEIPT);
    write_receipt(&at, &m).unwrap();
    let back = read_receipt(&at).expect("a receipt just written must parse");
    assert_eq!(back.version, "0.2.0");
    assert_eq!(back.files, m.files);
    assert!(read_receipt(&dir.join("absent.json")).is_none());
    let _ = std::fs::remove_dir_all(&dir);
}
