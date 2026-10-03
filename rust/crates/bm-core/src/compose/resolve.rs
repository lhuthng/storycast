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
pub fn resolve(assets: &Path, dry_run: bool) -> Result<Report> {
    let old = read_marker(assets);
    let mut report = Report {
        dry_run,
        ..Report::default()
    };

    // The live registries, read once. Only the ones that exist are kept, so a
    let mut pools: BTreeMap<&'static str, ClipPool> = BTreeMap::new();
    for kind in PoolKind::ALL {
        let path = assets.join(kind.registry());
        if path.is_file() {
            pools.insert(kind.registry(), load_pool(&path));
        }
    }
    let mut dirty: BTreeSet<&'static str> = BTreeSet::new();
    // What the tree looked like before this resolve touched it, so a resolve
    let original = pools.clone();
    let mut adopted_keys: BTreeSet<(String, String)> = BTreeSet::new();
    let mut adopted_files: BTreeSet<String> = BTreeSet::new();
    // The layered files, opened once: their members merge by name rather than
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
                Some(_) => {
                    adopted_keys.insert((kind.registry().to_string(), key.clone()));
                }
                None => {}
            }
        }
    }
    // An inherited file is *marked*, not deleted, here: whether it comes back is
    let mut withdrawable: BTreeSet<String> = BTreeSet::new();
    for (rel, hash) in &old.files {
        // A file that layers now is the layers' business, whatever a marker
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
    for layer in layers.iter_mut() {
        if let Some(records) = old.keys.get(layer.file) {
            layer.withdraw(records, &old.deps, &mut adopted_keys);
        }
    }

    // 2. Fold the dependencies in, weakest first. A later dependency overrides
    //    why "already present" has to be told apart from "filled in by a
    //    dependency", and not simply tested with `contains_key`.
    let tree = closure(assets)?;
    report.tree = tree.len();
    let mut marker = Inherited {
        tree: tree.clone(),
        // Carried forward, because a fold does not discover release versions
        versions: old.versions.clone(),
        ..Inherited::default()
    };
    let mut filled_keys: BTreeMap<&'static str, BTreeSet<String>> = BTreeMap::new();
    let mut filled_files: BTreeSet<String> = BTreeSet::new();
    // What the last resolve folded, to compare against. The closure when the
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
        if previous
            .get(dep.as_str())
            .is_some_and(|was| *was != record.hash)
        {
            report.stale.push(dep.clone());
        }
        // Only the direct ones are "the dependencies" — what a release of this
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
            if rel == PACK_FILE
                || rel == MARKER_FILE
                || kind_of(&rel).is_some()
                || layered(&rel).is_some()
            {
                continue;
            }
            let to = assets.join(&rel);
            // `to.exists()` alone cannot tell this asset's own file from one a
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
    marker
        .versions
        .retain(|name, _| tree.iter().any(|n| n.name == *name));

    // And now the files a dependency no longer provides: marked withdrawable
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
    for node in tree.iter().rev() {
        let dir = extends_dir(assets).join(&node.name);
        for layer in layers.iter_mut() {
            layer.fill_lists(&dir, &node.name);
        }
    }

    // What the layered files put in is recorded in the same map the pools use,
    for layer in &layers {
        if !layer.records.is_empty() {
            marker
                .keys
                .insert(layer.file.to_string(), layer.records.clone());
        }
    }

    // 3. Count, then record. The counters describe the *tree*, not the
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
            if pools[registry] != original.get(registry).cloned().unwrap_or_default() {
                save_pool(&assets.join(registry), kind, &pools[registry])?;
            }
        }
        for layer in &layers {
            layer.finish(assets)?;
        }
        // A record that would say nothing is not written, and an empty one left
        if marker != old {
            if marker.is_empty() {
                let _ = std::fs::remove_file(marker_path(assets));
            } else {
                write_marker(assets, &marker)?;
            }
        }
    }
    // A layered file is a change the marker cannot always see, so it is counted
    report.rewritten = layers.iter().filter(|l| l.text != l.original).count();
    Ok(report)
}
