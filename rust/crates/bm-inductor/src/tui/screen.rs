//! Screens: the modal states the key chain and the painter agree on.
use crate::tui::audition::AuditionLine;
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
    /// `:` command line: the buffer names a key (`m`) or a word
    /// (`reconcile`) and Enter presses it for you. Never dispatched —
    /// handled inline so one keypress can open another prompt.
    Command,
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
    Provision { addr: String, force: bool },
    DropMachine { addr: String },
    SwapVoice { character: String, voice: String },
    StopBackend,
    Reconcile,
    Rerender,
    /// Take one entry out of a sound-design pool. Carries the view it was
    /// asked from, so answering the dialog returns to the same tab and row
    /// instead of dumping the operator back on the dashboard.
    SoundRemove(SoundRemoval),
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
}

/// Read-only overview of the whole cast: who speaks with what, which voices
/// are shared, and which assignments the accent policy would reject.
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
}

impl CastView {
    pub(crate) fn new() -> Self {
        CastView {
            cursor: 0,
            scroll: 0,
            filter: String::new(),
            line: None,
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
}

impl TasksView {
    pub(crate) fn new() -> Self {
        TasksView {
            cursor: 0,
            scroll: 0,
            filter: String::new(),
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
    Confirm(Confirm),
    /// Machine detail, keyed by address so a refresh can never retarget it.
    Machine(String),
}
