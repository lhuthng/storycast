use super::*;
use bm_proto::TaskPref;

/// A checkout with the tracked fixture profile, a cast file, a crawler, and
fn fixture(name: &str) -> crate::Layout {
    let dir = std::env::temp_dir().join(format!("bm-sources-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    crate::profile::install_fixture(&dir).expect("fixture profile");
    let l = crate::Layout::new(&dir);
    for rel in [
        "assets/effects/night-1.mp3",
        "assets/music/market-bg-1.mp3",
        "assets/injects/coin-1.mp3",
        // In the directory, named by no registry: the file the old
        "assets/music/leftover-bg-9.mp3",
    ] {
        let p = l.assets().join(rel.trim_start_matches("assets/"));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, b"x").unwrap();
    }
    std::fs::create_dir_all(l.work.join("data")).unwrap();
    std::fs::write(l.work.join("data/cast-default-vieneu.json"), "{}").unwrap();
    std::fs::write(
        l.root.join("voices.json"),
        r#"{"Narrator":"refs/narrator.mp3"}"#,
    )
    .unwrap();
    std::fs::create_dir_all(l.work.join("crawl")).unwrap();
    std::fs::write(l.work.join("crawl/site.lua"), "-- crawl").unwrap();
    // The global crawler tree: a known site, shipped to every crawl box.
    std::fs::create_dir_all(l.root.join("crawlers/known")).unwrap();
    std::fs::write(l.root.join("crawlers/known/storya.lua"), "-- global").unwrap();
    // Finder noise must never enter the manifest.
    std::fs::write(l.assets().join(".DS_Store"), b"junk").unwrap();
    l
}

fn policy(stages: &[Stage]) -> Vec<TaskPref> {
    Stage::ALL
        .iter()
        .map(|s| TaskPref {
            stage: *s,
            enabled: stages.contains(s),
        })
        .collect()
}

fn paths(l: &crate::Layout, stages: &[Stage]) -> Vec<String> {
    let mut s = Sources::plan(l, stages).unwrap();
    s.members.sort_by(|a, b| a.to.cmp(&b.to));
    s.members.into_iter().map(|m| m.to).collect()
}

#[test]
fn a_digest_box_gets_the_prompts_and_registries_and_no_clip() {
    let l = fixture("digest");
    let got = paths(&l, &[Stage::Digest]);
    assert!(got.contains(&"prompts/analyze.txt".to_string()), "{got:?}");
    for reg in [
        "assets/scene-map.json",
        "assets/effect-pool.json",
        "assets/inject-pool.json",
        "assets/tag-aliases.json",
    ] {
        assert!(got.contains(&reg.to_string()), "{reg} missing: {got:?}");
    }
    // The digest never opens the music pool, and no stage reads a clip.
    assert!(
        !got.contains(&"assets/music-pool.json".to_string()),
        "{got:?}"
    );
    assert!(
        !got.iter().any(|p| p.ends_with(".mp3")),
        "a digest box was sent audio: {got:?}"
    );
    assert!(!got.iter().any(|p| p.starts_with("crawl/")), "{got:?}");
}

#[test]
fn a_merge_box_gets_the_pools_and_only_registered_clips() {
    let l = fixture("merge");
    let got = paths(&l, &[Stage::Merge]);
    for reg in [
        "assets/scene-map.json",
        "assets/effect-pool.json",
        "assets/music-pool.json",
        "assets/inject-pool.json",
    ] {
        assert!(got.contains(&reg.to_string()), "{reg} missing: {got:?}");
    }
    assert!(
        got.contains(&"assets/music/market-bg-1.mp3".to_string()),
        "{got:?}"
    );
    // A file nothing registers never travels: the registry is the pool.
    assert!(
        !got.iter().any(|p| p.contains("leftover")),
        "an unregistered clip was shipped: {got:?}"
    );
    // Nothing on the merge path opens the aliases.
    assert!(
        !got.contains(&"assets/tag-aliases.json".to_string()),
        "{got:?}"
    );
}

#[test]
fn a_crawl_box_gets_the_crawlers_and_nothing_else() {
    let l = fixture("crawl");
    let got = paths(&l, &[Stage::Crawl]);
    assert!(got.contains(&"crawl/site.lua".to_string()), "{got:?}");
    assert!(
        got.iter().any(|p| p.starts_with("crawlers/known/")),
        "the global crawler tree ships: {got:?}"
    );
    assert!(
        !got.contains(&"assets/scene-map.json".to_string()),
        "a crawler does not mix: {got:?}"
    );
    assert!(
        got.contains(&"prompts/analyze.txt".to_string()),
        "the prompts are not a per-stage file: {got:?}"
    );
}

/// Render reads none of these trees — its voice store travels in `models/`
#[test]
fn a_render_box_gets_its_cast_and_no_media() {
    let l = fixture("render");
    let got = paths(&l, &[Stage::Render]);
    assert_eq!(
        got,
        vec![
            "data/cast-default-vieneu.json".to_string(),
            "prompts/analyze.txt".to_string(),
            "prompts/script.txt".to_string(),
            "voices.json".to_string(),
        ]
    );
    assert!(
        !got.iter().any(|p| p.starts_with("assets/")),
        "no media, no registries: {got:?}"
    );
}

/// A checkout with **no adapter home at all** — the pre-split shape, where
#[test]
fn a_workspace_adapter_ships_its_own_prompts_under_the_same_names() {
    let root = fixture("adapter-prompts");
    let book = root.root.join("workspaces/book");
    std::fs::create_dir_all(book.join("prompts")).unwrap();
    std::fs::write(book.join("prompts/analyze.txt"), "xianxia-en-US").unwrap();
    let l = crate::Layout {
        root: root.root.clone(),
        work: book.clone(),
        adapter: "xianxia-en-US".into(),
        engine: crate::paths::DEFAULT_ENGINE.into(),
    };

    let shipped = Sources::plan(&l, &[Stage::Digest]).unwrap();
    let prompt = shipped
        .members
        .iter()
        .find(|m| m.to == "prompts/analyze.txt")
        .expect("the prompts ride every bundle");
    assert_eq!(prompt.from, book.join("prompts/analyze.txt"));
    assert_eq!(prompt.base, book, "cut from the adapter's tree");

    // The checkout's tree answers when the workspace has none: the
    let bare = Sources::plan(&root, &[Stage::Digest]).unwrap();
    let prompt = bare
        .members
        .iter()
        .find(|m| m.to == "prompts/analyze.txt")
        .expect("the prompts ride every bundle");
    assert_eq!(prompt.from, root.prompts_dir().join("analyze.txt"));
    assert_eq!(prompt.base, root.root);
}

/// **Every adapter this checkout carries ships**, not only the one in
#[test]
fn every_adapter_home_ships_and_the_bundle_says_which_languages_it_holds() {
    let l = fixture("adapters-all");
    let adapters = l.root.join(crate::paths::ADAPTERS_DIR);
    for (name, file) in [
        ("xianxia-vi-VN", "prompts/analyze.txt"),
        ("xianxia-en-US", "prompts/analyze.txt"),
    ] {
        let p = adapters.join(name).join(file);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, format!("{name}/{file}")).unwrap();
    }

    let s = Sources::plan(&l, &[Stage::Crawl, Stage::Digest]).unwrap();
    assert_eq!(
        s.adapters,
        vec!["xianxia-en-US".to_string(), "xianxia-vi-VN".to_string()],
        "sorted, both, and neither had to be in force"
    );
    assert_eq!(
        s.slots(),
        vec![
            "crawl@xianxia-en-US",
            "crawl@xianxia-vi-VN",
            "digest@xianxia-en-US",
            "digest@xianxia-vi-VN",
        ]
    );
    let got = paths(&l, &[Stage::Crawl, Stage::Digest]);
    for want in [
        "adapters/xianxia-vi-VN/prompts/analyze.txt",
        "adapters/xianxia-en-US/prompts/analyze.txt",
    ] {
        assert!(got.contains(&want.to_string()), "{want} missing: {got:?}");
    }
    // The flat prompts are *not* beside them: the resolver prefers the home,
    assert!(
        !got.contains(&"prompts/analyze.txt".to_string()),
        "the checkout's own prompts travelled beside the homes: {got:?}"
    );
    assert!(
        !got.iter().any(|p| p.starts_with("assets/crawl/")),
        "the retired pack crawlers travelled: {got:?}"
    );
    // …the global crawler tree ships once for every language…
    assert!(
        got.contains(&"crawlers/known/storya.lua".to_string()),
        "{got:?}"
    );
    // …and a book's own crawlers still ride along, which is what keeps a
    assert!(got.contains(&"crawl/site.lua".to_string()), "{got:?}");

    // The manifest carries the same claim, so the box can report it.
    let manifest = s.manifest().unwrap();
    assert_eq!(manifest.slots, s.slots());
}

/// The two halves of the gate's vocabulary: the spelling a slot travels
#[test]
fn a_slot_names_its_adapter_and_a_bare_stage_covers_them_all() {
    assert_eq!(slot(Stage::Digest, "vi-VN"), "digest@vi-VN");
    let slots = vec![slot(Stage::Digest, "vi-VN"), slot(Stage::Merge, "en-US")];
    assert!(holds(&slots, Stage::Digest, "vi-VN"));
    assert!(
        !holds(&slots, Stage::Digest, "en-US"),
        "the pair is the unit, not the stage"
    );
    assert!(!holds(&slots, Stage::Render, "vi-VN"));
    // A stage with no adapter is the pre-slot spelling, and reads as every
    let bare = vec!["digest".to_string()];
    assert!(holds(&bare, Stage::Digest, "vi-VN"));
    assert!(holds(&bare, Stage::Digest, "anything-at-all"));
    assert!(!holds(&bare, Stage::Merge, "vi-VN"));
    assert!(
        !holds(&[], Stage::Digest, "vi-VN"),
        "nothing covers nothing"
    );
}

/// **The prompt is not a stage's file.** A box can gain `digest` with one
#[test]
fn every_bundle_carries_the_prompts_whatever_the_policy() {
    let l = fixture("prompts-always");
    for stages in [
        vec![Stage::Crawl],
        vec![Stage::Digest],
        vec![Stage::Render],
        vec![Stage::Merge],
        vec![Stage::Render, Stage::Merge],
        Stage::ALL.to_vec(),
    ] {
        let got = paths(&l, &stages);
        for prompt in ["prompts/analyze.txt", "prompts/script.txt"] {
            assert!(
                got.contains(&prompt.to_string()),
                "{prompt} missing for {stages:?}: {got:?}"
            );
        }
    }
    // …but a policy that covers no stage is refused outright, because the
    let err = Sources::plan(&l, &[]).unwrap_err().to_string();
    assert!(err.contains("covers no stage"), "{err}");
}

/// The whole point of hashing a set rather than a tree: a policy change has
#[test]
fn widening_the_policy_changes_the_digest_and_reordering_does_not() {
    let l = fixture("policy");
    let digest = Sources::plan(&l, &[Stage::Digest]).unwrap();
    let both = Sources::plan(&l, &[Stage::Digest, Stage::Merge]).unwrap();
    let dh = Sources::hash(&digest.manifest().unwrap());
    let bh = Sources::hash(&both.manifest().unwrap());
    assert_ne!(dh, bh, "adding merge must drift the set");

    // The policy list's order is the scheduler's preference, not content.
    let reordered = stages_of(&policy(&[Stage::Merge, Stage::Digest]));
    assert_eq!(
        reordered,
        vec![Stage::Digest, Stage::Merge],
        "canonical order, whatever the policy's own order was"
    );
    let plain = stages_of(&policy(&[Stage::Digest, Stage::Merge]));
    assert_eq!(reordered, plain);
}

#[test]
fn the_manifest_is_stable_and_moves_with_content() {
    let l = fixture("manifest");
    let a = Sources::plan(&l, &[Stage::Merge])
        .unwrap()
        .manifest()
        .unwrap();
    let b = Sources::plan(&l, &[Stage::Merge])
        .unwrap()
        .manifest()
        .unwrap();
    assert_eq!(
        Sources::hash(&a),
        Sources::hash(&b),
        "same set, same digest"
    );
    assert!(
        !a.files.keys().any(|k| k.contains(".DS_Store")),
        "OS noise entered the manifest: {:?}",
        a.files.keys()
    );

    std::fs::write(l.assets().join("music/market-bg-1.mp3"), b"different").unwrap();
    let c = Sources::plan(&l, &[Stage::Merge])
        .unwrap()
        .manifest()
        .unwrap();
    assert_ne!(
        a.files["assets/music/market-bg-1.mp3"],
        c.files["assets/music/market-bg-1.mp3"]
    );
    assert_ne!(Sources::hash(&a), Sources::hash(&c));
}

/// A workspace that owns its own `assets/` — the shape `workspace new
#[test]
fn a_workspace_owning_assets_ships_its_own_tree_with_a_consistent_base() {
    let dir = std::env::temp_dir().join("bm-sources-ws-assets");
    let _ = std::fs::remove_dir_all(&dir);
    // The checkout has the fixture's assets too, so a wrong base would still
    crate::profile::install_fixture(&dir).unwrap();
    let work = dir.join("workspaces/book");
    std::fs::create_dir_all(work.join("assets/effects")).unwrap();
    std::fs::write(work.join("assets/scene-map.json"), "{}").unwrap();
    std::fs::write(
        work.join("assets/effect-pool.json"),
        r#"{"clash":{"tags":["clash"],"files":["effects/clash-1.mp3"]}}"#,
    )
    .unwrap();
    std::fs::write(work.join("assets/effects/clash-1.mp3"), b"clip").unwrap();

    let l = crate::Layout {
        root: dir.clone(),
        work: work.clone(),
        ..crate::Layout::new(dir.clone())
    };
    // `debug_assert_eq!` inside `push_member` is the invariant; asserting it
    let s = Sources::plan(&l, &[Stage::Merge]).unwrap();
    for m in &s.members {
        assert_eq!(
            m.base.join(&m.to),
            m.from,
            "member {} addressed against its base",
            m.to
        );
    }
    let clip = s
        .members
        .iter()
        .find(|m| m.to == "assets/effects/clash-1.mp3")
        .expect("the workspace's registered clip rides the bundle");
    assert_eq!(
        clip.from,
        work.join("assets/effects/clash-1.mp3"),
        "the clip comes from the workspace's own tree"
    );
    assert_eq!(
        clip.base, work,
        "and is addressed relative to the workspace, not the checkout root"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A registry naming a clip that is not here is reported, not shipped and
#[test]
fn a_registry_naming_an_absent_clip_is_reported_rather_than_shipped() {
    let l = fixture("missing-clip");
    // Self-contained registries: the fixture's own name clips this checkout
    std::fs::write(l.assets().join("effect-pool.json"), "{}").unwrap();
    std::fs::write(l.assets().join("inject-pool.json"), "{}").unwrap();
    std::fs::write(
        l.assets().join("music-pool.json"),
        r#"{"market":{"tags":["market"],"files":["music/market-bg-1.mp3","music/ghost-bg-1.mp3"]}}"#,
    )
    .unwrap();
    let s = Sources::plan(&l, &[Stage::Merge]).unwrap();
    assert_eq!(s.missing, vec!["market: music/ghost-bg-1.mp3".to_string()]);
    assert!(!s.members.iter().any(|m| m.to.contains("ghost")));
    // …and the take that *is* here goes, under the path the merge resolves.
    assert!(s
        .members
        .iter()
        .any(|m| m.to == "assets/music/market-bg-1.mp3"));
}

/// A base outside `assets/` is a registry line that would have escaped the
#[test]
fn a_registry_clip_outside_assets_is_refused() {
    let l = fixture("escape");
    std::fs::write(l.assets().join("effect-pool.json"), "{}").unwrap();
    std::fs::write(l.assets().join("inject-pool.json"), "{}").unwrap();
    std::fs::write(
        l.assets().join("music-pool.json"),
        r#"{"market":{"tags":["market"],"files":["../../secrets.mp3"]}}"#,
    )
    .unwrap();
    let s = Sources::plan(&l, &[Stage::Merge]).unwrap();
    assert_eq!(s.missing.len(), 1);
    assert!(s.missing[0].contains("not under assets"), "{:?}", s.missing);
}

#[test]
fn the_extract_script_prunes_the_trees_it_owns_and_removes_refs() {
    let s = extract_script();
    assert!(
        s.contains(r#"rm -rf "$D/prompts" "$D/assets" "$D/crawl" "$D/adapters""#),
        "{s}"
    );
    assert!(
        s.contains(r#""$D/refs""#),
        "the inductor's own material must leave: {s}"
    );
    assert!(s.contains("zstd -dc"), "{s}");
    assert!(s.contains("SOURCES-OK"), "{s}");
}

/// With a pack release configured the bundle step spares `$D/assets`: the
#[test]
fn the_pack_extract_spares_assets_and_prunes_the_rest() {
    let s = extract_script_keep_assets();
    assert!(
        s.contains(r#"rm -rf "$D/prompts" "$D/crawl" "$D/adapters""#),
        "{s}"
    );
    assert!(!s.contains(r#""$D/assets""#), "assets/ must survive: {s}");
    assert!(
        s.contains(r#""$D/refs""#),
        "the inductor's own material must still leave: {s}"
    );
    assert!(s.contains("SOURCES-OK"), "{s}");
}

/// A fetched pack takes the **whole** `assets/` subtree out of the bundle,
#[test]
fn a_fetched_pack_takes_the_whole_assets_subtree_out_of_the_bundle() {
    let l = fixture("pack-split");
    let stages = [Stage::Digest, Stage::Merge, Stage::Crawl];
    let pushed = Sources::plan(&l, &stages).unwrap();
    let release = crate::artifact::PackRelease::for_repo("o/n", "xianxia", "0.1.0", "aa").unwrap();
    let fetched = Sources::plan_for(&l, &stages, Some(&release)).unwrap();

    let under = |s: &Sources, prefix: &str| {
        s.members
            .iter()
            .filter(|m| m.to.starts_with(prefix))
            .map(|m| m.to.clone())
            .collect::<Vec<_>>()
    };
    let had_assets = under(&pushed, "assets/");
    assert!(
        !had_assets.is_empty(),
        "the fixture must have assets to make this a real reduction"
    );
    assert!(
        under(&fetched, "assets/").is_empty(),
        "a fetched pack owns the whole subtree"
    );
    // Everything else is untouched: the prompts, the casts, the workspace's
    assert_eq!(under(&fetched, "prompts/"), under(&pushed, "prompts/"));
    assert_eq!(under(&fetched, "crawl/"), under(&pushed, "crawl/"));
    assert_eq!(fetched.slots(), pushed.slots(), "the policy is unchanged");
    assert!(
        fetched.bytes() < pushed.bytes(),
        "…and the point of it is fewer bytes over the uplink"
    );

    // The delivery is a replacement of the trees it owns, `assets/` among
    let s = extract_script();
    assert!(s.contains(r#""$D/assets""#), "{s}");
}

/// The bundle and the stamp must describe the same files, and the pack is
#[test]
fn a_pack_digest_moves_only_the_pack() {
    let l = fixture("pack-digest");
    let stages = [Stage::Digest, Stage::Merge];
    let release = crate::artifact::PackRelease::for_repo("o/n", "xianxia", "0.1.0", "aa").unwrap();
    let a = Sources::plan_for(&l, &stages, Some(&release)).unwrap();
    let b = Sources::plan_for(&l, &stages, Some(&release)).unwrap();
    assert_eq!(
        Sources::hash(&a.manifest().unwrap()),
        Sources::hash(&b.manifest().unwrap()),
        "the same pack and the same policy are the same bundle"
    );
    let pushed = Sources::plan(&l, &stages).unwrap();
    assert_ne!(
        Sources::hash(&pushed.manifest().unwrap()),
        Sources::hash(&a.manifest().unwrap()),
        "taking the profile out of the bundle changes the bundle"
    );
}

#[test]
fn a_bundle_round_trips_through_tar_and_zstd() {
    // Needs the real tools; the whole push path does, and a silent skip
    let l = fixture("roundtrip");
    let s = Sources::plan(&l, &[Stage::Merge, Stage::Digest]).unwrap();
    let manifest = s.manifest().unwrap();
    let dir = std::env::temp_dir().join("bm-sources-roundtrip-out");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join(BUNDLE_NAME);
    s.pack(&manifest, &out).unwrap();
    assert!(out.is_file(), "no bundle was written");
    assert!(std::fs::metadata(&out).unwrap().len() > 0);

    // Unpack it the way a worker does and check the paths landed.
    let dst = dir.join("worker");
    std::fs::create_dir_all(&dst).unwrap();
    let sh = format!(
        "set -e; zstd -dc {bundle} | tar -xf - -C {dst}",
        bundle = shq(&out.display().to_string()),
        dst = shq(&dst.display().to_string())
    );
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(&sh)
        .status()
        .unwrap();
    assert!(status.success(), "extract failed: {sh}");
    for rel in [
        "assets/music/market-bg-1.mp3",
        "assets/effect-pool.json",
        "prompts/analyze.txt",
        "voices.json",
        "data/cast-default-vieneu.json",
        MANIFEST_NAME,
    ] {
        assert!(dst.join(rel).is_file(), "{rel} did not land");
    }
    // The manifest was carried too, and describes what landed.
    let back: SourcesManifest =
        serde_json::from_str(&std::fs::read_to_string(dst.join(MANIFEST_NAME)).unwrap()).unwrap();
    assert_eq!(back.files, manifest.files);
    assert_eq!(back.slots, vec!["digest@default", "merge@default"]);
    assert!(!dst.join("assets/music/leftover-bg-9.mp3").exists());
}

/// The archive holds the plan and nothing beside it.
#[test]
fn the_bundle_holds_exactly_the_plan_and_no_appledouble_junk() {
    let l = fixture("exact");
    // The attribute is what makes libarchive write the sidecar, so the
    #[cfg(target_os = "macos")]
    {
        let marked = l.assets().join("music/market-bg-1.mp3");
        let ok = std::process::Command::new("xattr")
            .args(["-w", "com.apple.provenance", "x"])
            .arg(&marked)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "could not set the attribute this test needs");
    }

    let s = Sources::plan(&l, &[Stage::Merge, Stage::Digest]).unwrap();
    let manifest = s.manifest().unwrap();
    let dir = std::env::temp_dir().join("bm-sources-exact-out");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join(BUNDLE_NAME);
    s.pack(&manifest, &out).unwrap();

    let raw = std::process::Command::new("zstd")
        .arg("-dc")
        .arg(&out)
        .output()
        .expect("zstd -dc");
    assert!(raw.status.success(), "zstd refused the bundle");
    let members = tar_members(&raw.stdout);
    for m in &members {
        assert!(
            m == MANIFEST_NAME || manifest.files.contains_key(m),
            "{m} is in the archive and not in the plan"
        );
        assert!(!m.starts_with("._"), "an AppleDouble sidecar traveled: {m}");
    }
    assert_eq!(
        members.len(),
        manifest.files.len() + 1,
        "members: {members:?}"
    );
}

/// The member names of a `tar` stream, exactly as a Linux box would see
fn tar_members(bytes: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at + 512 <= bytes.len() {
        let block = &bytes[at..at + 512];
        if block.iter().all(|b| *b == 0) {
            break;
        }
        let name = block[..100].split(|b| *b == 0).next().unwrap_or_default();
        let size = std::str::from_utf8(&block[124..136])
            .ok()
            .map(|s| s.trim_matches(['\0', ' ']))
            .and_then(|s| usize::from_str_radix(s, 8).ok())
            .unwrap_or(0);
        if block[156] != b'x' && block[156] != b'g' && !name.is_empty() {
            out.push(String::from_utf8_lossy(name).to_string());
        }
        at += 512 + size.div_ceil(512) * 512;
    }
    out
}

fn shq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
