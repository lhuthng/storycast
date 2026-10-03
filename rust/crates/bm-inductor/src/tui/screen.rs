//! Screens: the modal states the key chain and the painter agree on.
use crate::tui::app::App;
use crate::tui::audition::AuditionLine;
use crate::tui::model::Facet;
use crate::tui::sound::SoundView;
use bm_proto::Stage;

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

pub(crate) mod prompt;
pub(crate) mod views;

pub(crate) use prompt::*;
pub(crate) use views::*;
