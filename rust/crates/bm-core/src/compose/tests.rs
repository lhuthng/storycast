use super::graph::closure;
use super::patch::value_hash;
use super::*;
use crate::audio_pool::load_pool;

/// A bare `assets/` tree, no pack and no dependencies.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bm-compose-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    let assets = dir.join("assets");
    std::fs::create_dir_all(&assets).unwrap();
    assets
}

fn dep_tree(assets: &Path, name: &str) -> PathBuf {
    let dir = extends_dir(assets).join(name);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn sound(tag: &str) -> String {
    format!(r#"{{"tags":["{tag}"],"files":["effects/{tag}-1.mp3"]}}"#)
}

fn write_pool(dir: &Path, registry: &str, entries: &[(&str, &str)]) {
    let body: Vec<String> = entries
        .iter()
        .map(|(k, t)| format!(r#"  "{k}": {}"#, sound(t)))
        .collect();
    std::fs::write(
        dir.join(registry),
        format!("{{\n{}\n}}\n", body.join(",\n")),
    )
    .unwrap();
}

/// **The diamond, folded once.** `B` and `C` both name `E`, which is the
#[test]
fn a_shared_parent_is_folded_once_and_the_map_says_how_it_was_reached() {
    let assets = scratch("diamond");
    std::fs::write(pack_path(&assets), r#"{"deps":["B","C"]}"#).unwrap();
    // E is the shared parent; F overrides one of its keys (the "some" case).
    write_pool(
        &dep_tree(&assets, "E"),
        "effect-pool.json",
        &[("wind", "E-wind"), ("rain", "E-rain")],
    );
    write_pool(
        &dep_tree(&assets, "F"),
        "effect-pool.json",
        &[("wind", "F-wind")],
    );
    let b = dep_tree(&assets, "B");
    std::fs::write(pack_path(&b), r#"{"deps":["E","F"]}"#).unwrap();
    write_pool(&b, "effect-pool.json", &[("night", "B-night")]);
    write_pool(
        &dep_tree(&assets, "G"),
        "effect-pool.json",
        &[("snow", "G-snow")],
    );
    let c = dep_tree(&assets, "C");
    std::fs::write(pack_path(&c), r#"{"deps":["E","G"]}"#).unwrap();
    // C's own rain, so the strongest dependency visibly wins a shared key.
    write_pool(&c, "effect-pool.json", &[("rain", "C-rain")]);

    let tree = closure(&assets).unwrap();
    let names: Vec<&str> = tree.iter().map(|n| n.name.as_str()).collect();
    assert_eq!(
        names,
        ["E", "F", "B", "G", "C"],
        "weakest first, parents before children, each pack once"
    );
    assert_eq!(
        tree.iter().filter(|n| n.name == "E").count(),
        1,
        "E is one node, not one per parent"
    );
    let shared = tree.iter().find(|n| n.name == "E").unwrap();
    assert!(!shared.direct, "E is reached through its children");
    assert_eq!(shared.via, ["B", "C"], "and the map remembers both paths");
    assert_eq!(
        shared.hash.len(),
        64,
        "a node carries the hash it folded at"
    );
    let direct = tree.iter().find(|n| n.name == "B").unwrap();
    assert!(direct.direct && direct.via.is_empty(), "B is named here");

    let r = resolve(&assets, false).unwrap();
    assert_eq!(r.tree, 5, "the report counts the closure: {r:?}");
    assert_eq!(r.deps.len(), 2, "but `deps` stays the direct list");

    // The record is the map, and it says which node is a dependency and
    let marker = read_marker(&assets);
    let recorded: Vec<&str> = marker.tree.iter().map(|n| n.name.as_str()).collect();
    assert_eq!(recorded, names, "the record is the closure, in fold order");
    let deps: Vec<&str> = marker.deps.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(deps, ["B", "C"], "a release names the direct ones");
    assert_eq!(
        marker.tree.iter().find(|n| n.name == "E").unwrap().via,
        ["B", "C"],
        "the shared parent survives a re-resolve with both paths"
    );

    // And the order is the precedence: E first, F overrides it, C beats B.
    let pool = load_pool(&assets.join("effect-pool.json"));
    assert_eq!(pool["wind"].tags, vec!["F-wind"], "F overrides E");
    assert_eq!(pool["rain"].tags, vec!["C-rain"], "C beats B and E");
    assert_eq!(pool["night"].tags, vec!["B-night"]);
    assert_eq!(pool["snow"].tags, vec!["G-snow"]);
    assert!(
        marker.keys["effect-pool.json"].contains_key("snow"),
        "G's own key is recorded as inherited"
    );

    // A second resolve reaches the same answer, E still once.
    let again = resolve(&assets, false).unwrap();
    assert!(!again.changed(), "{again:?}");
    assert_eq!(
        read_marker(&assets)
            .tree
            .iter()
            .filter(|n| n.name == "E")
            .count(),
        1
    );
}

/// A parent that moves *under* a dependency the child never named directly
#[test]
fn a_parent_that_moves_below_a_dependency_makes_the_child_stale() {
    let assets = scratch("deep-stale");
    std::fs::write(pack_path(&assets), r#"{"deps":["B"]}"#).unwrap();
    let b = dep_tree(&assets, "B");
    std::fs::write(pack_path(&b), r#"{"deps":["E"]}"#).unwrap();
    let e = dep_tree(&assets, "E");
    write_pool(&e, "effect-pool.json", &[("wind", "E-wind")]);

    assert!(resolve(&assets, false).unwrap().stale.is_empty());

    // E moves — one more clip — and only the closure can say so.
    std::fs::create_dir_all(e.join("effects")).unwrap();
    std::fs::write(e.join("effects/wind-2.mp3"), b"clip").unwrap();
    let r = resolve(&assets, false).unwrap();
    assert_eq!(r.stale, ["E"], "the grandparent moved: {r:?}");
    assert!(
        assets.join("effects/wind-2.mp3").is_file(),
        "and its content still arrives"
    );
    assert!(resolve(&assets, false).unwrap().stale.is_empty());
}

/// A graph that loops is a mistake to name, not to follow: the walk says
#[test]
fn a_graph_that_loops_is_refused_with_the_path() {
    let assets = scratch("cycle");
    std::fs::write(pack_path(&assets), r#"{"deps":["B"]}"#).unwrap();
    let b = dep_tree(&assets, "B");
    std::fs::write(pack_path(&b), r#"{"deps":["B2"]}"#).unwrap();
    let b2 = dep_tree(&assets, "B2");
    std::fs::write(pack_path(&b2), r#"{"deps":["B"]}"#).unwrap();

    let err = closure(&assets).unwrap_err().to_string();
    assert!(err.contains("depends on itself"), "{err}");
    assert!(err.contains("B -> B2 -> B"), "it names the loop: {err}");
}

/// A dependency that is not on disk is named before anything is folded in,
#[test]
fn a_missing_pack_in_the_closure_is_refused_by_name() {
    let assets = scratch("missing-in-closure");
    std::fs::write(pack_path(&assets), r#"{"deps":["B"]}"#).unwrap();
    let b = dep_tree(&assets, "B");
    std::fs::write(pack_path(&b), r#"{"deps":["E"]}"#).unwrap();

    let err = resolve(&assets, false).unwrap_err().to_string();
    assert!(err.contains("'E' is not unpacked"), "{err}");
    assert!(
        !marker_path(&assets).exists(),
        "nothing was recorded for a tree that never resolved"
    );
}

#[test]
fn a_pack_with_no_dependencies_resolves_to_nothing() {
    let assets = scratch("no-deps");
    std::fs::write(assets.join("scene-map.json"), "{}").unwrap();
    let before = std::fs::read_dir(&assets).unwrap().count();

    let r = resolve(&assets, false).unwrap();
    assert!(!r.changed(), "{r:?}");
    assert!(r.deps.is_empty() && r.stale.is_empty());
    assert!(
        !marker_path(&assets).exists(),
        "nothing inherited, no record"
    );
    assert_eq!(std::fs::read_dir(&assets).unwrap().count(), before);
    // And it stays a no-op however often it runs.
    assert!(!resolve(&assets, false).unwrap().changed());
}

#[test]
fn a_dependency_fills_in_only_what_the_asset_lacks() {
    let assets = scratch("fill-in");
    std::fs::write(pack_path(&assets), r#"{"deps":["common"]}"#).unwrap();
    // The asset's own `door`, spelled differently so a wrong winner shows.
    write_pool(&assets, "effect-pool.json", &[("door", "my-own-door")]);
    let dep = dep_tree(&assets, "common");
    write_pool(
        &dep,
        "effect-pool.json",
        &[("door", "inherited"), ("wind", "wind")],
    );
    std::fs::create_dir_all(dep.join("effects")).unwrap();
    std::fs::write(dep.join("effects/wind-1.mp3"), b"clip").unwrap();

    let r = resolve(&assets, false).unwrap();
    assert_eq!(r.added, 2, "wind's key and wind's clip: {r:?}");
    assert_eq!(r.withdrawn, 0);

    let pool = load_pool(&assets.join("effect-pool.json"));
    assert_eq!(
        pool["door"].tags,
        vec!["my-own-door"],
        "the asset's own won"
    );
    assert_eq!(
        pool["wind"].tags,
        vec!["wind"],
        "and the missing one filled in"
    );
    assert!(
        assets.join("effects/wind-1.mp3").is_file(),
        "the clip came with its registry entry"
    );

    // The record names the dependency and what it contributed.
    let marker = read_marker(&assets);
    assert_eq!(marker.deps.len(), 1);
    assert_eq!(marker.deps[0].name, "common");
    assert!(marker.keys["effect-pool.json"].contains_key("wind"));
    assert!(!marker.keys["effect-pool.json"].contains_key("door"));
    assert!(marker.files.contains_key("effects/wind-1.mp3"));

    // Idempotent: nothing in, nothing out — and the asset's own entry is
    let again = resolve(&assets, false).unwrap();
    assert_eq!(again.added, 0, "{again:?}");
    assert_eq!(again.withdrawn, 0);
    assert_eq!(
        load_pool(&assets.join("effect-pool.json"))["door"].tags,
        vec!["my-own-door"]
    );
}

#[test]
fn later_dependencies_override_earlier_ones() {
    let assets = scratch("order");
    std::fs::write(pack_path(&assets), r#"{"deps":["common","xianxia-base"]}"#).unwrap();
    write_pool(
        &dep_tree(&assets, "common"),
        "effect-pool.json",
        &[("wind", "common-wind")],
    );
    write_pool(
        &dep_tree(&assets, "xianxia-base"),
        "effect-pool.json",
        &[("wind", "genre-wind")],
    );

    resolve(&assets, false).unwrap();
    let pool = load_pool(&assets.join("effect-pool.json"));
    assert_eq!(pool["wind"].tags, vec!["genre-wind"], "the later one wins");
    // The record follows the winner, so the withdrawal takes the right one.
    let marker = read_marker(&assets);
    assert_eq!(
        marker.keys["effect-pool.json"]["wind"],
        value_hash(&pool["wind"])
    );
}

#[test]
fn a_re_resolve_withdraws_a_key_the_parent_has_dropped() {
    let assets = scratch("withdraw");
    std::fs::write(pack_path(&assets), r#"{"deps":["common"]}"#).unwrap();
    let dep = dep_tree(&assets, "common");
    write_pool(
        &dep,
        "effect-pool.json",
        &[("wind", "wind"), ("rain", "rain")],
    );
    std::fs::create_dir_all(dep.join("effects")).unwrap();
    std::fs::write(dep.join("effects/rain-1.mp3"), b"clip").unwrap();

    assert_eq!(resolve(&assets, false).unwrap().added, 3);
    assert!(assets.join("effects/rain-1.mp3").is_file());

    // The parent drops `rain` and its clip. A fill-in-only merge would leave
    std::fs::remove_file(dep.join("effects/rain-1.mp3")).unwrap();
    write_pool(&dep, "effect-pool.json", &[("wind", "wind")]);
    let r = resolve(&assets, false).unwrap();
    assert_eq!(r.withdrawn, 2, "rain's key and rain's clip: {r:?}");

    let pool = load_pool(&assets.join("effect-pool.json"));
    assert!(!pool.contains_key("rain"), "the dropped key is gone");
    assert!(pool.contains_key("wind"), "and the survivor stayed");
    assert!(!assets.join("effects/rain-1.mp3").exists());
    assert!(!read_marker(&assets).keys["effect-pool.json"].contains_key("rain"));
}

#[test]
fn an_edited_inherited_entry_is_adopted_rather_than_overwritten() {
    let assets = scratch("adopt");
    std::fs::write(pack_path(&assets), r#"{"deps":["common"]}"#).unwrap();
    let dep = dep_tree(&assets, "common");
    write_pool(&dep, "effect-pool.json", &[("wind", "wind")]);
    resolve(&assets, false).unwrap();

    // The operator retunes it — the reason the live tree is the source of
    write_pool(&assets, "effect-pool.json", &[("wind", "retuned")]);
    let r = resolve(&assets, false).unwrap();
    assert_eq!(r.adopted, 1, "{r:?}");
    assert_eq!(r.added, 0, "an adopted entry is not re-filled");
    assert_eq!(r.withdrawn, 0, "and never withdrawn");
    assert_eq!(
        load_pool(&assets.join("effect-pool.json"))["wind"].tags,
        vec!["retuned"]
    );
    let marker = read_marker(&assets);
    assert!(
        !marker
            .keys
            .get("effect-pool.json")
            .is_some_and(|ks| ks.contains_key("wind")),
        "it is the asset's own now, and the record says so: {marker:?}"
    );
}

#[test]
fn a_dry_run_reports_what_a_real_one_would_do_and_writes_nothing() {
    let assets = scratch("dry-run");
    std::fs::write(pack_path(&assets), r#"{"deps":["common"]}"#).unwrap();
    let dep = dep_tree(&assets, "common");
    write_pool(&dep, "effect-pool.json", &[("wind", "wind")]);
    std::fs::create_dir_all(dep.join("effects")).unwrap();
    std::fs::write(dep.join("effects/wind-1.mp3"), b"clip").unwrap();

    let dry = resolve(&assets, true).unwrap();
    assert!(dry.dry_run && dry.changed());
    assert_eq!(dry.added, 2);
    assert!(!marker_path(&assets).exists(), "no record written");
    assert!(
        !assets.join("effect-pool.json").exists(),
        "no registry written"
    );
    assert!(
        !assets.join("effects/wind-1.mp3").exists(),
        "no clip copied"
    );

    // The real run does exactly what the dry one said.
    let real = resolve(&assets, false).unwrap();
    assert_eq!(
        (real.added, real.withdrawn),
        (dry.added, dry.withdrawn),
        "a dry run that disagrees with the run is worse than none"
    );
    assert!(assets.join("effects/wind-1.mp3").is_file());
}

#[test]
fn a_second_resolve_leaves_the_files_already_in_place_alone() {
    // Delete-then-refill rewrites every inherited clip on every resolve —
    let assets = scratch("file-churn");
    std::fs::write(pack_path(&assets), r#"{"deps":["common"]}"#).unwrap();
    let dep = dep_tree(&assets, "common");
    write_pool(&dep, "effect-pool.json", &[("wind", "wind")]);
    std::fs::create_dir_all(dep.join("effects")).unwrap();
    std::fs::write(dep.join("effects/wind-1.mp3"), b"clip").unwrap();

    resolve(&assets, false).unwrap();
    let clip = assets.join("effects/wind-1.mp3");
    let before = std::fs::metadata(&clip).unwrap().modified().unwrap();

    let dry = resolve(&assets, true).unwrap();
    assert!(
        !dry.changed(),
        "a dry run over a settled tree changes nothing: {dry:?}"
    );
    let real = resolve(&assets, false).unwrap();
    assert!(!real.changed(), "{real:?}");
    assert_eq!(
        std::fs::metadata(&clip).unwrap().modified().unwrap(),
        before,
        "and the clip was never rewritten"
    );
}

#[test]
fn a_missing_dependency_tree_refuses_and_names_the_path() {
    let assets = scratch("missing-dep");
    std::fs::write(pack_path(&assets), r#"{"deps":["common"]}"#).unwrap();
    let err = resolve(&assets, false).unwrap_err().to_string();
    assert!(err.contains("common"), "{err}");
    assert!(err.contains("_extends"), "it says where it looked: {err}");
}

#[test]
fn a_moving_dependency_is_reported_stale() {
    let assets = scratch("stale");
    std::fs::write(pack_path(&assets), r#"{"deps":["common"]}"#).unwrap();
    let dep = dep_tree(&assets, "common");
    write_pool(&dep, "effect-pool.json", &[("wind", "wind")]);
    assert!(resolve(&assets, false).unwrap().stale.is_empty());

    // The parent gains a sound. Until this tree is re-packed against it, it
    write_pool(
        &dep,
        "effect-pool.json",
        &[("wind", "wind"), ("rain", "rain")],
    );
    let r = resolve(&assets, false).unwrap();
    assert_eq!(r.stale, vec!["common"]);
    assert!(r.summary().contains("STALE"), "{}", r.summary());
    assert!(
        resolve(&assets, false).unwrap().stale.is_empty(),
        "then clean"
    );
}

#[test]
fn dropping_the_last_dependency_withdraws_everything_it_gave() {
    let assets = scratch("drop-all");
    std::fs::write(pack_path(&assets), r#"{"deps":["common"]}"#).unwrap();
    let dep = dep_tree(&assets, "common");
    write_pool(&dep, "effect-pool.json", &[("wind", "wind")]);
    resolve(&assets, false).unwrap();
    assert!(marker_path(&assets).exists());

    std::fs::write(pack_path(&assets), r#"{"deps":[]}"#).unwrap();
    let r = resolve(&assets, false).unwrap();
    assert_eq!(r.withdrawn, 1);
    assert!(
        !assets.join("effect-pool.json").exists()
            || load_pool(&assets.join("effect-pool.json")).is_empty()
    );
    assert!(
        !marker_path(&assets).exists(),
        "a tree with no dependencies carries no record claiming otherwise"
    );
}

/// A scene map on disk, laid out the way the shipped one is.
fn write_map(dir: &Path, body: &str) {
    std::fs::create_dir_all(dir).unwrap();
    let text = format!("{{\n  {body}\n}}\n");
    std::fs::write(dir.join("scene-map.json"), text).unwrap();
}

fn read(assets: &Path, file: &str) -> String {
    std::fs::read_to_string(assets.join(file)).unwrap()
}

/// Every rule's first `match`, in order — the one thing about a rule list
fn matches_of(assets: &Path) -> Vec<String> {
    let text = read(assets, "scene-map.json");
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    v["rules"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["match"][0].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn a_root_asset_owns_the_rules_and_a_genres_own_are_seen_first() {
    let assets = scratch("layered-rules");
    std::fs::write(pack_path(&assets), r#"{"deps":["common"]}"#).unwrap();
    write_map(
        &assets,
        r#""_note": "xianxia only", "rules": [{"match": ["sect"], "effect": ["hall"]}]"#,
    );
    write_map(
        &dep_tree(&assets, "common"),
        r#""_note": "the world", "rules": [{"match": ["rain"]}, {"match": ["night"]}], "layers": {"effect": {"trim": 1.0}}"#,
    );

    resolve(&assets, false).unwrap();
    assert_eq!(
        matches_of(&assets),
        vec!["sect", "rain", "night"],
        "the genre's rule is seen first and the world's sit behind it"
    );
    let text = read(&assets, "scene-map.json");
    assert!(
        text.contains(r#""match": ["sect"]"#),
        "the genre's own rule is still its own bytes: {text}"
    );
    assert!(
        text.contains("\"layers\""),
        "a member it never stated came in: {text}"
    );
    assert!(
        text.contains("xianxia only") && !text.contains("the world"),
        "its own note is the one that stays: {text}"
    );

    // Idempotent: the same answer, and the file is left alone.
    let before = read(&assets, "scene-map.json");
    let again = resolve(&assets, false).unwrap();
    assert!(!again.changed(), "{again:?}");
    assert_eq!(read(&assets, "scene-map.json"), before);
}

#[test]
fn a_rule_the_root_drops_is_withdrawn_from_the_genre() {
    let assets = scratch("layer-withdraw");
    std::fs::write(pack_path(&assets), r#"{"deps":["common"]}"#).unwrap();
    write_map(&assets, r#""rules": [{"match": ["sect"]}]"#);
    let dep = dep_tree(&assets, "common");
    write_map(
        &dep,
        r#""rules": [{"match": ["rain"]}, {"match": ["night"]}]"#,
    );
    resolve(&assets, false).unwrap();
    assert_eq!(matches_of(&assets), vec!["sect", "rain", "night"]);

    // The root drops `night`. A fill-in-only merge would leave it here for
    write_map(&dep, r#""rules": [{"match": ["rain"]}]"#);
    let r = resolve(&assets, false).unwrap();
    assert_eq!(r.withdrawn, 1, "{r:?}");
    assert_eq!(matches_of(&assets), vec!["sect", "rain"]);
    assert!(!resolve(&assets, false).unwrap().changed());
}

#[test]
fn a_stronger_dependency_has_its_rules_seen_before_a_weaker_one() {
    // `deps` is weakest first, and a keyed name wins by replacing a value —
    let assets = scratch("rules-order");
    std::fs::write(
        pack_path(&assets),
        r#"{"deps":["common","weapons","magic"]}"#,
    )
    .unwrap();
    write_map(&assets, r#""rules": [{"match": ["mine"]}]"#);
    write_map(
        &dep_tree(&assets, "common"),
        r#""rules": [{"match": ["common"]}]"#,
    );
    write_map(
        &dep_tree(&assets, "weapons"),
        r#""rules": [{"match": ["weapons"]}]"#,
    );
    write_map(
        &dep_tree(&assets, "magic"),
        r#""rules": [{"match": ["magic"]}]"#,
    );

    resolve(&assets, false).unwrap();
    assert_eq!(
        matches_of(&assets),
        vec!["mine", "magic", "weapons", "common"],
        "the file's own first, then the strongest dependency"
    );
    // And it settles. Asserting the list rather than only the report: a
    let again = resolve(&assets, false).unwrap();
    assert!(!again.changed(), "{again:?}");
    assert_eq!(
        matches_of(&assets),
        vec!["mine", "magic", "weapons", "common"]
    );
}

#[test]
fn a_genre_that_states_a_knob_keeps_its_own_and_inherits_none_of_it() {
    let assets = scratch("layer-whole");
    std::fs::write(pack_path(&assets), r#"{"deps":["common"]}"#).unwrap();
    write_map(&assets, r#""layers": {"effect": {"trim": 0.8}}"#);
    write_map(
        &dep_tree(&assets, "common"),
        r#""layers": {"effect": {"trim": 1.0}, "music": {"level": 0.16}}"#,
    );

    resolve(&assets, false).unwrap();
    let text = read(&assets, "scene-map.json");
    assert!(
        text.contains("0.8"),
        "the genre's block is the one in force: {text}"
    );
    assert!(
        !text.contains("0.16"),
        "and none of the dependency's is half-merged into it: {text}"
    );
}

#[test]
fn a_licence_line_from_the_root_survives_a_genre_stating_its_own() {
    let assets = scratch("layer-licences");
    std::fs::write(pack_path(&assets), r#"{"deps":["common"]}"#).unwrap();
    std::fs::write(
        assets.join("LICENSES.json"),
        r#"{"_note": "ours", "background music (music/)": "Suno"}"#,
    )
    .unwrap();
    let dep = dep_tree(&assets, "common");
    std::fs::write(
        dep.join("LICENSES.json"),
        r#"{"_note": "theirs", "sound effects (effects/, injects/)": "Pixabay"}"#,
    )
    .unwrap();

    resolve(&assets, false).unwrap();
    let text = read(&assets, "LICENSES.json");
    assert!(
        text.contains("Pixabay"),
        "the line that came with the clips is still here: {text}"
    );
    assert!(text.contains("Suno"), "and the genre's own is untouched");
    assert!(
        text.contains("ours") && !text.contains("theirs"),
        "the nearer note wins: {text}"
    );
}

#[test]
fn a_member_name_that_looks_like_a_record_key_is_still_withdrawn() {
    // `LICENSES.json`'s categories are prose, and a record's two separators
    let assets = scratch("layer-separators");
    std::fs::write(pack_path(&assets), r#"{"deps":["common"]}"#).unwrap();
    std::fs::write(assets.join("LICENSES.json"), r#"{"_note": "ours"}"#).unwrap();
    let dep = dep_tree(&assets, "common");
    std::fs::write(
        dep.join("LICENSES.json"),
        r#"{"sound effects (effects/, injects/)": "Pixabay", "ns/+odd": "x"}"#,
    )
    .unwrap();

    resolve(&assets, false).unwrap();
    let text = read(&assets, "LICENSES.json");
    assert!(text.contains("Pixabay") && text.contains("odd"), "{text}");
    let again = resolve(&assets, false).unwrap();
    assert!(!again.changed(), "and it settles: {again:?}");
    assert_eq!(read(&assets, "LICENSES.json"), text);
}

#[test]
fn a_palette_gains_a_mood_the_root_has_and_keeps_the_one_it_had() {
    let assets = scratch("layer-palette");
    std::fs::write(pack_path(&assets), r#"{"deps":["common"]}"#).unwrap();
    write_map(&assets, r#""music_palette": {"quiet": {"tags": ["soft"]}}"#);
    write_map(
        &dep_tree(&assets, "common"),
        r#""music_palette": {"quiet": {"tags": ["quiet", "calm"]}, "tense": {"tags": ["tense"]}}"#,
    );

    resolve(&assets, false).unwrap();
    let text = read(&assets, "scene-map.json");
    assert!(text.contains("tense"), "the mood it lacked came in: {text}");
    assert!(
        text.contains(r#""tags": ["soft"]"#),
        "and the name it had is its own, not the root's: {text}"
    );
}

#[test]
fn a_later_dependency_wins_a_keyed_name_over_an_earlier_one() {
    let assets = scratch("layer-order");
    std::fs::write(pack_path(&assets), r#"{"deps":["common","xianxia-base"]}"#).unwrap();
    write_map(
        &dep_tree(&assets, "common"),
        r#""music_palette": {"tense": {"tags": ["common-tense"]}}"#,
    );
    write_map(
        &dep_tree(&assets, "xianxia-base"),
        r#""music_palette": {"tense": {"tags": ["genre-tense"]}}"#,
    );

    resolve(&assets, false).unwrap();
    let text = read(&assets, "scene-map.json");
    assert!(text.contains("genre-tense"), "the later one wins: {text}");
    assert!(
        !text.contains("common-tense"),
        "and the earlier one is gone: {text}"
    );
}

#[test]
fn a_keyed_member_accumulates_across_dependencies() {
    // The shape a real asset now has: `common` states the world's words,
    let assets = scratch("layer-keyed");
    std::fs::write(pack_path(&assets), r#"{"deps":["common","weapons"]}"#).unwrap();
    std::fs::create_dir_all(dep_tree(&assets, "common")).unwrap();
    std::fs::write(
        dep_tree(&assets, "common").join("tag-aliases.json"),
        r#"{"sound": {"knock": "door-knock", "clang": "metal-hit"}}"#,
    )
    .unwrap();
    std::fs::write(
        dep_tree(&assets, "weapons").join("tag-aliases.json"),
        r#"{"sound": {"slash": "sword-slash"}}"#,
    )
    .unwrap();

    resolve(&assets, false).unwrap();
    let text = read(&assets, "tag-aliases.json");
    for name in ["knock", "clang", "slash"] {
        assert!(text.contains(name), "{name} survived: {text}");
    }
}

#[test]
fn a_stronger_dependency_takes_a_whole_member_a_weaker_one_filled() {
    // `Whole` means the *file's* member wins, not the first dependency's to
    let assets = scratch("layer-whole-order");
    std::fs::write(pack_path(&assets), r#"{"deps":["common","xianxia-base"]}"#).unwrap();
    write_map(&dep_tree(&assets, "common"), r#""pause": {"min_s": 1.0}"#);
    write_map(
        &dep_tree(&assets, "xianxia-base"),
        r#""pause": {"min_s": 3.0}"#,
    );

    resolve(&assets, false).unwrap();
    let text = read(&assets, "scene-map.json");
    assert!(
        text.contains("3.0"),
        "the stronger dependency's knob: {text}"
    );
    assert!(!text.contains("1.0"), "and not the weaker one's: {text}");
}

/// **The release record is a record, not a fold output.** A fold hashes the
#[test]
fn a_resolve_keeps_the_release_record_and_drops_what_left_the_tree() {
    let assets = scratch("versions");
    std::fs::write(pack_path(&assets), r#"{"deps":["common"]}"#).unwrap();
    write_pool(
        &dep_tree(&assets, "common"),
        "effect-pool.json",
        &[("wind", "common-wind")],
    );
    resolve(&assets, false).unwrap();

    let mut versions = BTreeMap::new();
    versions.insert("common".to_string(), "0.1.0".to_string());
    versions.insert("ghost".to_string(), "9.9.9".to_string());
    set_release_versions(&assets, &versions).unwrap();
    assert_eq!(read_marker(&assets).versions.len(), 2, "written as asked");

    let r = resolve(&assets, false).unwrap();
    assert_eq!(r.tree, 1);
    let marker = read_marker(&assets);
    assert_eq!(
        marker.versions.get("common").map(String::as_str),
        Some("0.1.0"),
        "a fold does not forget which release it is holding"
    );
    assert!(
        !marker.versions.contains_key("ghost"),
        "and a name the closure no longer reaches is not a record of anything"
    );

    // Emptying a version is how a dependency stops claiming a release.
    let mut cleared = BTreeMap::new();
    cleared.insert("common".to_string(), String::new());
    set_release_versions(&assets, &cleared).unwrap();
    assert!(read_marker(&assets).versions.is_empty());
}
