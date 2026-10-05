//! A fingerprint of the plugin's own source, so the plan owner and a box can
//! prove they are running the same code.
//!
//! The digest covers the files that decide the binary — everything under `src/`,
//! plus `Cargo.toml` and `Cargo.lock` — as each one's relative path followed by
//! its bytes, in sorted order. Docs and the Makefile are copied to a box but not
//! hashed, so editing the README does not make every box look stale.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// The crate root this binary was built in.
pub fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Every file of the crate, relative to the root and sorted — the set a box
/// copies when it fetches the source.
pub fn files() -> Result<Vec<String>> {
    list(&root())
}

/// The digest of this binary's own source.
pub fn digest() -> Result<String> {
    digest_tree(&root())
}

/// The digest of the source under `root`. The same tree hashes the same on
/// every box, so two ends that agree are provably on the same code.
pub fn digest_tree(root: &Path) -> Result<String> {
    let mut h = Sha256::new();
    for rel in list(root)?.iter().filter(|rel| is_code(rel)) {
        let p = root.join(rel);
        let bytes = std::fs::read(&p).with_context(|| format!("reading {}", p.display()))?;
        h.update(rel.as_bytes());
        h.update([0]);
        h.update((bytes.len() as u64).to_le_bytes());
        h.update(&bytes);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// The first few hex digits, for a log line.
pub fn short(digest: &str) -> &str {
    &digest[..digest.len().min(12)]
}

/// Only what the compiler reads.
fn is_code(rel: &str) -> bool {
    let p = Path::new(rel);
    p.starts_with("src") || p == Path::new("Cargo.toml") || p == Path::new("Cargo.lock")
}

fn list(root: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    walk(root, root, &mut out)?;
    out.sort();
    Ok(out)
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
    for e in std::fs::read_dir(dir).with_context(|| format!("listing {}", dir.display()))? {
        let e = e?;
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if name == "target" || name == ".git" {
            continue;
        }
        if p.is_dir() {
            walk(root, &p, out)?;
        } else if let Ok(rel) = p.strip_prefix(root) {
            out.push(rel.display().to_string());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
