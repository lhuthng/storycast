//! The model artifact: what it is called, where it lives, and how a box takes
//! delivery of it.
//!
//! The weights are the one payload that is identical on every machine and
//! changes only when the operator re-bakes them, so they are also the only
//! payload worth naming. `tools/models.sh` packs the 16 immutable weight files
//! plus `manifest.json` into `models.tar.zst` and cuts a GitHub release tagged
//! `models-v<hash>`; the hash is a function of the *contents* (see
//! [`manifest_hash`]), so a tag can never name bytes it does not hold and two
//! machines with the same bake ask for the same artifact.
//!
//! Everything here is what the publishing half alone cannot do. A box used to
//! receive the weights over rsync, which means every provision paid for them
//! on the operator's uplink — serially, once per box — and which had a failure
//! mode nothing could see: the readiness check only asked whether
//! `models/manifest.json` *existed*, so a box that died mid-transfer passed it.
//! [`fetch`] is the other half: download beside the destination, verify every
//! file against the manifest that travelled in the same archive, and only then
//! swap the directory into place. A half-fetched tree is unrepresentable
//! rather than merely unlikely.
//!
//! The split of failures is deliberate and is what makes a fallback safe:
//! [`FetchError::Unreachable`] means the artifact was not *there* (no such
//! release, no route, a 5xx), and the answer is the rsync that already exists;
//! [`FetchError::Corrupt`] means bytes arrived and are not the ones asked for,
//! and the answer is to stop and say which file disagreed. Retrying harder at
//! wrong bytes is how a corrupt tree becomes a permanent one.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};

/// How much of the hash goes in the tag. A tag is a name a human reads in a
/// URL and a shell command, so it is short; the full hash rides in the release
/// notes and, more usefully, is what the box checks the bytes against.
const TAG_HASH_LEN: usize = 12;

/// The file name both sides agree on. `tools/models.sh` cuts it, and the
/// release asset has to carry the same name the download URL ends in.
pub const BUNDLE_NAME: &str = "models.tar.zst";

/// A GitHub Releases artifact for one bake: the repo that hosts it, the hash
/// it is named by, and the URL a box fetches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelsRelease {
    pub repo: String,
    pub hash: String,
    pub tag: String,
    pub url: String,
}

impl ModelsRelease {
    /// The release this bake *would* be published as, if `repo` names one.
    ///
    /// `models_dir` is the bake itself — `Layout::models_dir()` — rather than
    /// the repo root: the weights moved under the engine's own tree, and the
    /// manifest that names the bake travels with them.
    ///
    /// `None` when no repo is configured — the release path is opt-in, and
    /// "no repo" must keep meaning today's behaviour (the rsync) rather than
    /// an error, because a box that cannot reach a release is still a box that
    /// can be provisioned.
    pub fn resolve(models_dir: &Path, repo: &str) -> Option<Self> {
        let repo = repo.trim();
        if repo.is_empty() {
            return None;
        }
        let hash = manifest_hash(&read_manifest(models_dir).ok()?).ok()?;
        Self::for_repo(repo, &hash).ok()
    }

    /// The same, from parts a caller already has. Validates the repo shape
    /// rather than interpolating whatever it was handed into a URL.
    pub fn for_repo(repo: &str, hash: &str) -> Result<Self> {
        let (owner, name) = parse_repo(repo)?;
        let tag = tag_for(hash);
        Ok(Self {
            repo: format!("{owner}/{name}"),
            hash: hash.to_string(),
            tag: tag.clone(),
            url: release_url(&format!("{owner}/{name}"), &tag),
        })
    }
}

/// `owner/name`, and nothing else.
///
/// A release URL is built by string concatenation, so this is the boundary
/// that keeps a mistyped setting from fetching something that is not a GitHub
/// release asset — and, on a box, from writing outside the destination.
fn parse_repo(repo: &str) -> Result<(&str, &str)> {
    let mut parts = repo.split('/');
    let (Some(owner), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
        bail!("models release repo must be `owner/name`, got `{repo}`");
    };
    let ok = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    if !ok(owner) || !ok(name) {
        bail!("models release repo must be `owner/name`, got `{repo}`");
    }
    Ok((owner, name))
}

/// `models-v<first 12 of the hash>`, the tag `tools/models.sh publish` cuts.
pub fn tag_for(hash: &str) -> String {
    format!("models-v{}", &hash[..TAG_HASH_LEN.min(hash.len())])
}

/// The download URL for a tag's bundle, of the shape
/// `docs/ARTIFACTS.md` documents.
pub fn release_url(repo: &str, tag: &str) -> String {
    format!("https://github.com/{repo}/releases/download/{tag}/{BUNDLE_NAME}")
}

/// sha256 over sorted `name + NUL + content-sha256 + NUL` lines.
///
/// The rule `tools/profile.sh::manifest_hash` already uses, read one level
/// deeper because the models manifest stores `bytes` beside each hash. Two
/// implementations of one rule is a hazard, so this is pinned by a test with a
/// hash computed by the *other* implementation, and the script says in its own
/// comment that the two must agree.
pub fn manifest_hash(doc: &Value) -> Result<String> {
    let files = doc
        .get("files")
        .and_then(|v| v.as_object())
        .ok_or_else(|| anyhow!("models manifest has no `files` object"))?;
    let mut names: Vec<&String> = files.keys().collect();
    names.sort();
    let mut h = Sha256::new();
    for name in names {
        let sha = files[name]
            .get("sha256")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("models manifest entry `{name}` has no sha256"))?;
        h.update(name.as_bytes());
        h.update([0u8]);
        h.update(sha.as_bytes());
        h.update([0u8]);
    }
    Ok(hex(&h.finalize()))
}

fn read_manifest(models_dir: &Path) -> Result<Value> {
    let path = models_dir.join("manifest.json");
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// What a landed tree is, for the log line that says so.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Landing {
    pub files: usize,
    pub bytes: u64,
    pub tag: String,
}

#[derive(Debug)]
pub enum FetchError {
    /// The artifact was not there. Fall back to pushing it.
    Unreachable(String),
    /// Bytes arrived and are not the ones asked for. Stop.
    Corrupt(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Unreachable(m) => write!(f, "release unreachable: {m}"),
            FetchError::Corrupt(m) => write!(f, "artifact does not verify: {m}"),
        }
    }
}

impl std::error::Error for FetchError {}

/// Download the bundle and land it at `dest`, which is the models directory
/// itself (`$HOME/bm-worker/models` on a box).
///
/// `expect_hash` is the hash of the manifest the *inductor* read, not one read
/// back out of what just arrived: a bundle that verifies against its own
/// manifest proves nothing if the manifest is the wrong one. Passing the
/// expectation is what binds the box to what the operator's bake actually is.
pub fn fetch(
    url: &str,
    dest: &Path,
    expect_hash: &str,
    mut on_progress: impl FnMut(u64, Option<u64>),
) -> Result<Landing, FetchError> {
    let parent = dest.parent().unwrap_or(Path::new("."));
    let scratch = scratch_dir(parent, "models-fetch");
    // The download lands here, so the scratch has to exist before it —
    // `dest.parent()` is the worker root, which does, and the scratch does not.
    std::fs::create_dir_all(&scratch)
        .map_err(|e| FetchError::Unreachable(format!("{}: {e}", scratch.display())))?;
    let archive = scratch.join(BUNDLE_NAME);
    let r = (|| -> Result<Landing, FetchError> {
        download(url, &archive, &mut on_progress)?;
        land(&archive, dest, expect_hash)
    })();
    // The download is the big allocation and the failure is the common one, so
    // it goes whether the landing worked or not; the stage directory is
    // `land`'s to clean up, because only `land` knows whether it is mid-swap.
    let _ = std::fs::remove_file(&archive);
    let _ = std::fs::remove_dir(&scratch);
    r
}

/// Download and land, with no expectation to check the result against.
pub fn fetch_unpinned(
    url: &str,
    dest: &Path,
    mut on_progress: impl FnMut(u64, Option<u64>),
) -> Result<Landing, FetchError> {
    let parent = dest.parent().unwrap_or(Path::new("."));
    let scratch = scratch_dir(parent, "models-fetch");
    std::fs::create_dir_all(&scratch)
        .map_err(|e| FetchError::Unreachable(format!("{}: {e}", scratch.display())))?;
    let archive = scratch.join(BUNDLE_NAME);
    let r = (|| -> Result<Landing, FetchError> {
        download(url, &archive, &mut on_progress)?;
        land_unpinned(&archive, dest)
    })();
    let _ = std::fs::remove_file(&archive);
    let _ = std::fs::remove_dir(&scratch);
    r
}

/// Stream `url` to `path`, reporting `(bytes so far, total if known)`.
///
/// The total is the `Content-Length` when the host sends one, and `None` when
/// it does not — reported as `None` rather than guessed, because a progress
/// line that invents its denominator is worse than one that admits it has none.
pub fn download(
    url: &str,
    path: &Path,
    on_progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<u64, FetchError> {
    let client = reqwest::blocking::Client::builder()
        // A 380 MB body off a CDN is minutes on a slow box; the connect is
        // what has to be quick, or an unreachable host hangs the provision.
        .connect_timeout(std::time::Duration::from_secs(20))
        .timeout(std::time::Duration::from_secs(1800))
        .build()
        .map_err(|e| FetchError::Unreachable(e.to_string()))?;
    let mut resp = client
        .get(url)
        .send()
        .map_err(|e| FetchError::Unreachable(format!("{url}: {e}")))?;
    if !resp.status().is_success() {
        return Err(FetchError::Unreachable(format!(
            "{url}: HTTP {}",
            resp.status().as_u16()
        )));
    }
    let total = resp.content_length();
    let mut file = std::fs::File::create(path)
        .map_err(|e| FetchError::Unreachable(format!("{}: {e}", path.display())))?;
    let mut buf = vec![0u8; 1 << 20];
    let mut done: u64 = 0;
    loop {
        let n = resp
            .read(&mut buf)
            .map_err(|e| FetchError::Unreachable(format!("{url}: {e}")))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .map_err(|e| FetchError::Unreachable(format!("{}: {e}", path.display())))?;
        done += n as u64;
        on_progress(done, total);
    }
    file.flush()
        .map_err(|e| FetchError::Unreachable(format!("{}: {e}", path.display())))?;
    if let Some(t) = total {
        if done != t {
            return Err(FetchError::Unreachable(format!(
                "{url}: {done} of {t} bytes, the transfer was cut short"
            )));
        }
    }
    Ok(done)
}

/// Open a bundle, verify it against `expect_hash`, and swap it into `dest`.
///
/// Extract beside the destination, never over it: the directory in place may be
/// a working install that a drifted *voice store* left alone, and a fetch that
/// failed halfway must not be able to damage it.
pub fn land(archive: &Path, dest: &Path, expect_hash: &str) -> Result<Landing, FetchError> {
    land_with(archive, dest, Some(expect_hash))
}

/// The same, for a caller that has no expectation to offer.
///
/// Self-consistency only: the archive verifies against its own manifest, and
/// the tag that comes back is the one *this* bundle claims. An operator reading
/// it learns which bake arrived; nobody is asserting it is the one wanted, and
/// the provisioner — which always has the expectation — is the one that can.
pub fn land_unpinned(archive: &Path, dest: &Path) -> Result<Landing, FetchError> {
    land_with(archive, dest, None)
}

fn land_with(archive: &Path, dest: &Path, expect: Option<&str>) -> Result<Landing, FetchError> {
    let parent = dest.parent().unwrap_or(Path::new("."));
    let stage = scratch_dir(parent, "models-stage");
    // `unpack_in` resolves every member against an existing directory, so the
    // stage has to be there before the first entry rather than created by it.
    std::fs::create_dir_all(&stage)
        .map_err(|e| FetchError::Corrupt(format!("{}: {e}", stage.display())))?;
    let result = (|| -> Result<Landing, FetchError> {
        unpack_to_stage(archive, &stage)?;
        let hash = match expect {
            Some(h) => h.to_string(),
            None => own_hash(&stage).map_err(FetchError::Corrupt)?,
        };
        let files = verify_dir(&stage, &hash).map_err(FetchError::Corrupt)?;
        swap(&stage, dest)
            .map_err(|e| FetchError::Corrupt(format!("{}: {e:#}", dest.display())))?;
        Ok(Landing {
            files,
            bytes: dir_bytes(dest),
            tag: tag_for(&hash),
        })
    })();
    // A stage left behind is 668 MB of the box's disk and nothing else; the
    // successful path has already renamed it away, so this is a no-op there.
    let _ = std::fs::remove_dir_all(&stage);
    result
}

fn unpack_to_stage(archive: &Path, stage: &Path) -> Result<(), FetchError> {
    let file = std::fs::File::open(archive)
        .map_err(|e| FetchError::Unreachable(format!("{}: {e}", archive.display())))?;
    // `StreamingDecoder`, not `FrameDecoder`: the latter only drains what has
    // already been decoded and never drives the loop, so a `Read` straight off
    // it returns end-of-file immediately.
    let decoder = ruzstd::decoding::StreamingDecoder::new(BufReader::new(file))
        .map_err(|e| FetchError::Corrupt(format!("zstd: {e}")))?;
    let mut tar = tar::Archive::new(decoder);
    // Permissions come from the archive, mtimes do not: a box's clock is nobody's
    // problem and a preserved 1970 timestamp only confuses a later rsync into
    // re-sending what it already has.
    tar.set_preserve_mtime(false);
    tar.set_overwrite(true);
    // `unpack_in` is the safe one: it refuses absolute paths and `..`, so a
    // hostile or malformed bundle cannot write outside the stage.
    for entry in tar
        .entries()
        .map_err(|e| FetchError::Corrupt(format!("tar: {e}")))?
    {
        let mut entry = entry.map_err(|e| FetchError::Corrupt(format!("tar: {e}")))?;
        entry
            .unpack_in(stage)
            .map_err(|e| FetchError::Corrupt(format!("tar: {e}")))?;
    }
    Ok(())
}

fn own_hash(dir: &Path) -> std::result::Result<String, String> {
    let doc: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join("manifest.json"))
            .map_err(|e| format!("no manifest.json in the bundle: {e}"))?,
    )
    .map_err(|e| format!("unreadable manifest.json: {e}"))?;
    manifest_hash(&doc).map_err(|e| e.to_string())
}

/// Verify a tree against the manifest inside it, and against `expect_hash`.
///
/// Both directions, because both are failures: a file the manifest lists and
/// the tree lacks is an install that will fail at load time, and a file the
/// tree holds and the manifest does not list is a box being asked to check a
/// set it was never told about.
pub fn verify_dir(dir: &Path, expect_hash: &str) -> std::result::Result<usize, String> {
    let doc: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join("manifest.json"))
            .map_err(|e| format!("no manifest.json in the bundle: {e}"))?,
    )
    .map_err(|e| format!("unreadable manifest.json: {e}"))?;
    let found = manifest_hash(&doc).map_err(|e| e.to_string())?;
    if found != expect_hash {
        return Err(format!(
            "the bundle is a different bake: manifest hash {found}, expected {expect_hash}"
        ));
    }
    let files = doc["files"].as_object().expect("checked above");

    let mut present: Vec<String> = walk(dir)
        .into_iter()
        .map(|p| {
            p.strip_prefix(dir)
                .unwrap_or(&p)
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();
    present.sort();
    let unlisted: Vec<&String> = present
        .iter()
        .filter(|p| p.as_str() != "manifest.json" && !files.contains_key(p.as_str()))
        .collect();
    if !unlisted.is_empty() {
        return Err(format!(
            "{} file(s) in the bundle are absent from its manifest: {}",
            unlisted.len(),
            crate::util::head_chars(
                &unlisted
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                120,
            )
        ));
    }

    for (name, entry) in files {
        let want = entry
            .get("sha256")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("manifest entry `{name}` has no sha256"))?;
        let path = dir.join(name);
        let got = sha256_file(&path).map_err(|e| format!("{name}: {e}"))?;
        if got != want {
            return Err(format!("{name}: sha256 {got} does not match the manifest"));
        }
    }
    Ok(files.len())
}

/// Put `stage` where `dest` is, in two renames.
///
/// Not one atomic swap — there is no such thing for a non-empty directory
/// without a syscall Linux does not offer for this — but the window is two
/// `rename` calls wide, and the old tree is only removed once the new one is
/// in place. A crash between them leaves `dest` absent and the old tree
/// beside it, which the next provision overwrites; it never leaves a *mixed*
/// tree, which is the failure this design exists to remove.
fn swap(stage: &Path, dest: &Path) -> Result<()> {
    let old = dest.with_extension(format!("old-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&old);
    if dest.exists() {
        std::fs::rename(dest, &old)
            .with_context(|| format!("moving the old {} aside", dest.display()))?;
    }
    if let Err(e) = std::fs::rename(stage, dest) {
        // Put the old tree back rather than leaving the box with nothing: a
        // rename that failed is a box problem, not a reason to end worse.
        if old.exists() {
            let _ = std::fs::rename(&old, dest);
        }
        let _ = std::fs::remove_dir_all(&old);
        return Err(e).with_context(|| format!("moving the new tree into {}", dest.display()));
    }
    let _ = std::fs::remove_dir_all(&old);
    Ok(())
}

fn scratch_dir(parent: &Path, what: &str) -> PathBuf {
    parent.join(format!(".{what}.{}", std::process::id()))
}

fn dir_bytes(dir: &Path) -> u64 {
    walk(dir)
        .into_iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum()
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                stack.push(e.path());
            } else {
                out.push(e.path());
            }
        }
    }
    out
}

/// Streamed because the largest weight is 415 MB and hashing it into memory
/// would be a 415 MB allocation on a box that has just fetched one.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut f = BufReader::new(std::fs::File::open(path)?);
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Pinned against the *other* implementation: this is the value
    /// `tools/models.sh::manifest_hash` prints for this exact manifest, so the
    /// two definitions of "the same bundle" are known to agree rather than
    /// assumed to.
    #[test]
    fn the_manifest_hash_matches_the_shell_rule() {
        let doc = json!({"files": {
            "b.npz": {"sha256": "bb", "bytes": 2},
            "a.onnx": {"sha256": "aa", "bytes": 1},
            "c.bin": {"sha256": "cc", "bytes": 3}
        }});
        // sha256 over: "a.onnx\0aa\0" "b.npz\0bb\0" "c.bin\0cc\0", computed by
        // python's hashlib rather than by this code.
        assert_eq!(
            manifest_hash(&doc).unwrap(),
            "be11da56be81cc3ed33566e46257e1c7bbcb8d4072b53cbb483c6fe01ecc1da8"
        );
        // The name is a function of the content and the *order* it is read in,
        // so a reordered manifest hashes the same and a changed file does not.
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
        // erroring: the push is the fallback.
        assert!(ModelsRelease::resolve(Path::new("/nonexistent"), "o/n").is_none());
    }

    /// The tree the box ends up with, and the two ways it can be wrong: a file
    /// that does not hash to what the manifest says, and a file the manifest
    /// never mentioned.
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
    /// against *its own* manifest still has to be refused when it is a
    /// different bake.
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
    /// the way `tools/models.sh` packs one, land it, and have it replace a
    /// directory that was in use.
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
        // way past and must not leave behind afterwards.
        let dest = root.join("models");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("voices.json"), b"{\"live\":true}").unwrap();

        let landed = land(&bundle, &dest, &want).unwrap();
        assert_eq!(landed.files, 2);
        assert_eq!(landed.tag, tag_for(&want));
        assert_eq!(std::fs::read(dest.join("tts.onnx")).unwrap(), b"graph");
        // The bundle is the manifest plus the weights, and the voice store is
        // not a bake output — so it is not in the bundle, and the box's own
        // copy of it is the *inductor's* to push, never the bundle's to carry.
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
    /// was. This is the failure the readiness check used to be blind to.
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
    /// theoretical shape: macOS `tar` writes a `._name` sidecar for every
    /// member carrying an extended attribute, hides them from its own listing,
    /// and the published `models-vdda4efee13df` release has 17 of them — a
    /// Linux box would unpack every one as a real file.
    ///
    /// So "nothing unlisted" is not politeness: it is the check that catches a
    /// packer nobody audited, and it has to name the file it found.
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
    /// an unreachable release, and so not a reason to fall back to the push.
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

    /// The pack half of `tools/models.sh`, through the same two binaries the
    /// script uses — `ruzstd` decodes but does not encode, and a hand-rolled
    /// zstd writer in a test would be a second thing to keep correct.
    ///
    /// `COPYFILE_DISABLE=1` because that is what the script now sets: without
    /// it macOS `tar` writes a `._name` sidecar for every member carrying an
    /// xattr, and `tar -t` hides them, so the junk is invisible from the
    /// machine that packed it. The published `models-vdda4efee13df` release has
    /// 17 of them and this module refuses it for exactly that reason.
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
    /// that leaves a directory behind in a shared `/tmp` is a test that finds
    /// someone else's file.
    fn tstdir(what: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bm-artifact-{what}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }
}
