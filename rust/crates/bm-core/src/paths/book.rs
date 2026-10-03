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
    ///
    /// An artifact, not state, and deliberately a file an operator can read and
    /// hand-edit — for a book whose URLs are arbitrary slugs, authoring this by
    /// hand is more reliable than any script that re-derives it.
    pub fn crawl_index(&self) -> PathBuf {
        self.data().join("crawl-index.json")
    }

    /// The **global** crawler tree: `crawlers/`, at the checkout root.
    ///
    /// Not a language's and not a workspace's. A crawler is one site read in one
    /// language, but the *set* of crawlers this project has written is a fact
    /// about the project, and keeping it in one place is what lets a preset name
    /// a known site (`crawlers/known/storya.lua`) or the example EPUB crawler
    /// (`crawlers/examples/epub.lua`) without a copy per workspace. One tree, so
    /// an edit reaches every book and a new known site is one file and one
    /// registry row.
    ///
    /// The registry that names these files is `crawlers/knownsites.json`, read
    /// through [`crate::crawl::known_sites`].
    pub fn crawlers_dir(&self) -> PathBuf {
        self.root.join("crawlers")
    }

    /// The crawlers on this machine, for the screens that list them.
    ///
    /// Kept as a method because two callers ask it (the TUI's crawl view and the
    /// provision planner) and answers have to agree: the global tree. A book's
    /// own crawlers are [`Self::crawl_workspace`].
    pub fn crawl_scripts(&self) -> PathBuf {
        self.crawlers_dir()
    }

    /// The active workspace's own crawlers: `workspaces/<name>/crawl/`.
    ///
    /// The adapter's crawlers are shared by every workspace on this root and
    /// replaced wholesale by `:profile load`; a book whose site needs its own
    /// crawler therefore lives here, where `:profile load` cannot reach it and a
    /// second workspace never sees it. Searched **first** by
    /// `crawl::resolve_script`, so a same-named file shadows the profile's —
    /// the workspace's answer wins over the profile's.
    ///
    /// Provisioning rsyncs this directory to every worker (see
    /// `provision::steps::install_sources`) and the stamp hashes it, so an edit
    /// here reaches the cluster with the next `:prov`.
    pub fn crawl_workspace(&self) -> PathBuf {
        self.work.join("crawl")
    }

    /// The chapter's script: `data/script/NN.json`.
    pub fn script(&self, n: u32) -> PathBuf {
        self.script_dir().join(format!("{n:02}.json"))
    }

    /// Every script in the workspace, in chapter order.
    ///
    /// **One definition, because eight places ask it** — the audition index, the
    /// cast refill, the inject screen's usage map, the speaker index, the
    /// character's-lines test, the two planners that walk a range, and the
    /// reconciler — and because they cannot be allowed to disagree. A `read_dir`
    /// order is arbitrary, so an unsorted answer makes a "random" pick differ
    /// between two runs of the same session for no reason anyone could see.
    ///
    /// **Sorted by chapter number, not by path.** `PathBuf`'s own `Ord` is
    /// lexical over the file name, and `NN.json` is only zero-padded to two
    /// digits — so chapter 100 sorted between 10 and 11, and a book past 99 came
    /// out as `1 … 10, 100 … 109, 11, 110 …`. Every consumer walked chapters
    /// backwards and the script window listed them that way. Comparing the
    /// parsed number is the only order that means "chapter order" past 99.
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
    ///
    /// A directory, because a book of five hundred chapters wrote five hundred
    /// `script-NN.json` and five hundred `render-NN.json` in one folder beside
    /// the cast and the bible — so *finding* the scripts meant filtering a name
    /// prefix, which every reader spelled out for itself. The name now lives in
    /// the folder and the file is the chapter, which is the shape
    /// `chapters/chNN.txt` beside it already had.
    pub fn script_dir(&self) -> PathBuf {
        self.data().join("script")
    }

    /// Whether this chapter has been digested.
    ///
    /// **One definition, because three places ask it** — the digest manager's
    /// list, its filter and its painter — and they have to agree: the cursor
    /// indexes the *filtered* rows, so a predicate that answered differently in
    /// the draw than in the filter would highlight one chapter while acting on
    /// another. A test found exactly that divergence when each site spelled the
    /// question out for itself.
    ///
    /// The script file is the whole answer: it is what the digest stage produces
    /// and what every downstream stage reads.
    pub fn digested(&self, n: u32) -> bool {
        self.script(n).is_file()
    }

    /// The recorded render plan: the single namer for a chapter's audio. See
    /// [`crate::assemble::RenderPlan`].
    pub fn plan(&self, n: u32) -> PathBuf {
        self.render_dir().join(format!("{n:02}.json"))
    }

    /// The plans, one directory: `data/render/` — the sibling of
    /// [`script_dir`](Self::script_dir), and for the same reason.
    pub fn render_dir(&self) -> PathBuf {
        self.data().join("render")
    }

    /// The workspace a chapter's script lives in, and the chapter, both read
    /// back off the script's own path.
    ///
    /// The merge path is handed one file and nothing else — `data/script/NN.json`
    /// — and must answer for the layout behind it. The workspace is the parent
    /// of the `data` that holds the script folder, and the folder is recognised
    /// **by name** rather than by counting levels: a count is silent when it is
    /// wrong, and being one level short of the workspace does not fail, it hands
    /// back a layout whose `chapters/` is somewhere else entirely. A path that
    /// is not in a script folder is `None` rather than a guess.
    /// The layout a script at this path belongs to, and its chapter number.
    ///
    /// **Resolved, not defaulted.** This used to hand back [`Layout::new`],
    /// whose adapter is the hardcoded `"default"` — so every adapter-scoped
    /// fact read through a script path silently degraded, and the most visible
    /// was the spoken chapter heading: `title_speech_for_script` asks the
    /// layout's adapter for its language, found no `adapters/default/`, and
    /// announced `Chương` for an English book whose adapter declares `en-US`.
    /// A checkout resolves its binding, a book-rooted workspace its own
    /// `settings.json`, and a provisioned box its pushed `.bm/profile`, so all
    /// three say which language they write. `new` remains the fallback for a
    /// root that cannot resolve at all.
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
    /// never poison the other's segment cache.
    pub fn cast(&self, engine: &str) -> PathBuf {
        self.data()
            .join(format!("cast-{}-{}.json", self.adapter, engine_key(engine)))
    }

    /// Per-chapter segment cache, keyed the same way.
    ///
    /// The engine half is what the old `if engine == "vieneu" … else
    /// "gemini-v2"` got wrong: the `else` named one engine for *every* engine
    /// that was not VieNeu, so a third engine would have read and written
    /// Gemini's segments — one engine's audio served under another's name.
    /// Deriving the key is what makes a second engine possible at all.
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
    ///
    /// **The script's own `title` wins — unless `title_mode` says `default`.**
    /// The crawled headline is the site's
    /// auto-excerpt of the chapter — `Chương 9: Tê! Thật là khủng khiếp dao
    /// phay`, `Chương 10: Tiền bối đối với dao phay yêu cầu đều cao như vậy?` —
    /// a sentence out of the prose with the punctuation still on it, which then
    /// lands on the cover of the mp3. It is a title only in the sense that the
    /// site put it on the first line. The digest has read the chapter and can
    /// name it, so its `title` is the one used; the headline stays as the
    /// fallback for a chapter that was digested before the field existed.
    ///
    /// [`crate::config::Settings::title_mode`] = `default` inverts that and
    /// takes the headline only. A digest is a fresh model call, so under
    /// `auto` a re-digest can answer a different `title` — and that moves both
    /// the spoken headline and this filename. A book that must not drift pins
    /// the headline instead.
    ///
    /// Both routes end in the same scrub, so a title from either source is a
    /// legal filename and the spoken headline and the file agree.
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
