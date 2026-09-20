//! Profiles: genre bundles (assets + prompts) as versioned transfer files.
//!
//! A profile is `profiles/<name>.tar.zst`: the `assets/` and `prompts/` trees
//! plus a `manifest.json` (`{name, version, files: {path: sha256}}`). The
//! bundle is transfer and archive only — day to day the pipeline reads the
//! unpacked live tree, so no code path reaches through decompression.
//!
//! "Loaded" is a pointer file, `.bm/profile` (`{name, hash}`), where `hash`
//! is the manifest hash recomputed over the live tree. Anything that runs
//! ([`verify`]) recomputes and compares: a hand-edited live tree, or a tree
//! unpacked from a different profile, fails loudly with the fix named,
//! instead of rendering one genre with another's sound design.
//!
//! Packing and unpacking (tar + zstd) live in `tools/profile.sh` — shell,
//! like ssh/rsync/ffmpeg. This module only reads pointers and verifies.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The live trees a profile owns, relative to the repo/workspace root.
pub const LIVE_DIRS: [&str; 2] = ["assets", "prompts"];

/// Manifest stored as `manifest.json` at the bundle root.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub name: String,
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default)]
    pub files: BTreeMap<String, String>,
}

fn default_version() -> String {
    "1".into()
}

/// The load pointer: which profile the live tree claims to be. Empty
/// (`Default`) means unset — a workspace that never named one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pointer {
    pub name: String,
    pub hash: String,
}

pub fn bundle_path(root: &Path, name: &str) -> PathBuf {
    root.join("profiles").join(format!("{name}.tar.zst"))
}

pub fn pointer_path(root: &Path) -> PathBuf {
    root.join(".bm").join("profile")
}

/// sha256 of one file, hex.
fn file_hash(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let mut h = Sha256::new();
    h.update(&bytes);
    Ok(hex_digest(h.finalize()))
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    let mut out = String::with_capacity(bytes.as_ref().len() * 2);
    for b in bytes.as_ref() {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// The manifest hash over the live tree: sha256 of `path + NUL + content-hash`
/// lines in sorted order. Sorting (BTreeMap) keeps it stable across machines.
///
/// Two steps, deliberately: walk, then hash. The walk is a few dozen `readdir`
/// calls and the hashing is 57 MB of sha256 on this repo — measured at 0.60 s
/// release and 2.07 s debug, single-threaded, on **every** `serve` and `worker`
/// start. Splitting them lets the hashing run on every core, which is the one
/// thing that made that number worth attacking.
pub fn hash_live(root: &Path) -> Result<BTreeMap<String, String>> {
    hash_files(root, live_files(root))
}

/// Every file a profile owns, relative to `root`, sorted.
///
/// `.DS_Store` is skipped because OS noise is not content: the pack manifest
/// skips it too, so a Finder visit must never hash-drift the live tree.
fn live_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in LIVE_DIRS {
        let mut stack = vec![root.join(dir)];
        while let Some(d) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&d) else {
                continue;
            };
            let mut paths: Vec<_> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
            paths.sort();
            for p in paths {
                if p.file_name().and_then(|n| n.to_str()) == Some(".DS_Store") {
                    continue;
                }
                if p.is_dir() {
                    stack.push(p);
                } else {
                    out.push(p);
                }
            }
        }
    }
    out.sort();
    out
}

/// Hash `files` on as many threads as the machine has cores, and fold them into
/// the same sorted map the sequential version produced.
///
/// The result must be **byte-identical** to hashing one at a time in sorted
/// order — the pointer on every machine was computed that way, and a different
/// order would be a different hash, i.e. a false "profile drift" on every box.
/// A worker takes the next index from an atomic counter, so the assignment is
/// dynamic and a slow file cannot leave a thread idle; the fold is a BTreeMap,
/// so insertion order does not matter.
///
/// One thread is used when there is one file or one core: spawning a thread to
/// hash a fixture is slower than doing it.
pub(crate) fn hash_files(root: &Path, files: Vec<PathBuf>) -> Result<BTreeMap<String, String>> {
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(files.len());
    if threads <= 1 {
        let mut map = BTreeMap::new();
        for p in files {
            let Ok(rel) = p.strip_prefix(root) else {
                continue;
            };
            map.insert(rel.display().to_string(), file_hash(&p)?);
        }
        return Ok(map);
    }

    let next = std::sync::atomic::AtomicUsize::new(0);
    let parts: Vec<Result<Vec<(String, String)>>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let next = &next;
                let files = &files;
                scope.spawn(move || {
                    let mut out = Vec::new();
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(p) = files.get(i) else { break };
                        let Ok(rel) = p.strip_prefix(root) else {
                            continue;
                        };
                        out.push((rel.display().to_string(), file_hash(p)?));
                    }
                    Ok(out)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join()
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("a hashing worker panicked")))
            })
            .collect()
    });

    let mut map = BTreeMap::new();
    for part in parts {
        for (rel, sum) in part? {
            map.insert(rel, sum);
        }
    }
    Ok(map)
}

pub fn manifest_hash(files: &BTreeMap<String, String>) -> String {
    let mut h = Sha256::new();
    for (path, sum) in files {
        h.update(path.as_bytes());
        h.update([0]);
        h.update(sum.as_bytes());
        h.update([0]);
    }
    hex_digest(h.finalize())
}

pub fn read_pointer(root: &Path) -> Result<Pointer> {
    let path = pointer_path(root);
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

pub fn write_pointer(root: &Path, pointer: &Pointer) -> Result<()> {
    let path = pointer_path(root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::atomic_write(&path, &serde_json::to_string_pretty(pointer)?)?;
    Ok(())
}

/// The load gate: the pointer must exist and the live tree must still hash to
/// what it claims. Anything that runs calls this first; the TUI (which loads
/// and switches profiles) does not.
pub fn verify(root: &Path) -> Result<Pointer> {
    let pointer = read_pointer(root)
        .with_context(|| "no profile loaded (.bm/profile missing) — load one before running")?;
    let live = hash_live(root).context("hashing the live profile tree")?;
    let hash = manifest_hash(&live);
    if hash != pointer.hash {
        anyhow::bail!(
            "live {} + {} do not match profile '{}' (hash drift — hand edit or a different unpack; `profile load {}` to repair)",
            LIVE_DIRS[0],
            LIVE_DIRS[1],
            pointer.name,
            pointer.name,
        );
    }
    Ok(pointer)
}

/// Copy the tracked test fixture (`rust/fixtures/profile/`) over `dest`,
/// creating `assets/` + `prompts/`. The one way tests get a profile: they
/// must never read the live (ignored, maybe absent) tree.
///
/// The path resolves from bm-core's own manifest dir, so callers in other
/// crates land in the same place.
pub fn install_fixture(dest: &Path) -> Result<()> {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/profile");
    for dir in LIVE_DIRS {
        copy_dir(&src.join(dir), &dest.join(dir))?;
    }
    Ok(())
}

fn copy_dir(src: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest).with_context(|| format!("creating {}", dest.display()))?;
    let entries =
        std::fs::read_dir(src).with_context(|| format!("reading fixture {}", src.display()))?;
    for e in entries {
        let e = e?;
        let (from, to) = (e.path(), dest.join(e.file_name()));
        if e.file_type()?.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live_fixture(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bm-profile-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        install_fixture(&dir).unwrap();
        dir
    }

    #[test]
    fn concurrent_hashing_matches_hashing_one_at_a_time() {
        // The whole safety argument for the parallel path: the pointer on every
        // machine was computed sequentially, so a different map is a different
        // hash — a false "profile drift" on every box at once. Cross-check the
        // concurrent implementation against the sequential one, same primitive.
        let dir = live_fixture("parallel");
        let files = live_files(&dir);
        assert!(files.len() > 1, "the fixture must have something to spread");

        let concurrent = hash_files(&dir, files.clone()).unwrap();
        let mut sequential = BTreeMap::new();
        for p in &files {
            sequential.insert(
                p.strip_prefix(&dir).unwrap().display().to_string(),
                file_hash(p).unwrap(),
            );
        }
        assert_eq!(concurrent, sequential, "same files, same map");
        // And the fold over it is what the pointer holds.
        assert_eq!(manifest_hash(&concurrent), manifest_hash(&sequential));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_passes_on_a_fresh_tree_and_fails_on_drift() {
        let dir = live_fixture("verify");
        let hash = manifest_hash(&hash_live(&dir).unwrap());
        write_pointer(
            &dir,
            &Pointer {
                name: "fixture".into(),
                hash,
            },
        )
        .unwrap();
        assert_eq!(verify(&dir).unwrap().name, "fixture");

        // A hand edit drifts the hash: the gate must refuse, naming the fix.
        std::fs::write(dir.join("prompts/analyze.txt"), "tampered").unwrap();
        let err = verify(&dir).unwrap_err();
        assert!(err.to_string().contains("profile load fixture"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_without_a_pointer_names_the_load() {
        let dir = std::env::temp_dir().join("bm-profile-nopointer");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let err = verify(&dir).unwrap_err();
        assert!(err.to_string().contains("no profile loaded"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_fixture_resolves_from_any_crate() {
        // install_fixture anchors on bm-core's manifest dir, not the
        // caller's cwd — a bm-inductor test lands in the same fixture.
        let dir = std::env::temp_dir().join("bm-profile-anchor");
        let _ = std::fs::remove_dir_all(&dir);
        install_fixture(&dir).unwrap();
        assert!(dir.join("assets/scene-map.json").is_file());
        assert!(dir.join("prompts/analyze.txt").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
