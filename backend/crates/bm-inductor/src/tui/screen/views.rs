use super::*;

/// Read-only overview of the whole cast: who speaks with what, which voices
#[derive(Debug, Clone)]
pub(crate) struct CastView {
    pub(crate) cursor: usize,
    pub(crate) scroll: usize,
    pub(crate) filter: String,
    /// The real line the highlighted speaker is auditioned on, held so pressing
    pub(crate) line: Option<AuditionLine>,
    /// Filter focus: every letter types (t/T included) and the audition
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
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ScriptSegment {
    pub(crate) n: usize,
    pub(crate) speaker: String,
    pub(crate) text: String,
}

/// The script inspection window: the digested chapters as a list, one open
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
    pub(crate) pick: Option<ScriptPick>,
    /// Filter text over the chapter list.
    pub(crate) filter: String,
    /// The excerpt panel (`e` at the segment depth) is up over the segments.
    pub(crate) excerpt_open: bool,
    /// The open chapter's own excerpt — the state its end leaves for the
    pub(crate) excerpt_own: String,
    /// The chain this chapter was fed: the previous chapters' excerpts, newest
    pub(crate) excerpt_fed: Vec<(u32, String)>,
    /// Scroll offset into the excerpt panel (rows). Clamped in the draw, where
    pub(crate) excerpt_scroll: usize,
}

/// A speaker being picked for one segment.
#[derive(Debug, Clone)]
pub(crate) struct ScriptPick {
    /// The segment the pick is for (1-based). Carried so the header can
    pub(crate) segment: usize,
    /// Who the segment speaks as now — `Enter` on them is a no-op, and
    pub(crate) expect: String,
    pub(crate) filter: String,
    pub(crate) cursor: usize,
    pub(crate) scroll: usize,
    /// The open chapter's roster, read when the pick was opened. The
    pub(crate) roster_cache: Vec<String>,
}

impl ScriptPick {
    /// Visible at once. Five, like the model list on the LLM screen: a
    pub(crate) const SHOW: usize = 5;
}

impl ScriptView {
    /// Chapters per row in the chapter list.
    pub(crate) const PER_ROW: usize = 12;

    /// Open on the chapters that have scripts — `Layout::script_chapters`,
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
    pub(crate) fn close_excerpts(&mut self) {
        self.excerpt_open = false;
        self.excerpt_own.clear();
        self.excerpt_fed.clear();
        self.excerpt_scroll = 0;
    }

    /// The chapter rows actually drawn: the filter applies here, and the
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
        let mut others: Vec<String> = crate::tui::screen::known_speakers(app);
        others.sort_by(|a, b| fold(a).cmp(&fold(b)).then_with(|| a.cmp(b)));
        for name in others {
            push(name, &mut out);
        }
        out
    }
}

/// Every speaker the TUI knows about, folded into one sorted list: the
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
/// The dashboard's Tasks pane is a roll-up — counts per stage. It answers "is
/// anything wrong" but never "which chapter, and why". This screen answers the
#[derive(Debug, Clone)]
pub(crate) struct TasksView {
    pub(crate) cursor: usize,
    pub(crate) scroll: usize,
    pub(crate) filter: String,
    /// Which kind the ledger is narrowed to. See [`Facet`](crate::tui::model::Facet):
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
#[derive(Debug, Clone)]
pub(crate) struct WsItem {
    pub(crate) label: String,
    pub(crate) note: String,
    /// A preset id, a crawler kind, or a known site's host.
    pub(crate) value: String,
}

/// The guided `workspace new`: name, then profile, then how chapters arrive.
#[derive(Debug, Clone)]
pub(crate) struct WorkspaceNew {
    pub(crate) step: WsStep,
    pub(crate) name: String,
    pub(crate) name_cursor: usize,
    /// Every preset, `(id, label)`, read when the screen opens.
    pub(crate) profiles: Vec<WsItem>,
    pub(crate) profile: Option<usize>,
    /// Built once the profile is chosen: none / local file / known sites /
    pub(crate) crawlers: Vec<WsItem>,
    /// The custom-site URL template, typed on the [`WsStep::CustomUrl`] step.
    pub(crate) url: String,
    pub(crate) url_cursor: usize,
    /// The EPUB path, typed on the [`WsStep::Epub`] step — a `.epub` copied
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
#[derive(Debug, Clone)]
pub(crate) struct WsList {
    pub(crate) rows: Vec<WsItem>,
    /// The rows that are directories but not workspaces, by index, each with
    pub(crate) unusable: std::collections::BTreeMap<usize, String>,
    pub(crate) cursor: usize,
    pub(crate) scroll: usize,
    /// Why the last Enter was refused, if it was. Drawn in place of the hint.
    pub(crate) error: Option<String>,
}

impl WsList {
    /// One row per directory under `workspaces/`, and the rows `Enter` refuses.
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
