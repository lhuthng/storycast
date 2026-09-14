//! Where everything lives. Every path the pipeline touches is derived here, so
//! a worker that was told `--root /srv/bm` resolves exactly the same tree the
//! inductor resolved locally.

use crate::util::squeeze_ws;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Layout {
    pub root: PathBuf,
}

impl Layout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Layout { root: root.into() }
    }

    /// Walk up from the current directory looking for the repo marker
    /// (`prompts/analyze.txt`). Lets a worker started from anywhere find home.
    pub fn discover() -> Result<Self> {
        let cwd = std::env::current_dir().context("reading current dir")?;
        let mut cur: Option<&Path> = Some(cwd.as_path());
        while let Some(dir) = cur {
            if dir.join("prompts/analyze.txt").is_file() {
                return Ok(Layout::new(dir));
            }
            cur = dir.parent();
        }
        anyhow::bail!(
            "no repo root found above {} (expected prompts/analyze.txt)",
            cwd.display()
        )
    }

    pub fn data(&self) -> PathBuf {
        self.root.join("data")
    }

    pub fn chapters(&self) -> PathBuf {
        self.data().join("chapters")
    }

    pub fn chapter_txt(&self, n: u32) -> PathBuf {
        self.chapters().join(format!("ch{n:02}.txt"))
    }

    pub fn script(&self, n: u32) -> PathBuf {
        self.data().join(format!("script-{n:02}.json"))
    }

    pub fn bible(&self) -> PathBuf {
        self.data().join("bible.json")
    }

    /// Cast file is per-engine so swapping voices never poisons another
    /// engine's segment cache.
    pub fn cast(&self, engine: &str) -> PathBuf {
        if engine == "vieneu" {
            self.data().join("cast-vieneu.json")
        } else {
            self.data().join("cast.json")
        }
    }

    /// Per-chapter segment cache, also per-engine.
    pub fn seg_dir(&self, engine: &str, n: u32) -> PathBuf {
        if engine == "vieneu" {
            self.data().join(format!("audio/segments-vieneu-{n:02}"))
        } else {
            self.data().join(format!("audio/segments-gemini-v2-{n:02}"))
        }
    }

    pub fn audio(&self) -> PathBuf {
        self.data().join("audio")
    }

    pub fn output(&self) -> PathBuf {
        self.root.join("output")
    }

    pub fn prompt(&self) -> PathBuf {
        self.root.join("prompts/analyze.txt")
    }

    pub fn assets(&self) -> PathBuf {
        self.root.join("assets")
    }

    pub fn scene_map(&self) -> PathBuf {
        self.assets().join("scene-map.json")
    }

    pub fn beds(&self) -> PathBuf {
        self.assets().join("ambience")
    }

    pub fn refs(&self) -> PathBuf {
        self.root.join("refs")
    }

    pub fn python_dir(&self) -> PathBuf {
        self.root.join("python")
    }

    /// Inductor-private state (cluster registry, settings, stats).
    pub fn bm_state(&self) -> PathBuf {
        self.root.join(".bm")
    }

    pub fn stats(&self) -> PathBuf {
        self.bm_state().join("stats.jsonl")
    }

    pub fn settings(&self) -> PathBuf {
        self.bm_state().join("settings.json")
    }

    pub fn ensure(&self) -> Result<()> {
        for d in [
            self.data(),
            self.chapters(),
            self.audio(),
            self.output(),
            self.bm_state(),
        ] {
            std::fs::create_dir_all(&d).with_context(|| format!("creating {}", d.display()))?;
        }
        Ok(())
    }

    /// Chapter title for the output filename, ported from `main._chapter_title`.
    ///
    /// The first line of the chapter text is normally `Chương 12: Some Title`;
    /// we keep the part after the colon and scrub it into something every
    /// filesystem accepts.
    pub fn chapter_title(&self, n: u32) -> String {
        let raw = std::fs::read_to_string(self.chapter_txt(n)).unwrap_or_default();
        let first = raw.lines().next().unwrap_or("").trim().to_string();
        let title = match first.split_once(':') {
            Some((_, rest)) => rest.to_string(),
            None => first,
        };
        let mut title = squeeze_ws(&title);
        // trailing dot-runs: ". . ." / "..." / "…"
        title = title
            .trim_end_matches([' ', '.', '…'])
            .to_string();
        // windows-illegal filename characters
        title = title
            .chars()
            .filter(|c| !matches!(c, '?' | ':' | '"' | '*' | '<' | '>' | '|'))
            .collect();
        let title = squeeze_ws(&title);
        if title.is_empty() {
            format!("Chapter {n}")
        } else {
            title
        }
    }

    /// Final per-chapter deliverable: `output/Ch.N - Title.mp3`.
    pub fn final_mp3(&self, n: u32) -> PathBuf {
        self.output()
            .join(format!("Ch.{n} - {}.mp3", self.chapter_title(n)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bm-layout-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("prompts")).unwrap();
        std::fs::write(dir.join("prompts/analyze.txt"), "x").unwrap();
        dir
    }

    #[test]
    fn paths_are_engine_scoped() {
        let l = Layout::new("/repo");
        assert!(l.cast("vieneu").ends_with("cast-vieneu.json"));
        assert!(l.cast("gemini").ends_with("cast.json"));
        assert!(l
            .seg_dir("vieneu", 7)
            .ends_with("data/audio/segments-vieneu-07"));
        assert!(l
            .seg_dir("gemini", 7)
            .ends_with("data/audio/segments-gemini-v2-07"));
    }

    #[test]
    fn chapter_title_prefers_subtitle_and_scrubs_illegal_chars() {
        let root = fixture_root("title");
        let l = Layout::new(&root);
        std::fs::create_dir_all(l.chapters()).unwrap();
        std::fs::write(
            l.chapter_txt(3),
            "Chương 3: Kiếm khí xung thiên...\n\nbody\n",
        )
        .unwrap();
        assert_eq!(l.chapter_title(3), "Kiếm khí xung thiên");
    }

    #[test]
    fn chapter_title_falls_back_when_no_text() {
        let root = fixture_root("missing");
        let l = Layout::new(&root);
        assert_eq!(l.chapter_title(9), "Chapter 9");
    }

    #[test]
    fn discover_finds_repo_root_from_a_subdir() {
        let root = fixture_root("discover");
        let sub = root.join("data/audio");
        std::fs::create_dir_all(&sub).unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&sub).unwrap();
        let found = Layout::discover().unwrap();
        std::env::set_current_dir(prev).unwrap();
        assert_eq!(
            found.root.canonicalize().unwrap(),
            root.canonicalize().unwrap()
        );
    }
}
