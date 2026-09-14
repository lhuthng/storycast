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

    /// The committed voice catalogue: every preset, its metadata, the accent
    /// policy and the default cast, for both engines.
    ///
    /// Tracked, and read at compile time by `voices::CATALOGUE_JSON`, so this
    /// path exists for tooling (`roster init`, a diff against a fresh clone)
    /// rather than for the render path.
    pub fn roster_default(&self) -> PathBuf {
        self.root.join("voices.default.json")
    }

    /// The operator's roster: a machine-local delta over the catalogue —
    /// enabled flags, enrolled clones, policy overrides.
    ///
    /// Inside `.bm/`, which `.gitignore` already covers, so keeping personal
    /// voices out of git needs no `.gitignore` change at all.
    pub fn roster(&self) -> PathBuf {
        self.bm_state().join("voices.json")
    }

    /// Linked machines: the boxes this inductor may provision, by name.
    ///
    /// Same deal as `roster()`: inside `.bm/`, so SSH users, addresses and key
    /// paths stay on the machine and out of git with no extra ignore rules.
    pub fn machines(&self) -> PathBuf {
        self.bm_state().join("machines.json")
    }

    /// Reference clips for enrolled clones — supplied by the operator and the
    /// input to enrolment. Ignored, and the only voice asset that reaches a
    /// worker.
    pub fn voice_refs(&self) -> PathBuf {
        self.bm_state().join("voices/refs")
    }

    /// Audition clips for the picker — generated, disposable, and deliberately
    /// never synced to a worker, which needs only `voice_refs()` to enrol.
    pub fn voice_samples(&self) -> PathBuf {
        self.bm_state().join("voices/samples")
    }

    /// Transient working space for the merge stage.
    ///
    /// Deliberately inside `.bm/` rather than the system temp dir. `publish`
    /// moves the finished mp3 into `output()` with `fs::rename`, which fails
    /// across filesystems (`EXDEV`); scratch must share a device with the
    /// output, and `.bm/` always does because it hangs off the same root.
    pub fn scratch(&self) -> PathBuf {
        self.bm_state().join("tmp")
    }

    /// One chapter's scratch directory. Everything the merge writes — the
    /// concat, the ambience pass, the tempo pass and their intermediates —
    /// lands here and is removed once the chapter is published.
    pub fn scratch_ch(&self, n: u32) -> PathBuf {
        self.scratch().join(format!("ch{n:02}"))
    }

    pub fn ensure(&self) -> Result<()> {
        for d in [
            self.data(),
            self.chapters(),
            self.audio(),
            self.output(),
            self.bm_state(),
            self.scratch(),
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
    fn scratch_shares_the_output_root_so_publish_can_rename() {
        let l = Layout::new("/repo");
        assert!(l.scratch().ends_with(".bm/tmp"));
        assert!(l.scratch_ch(7).ends_with(".bm/tmp/ch07"));
        assert_eq!(l.scratch_ch(7).parent().unwrap(), l.scratch());
        // `publish` renames scratch -> output. Both must hang off the same
        // root or the rename fails with EXDEV.
        assert!(l.scratch().starts_with(&l.root));
        assert!(l.output().starts_with(&l.root));
    }

    #[test]
    fn the_catalogue_is_tracked_but_the_operator_roster_is_not() {
        let l = Layout::new("/repo");
        // The catalogue is repo content: a fresh clone has to render with no
        // local config, so this one is committed at the root.
        assert_eq!(l.roster_default(), Path::new("/repo/voices.default.json"));
        // Everything personal lives under `.bm/`, which `.gitignore` already
        // covers — which is the whole reason the split needs no ignore churn.
        for p in [l.roster(), l.voice_refs(), l.voice_samples()] {
            assert!(p.starts_with(l.bm_state()), "{} escaped .bm/", p.display());
        }
        assert!(l.roster().ends_with(".bm/voices.json"));
        assert!(l.voice_refs().ends_with(".bm/voices/refs"));
        assert!(l.voice_samples().ends_with(".bm/voices/samples"));
        // refs and samples are different things and stay separable, so
        // `roster ls` can tell "no sample rendered yet" from "no ref provided".
        assert_ne!(l.voice_refs(), l.voice_samples());
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
