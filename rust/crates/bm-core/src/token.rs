//! The cluster token: what lets a worker tell its own inductor from anything
//! else that can open a socket to it.
//!
//! Until now nothing was authenticated anywhere, and that was survivable: the
//! inductor asked workers *questions* and the workers answered. The inverted
//! protocol changes that — a worker starts accepting **instructions**, and an
//! instruction channel with no authentication is a remote-code-ish surface on
//! every box in the cluster. A task offer names a chapter and a stage; a
//! stranger who can reach the port can make the box render whatever they like,
//! or hold it busy forever.
//!
//! One shared secret per cluster, and its whole lifecycle is three lines:
//! the inductor generates it on first use, provisioning copies it to the
//! worker's root, and the worker requires it on every request that does
//! something. It never appears on a command line — the worker reads it from its
//! own root, because a secret in `argv` is a secret in `ps` and in the shell
//! history of every box it was typed on.
//!
//! It is deliberately *not* a user/role system. There is one cluster, one
//! operator, and the token answers exactly one question: is this request from
//! the inductor that launched me?

use anyhow::{Context, Result};
use std::path::Path;

/// The file, under `.bm/` on the inductor and on a worker alike. Both roots
/// ignore `.bm/`, so the secret is out of git by construction.
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
///
/// Idempotent on purpose: `serve` calls it every start, and a token that
/// changed per run would lock out every worker that outlived a restart — which
/// is the normal case for a cluster that drains and resumes.
pub fn load_or_create(root: &Path) -> Result<String> {
    if let Some(existing) = read(root) {
        return Ok(existing);
    }
    let token = generate()?;
    write(root, &token)?;
    Ok(token)
}

/// Store a token at `<root>/.bm/cluster-token`, readable only by its owner.
///
/// 0600 rather than the default: the whole value of the token is that nobody
/// else can present it, and a world-readable secret on a shared box is not one.
pub fn write(root: &Path, token: &str) -> Result<()> {
    let path = root.join(".bm").join(FILE);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    crate::atomic_write(&path, &format!("{token}\n"))?;
    restrict(&path)?;
    Ok(())
}

/// Owner-only permissions, where the platform has them.
///
/// Unix-only by construction: the inductor and the workers are macOS and Linux,
/// and there is no cross-build target that is not. Elsewhere this is a no-op
/// rather than an error, so a port to another platform is a build, not a
/// rewrite — but it should be revisited, because a token with default
/// permissions is the failure this function exists to prevent.
fn restrict(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restricting {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// 32 bytes of `/dev/urandom`, hex.
///
/// `/dev/urandom` rather than a `rand` dependency: this is one call, the tool is
/// Unix-only, and the alternative is a crate tree for 32 bytes. It is the
/// kernel's CSPRNG, which is the same source `rand` would seed from.
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
///
/// `==` on `String` short-circuits at the first differing byte, which turns
/// "guess the token" into 64 independent single-byte guesses against a service
/// that answers fast. The accumulator below always reads both sides in full, so
/// the time taken depends on the length alone.
///
/// A length difference is reported immediately — that is not a secret, since
/// every token this generates is the same length.
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
        // lock out every worker that outlived the restart, which is the normal
        // case for a cluster that drains and resumes.
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
        // shared box a 0644 secret is not a secret, and the default file mode
        // is exactly that — which is why this is written and then restricted
        // rather than left to `fs::write`.
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
        // that can reach the port in with an empty header.
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
