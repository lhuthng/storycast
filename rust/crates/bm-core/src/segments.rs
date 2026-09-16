//! Segment inventory: which audio files a box holds.
//!
//! The inductor-owned-segments migration needs one machine-readable answer per
//! box — "what do you have?" — diffed against `expected_wavs`, the single
//! namer the renderer, the completeness check and the merger share. Both the
//! agent (`bm-agent segments --json`) and the inductor's `segments` report use
//! this, so two implementations can never disagree about the shape.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Where segments live, behind one seam. Today the inductor's own
/// `data/audio/` is the only store; the roadmap's S3 backend becomes a second
/// implementation instead of a rewrite of every module that resolves a seg
/// dir today. Same directory, resolved once, never independently in five
/// modules.
pub trait SegmentStore {
    /// Where a chapter's units live.
    fn dir(&self, engine: &str, chapter: u32) -> PathBuf;
    /// Store one file, atomically (tmp + rename — a killed write never leaves
    /// a half-file `expected_wavs` can see).
    fn put(&self, engine: &str, chapter: u32, name: &str, bytes: &[u8]) -> anyhow::Result<()>;
    /// Fetch one file.
    fn get(&self, engine: &str, chapter: u32, name: &str) -> anyhow::Result<Vec<u8>>;
    /// File names present, sorted. A missing directory reads as empty.
    fn names(&self, engine: &str, chapter: u32) -> anyhow::Result<Vec<String>>;
}

/// The local filesystem store, rooted at a `Layout`.
pub struct LocalStore {
    layout: crate::Layout,
}

impl LocalStore {
    pub fn new(layout: crate::Layout) -> Self {
        LocalStore { layout }
    }
}

impl SegmentStore for LocalStore {
    fn dir(&self, engine: &str, chapter: u32) -> PathBuf {
        self.layout.seg_dir(engine, chapter)
    }

    fn put(&self, engine: &str, chapter: u32, name: &str, bytes: &[u8]) -> anyhow::Result<()> {
        let dir = self.dir(engine, chapter);
        std::fs::create_dir_all(&dir)?;
        let dest = dir.join(name);
        let tmp = dest.with_extension("wav.incoming");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &dest)?;
        Ok(())
    }

    fn get(&self, engine: &str, chapter: u32, name: &str) -> anyhow::Result<Vec<u8>> {
        Ok(std::fs::read(self.dir(engine, chapter).join(name))?)
    }

    fn names(&self, engine: &str, chapter: u32) -> anyhow::Result<Vec<String>> {
        let dir = self.dir(engine, chapter);
        let Ok(rd) = std::fs::read_dir(&dir) else {
            return Ok(Vec::new());
        };
        let mut out: Vec<String> = rd
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_file())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        out.sort();
        Ok(out)
    }
}

/// One file in a box's segment store: which chapter, which engine, which name,
/// how big, and what it hashes to.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SegmentEntry {
    pub chapter: u32,
    pub engine: String,
    pub name: String,
    pub bytes: u64,
    pub sha256: String,
}

/// Walk an `audio/` directory, grouping `segments-<engine>-NN` directories.
/// Anything else (previews, strays) is not inventory. Sorts by
/// (chapter, engine, name) so two runs diff cleanly. A missing directory reads
/// as empty, not an error.
pub fn manifest(audio_dir: &Path) -> Vec<SegmentEntry> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(audio_dir) else {
        return out;
    };
    for entry in rd.filter_map(|e| e.ok()) {
        let dir_name = entry.file_name().to_string_lossy().into_owned();
        let Some(rest) = dir_name.strip_prefix("segments-") else {
            continue;
        };
        let Some((engine, num)) = rest.rsplit_once('-') else {
            continue;
        };
        let Ok(chapter) = num.parse::<u32>() else {
            continue;
        };
        if !entry.path().is_dir() {
            continue;
        }
        let Ok(files) = std::fs::read_dir(entry.path()) else {
            continue;
        };
        for f in files.filter_map(|e| e.ok()) {
            let path = f.path();
            if !path.is_file() {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let mut hasher = Sha256::new();
            hasher.update(&bytes);
            out.push(SegmentEntry {
                chapter,
                engine: engine.to_string(),
                name: f.file_name().to_string_lossy().into_owned(),
                bytes: bytes.len() as u64,
                sha256: format!("{:x}", hasher.finalize()),
            });
        }
    }
    out.sort_by(|a, b| (a.chapter, &a.engine, &a.name).cmp(&(b.chapter, &b.engine, &b.name)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("bm-segments-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn local_store_round_trips_atomically() {
        let root = tmpdir("store");
        let store = LocalStore::new(crate::Layout::new(root.clone()));
        assert!(
            store.names("vieneu", 3).unwrap().is_empty(),
            "missing dir reads empty"
        );
        store
            .put("vieneu", 3, "0000_Adam.wav", b"RIFF-data")
            .unwrap();
        // No half-file visible: only the final name lands.
        let names = store.names("vieneu", 3).unwrap();
        assert_eq!(names, vec!["0000_Adam.wav".to_string()]);
        assert_eq!(
            store.get("vieneu", 3, "0000_Adam.wav").unwrap(),
            b"RIFF-data"
        );
        assert!(store.get("vieneu", 3, "nope.wav").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn manifest_lists_chapter_engine_name_bytes_and_hash() {
        let root = tmpdir("basic");
        let dir = root.join("segments-vieneu-7");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("0000_Adam.wav"), b"RIFF-fake").unwrap();
        std::fs::create_dir_all(root.join("not-segments")).unwrap();
        std::fs::write(root.join("not-segments/x.wav"), b"nope").unwrap();

        let got = manifest(&root);
        assert_eq!(
            got.len(),
            1,
            "only segments-<engine>-NN dirs count: {got:?}"
        );
        let e = &got[0];
        assert_eq!(
            (e.chapter, e.engine.as_str(), e.name.as_str()),
            (7, "vieneu", "0000_Adam.wav")
        );
        assert_eq!(e.bytes, 9);
        assert_eq!(e.sha256.len(), 64, "hex sha256: {}", e.sha256);

        assert!(manifest(&root.join("missing")).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
