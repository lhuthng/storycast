//! The cluster token: what lets a worker tell its own inductor from anything

use anyhow::{Context, Result};
use std::path::Path;

/// The file, under `.bm/` on the inductor and on a worker alike. Both roots
pub const FILE: &str = "cluster-token";

/// Read the token from a root, or `None` when there is none yet.
pub fn read(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join(".bm").join(FILE)).ok()?;
    let token = text.trim().to_string();
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

/// The cluster's token, generating and storing one on first use.
pub fn load_or_create(root: &Path) -> Result<String> {
    if let Some(existing) = read(root) {
        return Ok(existing);
    }
    let token = generate()?;
    write(root, &token)?;
    Ok(token)
}

/// Store a token at `<root>/.bm/cluster-token`, readable only by its owner.
pub fn write(root: &Path, token: &str) -> Result<()> {
    let path = root.join(".bm").join(FILE);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    crate::atomic_write(&path, &format!("{token}\n"))?;
    crate::util::restrict(&path)?;
    Ok(())
}

/// 32 bytes of `/dev/urandom`, hex.
fn generate() -> Result<String> {
    use std::io::Read;
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .context("opening /dev/urandom")?
        .read_exact(&mut bytes)
        .context("reading /dev/urandom")?;
    let mut out = String::with_capacity(64);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    Ok(out)
}

/// Compare two tokens without leaking *where* they differ.
pub fn matches(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("bm-token-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_token_is_generated_once_and_survives_a_restart() {
        // `serve` calls this on every start. A token that changed per run would
        let dir = root("stable");
        let first = load_or_create(&dir).unwrap();
        assert_eq!(first.len(), 64, "32 bytes, hex");
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(load_or_create(&dir).unwrap(), first, "idempotent");
        assert_eq!(read(&dir).as_deref(), Some(first.as_str()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_clusters_do_not_share_a_token() {
        let a = load_or_create(&root("a")).unwrap();
        let b = load_or_create(&root("b")).unwrap();
        assert_ne!(a, b, "generated, not derived from anything shared");
    }

    #[cfg(unix)]
    #[test]
    fn the_token_is_owner_only() {
        // The whole value of the token is that nobody else can present it. On a
        use std::os::unix::fs::PermissionsExt;
        let dir = root("mode");
        let token = load_or_create(&dir).unwrap();
        let path = dir.join(".bm").join(FILE);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "got {mode:o}");
        // And it still reads back.
        assert_eq!(read(&dir).as_deref(), Some(token.as_str()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_or_empty_token_reads_as_absent() {
        let dir = root("absent");
        assert_eq!(read(&dir), None);
        // An empty file is not a token: treating it as one would let anything
        std::fs::create_dir_all(dir.join(".bm")).unwrap();
        std::fs::write(dir.join(".bm").join(FILE), "  \n").unwrap();
        assert_eq!(read(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn matching_does_not_short_circuit_on_the_first_byte() {
        let a = load_or_create(&root("match")).unwrap();
        assert!(matches(&a, &a));
        // Every single-byte change is rejected, including the first and last.
        for i in 0..a.len() {
            let mut bytes = a.clone().into_bytes();
            bytes[i] = if bytes[i] == b'0' { b'1' } else { b'0' };
            let other = String::from_utf8(bytes).unwrap();
            assert!(!matches(&a, &other), "byte {i} must not match");
        }
        // Length is not secret, and a length mismatch is not a match.
        assert!(!matches(&a, &a[..63]));
        assert!(!matches("", &a));
        assert!(matches("", ""));
    }
}
