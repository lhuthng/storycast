use super::workspaces::chapter_files;
use super::*;

impl Layout {
    pub fn data(&self) -> PathBuf {
        self.work.join("data")
    }

    pub fn chapters(&self) -> PathBuf {
        self.data().join("chapters")
    }

    pub fn chapter_txt(&self, n: u32) -> PathBuf {
        self.chapters().join(format!("ch{n:02}.txt"))
    }

    /// The chapter index: the frozen `n -> url` mapping a crawl run works from.
    pub fn crawl_index(&self) -> PathBuf {
        self.data().join("crawl-index.json")
    }

    /// The **global** crawler tree: `crawlers/`, at the checkout root.
    pub fn crawlers_dir(&self) -> PathBuf {
        self.root.join("crawlers")
    }

    /// The crawlers on this machine, for the screens that list them.
    pub fn crawl_scripts(&self) -> PathBuf {
        self.crawlers_dir()
    }

    /// The active workspace's own crawlers: `workspaces/<name>/crawl/`.
    pub fn crawl_workspace(&self) -> PathBuf {
        self.work.join("crawl")
    }

    /// The chapter's script: `data/script/NN.json`.
    pub fn script(&self, n: u32) -> PathBuf {
        self.script_dir().join(format!("{n:02}.json"))
    }

    /// Every script in the workspace, in chapter order.
    pub fn scripts(&self) -> Vec<PathBuf> {
        let mut out = chapter_files(&self.script_dir());
        out.sort_by_key(|p| chapter_of(p).unwrap_or(u32::MAX));
        out
    }

    /// Chapter numbers that have a script, ascending.
    pub fn script_chapters(&self) -> Vec<u32> {
        self.scripts()
            .iter()
            .filter_map(|p| chapter_of(p))
            .collect()
    }

    /// The scripts, one directory: `data/script/`.
    pub fn script_dir(&self) -> PathBuf {
        self.data().join("script")
    }

    /// Whether this chapter has been digested.
    pub fn digested(&self, n: u32) -> bool {
        self.script(n).is_file()
    }

    /// The recorded render plan: the single namer for a chapter's audio. See
    pub fn plan(&self, n: u32) -> PathBuf {
        self.render_dir().join(format!("{n:02}.json"))
    }

    /// The plans, one directory: `data/render/` — the sibling of
    pub fn render_dir(&self) -> PathBuf {
        self.data().join("render")
    }

    /// The workspace a chapter's script lives in, and the chapter, both read
    pub fn of_script(script_path: &Path) -> Option<(Self, u32)> {
        let chapter = chapter_of(script_path)?;
        let script_dir = script_path.parent()?;
        if script_dir.file_name()? != std::ffi::OsStr::new("script") {
            return None;
        }
        let root = script_dir.parent()?.parent()?;
        Some((
            Self::resolve(root).unwrap_or_else(|_| Self::new(root)),
            chapter,
        ))
    }

    pub fn bible(&self) -> PathBuf {
        self.data().join("bible.json")
    }

    /// Cast file, keyed by adapter **and** engine: swapping either one must
    pub fn cast(&self, engine: &str) -> PathBuf {
        self.data()
            .join(format!("cast-{}-{}.json", self.adapter, engine_key(engine)))
    }

    /// Per-chapter segment cache, keyed the same way.
    pub fn seg_dir(&self, engine: &str, n: u32) -> PathBuf {
        self.data().join(format!(
            "audio/segments-{}-{}-{n:02}",
            self.adapter,
            engine_key(engine)
        ))
    }

    pub fn ensure(&self) -> Result<()> {
        for d in [
            self.data(),
            self.chapters(),
            self.script_dir(),
            self.render_dir(),
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
    pub fn chapter_title(&self, n: u32) -> String {
        let auto = crate::config::Settings::load(&self.settings()).auto_title();
        let from_script = if auto {
            crate::read_json::<serde_json::Value>(&self.script(n))
                .ok()
                .and_then(|d| {
                    d.get("title")
                        .and_then(|t| t.as_str())
                        .map(str::trim)
                        .filter(|t| !t.is_empty())
                        .map(String::from)
                })
        } else {
            None
        };
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
