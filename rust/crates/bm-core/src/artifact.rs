//! The model artifact: what it is called, where it lives, and how a box takes

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};

/// How much of the hash goes in the tag. A tag is a name a human reads in a
const TAG_HASH_LEN: usize = 12;

/// The file name both sides agree on. `tools/models.sh` cuts it, and the
pub const BUNDLE_NAME: &str = "models.tar.zst";

/// A GitHub Releases artifact for one bake: the repo that hosts it, the hash
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelsRelease {
    pub repo: String,
    pub hash: String,
    pub tag: String,
    pub url: String,
}

impl ModelsRelease {
    /// The release this bake *would* be published as, if `repo` names one.
    pub fn resolve(models_dir: &Path, repo: &str) -> Option<Self> {
        let repo = repo.trim();
        if repo.is_empty() {
            return None;
        }
        let hash = manifest_hash(&read_manifest(models_dir).ok()?).ok()?;
        Self::for_repo(repo, &hash).ok()
    }

    /// The same, from parts a caller already has. Validates the repo shape
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
pub fn parse_repo(repo: &str) -> Result<(&str, &str)> {
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
pub fn release_url(repo: &str, tag: &str) -> String {
    format!("https://github.com/{repo}/releases/download/{tag}/{BUNDLE_NAME}")
}

// ---------------------------------------------------------------------------

/// The manifest a pack bundle carries at its top level, beside the tree.
pub const PACK_MANIFEST: &str = "manifest.json";

/// The receipt a box keeps of the pack it runs: the verified manifest,
pub const PACK_RECEIPT: &str = "pack-manifest.json";

/// The one directory a pack bundle holds, and the one a worker resolves its
pub const PACK_DIR: &str = "assets";

/// A published profile pack: the repo hosting it, the tag it was cut under, and
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackRelease {
    pub repo: String,
    pub name: String,
    pub version: String,
    pub hash: String,
    pub tag: String,
    pub url: String,
}

impl PackRelease {
    /// The pack release this checkout *is*, if a repo is configured and the
    pub fn resolve(root: &Path, repo: &str) -> Option<Self> {
        let repo = repo.trim();
        if repo.is_empty() {
            return None;
        }
        let p = crate::profile::read_pointer(root).ok()?;
        Self::for_repo(repo, &p.name, &p.version, &p.hash).ok()
    }

    /// The same, from the three parts a caller already has.
    pub fn for_repo(repo: &str, name: &str, version: &str, hash: &str) -> Result<Self> {
        let (owner, repo_name) = parse_repo(repo)?;
        for (what, value) in [("name", name), ("version", version)] {
            if !url_safe(value) {
                bail!(
                    "pack {what} must be non-empty and limited to letters, digits, `-`, `_` and `.`, got `{value}`"
                );
            }
        }
        let tag = pack_tag_for(name, version);
        Ok(Self {
            repo: format!("{owner}/{repo_name}"),
            name: name.to_string(),
            version: version.to_string(),
            hash: hash.to_string(),
            tag: tag.clone(),
            url: pack_release_url(&format!("{owner}/{repo_name}"), &tag, name),
        })
    }
}

fn url_safe(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// `<name>-pack-v<version>` — the tag `tools/profile.sh` cuts, and the one the
pub fn pack_tag_for(name: &str, version: &str) -> String {
    format!("{name}-pack-v{version}")
}

/// Where a pack release's `<name>.tar.zst` lives. The asset keeps the plain
pub fn pack_release_url(repo: &str, tag: &str, name: &str) -> String {
    format!("https://github.com/{repo}/releases/download/{tag}/{name}.tar.zst")
}

/// sha256 over sorted `name + NUL + content-sha256 + NUL` lines.
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
pub fn fetch(
    url: &str,
    dest: &Path,
    expect_hash: &str,
    mut on_progress: impl FnMut(u64, Option<u64>),
) -> Result<Landing, FetchError> {
    let (scratch, archive) = download_beside(url, dest, "models", BUNDLE_NAME, &mut on_progress)?;
    let r = land(&archive, dest, expect_hash);
    // The download is the big allocation and the failure is the common one, so
    let _ = std::fs::remove_dir_all(&scratch);
    r
}

/// Download a bundle into a scratch directory beside `dest`.
fn download_beside(
    url: &str,
    dest: &Path,
    what: &str,
    file_name: &str,
    on_progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<(PathBuf, PathBuf), FetchError> {
    let parent = dest.parent().unwrap_or(Path::new("."));
    let scratch = scratch_dir(parent, &format!("{what}-fetch"));
    // The download lands here, so the scratch has to exist before it —
    std::fs::create_dir_all(&scratch)
        .map_err(|e| FetchError::Unreachable(format!("{}: {e}", scratch.display())))?;
    let archive = scratch.join(file_name);
    match download(url, &archive, on_progress) {
        Ok(_) => Ok((scratch, archive)),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&scratch);
            Err(e)
        }
    }
}

/// Download and land, with no expectation to check the result against.
pub fn fetch_unpinned(
    url: &str,
    dest: &Path,
    mut on_progress: impl FnMut(u64, Option<u64>),
) -> Result<Landing, FetchError> {
    let (scratch, archive) = download_beside(url, dest, "models", BUNDLE_NAME, &mut on_progress)?;
    let r = land_unpinned(&archive, dest);
    let _ = std::fs::remove_dir_all(&scratch);
    r
}

/// Stream `url` to `path`, reporting `(bytes so far, total if known)`.
pub fn download(
    url: &str,
    path: &Path,
    on_progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<u64, FetchError> {
    let client = reqwest::blocking::Client::builder()
        // A 380 MB body off a CDN is minutes on a slow box; the connect is
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
pub fn land(archive: &Path, dest: &Path, expect_hash: &str) -> Result<Landing, FetchError> {
    land_with(archive, dest, Some(expect_hash))
}

/// The same, for a caller that has no expectation to offer.
pub fn land_unpinned(archive: &Path, dest: &Path) -> Result<Landing, FetchError> {
    land_with(archive, dest, None)
}

fn land_with(archive: &Path, dest: &Path, expect: Option<&str>) -> Result<Landing, FetchError> {
    let parent = dest.parent().unwrap_or(Path::new("."));
    let stage = scratch_dir(parent, "models-stage");
    // `unpack_in` resolves every member against an existing directory, so the
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
    let _ = std::fs::remove_dir_all(&stage);
    result
}

// ---------------------------------------------------------------------------

/// Download a published pack and land its `assets/` tree at `dest`, which is
pub fn fetch_pack(
    url: &str,
    dest: &Path,
    expect_hash: &str,
    tag: &str,
    mut on_progress: impl FnMut(u64, Option<u64>),
) -> Result<Landing, FetchError> {
    fetch_pack_with(url, dest, Some(expect_hash), tag, &mut on_progress).map(|(landing, _)| landing)
}

/// [`fetch_pack`] for a caller that has no outside expectation to offer, which
pub fn fetch_pack_unpinned(
    url: &str,
    dest: &Path,
    tag: &str,
    mut on_progress: impl FnMut(u64, Option<u64>),
) -> Result<(Landing, String), FetchError> {
    fetch_pack_with(url, dest, None, tag, &mut on_progress)
}

fn fetch_pack_with(
    url: &str,
    dest: &Path,
    expect: Option<&str>,
    tag: &str,
    on_progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<(Landing, String), FetchError> {
    // The scratch file's name never leaves the box — the URL already names the
    let (scratch, archive) = download_beside(url, dest, "pack", "pack.tar.zst", on_progress)?;
    let r = land_pack_with(&archive, dest, expect, tag);
    let _ = std::fs::remove_dir_all(&scratch);
    r
}

/// A pack diff: worker-relative paths to send, and paths to delete.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PackDelta {
    /// In the new manifest with a different (or no) entry in the old one.
    pub changed: Vec<String>,
    /// In the old manifest and gone from the new one.
    pub removed: Vec<String>,
}

/// Diff two pack manifests by path, as the receipt makes possible.
pub fn diff_manifests(
    old: &std::collections::BTreeMap<String, String>,
    new: &std::collections::BTreeMap<String, String>,
) -> PackDelta {
    let mut changed: Vec<String> = new
        .iter()
        .filter(|(path, sum)| old.get(*path) != Some(*sum))
        .map(|(path, _)| path.clone())
        .collect();
    changed.sort();
    let mut removed: Vec<String> = old
        .keys()
        .filter(|path| !new.contains_key(*path))
        .cloned()
        .collect();
    removed.sort();
    PackDelta { changed, removed }
}

/// Render a pack manifest as receipt text: pretty JSON and a trailing
pub fn receipt_text(manifest: &crate::profile::Manifest) -> Result<String> {
    let mut text = serde_json::to_string_pretty(manifest)?;
    text.push('\n');
    Ok(text)
}

/// Write a pack manifest as a box receipt: the record the next provision
pub fn write_receipt(path: &Path, manifest: &crate::profile::Manifest) -> Result<()> {
    crate::atomic_write(path, &receipt_text(manifest)?)
}

/// Read a receipt back. `None` for absent or unparseable — both mean the box
pub fn read_receipt(path: &Path) -> Option<crate::profile::Manifest> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Open a pack bundle, check it against `expect_hash`, and swap its `assets/`
pub fn land_pack(
    archive: &Path,
    dest: &Path,
    expect_hash: &str,
    tag: &str,
) -> Result<Landing, FetchError> {
    land_pack_with(archive, dest, Some(expect_hash), tag).map(|(landing, _)| landing)
}

/// [`land_pack`] with no expectation: the bundle's own manifest is the check.
pub fn land_pack_unpinned(
    archive: &Path,
    dest: &Path,
    tag: &str,
) -> Result<(Landing, String), FetchError> {
    land_pack_with(archive, dest, None, tag)
}

fn land_pack_with(
    archive: &Path,
    dest: &Path,
    expect: Option<&str>,
    tag: &str,
) -> Result<(Landing, String), FetchError> {
    let parent = dest.parent().unwrap_or(Path::new("."));
    let stage = scratch_dir(parent, "pack-stage");
    std::fs::create_dir_all(&stage)
        .map_err(|e| FetchError::Corrupt(format!("{}: {e}", stage.display())))?;
    let result = (|| -> Result<(Landing, String), FetchError> {
        unpack_to_stage(archive, &stage)?;
        let (files, hash) = verify_pack(&stage, expect).map_err(FetchError::Corrupt)?;
        // The landed tree is the subtree, and it is swapped rather than merged
        let tree = stage.join(PACK_DIR);
        if !tree.is_dir() {
            return Err(FetchError::Corrupt(format!(
                "the bundle carries no {PACK_DIR}/ to land"
            )));
        }
        swap(&tree, dest).map_err(|e| FetchError::Corrupt(format!("{}: {e:#}", dest.display())))?;
        // The receipt is the manifest this land verified, so a later diff
        let receipt = dest.parent().unwrap_or(Path::new(".")).join(PACK_RECEIPT);
        match crate::profile::read_manifest_at(&stage.join(PACK_MANIFEST)) {
            Ok(m) => {
                if let Err(e) = write_receipt(&receipt, &m) {
                    eprintln!(
                        "warning: pack landed but the receipt was not written ({}: {e:#})",
                        receipt.display()
                    );
                }
            }
            Err(e) => eprintln!(
                "warning: pack landed but its manifest could not be re-read for the receipt ({e:#})"
            ),
        }
        Ok((
            Landing {
                files,
                bytes: dir_bytes(dest),
                tag: tag.to_string(),
            },
            hash,
        ))
    })();
    let _ = std::fs::remove_dir_all(&stage);
    result
}

/// Check a pack bundle against its own manifest **and** against the hash the
fn verify_pack(stage: &Path, expect: Option<&str>) -> std::result::Result<(usize, String), String> {
    let m = crate::profile::read_manifest_at(&stage.join(PACK_MANIFEST))
        .map_err(|e| format!("{}: {e:#}", stage.join(PACK_MANIFEST).display()))?;
    if m.piece != "pack" {
        return Err(format!(
            "this is a `{}` release, not a pack — a box cannot run a language as its profile",
            if m.piece.is_empty() {
                "piece"
            } else {
                &m.piece
            }
        ));
    }
    let found = crate::profile::manifest_hash(&m.files);
    if let Some(expect) = expect {
        if found != expect {
            return Err(format!(
                "the bundle is a different pack: manifest hash {found}, expected {expect}"
            ));
        }
    }
    // The whole stage, not the subtree: a bundle carrying a `prompts/` or a
    // described, and the check that exists is "the tree is exactly what was
    // published". Keys are `assets/…`, which is why the pack is checked this
    let mut present: Vec<String> = walk(stage)
        .into_iter()
        .map(|p| {
            p.strip_prefix(stage)
                .unwrap_or(&p)
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();
    present.sort();
    let unlisted: Vec<&String> = present
        .iter()
        .filter(|p| p.as_str() != PACK_MANIFEST && !m.files.contains_key(p.as_str()))
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
    for (name, want) in &m.files {
        if !name.starts_with(&format!("{PACK_DIR}/")) {
            return Err(format!(
                "manifest key `{name}` is not under {PACK_DIR}/ — a pack is unpacked there, and this would land it elsewhere"
            ));
        }
        let got = sha256_file(&stage.join(name)).map_err(|e| format!("{name}: {e}"))?;
        if got != *want {
            return Err(format!("{name}: sha256 {got} does not match the manifest"));
        }
    }
    Ok((m.files.len(), found))
}

fn unpack_to_stage(archive: &Path, stage: &Path) -> Result<(), FetchError> {
    let file = std::fs::File::open(archive)
        .map_err(|e| FetchError::Unreachable(format!("{}: {e}", archive.display())))?;
    // `StreamingDecoder`, not `FrameDecoder`: the latter only drains what has
    let decoder = ruzstd::decoding::StreamingDecoder::new(BufReader::new(file))
        .map_err(|e| FetchError::Corrupt(format!("zstd: {e}")))?;
    let mut tar = tar::Archive::new(decoder);
    // Permissions come from the archive, mtimes do not: a box's clock is nobody's
    tar.set_preserve_mtime(false);
    tar.set_overwrite(true);
    // `unpack_in` is the safe one: it refuses absolute paths and `..`, so a
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
pub(crate) fn swap(stage: &Path, dest: &Path) -> Result<()> {
    let old = dest.with_extension(format!("old-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&old);
    if dest.exists() {
        std::fs::rename(dest, &old)
            .with_context(|| format!("moving the old {} aside", dest.display()))?;
    }
    if let Err(e) = std::fs::rename(stage, dest) {
        // Put the old tree back rather than leaving the box with nothing: a
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
mod tests;
