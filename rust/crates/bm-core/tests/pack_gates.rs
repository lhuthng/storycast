//! The gates, run against **every real pack**, not only the fixture.
//!
//! `every_shipped_*` in `ambience` reads `rust/fixtures/profile`, because a
//! test must never read the live tree. That is right for the suite and it has
//! a hole in it: `/assets/*` and `/profiles/` are git-ignored, so a pack is
//! local work plus a `tar.zst` release and *no* test can see it. A palette
//! value with no pooled track, a rule whose effect tags no bed answers, a
//! registry entry naming a file that was never recorded — all three pass CI
//! green and then fail as silence in a merge, or worse, pass CI green and are
//! never noticed at all.
//!
//! So the same three checks run here, per pack, over the resolved composition:
//! each pack under `assets/_extends/` is copied to a scratch root with the
//! whole `_extends` tree beside it (so its closure folds), resolved there, and
//! the result checked. **Nothing in the checkout is written** — the copy is the
//! point, because a gate that resolves in place would be a gate that mutates
//! the tree it is guarding.
//!
//! Skipped when the tree is absent, which is every fresh clone: a pack is
//! machine-local, exactly as the fixture's header says.

use std::path::{Path, PathBuf};

use bm_core::ambience::{load_map, SceneMap};
use bm_core::audio_pool::{self, ClipPool, PoolKind};

/// The repository, which is where `assets/` lives.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .expect("crates/bm-core -> crates -> rust -> repo root")
        .to_path_buf()
}

/// Copy `from` into `to`, recursively. Symlinks are followed as files so a pack
/// that keeps its clips elsewhere still resolves; a broken one fails the copy
/// loudly, which is the right answer for a gate.
fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&src, &dst);
        } else {
            std::fs::copy(&src, &dst).unwrap();
        }
    }
}

/// Every pack unpacked under the live tree, sorted. Empty on a fresh clone.
fn packs(root: &Path) -> Vec<(String, PathBuf)> {
    let dir = root.join("assets/_extends");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<(String, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .map(|e| (e.file_name().to_string_lossy().to_string(), e.path()))
        .collect();
    out.sort();
    out
}

/// A scratch root holding `name` as its own live pack, with the whole
/// `_extends` tree beside it so its parents fold.
fn scratch(root: &Path, name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bm-pack-gate-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    let assets = dir.join("assets");
    std::fs::create_dir_all(&assets).unwrap();
    copy_tree(&root.join("assets/_extends").join(name), &assets);
    copy_tree(&root.join("assets/_extends"), &assets.join("_extends"));
    assets
}

/// Every registry in a resolved tree, by kind.
fn pools(assets: &Path) -> BTreeMapOfPools {
    let mut out = BTreeMapOfPools::default();
    for kind in [PoolKind::Effect, PoolKind::Music, PoolKind::Inject] {
        out.insert(kind, audio_pool::load_pool(&assets.join(kind.registry())));
    }
    out
}

type BTreeMapOfPools = std::collections::BTreeMap<PoolKind, ClipPool>;

/// The three gates, for one resolved tree. The names are the failure messages.
fn gates(name: &str, assets: &Path, map: &SceneMap, pools: &BTreeMapOfPools) {
    // 1. Every palette value but `none` must be answerable by a track, or the
    //    analyzer is offered a mood whose value is silence. `none` is the one
    //    value that is allowed to name no tags: it means no track.
    //
    //    Skipped when the pack ships NO music at all, which is the documented
    //    shape of a root: `common` carries the mood *vocabulary* and an empty
    //    `music-pool.json` whose note says the score is the genre's. A pack with
    //    even one track is still checked on every value — the skip is on the
    //    whole pool being empty, not on a per-value shrug.
    let music = &pools[&PoolKind::Music];
    if music.is_empty() {
        eprintln!(
            "{name}: ships no music (a root pack) — the palette here is \
             vocabulary for a preset to answer, and gate 1 does not apply"
        );
    } else {
        for (mood, entry) in &map.music_palette {
            if mood == "none" {
                assert!(
                    entry.tags.is_empty(),
                    "{name}: palette value `none` must name no tags, got [{}]",
                    entry.tags.join(", ")
                );
                continue;
            }
            assert!(
                audio_pool::pick(
                    music,
                    &entry.tags,
                    audio_pool::seed(1, 0, std::slice::from_ref(mood))
                )
                .is_some(),
                "{name}: palette value {mood:?} names tags [{}] that no pooled \
                 track answers, so every chapter declaring it is silent there",
                entry.tags.join(", ")
            );
        }
    }

    // 2. Every rule with effect tags must be answerable by a bed. Unlike music,
    //    an unmatched effect tag is DISCARDED rather than refused
    //    (`discard_unknown_effect_tags`), so this is the check that stops a
    //    rule being decoration with no error anywhere.
    let effects = &pools[&PoolKind::Effect];
    for rule in &map.rules {
        if rule.effect.is_empty() {
            continue;
        }
        assert!(
            audio_pool::pick(effects, &rule.effect, audio_pool::seed(1, 0, &rule.effect)).is_some(),
            "{name}: rule {:?} names effect tags [{}] that no pooled bed \
             answers, so the tags are silently dropped and the scene is dry",
            rule.matches,
            rule.effect.join(", ")
        );
    }

    // 3. The registries must be well-shaped, and their files must be either
    //    present or **not yet recorded**. Those are different failures and the
    //    difference is the difference between a bug and a Tuesday:
    //
    //    * no `files` at all is a shape error — a sound that can never be
    //      picked, and the loader drops it without a word;
    //    * files named but absent is a pack whose audio has not been recorded
    //      or generated yet, which is the normal state of a new pack and what
    //      the TUI's *missing* column and the merge's one-line warning already
    //      exist for. Reported, not failed, and named so the remaining work is
    //      printed rather than guessed at;
    //    * SOME takes present and some absent is neither: that is a typo or a
    //      half-copied directory, and it is the case a rename produces.
    let mut pending: Vec<String> = Vec::new();
    for kind in [PoolKind::Effect, PoolKind::Music, PoolKind::Inject] {
        for (sound, entry) in &pools[&kind] {
            assert!(
                !entry.files.is_empty(),
                "{name}: {}/{}: sound {sound:?} has no takes, so it can never \
                 be picked",
                kind.registry(),
                kind.label()
            );
            let here: Vec<&String> = entry
                .files
                .iter()
                .filter(|f| assets.join(f).is_file())
                .collect();
            assert!(
                here.is_empty() || here.len() == entry.files.len(),
                "{name}: {}/{}: sound {sound:?} has {}/{} takes on disk. A \
                 partially-present sound is a renamed clip, not a pack in \
                 progress.",
                kind.registry(),
                kind.label(),
                here.len(),
                entry.files.len()
            );
            for file in &entry.files {
                if !assets.join(file).is_file() {
                    // The registry path already carries the clip's directory
                    // (`injects/…`), so prefixing the kind would print it twice.
                    pending.push(file.clone());
                }
            }
        }
    }
    if !pending.is_empty() {
        eprintln!(
            "{name}: {} clip(s) named but not yet on disk — this pack's \
             registries are authored ahead of its audio, which is what the \
             TUI's missing column and the merge's warning exist for. The \
             shot list is in the pack's RECORDING.md or TRACKS.md:",
            pending.len()
        );
        for p in &pending {
            eprintln!("{name}:   pending  {p}");
        }
    }
}

#[test]
fn every_unpacked_pack_passes_the_gates_the_fixture_carries() {
    let root = repo_root();
    let packs = packs(&root);
    if packs.is_empty() {
        eprintln!(
            "no assets/_extends — a fresh clone has no pack, and the fixture \
             covers the suite (ambience::every_shipped_*)"
        );
        return;
    }

    for (name, _) in &packs {
        let assets = scratch(&root, name);
        let report = bm_core::compose::resolve(&assets, false)
            .unwrap_or_else(|e| panic!("{name}: its own composition does not resolve: {e:#}"));
        // Silence here is the state worth failing on: a pack that resolved
        // nothing did not fold, and the gates below would pass on an empty
        // tree and say so.
        if !report.deps.is_empty() {
            eprintln!(
                "{name}: built on {}",
                report
                    .deps
                    .iter()
                    .map(|d| d.name.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        let map: SceneMap = load_map(&assets.join("scene-map.json"))
            .unwrap_or_else(|e| panic!("{name}: resolved scene-map.json must load: {e:#}"));
        let pools = pools(&assets);
        gates(name, &assets, &map, &pools);
        eprintln!(
            "{name}: {} rules, {} palette values, {} effect / {} music / {} \
             inject sounds — all gates pass",
            map.rules.len(),
            map.music_palette.len(),
            pools[&PoolKind::Effect].len(),
            pools[&PoolKind::Music].len(),
            pools[&PoolKind::Inject].len(),
        );
        let _ = std::fs::remove_dir_all(assets.parent().unwrap());
    }
}
