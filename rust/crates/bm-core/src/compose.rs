//! Asset composition: a genre's art, built from the assets it depends on.
//!
//! A pack used to be one flat tree, so a sound every genre wants — a door slam,
//! wind, a body hitting the floor — had to exist once per genre, and a
//! correction had to be made once per copy. An asset now names its
//! dependencies in `assets/pack.json`:
//!
//! ```json
//! { "deps": ["common", "xianxia-base"] }
//! ```
//!
//! and this module folds them into the live `assets/` tree, **weakest first**,
//! so `xianxia-base` overrides `common` on a shared key and the asset's own
//! entries override both. A single-parent chain is the one-element case; the
//! list is ordered rather than a set so the precedence is written down rather
//! than inferred.
//!
//! **The dependencies are unpacked, not referenced.** `assets/_extends/<name>/`
//! holds each one's tree — an unpacked release, put there by whoever cut it —
//! so the whole composition lives inside `assets/`, which is the tree the
//! binding hashes and the tree provisioning ships. Nothing resolves through a
//! reference at run time: a reader sees one tree, exactly as it always did.
//!
//! **Composition is fill-in, and whole entry by key.** A sound is a key in a
//! pool registry, so a dependency's `wind` is inherited only if the asset ships
//! no `wind` — tags, files, `mode`, `hold` and `level` together. Fields are not
//! merged: `serde`'s defaults cannot tell "unset" from "set to the default", and
//! a half-inherited entry is a clip whose `mode` came from a sound it is not.
//! Everything that is not a pool registry (a clip, `scene-map.json`,
//! `tag-aliases.json`) is inherited per file, on the same missing-wins rule.
//!
//! **And the resolution remembers what it did.** Fill-in alone is not enough to
//! be regenerable: without a record, a second resolve cannot tell an entry *it*
//! inserted from an entry the asset has always had, so it could never withdraw
//! one a dependency has since dropped. [`Inherited`] (`assets/_extends.json`) is
//! that record — per registry, the keys inserted and a hash of the value
//! inserted; per file, the paths copied in and their content hashes. On the next
//! resolve an inherited entry whose hash *still matches* is withdrawn and
//! re-filled from the dependency, so a parent's change propagates; one whose
//! hash **differs** has been edited by the operator, who has adopted it — it
//! stays, and it stops being tracked. Nothing is ever silently thrown away,
//! which is what makes the merge both idempotent (no change in, no change out)
//! and safe to run at any time.
//!
//! Nothing here runs by itself. `asset resolve` is the verb, and a checkout with
//! no `pack.json` has no dependencies, so a resolve leaves it byte for byte as
//! it was.

use crate::audio_pool::{load_pool, save_pool, ClipPool, PoolKind, Sound};
use crate::profile;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The authored dependency list, at `assets/pack.json`.
pub const PACK_FILE: &str = "pack.json";

/// Where the dependencies are unpacked: `assets/_extends/<name>/`.
///
/// Inside `assets/` on purpose. A composition that lived beside the pack would
/// be two trees to hash and two to ship, and the pack hash would then be a
/// claim about only half of what a reader resolves.
pub const EXTENDS_DIR: &str = "_extends";

/// The resolution record, at `assets/_extends.json`. Generated, never authored.
pub const MARKER_FILE: &str = "_extends.json";

pub fn pack_path(assets: &Path) -> PathBuf {
    assets.join(PACK_FILE)
}

pub fn extends_dir(assets: &Path) -> PathBuf {
    assets.join(EXTENDS_DIR)
}

pub fn marker_path(assets: &Path) -> PathBuf {
    assets.join(MARKER_FILE)
}

/// `assets/pack.json`: the other assets this one is built on, weakest first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pack {
    #[serde(default)]
    pub deps: Vec<String>,
}

/// One dependency, as it stood when the last resolve folded it in.
///
/// The hash is the point: it is what lets a child say "I was built against
/// `common` at *this* content" and therefore be told, cheaply and exactly, that
/// it is stale. A dependency that never moved leaves the child alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DepRecord {
    pub name: String,
    /// A content hash of the dependency's whole tree.
    pub hash: String,
}

/// `assets/_extends.json`: what a resolve put in the live tree, and from where.
///
/// Generated. Read back by the next resolve so it can withdraw what it owns,
/// and by the packer so a release can name the dependencies it was built
/// against.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inherited {
    /// Every dependency folded in, in `pack.json` order.
    #[serde(default)]
    pub deps: Vec<DepRecord>,
    /// Per registry filename, `key -> hash of the value inserted`.
    #[serde(default)]
    pub keys: BTreeMap<String, BTreeMap<String, String>>,
    /// Per file copied in, relative to `assets/`, its content hash.
    #[serde(default)]
    pub files: BTreeMap<String, String>,
}

impl Inherited {
    pub fn is_empty(&self) -> bool {
        self.deps.is_empty() && self.keys.is_empty() && self.files.is_empty()
    }
}

/// Read `assets/pack.json`. Absent or unreadable is *no dependencies*, not an
/// error: a plain pack is the shape every checkout has today, and a
/// bookkeeping file must never stop a render.
pub fn read_pack(assets: &Path) -> Pack {
    std::fs::read_to_string(pack_path(assets))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Read `assets/_extends.json`. A missing or broken record degrades to "nothing
/// was inherited", which is the safe direction: no withdrawal happens and every
/// entry already in the tree reads as the asset's own, so nothing is ever
/// deleted or overwritten on the strength of a file that did not parse.
pub fn read_marker(assets: &Path) -> Inherited {
    std::fs::read_to_string(marker_path(assets))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn write_marker(assets: &Path, marker: &Inherited) -> Result<()> {
    let path = marker_path(assets);
    let text = serde_json::to_string_pretty(marker)?;
    crate::atomic_write(&path, &format!("{text}\n"))?;
    Ok(())
}

/// What a resolve did — or, in a dry run, would do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// Every dependency folded in, in order, with the hash it was folded in at.
    pub deps: Vec<DepRecord>,
    /// Dependencies whose tree has moved since the last resolve. Empty is the
    /// steady state; non-empty is the child being stale, which is a
    /// comparison and never a guess.
    pub stale: Vec<String>,
    /// Entries and files removed because this resolve owned them.
    pub withdrawn: usize,
    /// Keys and files added (or overridden by a later dependency).
    pub added: usize,
    /// Entries an operator had edited, left alone and dropped from the record.
    pub adopted: usize,
    pub dry_run: bool,
}

impl Report {
    /// Whether the tree would change. A dry run that reports this must write.
    pub fn changed(&self) -> bool {
        self.withdrawn > 0 || self.added > 0
    }

    /// One line for the operator, in the order the questions are asked.
    pub fn summary(&self) -> String {
        let mut parts = vec![format!(
            "{} dep(s): {} withdrawn, {} filled in",
            self.deps.len(),
            self.withdrawn,
            self.added
        )];
        if self.adopted > 0 {
            parts.push(format!("{} kept (edited here)", self.adopted));
        }
        if !self.stale.is_empty() {
            parts.push(format!("STALE: {} moved", self.stale.join(", ")));
        }
        let verb = if self.dry_run { "would be" } else { "is" };
        format!("{} — {verb} {}", parts.join("; "), self.verb_nothing())
    }

    fn verb_nothing(&self) -> &'static str {
        if self.changed() {
            "changed"
        } else {
            "up to date"
        }
    }
}

/// Which registry a filename is, if it is one.
fn kind_of(registry: &str) -> Option<PoolKind> {
    PoolKind::ALL.into_iter().find(|k| k.registry() == registry)
}

/// A hash of one registry value, so a resolve can tell an entry it inserted
/// from the same key the operator has since edited.
fn value_hash(sound: &Sound) -> String {
    profile::content_hash(serde_json::to_string(sound).unwrap_or_default().as_bytes())
}

/// A content hash of a whole dependency tree.
fn tree_hash(dir: &Path) -> Result<String> {
    let files = profile::files_under(dir, &[""]);
    Ok(profile::manifest_hash(&profile::hash_files(dir, files)?))
}

/// Fold this asset's dependencies into the live tree.
///
/// Withdraw, then fill, then record — in that order, because a re-resolve has
/// to undo its own last answer before it can give a new one. Everything that
/// is not a pool registry is copied whole; every pool registry is merged key by
/// key through [`save_pool`], which keeps the bytes of every entry it was not
/// asked to change — so a resolve that changes one sound diffs as one sound.
///
/// `dry_run` computes the same answer and writes nothing, including the file
/// deletions and copies, so it can be run on a live tree by anyone asking
/// "what would this do".
pub fn resolve(assets: &Path, dry_run: bool) -> Result<Report> {
    let pack = read_pack(assets);
    let old = read_marker(assets);
    let mut report = Report {
        dry_run,
        ..Report::default()
    };

    // The live registries, read once. Only the ones that exist are kept, so a
    // layer a dependency fills is created by the fill rather than by an empty
    // file appearing out of nowhere.
    let mut pools: BTreeMap<&'static str, ClipPool> = BTreeMap::new();
    for kind in PoolKind::ALL {
        let path = assets.join(kind.registry());
        if path.is_file() {
            pools.insert(kind.registry(), load_pool(&path));
        }
    }
    let mut dirty: BTreeSet<&'static str> = BTreeSet::new();
    // What the tree looked like before this resolve touched it, so a resolve
    // that reaches the same answer can leave the files alone entirely.
    let original = pools.clone();
    let mut adopted_keys: BTreeSet<(String, String)> = BTreeSet::new();
    let mut adopted_files: BTreeSet<String> = BTreeSet::new();

    // 1. Withdraw what the last resolve put here and nobody has edited since.
    for (registry, keys) in &old.keys {
        let Some(kind) = kind_of(registry) else {
            continue;
        };
        let pool = pools.entry(kind.registry()).or_default();
        for (key, hash) in keys {
            match pool.get(key) {
                Some(sound) if value_hash(sound) == *hash => {
                    pool.remove(key);
                    dirty.insert(kind.registry());
                }
                // Edited since it was inherited: the operator has adopted it,
                // so it is theirs now and it leaves the record.
                Some(_) => {
                    adopted_keys.insert((kind.registry().to_string(), key.clone()));
                }
                None => {}
            }
        }
    }
    for (rel, hash) in &old.files {
        let path = assets.join(rel);
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if profile::content_hash(&bytes) == *hash {
            if !dry_run {
                std::fs::remove_file(&path)
                    .with_context(|| format!("withdrawing {}", path.display()))?;
            }
        } else {
            adopted_files.insert(rel.clone());
        }
    }

    // 2. Fold the dependencies in, weakest first. A later dependency overrides
    //    an earlier one on a shared key, but never the asset's own — which is
    //    why "already present" has to be told apart from "filled in by a
    //    dependency", and not simply tested with `contains_key`.
    let mut marker = Inherited::default();
    let mut filled_keys: BTreeMap<&'static str, BTreeSet<String>> = BTreeMap::new();
    let mut filled_files: BTreeSet<String> = BTreeSet::new();
    let previous: BTreeMap<&str, &str> = old
        .deps
        .iter()
        .map(|d| (d.name.as_str(), d.hash.as_str()))
        .collect();

    for dep in &pack.deps {
        let dir = extends_dir(assets).join(dep);
        if !dir.is_dir() {
            bail!(
                "asset '{dep}' is not unpacked: {} is missing — unpack its release under {}/ before resolving",
                dir.display(),
                extends_dir(assets).display(),
            );
        }
        let record = DepRecord {
            name: dep.clone(),
            hash: tree_hash(&dir)?,
        };
        if previous
            .get(dep.as_str())
            .is_some_and(|was| *was != record.hash)
        {
            report.stale.push(dep.clone());
        }
        marker.deps.push(record.clone());
        report.deps.push(record);

        for kind in PoolKind::ALL {
            let parent = load_pool(&dir.join(kind.registry()));
            if parent.is_empty() {
                continue;
            }
            let pool = pools.entry(kind.registry()).or_default();
            let slot = filled_keys.entry(kind.registry()).or_default();
            for (key, sound) in &parent {
                if pool.contains_key(key) && !slot.contains(key) {
                    continue; // the asset's own entry wins over every dependency
                }
                pool.insert(key.clone(), sound.clone());
                slot.insert(key.clone());
                marker
                    .keys
                    .entry(kind.registry().to_string())
                    .or_default()
                    .insert(key.clone(), value_hash(sound));
                dirty.insert(kind.registry());
            }
        }

        for path in profile::files_under(&dir, &[""]) {
            let Ok(rel) = path.strip_prefix(&dir) else {
                continue;
            };
            let rel = rel.display().to_string();
            // The dependency's own manifests are not content: `pack.json` and
            // the marker describe *it*, and inheriting them would make this
            // asset claim a dependency list it never wrote.
            //
            // And a pool registry is **merged, never copied**: it went through
            // the key-by-key pass above, so copying it whole would both bypass
            // that merge and record the same content twice — once as a file and
            // once as its keys.
            if rel == PACK_FILE || rel == MARKER_FILE || kind_of(&rel).is_some() {
                continue;
            }
            let to = assets.join(&rel);
            if to.exists() && !filled_files.contains(&rel) {
                continue; // the asset's own file wins
            }
            let bytes =
                std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            if !dry_run {
                if let Some(parent) = to.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("creating {}", parent.display()))?;
                }
                std::fs::write(&to, &bytes).with_context(|| format!("writing {}", to.display()))?;
            }
            filled_files.insert(rel.clone());
            marker.files.insert(rel, profile::content_hash(&bytes));
        }
    }

    // 3. Count, then record. The counters describe the *tree*, not the
    //    algorithm: withdrawing and refilling is how the merge works, so a
    //    resolve that reaches the same answer must report no change rather than
    //    narrate its own two steps. A key is withdrawn when it is no longer
    //    inherited at all; a key is added when it is inherited *as something
    //    else* — which is why the withdrawal compares names and the addition
    //    compares names and values.
    let names = |m: &Inherited| -> BTreeSet<(String, String)> {
        m.keys
            .iter()
            .flat_map(|(r, ks)| ks.keys().map(move |k| (r.clone(), k.clone())))
            .collect()
    };
    let named_values = |m: &Inherited| -> BTreeSet<(String, String, String)> {
        m.keys
            .iter()
            .flat_map(|(r, ks)| {
                ks.iter()
                    .map(move |(k, h)| (r.clone(), k.clone(), h.clone()))
            })
            .collect()
    };
    let named_files = |m: &Inherited| -> BTreeSet<(String, String)> {
        m.files
            .iter()
            .map(|(r, h)| (r.clone(), h.clone()))
            .collect()
    };
    report.withdrawn = names(&old)
        .difference(&names(&marker))
        .filter(|(r, k)| !adopted_keys.contains(&(r.clone(), k.clone())))
        .count()
        + old
            .files
            .keys()
            .filter(|rel| !marker.files.contains_key(*rel) && !adopted_files.contains(*rel))
            .count();
    report.added = named_values(&marker)
        .difference(&named_values(&old))
        .count()
        + named_files(&marker).difference(&named_files(&old)).count();
    report.adopted = adopted_keys.len() + adopted_files.len();

    if !dry_run {
        for registry in &dirty {
            let kind = kind_of(registry).expect("only a known registry is ever marked dirty");
            // Only a registry that actually differs is rewritten, so a resolve
            // that changes nothing leaves even the mtimes alone.
            if pools[registry] != original.get(registry).cloned().unwrap_or_default() {
                save_pool(&assets.join(registry), kind, &pools[registry])?;
            }
        }
        // A record that would say nothing is not written, and an empty one left
        // over from a dependency that has since been dropped is removed —
        // otherwise a tree with no dependencies would carry a file claiming it
        // inherited things.
        if marker != old {
            if marker.is_empty() {
                let _ = std::fs::remove_file(marker_path(assets));
            } else {
                write_marker(assets, &marker)?;
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // still its own.
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
        // both behind for good; the record is what makes them withdrawable.
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
        // truth. The next resolve must not throw that away.
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
        // is built on something that has moved — a comparison, not a guess.
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
}
