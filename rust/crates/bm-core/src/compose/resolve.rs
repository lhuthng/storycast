use super::graph::closure;
use super::graph::read_marker;
use super::graph::write_marker;
use super::graph::DepRecord;
use super::graph::Inherited;
use super::layer::Layer;
use super::patch::kind_of;
use super::patch::layered;
use super::patch::parse_list_record;
use super::patch::value_hash;
use super::patch::Report;
use super::patch::LAYERED;
use super::*;
use crate::audio_pool::{load_pool, save_pool, ClipPool, PoolKind};
use anyhow::{Context, Result};
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
    // The layered files, opened once: their members merge by name rather than
    // whole-file, which is what lets a root asset own the world's rules.
    let mut layers: Vec<Layer> = Vec::new();
    for l in LAYERED {
        layers.push(Layer::open(assets, l.file)?);
    }

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
    // An inherited file is *marked*, not deleted, here: whether it comes back is
    // a question for the fill, and deleting it first is how a resolve came to
    // rewrite every clip it had already put there — 58 MB of churn for a change
    // of nothing. It also made a dry run lie, because a dry run cannot delete,
    // so its fill found every file already present and reported them all as
    // withdrawn.
    let mut withdrawable: BTreeSet<String> = BTreeSet::new();
    for (rel, hash) in &old.files {
        // A file that layers now is the layers' business, whatever a marker
        // written before it did says.
        if layered(rel).is_some() {
            continue;
        }
        let path = assets.join(rel);
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if profile::content_hash(&bytes) == *hash {
            withdrawable.insert(rel.clone());
        } else {
            adopted_files.insert(rel.clone());
        }
    }

    // Then the layered members, whose record is what lets a re-resolve withdraw
    // its own last answer instead of stacking another on top of it.
    for layer in layers.iter_mut() {
        if let Some(records) = old.keys.get(layer.file) {
            layer.withdraw(records, &old.deps, &mut adopted_keys);
        }
    }

    // 2. Fold the dependencies in, weakest first. A later dependency overrides
    //    an earlier one on a shared key, but never the asset's own — which is
    //    why "already present" has to be told apart from "filled in by a
    //    dependency", and not simply tested with `contains_key`.
    // The **closure**, weakest first, each pack once: the graph is what is folded
    // in, not the direct list, so a pack two dependencies share is one tree here
    // and one fold — see [`closure`].
    let tree = closure(assets)?;
    report.tree = tree.len();
    let mut marker = Inherited {
        tree: tree.clone(),
        // Carried forward, because a fold does not discover release versions
        // and must not be the step that forgets them — `asset resolve` runs
        // after every pack edit, and losing the record every time would make
        // the next update re-download the world.
        versions: old.versions.clone(),
        ..Inherited::default()
    };
    let mut filled_keys: BTreeMap<&'static str, BTreeSet<String>> = BTreeMap::new();
    let mut filled_files: BTreeSet<String> = BTreeSet::new();
    // What the last resolve folded, to compare against. The closure when the
    // record has one, the direct list otherwise — a marker written before the
    // tree was recorded still says what the child was built against.
    let mut previous: BTreeMap<&str, &str> = old
        .deps
        .iter()
        .map(|d| (d.name.as_str(), d.hash.as_str()))
        .collect();
    for node in &old.tree {
        previous.insert(node.name.as_str(), node.hash.as_str());
    }

    for node in &tree {
        let dep = &node.name;
        let dir = extends_dir(assets).join(dep);
        let record = DepRecord {
            name: dep.clone(),
            hash: node.hash.clone(),
        };
        // Every pack in the closure is compared, so a parent that moved under a
        // dependency the child never named directly is caught here too.
        if previous
            .get(dep.as_str())
            .is_some_and(|was| *was != record.hash)
        {
            report.stale.push(dep.clone());
        }
        // Only the direct ones are "the dependencies" — what a release of this
        // asset names, and what the flat fold used to mean.
        if node.direct {
            marker.deps.push(record.clone());
            report.deps.push(record);
        }

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

        for layer in layers.iter_mut() {
            layer.fill(&dir);
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
            if rel == PACK_FILE
                || rel == MARKER_FILE
                || kind_of(&rel).is_some()
                || layered(&rel).is_some()
            {
                continue;
            }
            let to = assets.join(&rel);
            // `to.exists()` alone cannot tell this asset's own file from one a
            // previous resolve put here; the record can, and that is what keeps
            // a resolve from rewriting a clip that is already right.
            if to.exists() && !filled_files.contains(&rel) && !withdrawable.contains(&rel) {
                continue; // the asset's own file wins
            }
            let bytes =
                std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            let same = std::fs::read(&to).map(|now| now == bytes).unwrap_or(false);
            if !dry_run && !same {
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

    // A record of a pack the closure no longer reaches is a record of nothing:
    // pruning against the tree that was just folded is what keeps the map and
    // the tree the same set, so a dropped-and-re-added dependency cannot be
    // mistaken for one that never moved.
    marker
        .versions
        .retain(|name, _| tree.iter().any(|n| n.name == *name));

    // And now the files a dependency no longer provides: marked withdrawable
    // before the fill, still unclaimed after it. Deleting them here rather than
    // before the fill is what makes a dry run and a real one agree.
    if !dry_run {
        for rel in &withdrawable {
            if marker.files.contains_key(rel) {
                continue;
            }
            let path = assets.join(rel);
            if path.exists() {
                std::fs::remove_file(&path)
                    .with_context(|| format!("withdrawing {}", path.display()))?;
            }
        }
    }

    // The lists last, strongest pack first, over the whole closure: a diamond's
    // shared parent has one entry here, so its rules are appended once, at its
    // own (weakest) position — and the record names the pack that shipped them
    // rather than whichever dependency happened to contain a copy.
    for node in tree.iter().rev() {
        let dir = extends_dir(assets).join(&node.name);
        for layer in layers.iter_mut() {
            layer.fill_lists(&dir, &node.name);
        }
    }

    // What the layered files put in is recorded in the same map the pools use,
    // keyed by filename — a whole member by its name, a keyed member by
    // `member/name`, an appended list by `member+dep`.
    for layer in &layers {
        if !layer.records.is_empty() {
            marker
                .keys
                .insert(layer.file.to_string(), layer.records.clone());
        }
    }

    // 3. Count, then record. The counters describe the *tree*, not the
    //    algorithm: withdrawing and refilling is how the merge works, so a
    //    resolve that reaches the same answer must report no change rather than
    //    narrate its own two steps. A key is withdrawn when it is no longer
    //    inherited at all; a key is added when it is inherited *as something
    //    else* — which is why the withdrawal compares names and the addition
    //    compares names and values.
    // A list record is left out of both sets and counted by its own number
    // below: its *name* is the member and the dependency, which does not change
    // when the list does, so a name comparison cannot see a root gaining or
    // dropping a rule — and the honest unit for a list is entries.
    let names = |m: &Inherited| -> BTreeSet<(String, String)> {
        m.keys
            .iter()
            .flat_map(|(r, ks)| {
                ks.keys()
                    .filter(|k| !k.contains('+'))
                    .map(move |k| (r.clone(), k.clone()))
            })
            .collect()
    };
    let named_values = |m: &Inherited| -> BTreeSet<(String, String, String)> {
        m.keys
            .iter()
            .flat_map(|(r, ks)| {
                ks.iter()
                    .filter(|(k, _)| !k.contains('+'))
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
    // Then the lists, by entries: a dependency's rule that is gone is a rule
    // withdrawn, one it gained is a rule filled in, and an edit to one of its
    // rules is one filled in and none withdrawn.
    for (file, records) in &old.keys {
        for (key, record) in records.iter().filter(|(k, _)| k.contains('+')) {
            let Some((was, hash)) = parse_list_record(record) else {
                continue;
            };
            match marker
                .keys
                .get(file)
                .and_then(|r| r.get(key))
                .and_then(|r| parse_list_record(r))
            {
                Some((now, now_hash)) => {
                    if now > was {
                        report.added += now - was;
                    }
                    if was > now {
                        report.withdrawn += was - now;
                    }
                    if now == was && now_hash != hash {
                        report.added += 1;
                    }
                }
                None => report.withdrawn += was,
            }
        }
    }
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
        for layer in &layers {
            layer.finish(assets)?;
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
    // A layered file is a change the marker cannot always see, so it is counted
    // from the text: a list that duplicated itself keeps its records, keeps its
    // counts, and still rewrites the file.
    report.rewritten = layers.iter().filter(|l| l.text != l.original).count();
    Ok(report)
}
