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
    Sound(SoundView),
    /// What the EC2 account holds: one row per instance, `aws up`/`aws down`
    Cloud(CloudView),
    Confirm(Confirm),
    /// Machine detail, keyed by address so a refresh can never retarget it.
    Machine(String),
    /// One machine's work policy: which stages it may run, in priority order.
    Policy(PolicyView),
    /// The digest manager: every chapter, and a manual two-round digest for one.
    Digest(DigestView),
    /// The script inspection window: digested chapters, one open chapter's
    Script(ScriptView),
    /// What the crawl settings actually are, this book's links, the crawlers
    Crawl {
        scroll: usize,
        /// Whether the full configuration is showing instead of the verdict.
        expanded: bool,
    },
    /// LLM providers: keys, endpoints, models, and which one digests.
    Llm(LlmView),
    /// The guided `workspace new`: name → profile → crawler, then create.
    WorkspaceNew(WorkspaceNew),
    /// `:ws` with nothing typed: the books, with the config each one carries,
    WorkspaceList(WsList),
}

/// `1 chapter` but `0 chapters`: a row that said "0 chapter" would read as a
fn plural(n: usize, what: &str) -> String {
    if n == 1 {
        format!("1 {what}")
    } else {
        format!("{n} {what}s")
    }
}

/// Chapter numbers per row in the digest manager's grid.
pub(crate) const DIGEST_COLS: usize = 12;

/// The digest manager.
#[derive(Debug, Clone)]
pub(crate) struct DigestView {
    /// Every chapter the library knows, ascending.
    pub(crate) chapters: Vec<u32>,
    pub(crate) cursor: usize,
    /// Hide chapters that already have a script. **Off** to begin with, so the
    pub(crate) hide_done: bool,
    /// The chapter being digested, if one is open.
    pub(crate) open: Option<DigestChapter>,
}

/// One chapter's manual digest, in flight.
#[derive(Debug, Clone)]
pub(crate) struct DigestChapter {
    pub(crate) n: u32,
    /// Which round is waiting for a paste.
    pub(crate) round: bm_core::digest::Round,
    /// The prompt for `round` — already on the clipboard when it was built.
    pub(crate) prompt: String,
    pub(crate) cast: Option<serde_json::Value>,
    /// The part of the chapter this round is for, when a chapter is longer than
    pub(crate) part: Option<bm_core::digest::ManualPart>,
    /// The last thing that happened: a copy, a validator's complaint, or the
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
