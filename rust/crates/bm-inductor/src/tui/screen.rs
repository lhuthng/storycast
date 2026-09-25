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
    /// The address workers should dial (`:advertise`): save-only, like the ssh
    /// defaults. Empty clears it back to the routing-table guess.
    Advertise,
    /// Render batch size (`:batch`): how many of one chapter's takes a single
    /// offer carries. Save-only, like the ssh defaults — it is read by the
    /// scheduler when it builds the next offer, so nothing is dispatched.
    RenderBatch,
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
