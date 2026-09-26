//! Reading `vieneu_v3_heads.npz` — the tied embedding tables and the speaker
//! projection.
//!
//! An `.npz` is a ZIP of `.npy` files, and NumPy writes the `.npy` payloads
//! **stored** (method 0, no compression): the container exists for grouping, not
//! for size. So no inflater is needed, and this reads the central directory
//! directly rather than pulling in a zip crate for one file.
//!
//! Anything that is not a stored `.npy` is refused with the reason, never
//! guessed at. That is safe because this file is a hash-pinned artifact the bake
//! step ships (see `VENDORED.md` for the dictionary, and the model manifest for
//! this one) — a silently misread embedding table would not fail, it would just
//! render the wrong voice.
//!
//! The same reader serves any `.npy` payload we need later, including the
//! single-array case (a bare `.npy` has no ZIP wrapper).

use anyhow::{bail, Context, Result};
use std::collections::HashMap;

/// A parsed array: row-major `f32`, with the shape NumPy declared.
///
/// Everything the engine needs is `f32`; a `f64` table is converted rather than
/// refused, because a model re-exported at double precision would otherwise look
/// like a corrupt file.
#[derive(Debug, Clone)]
pub struct Array {
    pub shape: Vec<usize>,
    pub data: Vec<f32>,
}

impl Array {
    /// Rows of a 2-D array, panicking on a shape mismatch.
    ///
    /// A wrong shape here is a corrupt model directory, not a runtime condition:
    /// there is no sensible fallback, and continuing would index garbage.
    pub fn rows(&self, width: usize) -> &[f32] {
        assert_eq!(
            self.shape.len(),
            2,
            "expected a 2-D array, got shape {:?}",
            self.shape
        );
        assert_eq!(
            self.shape[1], width,
            "expected width {width}, got shape {:?}",
            self.shape
        );
        &self.data
    }

    pub fn scalar(&self) -> f32 {
        assert!(
            self.shape.is_empty() || self.shape.iter().product::<usize>() == 1,
            "expected a scalar, got shape {:?}",
            self.shape
        );
        self.data[0]
    }
}

/// Every member of an `.npz`, keyed by name without the `.npy` suffix.
pub fn read_npz(path: &std::path::Path) -> Result<HashMap<String, Array>> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let mut out = HashMap::new();
    for (name, body) in zip_stored_entries(&bytes).with_context(|| format!("{}", path.display()))? {
        let key = name.strip_suffix(".npy").unwrap_or(&name).to_string();
        out.insert(
            key,
            read_npy(body).with_context(|| format!("{name} in {}", path.display()))?,
        );
    }
    Ok(out)
}

/// `(name, payload)` for every stored entry, read via the central directory.
///
/// The central directory is the authoritative index; local headers can be
/// written with zeroed sizes when a data descriptor follows, so they are used
/// only to find where the payload starts.
fn zip_stored_entries(bytes: &[u8]) -> Result<Vec<(String, &[u8])>> {
    let eocd = find_eocd(bytes).context("no end-of-central-directory record; not a zip")?;
    let count = u16::from_le_bytes([bytes[eocd + 10], bytes[eocd + 11]]) as usize;
    let mut at = u32::from_le_bytes([
        bytes[eocd + 16],
        bytes[eocd + 17],
        bytes[eocd + 18],
        bytes[eocd + 19],
    ]) as usize;

    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        if u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
            != 0x0201_4b50
        {
            bail!("bad central directory entry at {at}");
        }
        let method = u16::from_le_bytes([bytes[at + 10], bytes[at + 11]]);
        let comp_size = u32::from_le_bytes([
            bytes[at + 20],
            bytes[at + 21],
            bytes[at + 22],
            bytes[at + 23],
        ]) as usize;
        let name_len = u16::from_le_bytes([bytes[at + 28], bytes[at + 29]]) as usize;
        let extra_len = u16::from_le_bytes([bytes[at + 30], bytes[at + 31]]) as usize;
        let comment_len = u16::from_le_bytes([bytes[at + 32], bytes[at + 33]]) as usize;
        let local_at = u32::from_le_bytes([
            bytes[at + 42],
            bytes[at + 43],
            bytes[at + 44],
            bytes[at + 45],
        ]) as usize;
        let name = String::from_utf8_lossy(&bytes[at + 46..at + 46 + name_len]).into_owned();

        if method != 0 {
            bail!(
                "{name} is compressed (method {method}); this reader only handles stored \
                 entries, and the model's .npz is stored by construction"
            );
        }
        if u32::from_le_bytes([
            bytes[local_at],
            bytes[local_at + 1],
            bytes[local_at + 2],
            bytes[local_at + 3],
        ]) != 0x0403_4b50
        {
            bail!("bad local header for {name}");
        }
        let l_name = u16::from_le_bytes([bytes[local_at + 26], bytes[local_at + 27]]) as usize;
        let l_extra = u16::from_le_bytes([bytes[local_at + 28], bytes[local_at + 29]]) as usize;
        let start = local_at + 30 + l_name + l_extra;
        let end = start
            .checked_add(comp_size)
            .filter(|e| *e <= bytes.len())
            .with_context(|| format!("{name} runs past the end of the file"))?;
        out.push((name, &bytes[start..end]));

        at += 46 + name_len + extra_len + comment_len;
    }
    Ok(out)
}

/// The EOCD record is at the end, but a zip comment can follow it, so scan back.
fn find_eocd(bytes: &[u8]) -> Option<usize> {
    let start = bytes.len().saturating_sub(22 + 0xffff);
    (start..bytes.len().saturating_sub(21))
        .rev()
        .find(|i| bytes[*i..*i + 4] == [0x50, 0x4b, 0x05, 0x06])
}

/// One `.npy` payload. Only the shapes and dtypes this model ships are accepted,
/// each with a message naming what was found.
pub fn read_npy(bytes: &[u8]) -> Result<Array> {
    if bytes.len() < 10 || &bytes[..6] != b"\x93NUMPY" {
        bail!("not a .npy payload (bad magic)");
    }
    let (_major, header_len_len) = (bytes[6], if bytes[6] == 1 { 2 } else { 4 });
    let hlen = if header_len_len == 2 {
        u16::from_le_bytes([bytes[8], bytes[9]]) as usize
    } else {
        u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize
    };
    let header_at = 8 + header_len_len;
    let header = std::str::from_utf8(&bytes[header_at..header_at + hlen])
        .context("npy header is not utf-8")?;

    let descr = quoted(header, "'descr':").context("npy header has no descr")?;
    let fortran = word(header, "'fortran_order':").unwrap_or("False");
    if fortran.trim() != "False" {
        bail!("fortran_order .npy is not supported (descr {descr})");
    }
    // The shape is a tuple, so it contains commas of its own — it has to be read
    // to its closing paren, not to the next comma, or `(2, 3)` becomes `(2`.
    let shape_s = parenthesised(header, "'shape':").context("npy header has no shape")?;
    let shape: Vec<usize> = shape_s
        .split(',')
        .filter_map(|s| s.trim().parse::<usize>().ok())
        .collect();
    let count: usize = shape.iter().product();

    let body = &bytes[header_at + hlen..];
    let data = match descr.trim() {
        "<f4" => {
            if body.len() < count * 4 {
                bail!("truncated: wanted {} bytes, have {}", count * 4, body.len());
            }
            // `as_chunks` rather than `chunks_exact`: the length above already
            // proved the slice is a whole number of floats, so the remainder is
            // empty, and each chunk arrives as `[u8; 4]` with no bounds check
            // per element. It is why the workspace floor is 1.88.
            let (chunks, []) = body[..count * 4].as_chunks::<4>() else {
                bail!("truncated: wanted {} bytes, have {}", count * 4, body.len());
            };
            chunks.iter().map(|c| f32::from_le_bytes(*c)).collect()
        }
        "<f8" => {
            if body.len() < count * 8 {
                bail!("truncated: wanted {} bytes, have {}", count * 8, body.len());
            }
            let (chunks, []) = body[..count * 8].as_chunks::<8>() else {
                bail!("truncated: wanted {} bytes, have {}", count * 8, body.len());
            };
            chunks.iter().map(|c| f64::from_le_bytes(*c) as f32).collect()
        }
        other => bail!("unsupported npy dtype {other}; expected <f4 or <f8"),
    };
    // A 0-d array reports shape `()`; keep it as an empty shape so `scalar` works.
    Ok(Array {
        shape: if count == 1 && shape.is_empty() {
            Vec::new()
        } else {
            shape
        },
        data,
    })
}

/// The text after a header key, trimmed.
fn header_value<'a>(header: &'a str, key: &str) -> Option<&'a str> {
    let at = header.find(key)? + key.len();
    Some(header[at..].trim())
}

/// A bare header value, up to the comma that ends it.
fn word<'a>(header: &'a str, key: &str) -> Option<&'a str> {
    let rest = header_value(header, key)?;
    Some(rest[..rest.find(',').unwrap_or(rest.len())].trim())
}

/// A single-quoted header value, without its quotes.
fn quoted<'a>(header: &'a str, key: &str) -> Option<&'a str> {
    let rest = header_value(header, key)?;
    let inner = rest.strip_prefix('\'')?;
    Some(&inner[..inner.find('\'')?])
}

/// A parenthesised tuple, without its parens.
fn parenthesised<'a>(header: &'a str, key: &str) -> Option<&'a str> {
    let rest = header_value(header, key)?;
    let inner = rest.strip_prefix('(')?;
    Some(&inner[..inner.find(')')?])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hand-built stored zip, so the reader is tested without the 50 MB model.
    fn zip_of(name: &str, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&[20, 0, 0, 0, 0, 0]); // version, flags, method 0
        out.extend_from_slice(&[0, 0, 0, 0]); // time, date
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes()); // crc (unused)
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes()); // comp size
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes()); // raw size
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra len
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(payload);

        let cd_at = out.len();
        out.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        out.extend_from_slice(&[20, 0, 20, 0, 0, 0, 0, 0]); // made by, version, flags, method
        out.extend_from_slice(&[0, 0, 0, 0]); // time, date
        out.extend_from_slice(&0u32.to_le_bytes()); // crc (unused)
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes()); // compressed size
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes()); // uncompressed size
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra
        out.extend_from_slice(&0u16.to_le_bytes()); // comment
        out.extend_from_slice(&0u16.to_le_bytes()); // disk
        out.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        out.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        out.extend_from_slice(&0u32.to_le_bytes()); // local header offset
        out.extend_from_slice(name.as_bytes());

        let cd_len = out.len() - cd_at;
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // disk
        out.extend_from_slice(&0u16.to_le_bytes()); // cd start disk
        out.extend_from_slice(&1u16.to_le_bytes()); // entries here
        out.extend_from_slice(&1u16.to_le_bytes()); // entries total
        out.extend_from_slice(&(cd_len as u32).to_le_bytes());
        out.extend_from_slice(&(cd_at as u32).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // comment len
        out
    }

    fn npy_f32(shape: &[usize], data: &[f32]) -> Vec<u8> {
        npy(shape, "<f4", data.iter().flat_map(|v| v.to_le_bytes()))
    }

    /// The same header, with a float64 body. The reader narrows to f32, so this
    /// is the only thing covering the 8-byte branch.
    fn npy_f64(shape: &[usize], data: &[f64]) -> Vec<u8> {
        npy(shape, "<f8", data.iter().flat_map(|v| v.to_le_bytes()))
    }

    fn npy(shape: &[usize], descr: &str, body: impl Iterator<Item = u8>) -> Vec<u8> {
        let shape_s = if shape.len() == 1 {
            format!("({},)", shape[0])
        } else {
            format!(
                "({})",
                shape
                    .iter()
                    .map(|d| d.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let header = format!("{{'descr': '{descr}', 'fortran_order': False, 'shape': {shape_s}, }}");
        let padded = (64 - (10 + header.len() + 1) % 64) % 64;
        let mut h = header.into_bytes();
        h.extend(std::iter::repeat_n(b' ', padded));
        h.push(b'\n');

        let mut out = b"\x93NUMPY\x01\x00".to_vec();
        out.extend_from_slice(&(h.len() as u16).to_le_bytes());
        out.extend_from_slice(&h);
        out.extend(body);
        out
    }

    #[test]
    fn reads_a_stored_npz_member() {
        let payload = npy_f32(&[2, 3], &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        let z = zip_of("text_emb.npy", &payload);
        let entries = zip_stored_entries(&z).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "text_emb.npy");

        let a = read_npy(entries[0].1).unwrap();
        assert_eq!(a.shape, vec![2, 3]);
        assert_eq!(a.data, vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
    }

    #[test]
    fn reads_a_zero_dimensional_scalar() {
        let a = read_npy(&npy_f32(&[], &[1e-6])).unwrap();
        assert!(a.shape.is_empty());
        assert_eq!(a.scalar(), 1e-6);
    }

    /// Float64 weights are read and narrowed, not refused. The values are
    /// chosen to be exact in f32 so a byte-order or width mistake cannot hide
    /// behind a tolerance.
    #[test]
    fn a_float64_member_is_narrowed_to_f32() {
        let a = read_npy(&npy_f64(&[3], &[1.0, -2.5, 0.5])).unwrap();
        assert_eq!(a.shape, vec![3]);
        assert_eq!(a.data, vec![1.0, -2.5, 0.5]);
    }

    /// A float64 header with a body that is not a whole number of doubles is
    /// truncated, not read as noise.
    #[test]
    fn a_float64_member_short_of_its_count_is_refused() {
        let mut bytes = npy_f64(&[2], &[1.0, 2.0]);
        bytes.truncate(bytes.len() - 3);
        let err = read_npy(&bytes).unwrap_err().to_string();
        assert!(err.contains("truncated"), "got {err}");
    }

    /// The refusal that matters: a compressed member must not be silently read
    /// as if it were raw, because the bytes would parse and the numbers would be
    /// noise.
    #[test]
    fn a_compressed_member_is_refused_by_name() {
        let payload = npy_f32(&[1], &[1.0]);
        let mut z = zip_of("text_emb.npy", &payload);
        // Flip the method field in both the local and central headers to deflate.
        z[8] = 8;
        let cd = find_eocd(&z).unwrap();
        let at = u32::from_le_bytes([z[cd + 16], z[cd + 17], z[cd + 18], z[cd + 19]]) as usize;
        z[at + 10] = 8;
        let err = zip_stored_entries(&z).unwrap_err().to_string();
        assert!(err.contains("compressed (method 8)"), "{err}");
        assert!(err.contains("text_emb.npy"), "{err}");
    }

    #[test]
    fn a_non_zip_is_refused() {
        let err = zip_stored_entries(b"not a zip at all")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a zip"), "{err}");
    }
}
