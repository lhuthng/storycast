//! Screens: the modal states the key chain and the painter agree on.
use crate::tui::app::App;
use crate::tui::audition::AuditionLine;
use crate::tui::model::Facet;
use crate::tui::sound::SoundView;
use bm_proto::Stage;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TextKind {
    AddMachine,
    /// Pooled sample: tags come from the filename, the voice auto-rolls.
    AddSample,
    /// Named voice (`path as Name`): manual assignment only, never rotates.
    AddNamed,
    /// Run-config editor (opened with `e` on the run screen): saves range,
    /// analyzer and model chain to the settings file. Launches nothing.
    RunConfig,
    /// App-wide ssh defaults (`:sshkey`, `:sshuser`, `:sshport`): save-only
    /// prompts in the `RunConfig` style — persist to settings.json, dispatch
    /// nothing.
    SshKey,
    SshUser,
    SshPort,
    /// The address workers should dial (`:advertise`): save-only, like the ssh
    /// defaults. Empty clears it back to the routing-table guess.
    Advertise,
    /// GitHub `owner/name` hosting the model artifact (`:release`): save-only.
    /// Empty means the weights are pushed to each box instead of fetched from
    /// a release, which is what every box did before the setting existed.
    ModelsRelease,
    /// GitHub `owner/name` hosting the profile pack (`:packrelease`): save-only.
    /// Empty means `assets/` is pushed to each box instead of fetched, which is
    /// what every box did before the setting existed. The *tag* is not asked for
    /// here — it comes from the loaded profile's version.
    PacksRelease,
    /// Render batch size (`:batch`): how many of one chapter's takes a single
    /// offer carries. Save-only, like the ssh defaults — it is read by the
    /// scheduler when it builds the next offer, so nothing is dispatched.
    RenderBatch,
    /// One box's TTS sidecar thread count (`:threads`): the ONNX threads its
    /// sidecar is launched with. A number sets it, empty clears the override
    /// back to the sidecar's own default. Dispatched to the API — config, so it
    /// persists in `machines.json` and converges on the box. One model behind a
    /// mutex, so more threads do not run two lines at once.
    TtsThreads,
    /// Mix levels (`:mix`): story speed plus the two layer volumes, saved to
    /// the settings file like the run config. Launches nothing.
    Mix,
    /// One sound-design pool entry (`:sound` → `a`): the whole entry as a
    /// `key=value` line, saved to that layer's registry. Launches nothing.
    SoundAdd(bm_core::audio_pool::PoolKind),
    /// The same line for an entry already in the pool, carrying its name: the
    /// name is the registry's key, so the prompt has to know which key it is
    /// rewriting rather than reading it out of the buffer.
    SoundEdit(bm_core::audio_pool::PoolKind, String),
    /// One pooled sound's own trim. Empty clears it.
    SoundLevel(bm_core::audio_pool::PoolKind, String),
    Translate,
    CrawlTemplate,
    /// `:import` — `<chapter> <path>`: text the operator supplies instead of a
    /// fetch. A **path**, not a paste: a terminal delivers a dropped file as its
    /// path, and a chapter pasted into a single-line prompt would submit on the
    /// first newline (so a whole chapter is `:import` over the API, or saved to
    /// a file first).
    Import,
    /// `:workspace` — list, switch or create. Switching only moves the
    /// `.bm/active-workspace` pointer, but the ledger, settings and data the
    /// running cluster reads all move with it, so the dispatch is gated on a
    /// quiet cluster.
    Workspace,
    /// `:profile` — list bundles, load one (unpack) or pack the live tree.
    /// Loading replaces `assets/` + `prompts/`, which workers are reading.
    Profile,
    /// `:login` — hand over the console's `accessKeys.csv`. The secret is
    /// never typed here, so the CSV is the whole prompt.
    AwsLogin,
    /// `:discover` — the flags for one account read, exactly as the CLI takes
    /// them (same clap definition), with any path tilde-expanded.
    AwsDiscover,
    /// `:` command line: the buffer names a key (`m`) or a word
    /// (`reconcile`) and Enter presses it for you. Never dispatched —
    /// handled inline so one keypress can open another prompt.
    Command,
    /// One LLM provider's secret key (`L` → `k`): typed, never displayed.
    LlmKey(String),
    /// One provider's base URL (`L` → `u`).
    LlmUrl(String),
    /// One provider's model name (`L` → `m`): typed, or picked from the
    /// fetched list with `f` then `Enter`.
    LlmModel(String),
}

/// A single-line editor with a real cursor. The old prompt could only append
/// and backspace; a mistyped address meant starting over.
#[derive(Debug, Clone)]
pub(crate) struct TextPrompt {
    pub(crate) kind: TextKind,
    pub(crate) title: String,
    pub(crate) hint: String,
    pub(crate) buf: String,
    /// Cursor position in *characters*, never bytes — the data is Vietnamese.
    pub(crate) cursor: usize,
}

impl TextPrompt {
    pub(crate) fn new(kind: TextKind, title: &str, hint: &str, initial: &str) -> Self {
        let buf = initial.to_string();
        let cursor = buf.chars().count();
        TextPrompt {
            kind,
            title: title.to_string(),
            hint: hint.to_string(),
            buf,
            cursor,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.buf.chars().count()
    }

    /// The site the buffer names, if this prompt takes a URL and the buffer
    /// holds one we have a crawler for.
    ///
    /// Recomputed from the buffer on every keystroke rather than cached, because
    /// the answer is a function of what is on screen and a stale answer is the
    /// kind of lie this whole module exists to avoid. It is `None` for every
    /// other prompt and for a URL we have nothing on, so the caller can simply
    /// ask.
    pub(crate) fn known_site(&self) -> Option<&'static bm_core::crawl::KnownSite> {
        if self.kind != TextKind::CrawlTemplate {
            return None;
        }
        // A template with a `{n}` in it is not a site URL — it is already a
        // mapping, and matching one against the registry would only ever match a
        // registry entry that happens to contain the same host, which says
        // nothing about whether the template is the right one for the site.
        if self.buf.contains("{n}") {
            return None;
        }
        bm_core::crawl::for_url(&self.buf)
    }

    /// The note to show under the prompt for a recognised URL: which crawler,
    /// what shape it is written against, and the one thing to know first.
    ///
    /// The paste block is deliberately **not** included. This is an 88-column
    /// dialog, not a terminal, and ten lines of JSON in it would push the input
    /// line itself off the top of a laptop screen. `bm-inductor check` prints
    /// the block for the person who wants to copy it.
    pub(crate) fn known_note(&self) -> Option<String> {
        let site = self.known_site()?;
        let mut s = format!("known site · {} · ", site.host);
        if site.is_crawlable() {
            s.push_str(&format!("crawler {}", site.script));
        } else {
            s.push_str("no bundled crawler");
        }
        s.push_str(&format!(" · {}", site.language));
        if site.url_template.is_empty() && site.is_crawlable() {
            s.push_str(" · no {n} in its URLs: submit empty and set crawl.script");
        }
        if !site.language.starts_with("Vietnamese") {
            // Said here, and only here: a Vietnamese G2P applied to another
            // language does not fail, it mispronounces, and the person watching
            // a hundred renders is the only one who can tell.
            s.push_str(&format!(
                "\nheads up · the voices and the G2P are Vietnamese, so this {} text will \
                 be pronounced against Vietnamese syllable rules — expect it to sound wrong, \
                 not to error.",
                site.language
            ));
        }
        if let Some(c) = site.caveat {
            s.push('\n');
            s.push_str(c);
        }
        Some(s)
    }

    pub(crate) fn byte_at(&self, char_idx: usize) -> usize {
        self.buf
            .char_indices()
            .nth(char_idx)
            .map(|(b, _)| b)
            .unwrap_or(self.buf.len())
    }

    pub(crate) fn insert(&mut self, c: char) {
        let b = self.byte_at(self.cursor);
        self.buf.insert(b, c);
        self.cursor += 1;
    }

    pub(crate) fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let (a, b) = (self.byte_at(self.cursor - 1), self.byte_at(self.cursor));
        self.buf.replace_range(a..b, "");
        self.cursor -= 1;
    }

    pub(crate) fn delete(&mut self) {
        if self.cursor >= self.len() {
            return;
        }
        let (a, b) = (self.byte_at(self.cursor), self.byte_at(self.cursor + 1));
        self.buf.replace_range(a..b, "");
    }

    pub(crate) fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub(crate) fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.len());
    }

    pub(crate) fn home(&mut self) {
        self.cursor = 0;
    }

    pub(crate) fn end(&mut self) {
        self.cursor = self.len();
    }

    pub(crate) fn kill_to_start(&mut self) {
        let b = self.byte_at(self.cursor);
        self.buf.replace_range(..b, "");
        self.cursor = 0;
    }

    pub(crate) fn kill_word(&mut self) {
        while self.cursor > 0 {
            let prev = self.buf.chars().nth(self.cursor - 1).unwrap_or(' ');
            if prev.is_whitespace() {
                self.backspace();
            } else {
                break;
            }
        }
        while self.cursor > 0 {
            let prev = self.buf.chars().nth(self.cursor - 1).unwrap_or(' ');
            if prev.is_whitespace() {
                break;
            }
            self.backspace();
        }
    }

    /// The buffer split at the cursor, for rendering a visible caret.
    pub(crate) fn split(&self) -> (String, String) {
        let b = self.byte_at(self.cursor);
        (self.buf[..b].to_string(), self.buf[b..].to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PickStage {
    Character,
    Voice,
}

#[derive(Debug, Clone)]
pub(crate) struct Picker {
    pub(crate) stage: PickStage,
    /// Chosen in step 1; empty until then.
    pub(crate) character: String,
    pub(crate) filter: String,
    pub(crate) cursor: usize,
    pub(crate) scroll: usize,
    /// Voices auditioned this session, so the operator can tell them apart
    /// from ones merely read about.
    pub(crate) previewed: Vec<String>,
    /// The real line the current and candidate voice are A/B'd on. Held here so
    /// both auditions speak the same sentence; re-picked when the character
    /// changes or the operator asks for another.
    pub(crate) line: Option<AuditionLine>,
    /// Filter focus on step 2: every letter types (t/T included) and the
    /// audition keys go quiet. Off by default — the screen opens in audition
    /// focus, where t/T/^T play and any other letter focuses the filter.
    /// Step 1 ignores it: picking a character needs every letter.
    pub(crate) filter_focus: bool,
}

impl Picker {
    pub(crate) fn new() -> Self {
        Picker {
            stage: PickStage::Character,
            character: String::new(),
            filter: String::new(),
            cursor: 0,
            scroll: 0,
            previewed: Vec::new(),
            line: None,
            filter_focus: false,
        }
    }
}

/// One entry's removal, and the screen it was asked from.
///
/// A named struct rather than three fields on the variant: the confirmation
/// carries where to *return to* as well as what to do, and spelling that inline
/// made the whole `ConfirmAction` enum too wide to stay on one line per variant.
#[derive(Debug, Clone)]
pub(crate) struct SoundRemoval {
    pub(crate) layer: bm_core::audio_pool::PoolKind,
    pub(crate) name: String,
    pub(crate) view: crate::tui::sound::SoundView,
}

#[derive(Debug, Clone)]
pub(crate) enum ConfirmAction {
    Quit,
    /// Terminate the named EC2 instance ids. Always explicit ids, never a
    /// filter: the destructive step is over a list the operator just read.
    AwsDown {
        ids: Vec<String>,
    },
    Provision {
        addr: String,
        force: bool,
    },
    DropMachine {
        addr: String,
    },
    /// Re-point a machine whose EC2 address drifted at the address the box
    /// carries now. Carries the old address; the instance id comes from the
    /// selected machine's note inside the job.
    RelinkMachine {
        old_addr: String,
    },
    SwapVoice {
        character: String,
        voice: String,
    },
    StopBackend,
    Reconcile,
    Rerender,
    /// Fold characters by hand: the first name survives, the rest are
    /// absorbed. Asked first like every other rewrite, then dispatched as
    /// one `merge` op.
    Merge {
        survivor: String,
        absorbed: Vec<String>,
    },
    /// Take one entry out of a sound-design pool. Carries the view it was
    /// asked from, so answering the dialog returns to the same tab and row
    /// instead of dumping the operator back on the dashboard.
    SoundRemove(SoundRemoval),
    /// Take every row one worker holds back off it. The only release that
    /// asks first: a single row is one keypress with nothing to lose, while
    /// this can be a whole box's afternoon and the operator cannot see the
    /// size of it from the row they pressed on. `list` is the ledger view it
    /// was asked from, so answering leaves the rows leaving in front of them.
    ReleaseWorker {
        worker: String,
        count: usize,
        beating: bool,
        list: TasksView,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct Confirm {
    pub(crate) title: String,
    pub(crate) body: Vec<String>,
    pub(crate) action: ConfirmAction,
    pub(crate) danger: bool,
}

impl Confirm {
    /// Full re-speak behind one Enter: every render back to pending with
    /// its merge, caches deleted. Shared by `:rerender` and the Tasks
    /// screen's `E`, so the two paths cannot disagree about the cost.
    pub(crate) fn rerender() -> Self {
        Confirm {
            title: "Re-render everything?".into(),
            danger: true,
            body: vec![
                "Every render task goes back to pending, with its merge.".into(),
                "Cached segments and finished mp3s are deleted, so every".into(),
                "voice is re-synthesized from scratch — slow and costly.".into(),
                String::new(),
                "For mix-only changes (speed, volumes, effect clips) use".into(),
                ":mix instead: it requeues merges and keeps this cache.".into(),
            ],
            action: ConfirmAction::Rerender,
        }
    }

    /// Take every row one worker holds back off it — `W` on the ledger.
    ///
    /// One dialog for both cases, because the *answer* is the same; only the
    /// sentence changes, and `beating` decides it. A silent box is the ordinary
    /// case and nothing is at risk. A box that is still answering is the
    /// surprising one, and there the operator has to be told the price in the
    /// one place they can still say no: the work is not lost, it is spoken
    /// twice.
    pub(crate) fn release_worker(
        worker: String,
        count: usize,
        beating: bool,
        list: TasksView,
    ) -> Self {
        let mut body = vec![
            format!("{count} row(s) held by {worker} go back to the pool."),
            String::new(),
        ];
        if beating {
            body.push("It is still beating, so whatever it has in hand finishes and".into());
            body.push("its report lands stale — and the rows here are offered again,".into());
            body.push("so a take can be spoken twice before the ledger settles.".into());
        } else {
            body.push("It has stopped beating, so nothing is in flight: every row".into());
            body.push("returns exactly as it was — attempts kept, nothing deleted —".into());
            body.push("and is offered to the next box that asks.".into());
        }
        Confirm {
            title: format!("Release everything {worker} holds?"),
            danger: beating,
            body,
            action: ConfirmAction::ReleaseWorker {
                worker,
                count,
                beating,
                list,
            },
        }
    }
}

/// Read-only overview of the whole cast: who speaks with what, which voices
/// are shared, and which assignments the roster cannot resolve.
///
/// The picker can only answer "what is this one character's voice"; this
/// answers "is the cast healthy", which previously meant reading the cast file
/// by hand. `Enter` hands the highlighted speaker to the picker's step 2, so
/// the overview is a starting point for a fix rather than just a report.
#[derive(Debug, Clone)]
pub(crate) struct CastView {
    pub(crate) cursor: usize,
    pub(crate) scroll: usize,
    pub(crate) filter: String,
    /// The real line the highlighted speaker is auditioned on, held so pressing
    /// the key twice does not hop between sentences.
    pub(crate) line: Option<AuditionLine>,
    /// Filter focus: every letter types (t/T included) and the audition
    /// keys go quiet. Off by default — the screen opens in audition focus.
    pub(crate) filter_focus: bool,
}

impl CastView {
    pub(crate) fn new() -> Self {
        CastView {
            cursor: 0,
            scroll: 0,
            filter: String::new(),
            line: None,
            filter_focus: false,
        }
    }
}

/// One segment of a script, as the inspection window shows it.
///
/// `n` is the **1-based** segment number — the same number `op_fix_speaker`
/// takes and the number the refusal message names, so a row on screen, the
/// number in the status line and the number in an error are all one count.
/// `speaker` is empty for a sound item, which is drawn dimmed with its tag.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ScriptSegment {
    pub(crate) n: usize,
    pub(crate) speaker: String,
    pub(crate) text: String,
}

/// The script inspection window: the digested chapters as a list, one open
/// chapter's segments beside their speakers, and `s` to re-point a segment.
///
/// Shaped like the digest manager on purpose — one view, two modes — so
/// `Esc` has exactly one meaning (step back) in both. Segments are read
/// from disk on open, not kept warm: a chapter is a hundred-odd rows, the
/// read is microseconds, and a warm cache would lie the moment a digest or
/// an edit rewrote the file underneath it.
#[derive(Debug, Clone)]
pub(crate) struct ScriptView {
    /// Every chapter with a script on disk, ascending.
    pub(crate) chapters: Vec<u32>,
    pub(crate) cursor: usize,
    /// The chapter whose segments are open, if any.
    pub(crate) open: Option<u32>,
    /// The open chapter's segments, read when it was opened.
    pub(crate) segments: Vec<ScriptSegment>,
    /// The segment cursor: which row `s` would re-point.
    pub(crate) seg_cursor: usize,
    /// The speaker picker (`s`): type-ahead filter, cursor, and the row it
    /// was opened on. `None` while the segments are the active list.
    pub(crate) pick: Option<ScriptPick>,
    /// Filter text over the chapter list.
    pub(crate) filter: String,
    /// The excerpt panel (`e` at the segment depth) is up over the segments.
    pub(crate) excerpt_open: bool,
    /// The open chapter's own excerpt — the state its end leaves for the
    /// *next* chapter. Read when `e` opened the panel, so a digest finishing
    /// behind the panel is picked up on the next open, never mid-view.
    pub(crate) excerpt_own: String,
    /// The chain this chapter was fed: the previous chapters' excerpts, newest
    /// first, from `bm_core::digest::excerpt_chain` — the same window and skip
    /// rules the prompt was built with.
    pub(crate) excerpt_fed: Vec<(u32, String)>,
    /// Scroll offset into the excerpt panel (rows). Clamped in the draw, where
    /// the wrapped height is known.
    pub(crate) excerpt_scroll: usize,
}

/// A speaker being picked for one segment.
#[derive(Debug, Clone)]
pub(crate) struct ScriptPick {
    /// The segment the pick is for (1-based). Carried so the header can
    /// name what is being re-pointed while the list scrolls under it.
    pub(crate) segment: usize,
    /// Who the segment speaks as now — `Enter` on them is a no-op, and
    /// `expect` is the check the op runs.
    pub(crate) expect: String,
    pub(crate) filter: String,
    pub(crate) cursor: usize,
    pub(crate) scroll: usize,
    /// The open chapter's roster, read when the pick was opened. The
    /// suggestion list's prefix and the drawn rows' "(this chapter)" tag
    /// both read this, so the two can never disagree about who is local.
    pub(crate) roster_cache: Vec<String>,
}

impl ScriptPick {
    /// Visible at once. Five, like the model list on the LLM screen: a
    /// whole-cast list is hundreds of names, and the cursor scrolls the
    /// window rather than growing the box.
    pub(crate) const SHOW: usize = 5;
}

impl ScriptView {
    /// Chapters per row in the chapter list.
    ///
    /// **One constant, because the draw and the keys must agree.** The list
    /// draws twelve across, and `↑`/`↓` walk rows while `←`/`→` walk columns:
    /// they used to both move the cursor by one, so on a twelve-wide grid `↓`
    /// stepped sideways and the horizontal arrows did nothing at all.
    pub(crate) const PER_ROW: usize = 12;

    /// Open on the chapters that have scripts — `Layout::script_chapters`,
    /// the same scan the audition index and the reconcile pass use.
    pub(crate) fn new(layout: &bm_core::Layout) -> Self {
        ScriptView {
            chapters: layout.script_chapters(),
            cursor: 0,
            open: None,
            segments: Vec::new(),
            seg_cursor: 0,
            pick: None,
            filter: String::new(),
            excerpt_open: false,
            excerpt_own: String::new(),
            excerpt_fed: Vec::new(),
            excerpt_scroll: 0,
        }
    }

    /// Read the excerpt panel's two halves for `chapter` and raise it: the
    /// chapter's own excerpt (what the next chapter is fed) from its script,
    /// and the chain it was fed, from the same `digest::excerpt_chain` the
    /// prompt uses. Reads once, like the segments: a re-open picks up a digest
    /// that landed behind the panel.
    pub(crate) fn open_excerpts(&mut self, layout: &bm_core::Layout, chapter: u32) {
        self.excerpt_own = bm_core::read_json::<serde_json::Value>(&layout.script(chapter))
            .ok()
            .and_then(|d| {
                d.get("excerpt")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_default();
        self.excerpt_fed = bm_core::digest::excerpt_chain(layout, chapter);
        self.excerpt_open = true;
        self.excerpt_scroll = 0;
    }

    /// Drop the excerpt panel's state — for the moves that leave the chapter
    /// (Esc back to the list, opening another one), so a stale excerpt can
    /// never be drawn against a different chapter.
    pub(crate) fn close_excerpts(&mut self) {
        self.excerpt_open = false;
        self.excerpt_own.clear();
        self.excerpt_fed.clear();
        self.excerpt_scroll = 0;
    }

    /// The chapter rows actually drawn: the filter applies here, and the
    /// cursor indexes this list rather than `chapters` — otherwise filtering
    /// would leave the highlight on a chapter the operator did not choose.
    pub(crate) fn rows(&self) -> Vec<u32> {
        self.chapters
            .iter()
            .copied()
            .filter(|n| self.filter.is_empty() || format!("{n}").contains(&self.filter))
            .collect()
    }

    /// The chapter under the list cursor.
    pub(crate) fn selected(&self) -> Option<u32> {
        self.rows().get(self.cursor).copied()
    }

    /// Move the list cursor by whole rows and columns, the way the grid is
    /// drawn.
    ///
    /// A vertical step is clamped to the list and a horizontal one wraps to the
    /// neighbouring row — so `→` off the last column steps to the first of the
    /// next row, which is what a grid does and what a plain `±1` never could.
    /// The column is clamped rather than wrapped for `↑`/`↓`: stepping off the
    /// end of a short row keeps the chapter, and only the row changes.
    pub(crate) fn move_cursor(&mut self, d_row: isize, d_col: isize) {
        let total = self.rows().len();
        if total == 0 {
            self.cursor = 0;
            return;
        }
        let last = total - 1;
        let (row, col) = (
            (self.cursor / Self::PER_ROW) as isize,
            (self.cursor % Self::PER_ROW) as isize,
        );
        let (row, col) = if d_row != 0 {
            (row + d_row, col)
        } else {
            // Horizontal: walk the row's own extent, then wrap.
            let flat = row * Self::PER_ROW as isize + col + d_col;
            let n = Self::PER_ROW as isize;
            ((flat.div_euclid(n)), flat.rem_euclid(n))
        };
        let idx = (row * Self::PER_ROW as isize + col).clamp(0, last as isize);
        self.cursor = idx as usize;
    }

    /// Read one chapter's segments off disk, newest file wins. Sound items
    /// (no `speaker`) are kept as rows with an empty speaker so the numbers
    /// on screen stay the numbers the op takes — hiding them would renumber
    /// every row below a sound, and a re-point aimed from this window must
    /// never be off by the number of sound items above it.
    pub(crate) fn read_segments(
        &self,
        layout: &bm_core::Layout,
        chapter: u32,
    ) -> Vec<ScriptSegment> {
        let Ok(data) = bm_core::read_json::<serde_json::Value>(&layout.script(chapter)) else {
            return Vec::new();
        };
        data.get("segments")
            .and_then(|s| s.as_array())
            .map(|segments| {
                segments
                    .iter()
                    .enumerate()
                    .map(|(i, seg)| ScriptSegment {
                        n: i + 1,
                        speaker: seg
                            .get("speaker")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .trim()
                            .to_string(),
                        text: seg
                            .get("text")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .trim()
                            .to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The speaker suggestions for the open pick: the open chapter's own
    /// roster first (the digest's answer to "who is here"), then every other
    /// known speaker alphabetically. Fold-folded so "thai son" finds
    /// "Thái Sơn" the way the picker's filter does — and deduped the same
    /// way, so a name the roster holds is never offered twice.
    ///
    /// Reads the chapter's script once per call (a keystroke in the picker,
    /// not a frame): a digest finishing between two keystrokes is picked up
    /// on the next one, which is the freshness a cached list would hide.
    pub(crate) fn suggestions(&self, app: &App, pick: &ScriptPick) -> Vec<String> {
        let fold = |s: &str| bm_core::util::fold(s);
        let want = fold(&pick.filter);
        let mut out: Vec<String> = Vec::new();
        let push = |name: String, out: &mut Vec<String>| {
            if name.trim().is_empty() || out.iter().any(|n| fold(n) == fold(&name)) {
                return;
            }
            if !want.is_empty() && !fold(&name).contains(&want) {
                return;
            }
            out.push(name);
        };
        for name in &pick.roster_cache {
            push(name.clone(), &mut out);
        }
        // The rest of the universe, alphabetically — the roster block stays
        // first because it is pushed before this walk, and the fold is what
        // "alphabetical" means for accented names.
        let mut others: Vec<String> = crate::tui::screen::known_speakers(app);
        others.sort_by(|a, b| fold(a).cmp(&fold(b)).then_with(|| a.cmp(b)));
        for name in others {
            push(name, &mut out);
        }
        out
    }
}

/// Every speaker the TUI knows about, folded into one sorted list: the
/// roster's characters (`Narrator` included), then any speaker a script
/// names — the picker's own universe, shared with the segment window's
/// suggestions. The inductor's `known_characters` computed over the files
/// it owns; here, over what the TUI has already fetched plus the scripts
/// on disk, because the TUI may be running while the inductor is not.
pub(crate) fn known_speakers(app: &App) -> Vec<String> {
    use std::collections::BTreeSet;
    let mut set: BTreeSet<String> = BTreeSet::new();
    if let Some(r) = &app.roster {
        set.extend(r.characters.iter().cloned());
    }
    if app.layout.root.as_os_str().is_empty() {
        return set.into_iter().collect();
    }
    let bible = bm_core::digest::load_bible(&app.layout.bible());
    if let Some(chars) = bible.get("characters").and_then(|c| c.as_array()) {
        for c in chars {
            if let Some(n) = c.get("name").and_then(|n| n.as_str()) {
                if !n.trim().is_empty() {
                    set.insert(n.to_string());
                }
            }
        }
    }
    for sp in app.layout.scripts() {
        let Ok(data) = bm_core::read_json::<serde_json::Value>(&sp) else {
            continue;
        };
        if let Some(list) = data.get("roster").and_then(|r| r.as_array()) {
            for n in list.iter().filter_map(|v| v.as_str()) {
                if !n.trim().is_empty() {
                    set.insert(n.to_string());
                }
            }
        }
        if let Some(segs) = data.get("segments").and_then(|s| s.as_array()) {
            for seg in segs {
                if let Some(n) = seg.get("speaker").and_then(|v| v.as_str()) {
                    if !n.trim().is_empty() {
                        set.insert(n.to_string());
                    }
                }
            }
        }
    }
    set.into_iter().collect()
}

/// The Cloud view: what the EC2 account holds, one row per instance.
///
/// Kept separate from the Machines pane on purpose. A `Machine` is a *linked*
/// box — it has an ssh key, it can be provisioned and driven; an `AwsInstance`
/// is an EC2 resource that may be linked to nothing. A box the CLI launched has
/// no registry entry to merge with, so a merged pane would need a correlation
/// key (an instance id on `Machine`) that deliberately does not exist. This view
/// shows the account and marks the rows the registry has never heard of.
#[derive(Debug, Clone)]
pub(crate) struct CloudView {
    pub(crate) cursor: usize,
    pub(crate) scroll: usize,
}

impl CloudView {
    pub(crate) fn new() -> Self {
        CloudView {
            cursor: 0,
            scroll: 0,
        }
    }
}

/// The Tasks screen: the whole ledger, navigable and filterable.
///
/// The dashboard's Tasks pane is a roll-up — counts per stage. It answers "is
/// anything wrong" but never "which chapter, and why". This screen answers the
/// second question, which is the one that actually blocks an operator: a
/// shelved digest is a row you can open, read, and re-queue from here.
#[derive(Debug, Clone)]
pub(crate) struct TasksView {
    pub(crate) cursor: usize,
    pub(crate) scroll: usize,
    pub(crate) filter: String,
    /// Which kind the ledger is narrowed to. See [`Facet`](crate::tui::model::Facet):
    /// `←/→` steps it, so "only render" is one keypress rather than a word.
    pub(crate) facet: Facet,
}

impl TasksView {
    pub(crate) fn new() -> Self {
        TasksView {
            cursor: 0,
            scroll: 0,
            filter: String::new(),
            facet: Facet::All,
        }
    }
}

/// One task's page, keyed by `(stage, chapter)`.
///
/// Deliberately a key and not a `Task` snapshot: the current task is looked up
/// on every draw, so re-queueing from here updates the page you are looking at
/// instead of leaving a stale copy on screen. `list` is the Tasks view this page
/// was opened from, so Esc returns to the same row under the same filter.
#[derive(Debug, Clone)]
pub(crate) struct TaskDetail {
    pub(crate) stage: Stage,
    pub(crate) chapter: u32,
    pub(crate) scroll: usize,
    pub(crate) list: TasksView,
}

/// Which step the guided `workspace new` is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WsStep {
    Name,
    Profile,
    Crawler,
    /// The chosen crawler is a local EPUB: where the book is.
    Epub,
    CustomUrl,
}

/// One row of a guided-create list: what it says, what it explains, and the
/// value the step reads back.
#[derive(Debug, Clone)]
pub(crate) struct WsItem {
    pub(crate) label: String,
    pub(crate) note: String,
    /// A preset id, a crawler kind, or a known site's host.
    pub(crate) value: String,
}

/// The guided `workspace new`: name, then profile, then how chapters arrive.
///
/// **One screen rather than a chain of prompts**, because the steps share
/// state — the preset list is read once, and the crawler list is built from the
/// chosen profile's adapter — and a prompt that closed between steps would have
/// to stash that on the app and re-read it. `Esc` steps *back* one step rather
/// than closing, so a wrong profile is one key from the name already typed.
#[derive(Debug, Clone)]
pub(crate) struct WorkspaceNew {
    pub(crate) step: WsStep,
    pub(crate) name: String,
    pub(crate) name_cursor: usize,
    /// Every preset, `(id, label)`, read when the screen opens.
    pub(crate) profiles: Vec<WsItem>,
    pub(crate) profile: Option<usize>,
    /// Built once the profile is chosen: none / local file / known sites /
    /// custom. Empty until then.
    pub(crate) crawlers: Vec<WsItem>,
    /// The custom-site URL template, typed on the [`WsStep::CustomUrl`] step.
    pub(crate) url: String,
    pub(crate) url_cursor: usize,
    /// The EPUB path, typed on the [`WsStep::Epub`] step — a `.epub` copied
    /// into the new workspace's `tmp/book.epub`, or a folder of volumes copied
    /// into its `books/`. Which one it is, is read off the path itself.
    pub(crate) epub: String,
    pub(crate) epub_cursor: usize,
    /// Highlight within whichever list is on screen.
    pub(crate) cursor: usize,
    pub(crate) scroll: usize,
    /// Why the last Enter was refused, if it was. Drawn in place of the hint.
    pub(crate) error: Option<String>,
}

impl WorkspaceNew {
    pub(crate) fn new(name: String, profiles: Vec<WsItem>) -> Self {
        WorkspaceNew {
            name_cursor: name.chars().count(),
            name,
            profiles,
            step: WsStep::Name,
            profile: None,
            crawlers: Vec::new(),
            url: String::new(),
            url_cursor: 0,
            epub: String::new(),
            epub_cursor: 0,
            cursor: 0,
            scroll: 0,
            error: None,
        }
    }

    /// The list the current step navigates, if it has one.
    pub(crate) fn list(&self) -> &[WsItem] {
        match self.step {
            WsStep::Profile => &self.profiles,
            WsStep::Crawler => &self.crawlers,
            _ => &[],
        }
    }

    pub(crate) fn move_cursor(&mut self, down: bool) {
        let len = self.list().len();
        if len == 0 {
            self.cursor = 0;
            return;
        }
        self.cursor = if down {
            (self.cursor + 1).min(len - 1)
        } else {
            self.cursor.saturating_sub(1)
        };
    }
}

/// `:ws` with no name: every book this checkout holds, chosen with arrows
/// rather than spelled.
///
/// The rows are [`WsItem`]s because the guided-create list already draws
/// `label` + `note`, and a switch wants those same two columns: the name, and
/// what the directory actually carries.
#[derive(Debug, Clone)]
pub(crate) struct WsList {
    pub(crate) rows: Vec<WsItem>,
    /// The rows that are directories but not workspaces, by index, each with
    /// the reason. Beside the rows rather than folded into `note` because
    /// `Enter` has to refuse *on the spot* — the one rule this TUI keeps: a
    /// thing that cannot work is refused where it is asked for, not queued as a
    /// job that fails a second later.
    pub(crate) unusable: std::collections::BTreeMap<usize, String>,
    pub(crate) cursor: usize,
    pub(crate) scroll: usize,
    /// Why the last Enter was refused, if it was. Drawn in place of the hint.
    pub(crate) error: Option<String>,
}

impl WsList {
    /// One row per directory under `workspaces/`, and the rows `Enter` refuses.
    ///
    /// Read by [`bm_core::paths::workspaces`] — the same inventory
    /// `workspace list` prints — so the picker and the CLI can never disagree
    /// about what counts as a book. A directory that is not one is listed with
    /// its reason rather than hidden: the usual way to find one is to have made
    /// it by accident, and `Enter` refusing on the row is the whole point.
    pub(crate) fn read(root: &std::path::Path) -> Self {
        let mut rows = Vec::new();
        let mut unusable = std::collections::BTreeMap::new();
        for entry in bm_core::paths::workspaces(root) {
            let reason = match entry.config {
                bm_core::paths::WorkspaceConfig::Valid => None,
                bm_core::paths::WorkspaceConfig::Missing => {
                    Some("no settings.json — a directory, not a workspace".to_string())
                }
                bm_core::paths::WorkspaceConfig::Broken => {
                    Some("settings.json does not parse — fix or remove it".to_string())
                }
            };
            let note = match &reason {
                Some(r) => r.clone(),
                None => {
                    let where_it = if entry.active { "active · " } else { "" };
                    format!(
                        "{where_it}{} · {}",
                        plural(entry.chapters, "chapter"),
                        plural(entry.scripts, "script"),
                    )
                }
            };
            if let Some(r) = reason {
                unusable.insert(rows.len(), r);
            }
            rows.push(WsItem {
                label: entry.name.clone(),
                note,
                value: entry.name,
            });
        }
        WsList {
            rows,
            unusable,
            cursor: 0,
            scroll: 0,
            error: None,
        }
    }

    pub(crate) fn list(&self) -> &[WsItem] {
        &self.rows
    }

    /// The highlight, arrows or `j`/`k`. Shared with the guided list's own so
    /// both feel the same on the first keypress.
    pub(crate) fn move_cursor(&mut self, down: bool) {
        let len = self.rows.len();
        if len == 0 {
            self.cursor = 0;
            return;
        }
        self.cursor = if down {
            (self.cursor + 1).min(len - 1)
        } else {
            self.cursor.saturating_sub(1)
        };
    }
}

#[derive(Debug, Clone)]
pub(crate) enum Screen {
    Normal,
    Jobs {
        scroll: usize,
        previous: Box<Screen>,
    },
    Help {
        scroll: usize,
    },
    Text(TextPrompt),
    Pick(Picker),
    Cast(CastView),
    /// Every task, with the failures readable and re-queueable in place.
    Tasks(TasksView),
    /// One task in full: `detail`, attempts, assignee, lease.
    TaskDetail(TaskDetail),
    /// System overview: backend, config, voices, tasks — Enter launches.
    Run,
    /// The three sound-design pools, one tab each: add, edit, remove, retune.
    /// Removal is refused for anything the mix still reaches; see `sound.rs`.
    Sound(SoundView),
    /// What the EC2 account holds: one row per instance, `aws up`/`aws down`
    /// from here, and a mark on rows the registry has not linked.
    Cloud(CloudView),
    Confirm(Confirm),
    /// Machine detail, keyed by address so a refresh can never retarget it.
    Machine(String),
    /// One machine's work policy: which stages it may run, in priority order.
    Policy(PolicyView),
    /// The digest manager: every chapter, and a manual two-round digest for one.
    Digest(DigestView),
    /// The script inspection window: digested chapters, one open chapter's
    /// segments with their speakers, `s` to re-point one. `:script` opens it.
    Script(ScriptView),
    /// What the crawl settings actually are, this book's links, the crawlers
    /// on this machine, and the sites we know. Scroll only.
    Crawl {
        scroll: usize,
        /// Whether the full configuration is showing instead of the verdict.
        /// Default off: the verdict is what the screen is for, and the detail
        /// is what you press when the verdict is not enough.
        expanded: bool,
    },
    /// LLM providers: keys, endpoints, models, and which one digests.
    /// `L` opens it; every edit saves `.bm/llm.json` at once and the next
    /// task offer carries the active key+model, so there is no second sync.
    Llm(LlmView),
    /// The guided `workspace new`: name → profile → crawler, then create.
    WorkspaceNew(WorkspaceNew),
    /// `:ws` with nothing typed: the books, with the config each one carries,
    /// arrows to move and Enter to switch.
    WorkspaceList(WsList),
}

/// `1 chapter` but `0 chapters`: a row that said "0 chapter" would read as a
/// count nobody kept.
fn plural(n: usize, what: &str) -> String {
    if n == 1 {
        format!("1 {what}")
    } else {
        format!("{n} {what}s")
    }
}

/// Chapter numbers per row in the digest manager's grid.
///
/// It lives here, beside the view, because **two things depend on it and they
/// have to agree**: the painter lays the numbers out in rows of this width, and
/// the arrow keys navigate by it — ↑/↓ step a whole row. A key handler with its
/// own idea of the width would move the highlight somewhere the eye did not ask
/// for, which is exactly the class of bug that only shows up on screen.
pub(crate) const DIGEST_COLS: usize = 12;

/// The digest manager.
///
/// **One view, two modes, because they are one task**: the list is where a
/// chapter is picked, `open` is the chapter that was picked. Keeping both here
/// means Esc has exactly one meaning (step back), and the list does not have to
/// be rebuilt when a chapter is closed.
///
/// The manual digest exists because a model the operator already has open beats
/// a fallback that is rate limited — so the prompts leave by clipboard and the
/// answers come back the same way. What it is *not* is a second, looser digest:
/// the answers go through the same validators, and the result is reported over
/// the same `/api/complete` a worker uses.
#[derive(Debug, Clone)]
pub(crate) struct DigestView {
    /// Every chapter the library knows, ascending.
    pub(crate) chapters: Vec<u32>,
    pub(crate) cursor: usize,
    /// Hide chapters that already have a script. **Off** to begin with, so the
    /// first look shows the whole book rather than a filtered guess at intent.
    pub(crate) hide_done: bool,
    /// The chapter being digested, if one is open.
    pub(crate) open: Option<DigestChapter>,
}

/// One chapter's manual digest, in flight.
///
/// `cast` is round 1's validated answer, held because round 2's prompt is
/// rendered *against* it — the same hand-off the worker makes between its two
/// calls, except this one has to survive two keystrokes and however long the
/// operator spends in their model.
#[derive(Debug, Clone)]
pub(crate) struct DigestChapter {
    pub(crate) n: u32,
    /// Which round is waiting for a paste.
    pub(crate) round: bm_core::digest::Round,
    /// The prompt for `round` — already on the clipboard when it was built.
    pub(crate) prompt: String,
    pub(crate) cast: Option<serde_json::Value>,
    /// The part of the chapter this round is for, when a chapter is longer than
    /// one answer carries: `None` on every chapter that fits one call. Shown in
    /// the note, because an operator pasting into a long chapter has to know how
    /// many rounds it still owes.
    pub(crate) part: Option<bm_core::digest::ManualPart>,
    /// The last thing that happened: a copy, a validator's complaint, or the
    /// outcome. Drawn on the screen, because a complaint *is* the instruction.
    pub(crate) note: String,
    /// Round 2 landed and the report was accepted.
    pub(crate) done: bool,
}

impl DigestView {
    pub(crate) fn new(chapters: Vec<u32>) -> Self {
        DigestView {
            chapters,
            cursor: 0,
            hide_done: false,
            open: None,
        }
    }

    /// The rows actually drawn: the whole list, or the undigested ones.
    ///
    /// Filtering is a *view* concern, so the cursor indexes this rather than
    /// `chapters` — otherwise toggling the filter would leave the cursor
    /// pointing at a different chapter than the highlighted one.
    pub(crate) fn rows(&self, digested: &dyn Fn(u32) -> bool) -> Vec<u32> {
        self.chapters
            .iter()
            .copied()
            .filter(|n| !self.hide_done || !digested(*n))
            .collect()
    }

    /// The chapter under the cursor.
    pub(crate) fn selected(&self, digested: &dyn Fn(u32) -> bool) -> Option<u32> {
        self.rows(digested).get(self.cursor).copied()
    }
}

/// The per-machine work policy editor.
///
/// `prefs` is the whole list, most-preferred first. `cursor` walks it; when
/// `grabbed` is `Some`, the arrows *move* the row at that index instead of
/// stepping the cursor — the Space-to-pick-up gesture. Edits save as they are
/// made, so there is no "unsaved" state to lose on Esc.
#[derive(Debug, Clone)]
pub(crate) struct PolicyView {
    pub(crate) addr: String,
    pub(crate) label: String,
    pub(crate) cursor: usize,
    pub(crate) grabbed: Option<usize>,
    pub(crate) prefs: Vec<bm_proto::TaskPref>,
}

impl PolicyView {
    pub(crate) fn new(addr: String, label: String, prefs: Vec<bm_proto::TaskPref>) -> Self {
        PolicyView {
            addr,
            label,
            cursor: 0,
            grabbed: None,
            prefs,
        }
    }
}

/// The LLM setup screen: one row per provider, all empty until the operator
/// adds a key.
///
/// The rows are the provider ids in `.bm/llm.json` (the four known ones
/// first, then any custom gateway the operator added by hand). `cursor`
/// walks them; `f` lists the highlighted provider's models from its own API
/// and `picking` turns the cursor onto that list, where `Enter` saves the
/// model. `note` is the last thing the fetch said, drawn under the table.
#[derive(Debug, Clone)]
pub(crate) struct LlmView {
    pub(crate) cursor: usize,
    pub(crate) picking: bool,
    pub(crate) model_cursor: usize,
    pub(crate) note: String,
}

impl LlmView {
    pub(crate) fn new() -> Self {
        LlmView {
            cursor: 0,
            picking: false,
            model_cursor: 0,
            note: String::new(),
        }
    }

    /// Provider ids in display order: whatever `.bm/llm.json` (or the
    /// shipped `llm.default.json`) names, sorted. No compiled-in list — the
    /// file is the whole roster.
    pub(crate) fn ids(cfg: &bm_core::config::LlmConfig) -> Vec<String> {
        let mut ids: Vec<String> = cfg.providers.keys().cloned().collect();
        ids.sort();
        ids
    }
}
