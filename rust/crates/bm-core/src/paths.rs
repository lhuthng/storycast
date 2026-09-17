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

    /// The digest's first pass: read the chapter, report the cast and the story.
    ///
    /// Also the repo-root marker (`find_root` looks for this exact file), so it
    /// keeps its name even though it is now one of two prompts.
    pub fn prompt(&self) -> PathBuf {
        self.root.join("prompts/analyze.txt")
    }

    /// The digest's second pass: the cast is already resolved, so this one only
    /// has to split the chapter and tag it.
    pub fn script_prompt(&self) -> PathBuf {
        self.root.join("prompts/script.txt")
    }

    pub fn assets(&self) -> PathBuf {
        self.root.join("assets")
    }

    pub fn scene_map(&self) -> PathBuf {
        self.assets().join("scene-map.json")
    }

    /// The effect layer's clip pool. The clips themselves live in
    /// `assets/effects/`, alongside this registry, so provisioning ships a pool
    /// and its clips as one directory.
    pub fn effect_pool(&self) -> PathBuf {
        self.assets().join("effect-pool.json")
    }

    /// The music layer's clip pool, with its clips in `assets/music/`.
    pub fn music_pool(&self) -> PathBuf {
        self.assets().join("music-pool.json")
    }

    pub fn effects(&self) -> PathBuf {
        self.assets().join("effects")
    }

    pub fn music(&self) -> PathBuf {
        self.assets().join("music")
    }

    pub fn refs(&self) -> PathBuf {
        self.root.join("refs")
    }

    pub fn python_dir(&self) -> PathBuf {
        self.root.join("python")
    }

    /// Interpreter for local voice work (enroll now, preview offline): the
    /// provision-managed `python/.venv` first, a repo-root `.venv` second.
    /// One order everywhere — enrollment and serving can never aim at two
    /// different voice stores, which is exactly how a fresh voice 500s.
    pub fn venv_python(&self) -> Option<PathBuf> {
        [self.python_dir().join(".venv/bin/python"), self.root.join(".venv/bin/python")]
            .into_iter()
            .find(|p| p.is_file())
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

    /// Linked machines: the per-machine connection config (addr/user/port/key),
    /// keyed by address. The inductor's join of this file with the ledger's
    /// `machine_state` is the `Machine` the API serves.
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
    /// **The script's own `title` wins.** The crawled headline is the site's
    /// auto-excerpt of the chapter — `Chương 9: Tê! Thật là khủng khiếp dao
    /// phay`, `Chương 10: Tiền bối đối với dao phay yêu cầu đều cao như vậy?` —
    /// a sentence out of the prose with the punctuation still on it, which then
    /// lands on the cover of the mp3. It is a title only in the sense that the
    /// site put it on the first line. The digest has read the chapter and can
    /// name it, so its `title` is the one used; the headline stays as the
    /// fallback for a chapter that was digested before the field existed.
    ///
    /// Both routes end in the same scrub, so a title from either source is a
    /// legal filename and the spoken headline and the file agree.
    pub fn chapter_title(&self, n: u32) -> String {
        let from_script = crate::read_json::<serde_json::Value>(&self.script(n))
            .ok()
            .and_then(|d| {
                d.get("title")
                    .and_then(|t| t.as_str())
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .map(String::from)
            });
        let raw = from_script.unwrap_or_else(|| {
            let raw = std::fs::read_to_string(self.chapter_txt(n)).unwrap_or_default();
            let first = raw.lines().next().unwrap_or("").trim().to_string();
            match first.split_once(':') {
                Some((_, rest)) => rest.to_string(),
                None => first,
            }
        });
        let mut title = squeeze_ws(&raw);
        // trailing dot-runs: ". . ." / "..." / "…"
        title = title.trim_end_matches([' ', '.', '…']).to_string();
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

    /// The crawled headline is a word-for-word machine translation of the
    /// Chinese title, and it is what the mp3 is named after. The digest reads
    /// the chapter and names it, so its `title` is the one that wins — for the
    /// filename *and* for the spoken headline, which is the whole point of
    /// routing both through this one function.
    #[test]
    fn chapter_title_prefers_the_digests_own_title_over_the_mt_headline() {
        let root = fixture_root("title-script");
        let l = Layout::new(&root);
        std::fs::create_dir_all(l.chapters()).unwrap();
        std::fs::write(
            l.chapter_txt(9),
            "Chương 9: Tê! Thật là khủng khiếp dao phay\n\nbody\n",
        )
        .unwrap();
        // No script yet: the headline is all there is.
        assert_eq!(l.chapter_title(9), "Tê! Thật là khủng khiếp dao phay");
        // A script with a title: it wins, and the headline is not consulted.
        std::fs::write(
            l.script(9),
            r#"{"title":"Bí Ẩn Dao Phay Trong Phòng Bếp","segments":[]}"#,
        )
        .unwrap();
        assert_eq!(l.chapter_title(9), "Bí Ẩn Dao Phay Trong Phòng Bếp");
        // An empty or whitespace title falls back rather than naming the file "".
        std::fs::write(l.script(9), r#"{"title":"   ","segments":[]}"#).unwrap();
        assert_eq!(l.chapter_title(9), "Tê! Thật là khủng khiếp dao phay");
        // And a script that predates the field does too.
        std::fs::write(l.script(9), r#"{"segments":[]}"#).unwrap();
        assert_eq!(l.chapter_title(9), "Tê! Thật là khủng khiếp dao phay");
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
