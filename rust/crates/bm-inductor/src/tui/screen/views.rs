use super::*;

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
