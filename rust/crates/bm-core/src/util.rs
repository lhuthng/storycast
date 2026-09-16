//! Small filesystem helpers shared by every stage.

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::path::Path;

/// Crash-safe write: write a sibling temp file, then rename over the target.
///
/// Ported from `synthesize.atomic_write`. Matters more now than it did on a
/// single box: other workers poll these same files, so a partially written
/// script or bible would be read as valid JSON-prefix garbage.
pub fn atomic_write(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "out".to_string());
    let tmp = path.with_file_name(format!(".{name}.tmp"));
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

/// Pretty-print JSON without escaping non-ASCII, matching Python's
/// `json.dumps(..., ensure_ascii=False, indent=1)`.
pub fn to_pretty_json<T: Serialize>(value: &T) -> Result<String> {
    let mut s = serde_json::to_string_pretty(value)?;
    s.push('\n');
    Ok(s)
}

pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    atomic_write(path, &to_pretty_json(value)?)
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let v = serde_json::from_str(&text)
        .with_context(|| format!("parsing {}", path.display()))?;
    Ok(v)
}

/// Character count, not byte count — the Python code sized everything in
/// characters and the group/segment caps depend on it.
pub fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// Collapse every whitespace run to a single space and trim the ends.
pub fn squeeze_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// First `n` characters, for log lines. Slicing a `&str` by bytes would panic
/// on Vietnamese text.
pub fn head_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Expand a leading `~` to `$HOME`. Prompts and `-i` flags are not a shell,
/// so neither expands by itself — and ssh handed a literal `~` path fails
/// with "not accessible" while rsync (which shells out) expands it, giving
/// one value two behaviours. `~user` is left alone: only the current user.
pub fn expand_tilde(s: &str) -> std::path::PathBuf {
    if s == "~" || s.starts_with("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return std::path::PathBuf::from(home + &s[1..]);
        }
    }
    std::path::PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_in_place() {
        let dir = std::env::temp_dir().join("bm-util-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.json");
        atomic_write(&p, "one").unwrap();
        atomic_write(&p, "two").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "two");
        // no temp file left behind
        assert!(!dir.join(".a.json.tmp").exists());
    }

    #[test]
    fn json_roundtrip_keeps_unicode_readable() {
        let dir = std::env::temp_dir().join("bm-util-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("u.json");
        let v = serde_json::json!({"name": "Thục Đoan"});
        write_json(&p, &v).unwrap();
        let raw = std::fs::read_to_string(&p).unwrap();
        assert!(raw.contains("Thục Đoan"), "expected raw UTF-8, got {raw}");
        let back: serde_json::Value = read_json(&p).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn char_helpers_are_not_byte_sliced() {
        let s = "Chương một";
        assert_eq!(char_len(s), 10);
        assert_eq!(head_chars(s, 6), "Chương");
        assert_eq!(squeeze_ws("  a\n\t b  "), "a b");
    }

    #[test]
    fn expand_tilde_grows_only_a_leading_bare_tilde() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand_tilde("~"), std::path::PathBuf::from(&home));
        assert_eq!(expand_tilde("~/x"), std::path::PathBuf::from(format!("{home}/x")));
        // `~user`, absolute, relative and empty pass through untouched.
        assert_eq!(expand_tilde("~other/x"), std::path::PathBuf::from("~other/x"));
        assert_eq!(expand_tilde("/abs/x"), std::path::PathBuf::from("/abs/x"));
        assert_eq!(expand_tilde("rel/x"), std::path::PathBuf::from("rel/x"));
        assert_eq!(expand_tilde(""), std::path::PathBuf::from(""));
    }
}
