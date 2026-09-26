//! Raw little-endian f32, which is what the Python side writes with
//! `.tofile()`.
//!
//! One reader for the tools that load one, because the copies disagreed: one
//! refused a file whose length was not a whole number of f32 and the rest
//! dropped the trailing bytes without saying so, which is a wrong answer
//! rather than an error.

use anyhow::{bail, Context, Result};

/// Decode a whole buffer of f32, refusing a partial trailing element.
pub fn from_le_bytes(bytes: &[u8]) -> Result<Vec<f32>> {
    // A non-empty remainder is exactly the "not a whole number of f32" case,
    // so one check does the work of a length test and a chunk walk.
    let (chunks, []) = bytes.as_chunks::<4>() else {
        bail!("{} bytes, not a whole number of f32", bytes.len());
    };
    Ok(chunks.iter().map(|c| f32::from_le_bytes(*c)).collect())
}

/// Read a file of raw f32, naming the path in both failure messages.
///
/// Takes `AsRef<Path>` because the callers hold a mix of `&str`, `String` and
/// `&Path` depending on where the argument came from.
pub fn read(path: impl AsRef<std::path::Path>) -> Result<Vec<f32>> {
    let path = path.as_ref();
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    from_le_bytes(&bytes).with_context(|| format!("decoding {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_little_endian_in_order() {
        let mut bytes = Vec::new();
        for v in [1.0f32, -2.5, 0.5] {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        assert_eq!(from_le_bytes(&bytes).unwrap(), vec![1.0, -2.5, 0.5]);
    }

    /// The case the copies got wrong: a trailing partial element is an error,
    /// not three f32 and a shrug.
    #[test]
    fn a_partial_trailing_element_is_refused() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1.0f32.to_le_bytes());
        bytes.push(0);
        let err = from_le_bytes(&bytes).unwrap_err().to_string();
        assert!(err.contains("whole number of f32"), "got {err}");
    }

    #[test]
    fn an_empty_buffer_is_no_samples() {
        assert!(from_le_bytes(&[]).unwrap().is_empty());
    }
}
