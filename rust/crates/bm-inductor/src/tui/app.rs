//! The dashboard state: what the poller fills and every pane reads.
use crate::tui::EVENT_CAP;
use crate::tui::{
    audio::Player,
    input::dispatch,
    jobs::{fetch_state, BackgroundJob, DoneKind, Ev, Job},
    model::{beat_backed, live_beats},
    model::{
        cast_rows, parse_dispatch, parse_stats, registry_machines, CastRow, Dispatch, WorkerStats,
    },
    screen::Screen,
    sound::SoundData,
    style::{level_from_str, style_bold_of, style_of, Conn, Level, LogLine, Theme},
};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use bm_core::provision::AwsInstance;
use bm_proto::{Heartbeat, Machine, Op, Roster, Task};
use ratatui::{
    layout::Rect,
    style::{Color, Style},
};
use std::collections::{HashMap, VecDeque};
use std::sync::{atomic::AtomicBool, Arc};
use std::time::Instant;

/// A dashboard pane the mouse can focus. Keeping this separate from `Screen`
/// means a click never accidentally opens an operator action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Panel {
    Machines,
    Workers,
    Tasks,
    Events,
    Footer,
}

impl Panel {
    /// The next pane in the focus cycle, skipping the Workers pane when the
    /// Machines pane is showing the rack.
    ///
    /// The rack *is* the Workers pane in that mode — every box is drawn with the
    /// worker that is on it — so focusing a pane that is not on screen would put
    /// the bright border somewhere invisible and leave the operator pressing `f`
    /// to escape from nowhere.
    pub(crate) fn next_visible(self, rack: bool) -> Self {
        const ALL: [Panel; 4] = [
            Panel::Machines,
            Panel::Workers,
            Panel::Events,
            Panel::Footer,
        ];
        let panels: &[Panel] = if rack {
            &[Panel::Machines, Panel::Events, Panel::Footer]
        } else {
            &ALL
        };
        let i = panels.iter().position(|p| *p == self).unwrap_or(0);
        // Starting from an unknown panel (Workers, when the rack took it away)
        // lands on the first, which is the rack itself.
        if i == 0 && self != panels[0] {
            return panels[0];
        }
        panels[(i + 1) % panels.len()]
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Machines => "Machines",
            Self::Workers => "Workers",
            Self::Tasks => "Tasks",
            Self::Events => "Logs",
            Self::Footer => "Status",
        }
    }
}

/// What a click or wheel event hit. Regions are rebuilt by the painter, so
/// mouse handling never has to duplicate terminal layout arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HitTarget {
    Panel {
        panel: Panel,
        row_start: usize,
        row_y: u16,
    },
    List {
        kind: ListTarget,
        row_start: usize,
        row_y: u16,
    },
    Confirm {
        confirm: bool,
    },
    SoundTabs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ListTarget {
    Cast,
    Tasks,
    Sound,
    Picker,
    Digest,
    Cloud,
    Jobs,
    Policy,
    Help,
    Crawl,
    TaskDetail,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct HitRegion {
    pub(crate) area: Rect,
    pub(crate) target: HitTarget,
}

impl HitRegion {
    pub(crate) fn new(area: Rect, target: HitTarget) -> Self {
        Self { area, target }
    }
}

mod state;
pub(crate) struct App {
    pub(crate) api: String,
    /// The root this dashboard was started on, *with* the workspace the
    /// pointer named. Both halves are needed and they are not the same thing:
    /// machines, roster, profile and the asset pools hang off `root`, while
    /// the ledger, settings, `data/` and `output/` belong to the book in
    /// `work`. A bare root here read the wrong ledger the moment a workspace
    /// was selected.
    pub(crate) layout: bm_core::Layout,
    /// The profile the live `assets/` + `prompts/` tree claims to be, read from
    /// `.bm/profile` at startup and after every load. `None` means none is
    /// loaded — which is a state the dashboard must be able to show, because
    /// every runner refuses to start in it.
    ///
    /// Cached rather than read per frame: it is a file open, and the footer is
    /// redrawn on every keystroke.
    pub(crate) profile: Option<bm_core::profile::Binding>,
    /// Shared HTTP client for the inductor API.
    pub(crate) http: reqwest::Client,
    pub(crate) machines: Vec<Machine>,
    /// Draw the Machines pane as a hub-and-spoke picture instead of the table.
    ///
    /// A view preference, not state: it changes nothing about the cluster and is
    /// deliberately not persisted, so a session that opened the graph does not
    /// hand the next one a pane nobody asked for. `g` toggles it, and the table
    /// stays the complete list — the graph is the glance.
    pub(crate) machines_graph: bool,
    /// The Machines rack's window: which band of servers it starts at, and how
    /// many columns wide a band is.
    ///
    /// Both are published by the drawer, not set by the keys — the keys only
    /// move the cursor. The width is read back by `↑`/`↓`, which have to move a
    /// whole *row of the rack* and cannot know its width from where they run;
    /// the same bargain the log's PageUp makes with `events_rows`.
    pub(crate) graph_band: usize,
    pub(crate) graph_cols: usize,
    pub(crate) beats: Vec<Heartbeat>,
    pub(crate) tasks: Vec<Task>,
    pub(crate) counts: serde_json::Value,
    /// Stats pane data: per-worker per-stage completions plus per-stage
    /// task averages, for the matrix and the TUI-side ETA.
    pub(crate) stats: WorkerStats,
    /// Whether the cluster is distributing, and the range it stands at, from
    /// `/api/state`. `None` until the first poll, and for an inductor older
    /// than the gate — see [`crate::tui::model::parse_dispatch`].
    pub(crate) dispatch: Option<Dispatch>,
    pub(crate) settings: Option<serde_json::Value>,
    pub(crate) events: VecDeque<LogLine>,
    pub(crate) selected: usize,
    pub(crate) machine_scroll: usize,
    /// Scroll offset for the live worker list; the pane sizes itself around
    /// the number of visible workers rather than painting empty table rows.
    pub(crate) worker_scroll: usize,
    /// 0 = pinned to the newest event; N = N rows scrolled back.
    ///
    /// N is a *distance from the live tail*, not a frozen index: a new event
    /// while the view is scrolled back grows N by one so the line under the
    /// operator's eyes stays put (see `push_log`). It never exceeds
    /// [`EVENT_CAP`] — the buffer keeps that many lines and no more, so the
    /// top is a real edge, named as such in the pane title.
    pub(crate) events_scroll: usize,
    /// The Logs pane's content height in rows, published by the draw and read
    /// by the page keys. PgUp/PgDn move this many rows, so one press really is
    /// one screenful: walking back from the top costs the same presses as
    /// walking out, and `G` (or one run of PgDn) still snaps to newest. The
    /// hardcoded 5 these replaced made a 500-line buffer a hundred presses each
    /// way — the reason "scrolled back" felt like a one-way trip.
    pub(crate) events_rows: usize,
    pub(crate) screen: Screen,
    pub(crate) roster: Option<Roster>,
    pub(crate) roster_loading: bool,
    pub(crate) roster_error: Option<String>,
    /// Models the last `f` fetch listed, and which provider they are for.
    /// Read by the `L` screen's picker; a fetch for another provider is
    /// shown as its note instead of offered as a pick.
    pub(crate) llm_models: Vec<String>,
    pub(crate) llm_models_for: String,
    /// Cursor to restore on the `L` screen after a `k`/`u`/`m` prompt closes.
    pub(crate) llm_cursor: usize,
    /// Jobs in flight, for the "working…" indicator and duplicate suppression.
    /// One key per op *instance* (see `op_key`), so retrying chapter 3 does not
    /// block retrying chapter 4 — but pressing the same key twice does.
    pub(crate) pending: usize,
    pub(crate) next_job_id: u64,
    pub(crate) background_jobs: Vec<BackgroundJob>,
    pub(crate) inflight: Vec<String>,
    /// Highest scheduler event id already folded into `events`. `None` until the
    /// first snapshot arrives, so the inductor's own history is shown once on
    /// startup and never duplicated afterwards.
    pub(crate) last_event_id: Option<u64>,
    /// A chapter range to enqueue once the inductor answers. Set when `B`
    /// starts a backend: the backend boots in the background, and the job
    /// follows on the first live refresh — so one keypress runs chapters,
    /// not just processes.
    pub(crate) pending_enqueue: Option<(u32, u32)>,
    /// A backend start sequence is in flight: refuses a second `B`/`R` start.
    ///
    /// Spans the whole sequence, not just the backend boot — the start job ends
    /// in seconds and hands its boxes to the scheduler, so the flag is released
    /// by the last catch-up provision finishing. See `catchup_jobs`.
    pub(crate) backend_start_outstanding: bool,
    /// Cancel flag for the catch-up provisions a `B` start handed out, set
    /// synchronously by `X`.
    ///
    /// `X` no longer waits for the provisions to drain before sweeping: the
    /// stop holds the cluster resource, a provision holds one box, and the two
    /// run together. Each provision reads this flag before it launches its
    /// worker, so a box being pushed when `X` lands finishes its push and then
    /// stays quiet — which is what "stopped" has to mean.
    pub(crate) start_cancel: Option<Arc<AtomicBool>>,
    /// The boxes a `B` start left to catch up, and the flag that stops them.
    ///
    /// Carried on the `App` rather than dispatched from the job because only
    /// the dashboard can allocate a job id: `dispatch` is what puts a row on
    /// the jobs screen, and a job spawning jobs behind its back would produce
    /// rows nothing could match an id to.
    pub(crate) pending_catchup: Option<(Vec<bm_proto::Machine>, Arc<AtomicBool>)>,
    /// Boxes a launch orphaned that the account has just given an address to,
    /// and that nobody has onboarded yet.
    ///
    /// Filled by every state poll from the marker `relink` writes, and drained
    /// by the dashboard loop into one provision job each — the same shape as
    /// `pending_catchup`, and for the same reason: only the dashboard can
    /// allocate a job id.
    pub(crate) pending_onboard: Vec<bm_proto::Machine>,
    /// Addresses an onboard job has already been handed out for.
    ///
    /// The one piece of bookkeeping here, and it covers a single gap: the poll
    /// runs every ~800 ms while a queued provision takes longer than that to
    /// reach the box, so without this the marker would still be in the note on
    /// the next two polls and the same box would be queued several times. An
    /// entry is dropped as soon as the marker goes away — which is the moment
    /// the job takes the box and rewrites its note.
    pub(crate) onboarded: std::collections::HashSet<String>,
    /// Ids of the catch-up provisions a `B` handed out that are still running.
    ///
    /// `backend_start_outstanding` spans these, not just the backend boot. The
    /// start job now ends in seconds while the boxes it handed out keep going,
    /// so without this a second `B` a moment later would be allowed and would
    /// queue a duplicate push at every box — which is the queueing this whole
    /// change exists to remove. The flag is released by the last one finishing.
    pub(crate) catchup_jobs: Vec<u64>,
    /// Screen a `:` command returns to after it runs: commands fire in the
    /// context they were typed in, so `:F` in the task list retries the
    /// highlighted row instead of losing it.
    pub(crate) command_return: Option<Screen>,
    /// The screens `Esc` steps back through, outermost first: one entry per
    /// **layer** currently open, each holding the screen that layer sits on.
    ///
    /// A *layer* is a screen drawn over another one — a dialog, a `:` line, the
    /// voice picker. A *place* is a screen you are standing in — the dashboard,
    /// the ledger, the cast table — and `Esc` closes those rather than stepping
    /// out of them, because the place under them is the dashboard and the
    /// dashboard is the floor.
    ///
    /// The two are pushed and popped by one rule, in `input::note_layer`, so a
    /// dialog raised over a picker raised over the cast table unwinds one press
    /// at a time — `Cast → picker → swap dialog → Esc → picker → Esc → Cast →
    /// Esc → dashboard` — instead of dropping the operator three screens at
    /// once, which is what every layer doing `Esc → Normal` used to do.
    pub(crate) back: Vec<Screen>,
    /// Set by the `:` line as it hands control to the screen its command runs
    /// in. The prompt is spent, not stacked: a dialog the command raises
    /// belongs over the screen the command ran in, not over a prompt that is no
    /// longer on screen.
    pub(crate) prompt_spent: Option<Screen>,
    /// The active palette. Replaces the old `colour: bool`: mono is now one
    /// of three themes, and `C` cycles all of them. `colour()` is the boolean
    /// the panes already read — false exactly when the theme is `Mono` — so
    /// "no bold/fg for terminals that cannot show it" keeps working.
    pub(crate) theme: Theme,
    /// Whether the terminal is reporting mouse events to us.
    ///
    /// **This is the reason an error message could not be copied out of the
    /// dashboard.** While mouse reporting is on, the terminal hands every drag
    /// to the program instead of treating it as a selection, so there is no way
    /// to highlight a stack trace and copy it — the single most useful thing to
    /// do with an error. `m` turns reporting off, selection works normally,
    /// and `m` again brings the click-to-select panes back.
    pub(crate) mouse_capture: bool,
    /// Set by the key handler, consumed by the event loop, which is the only
    /// place holding the terminal. The handler cannot talk to the terminal
    /// directly, so it leaves the intent here and the loop carries it out.
    pub(crate) mouse_toggle: bool,
    pub(crate) status: LogLine,
    pub(crate) conn: Conn,
    pub(crate) tick: u64,
    pub(crate) refreshed: Option<Instant>,
    /// Voice whose audition render is in flight, if any.
    ///
    /// On the `App`, not on the screen: an audition is one render against one
    /// sidecar, so "one at a time" is a property of the process, not of whichever
    /// screen asked. The picker and the cast overview both read this.
    pub(crate) audition: Option<String>,
    /// Sentences locked by Enter-pick, per character. Picking a voice locks
    /// the speech it was picked on, so reopening the picker for them resumes
    /// on that sentence instead of another random pick.
    pub(crate) locked_lines: HashMap<String, crate::tui::audition::AuditionLine>,
    /// `speaker -> their lines`, from `data/script-*.json`. Built once per
    /// session by a background job and never rebuilt: a full scan is seconds.
    pub(crate) lines: Option<std::collections::HashMap<String, Vec<String>>>,
    pub(crate) lines_loading: bool,
    /// The three sound-design pools, the scene map and what each entry is
    /// still used for. Built when the editor opens and rebuilt after every
    /// save, so the screen and the registry on disk cannot disagree about
    /// whether an entry is removable.
    pub(crate) sound: Option<SoundData>,
    pub(crate) sound_loading: bool,
    pub(crate) sound_error: Option<String>,
    /// The speaker on this desk. Owns the audio process, not the audio.
    pub(crate) player: Player,
    /// What the EC2 account holds, from the last `describe-instances`. Empty
    /// until `:pool` runs; `cloud_error` is set instead when the read failed, so
    /// the Cloud view renders the reason rather than an empty account.
    pub(crate) cloud: Vec<AwsInstance>,
    pub(crate) cloud_error: Option<String>,
    /// The Logs pane filter, stepped with `←/→`. TUI-local: polls never reset it.
    pub(crate) log_filter: crate::tui::model::LogFilter,
    /// The pane highlighted by keyboard focus or a mouse click.
    pub(crate) focused_panel: Panel,
    /// Rebuilt on every frame; mouse hit testing is therefore resize-safe.
    pub(crate) hit_regions: Vec<HitRegion>,
    /// Coordinates and time of the last left click, used only for activation
    /// (Enter-equivalent) on a double click.
    pub(crate) last_click: Option<(u16, u16, Instant)>,
}

impl App {
    pub(crate) fn new(api: &str) -> Self {
        App {
            api: api.trim_end_matches('/').to_string(),
            layout: bm_core::Layout::new(""),
            profile: None,
            http: reqwest::Client::new(),
            machines: Vec::new(),
            beats: Vec::new(),
            tasks: Vec::new(),
            counts: serde_json::Value::Null,
            stats: WorkerStats::default(),
            dispatch: None,
            settings: None,
            events: VecDeque::with_capacity(EVENT_CAP),
            selected: 0,
            machine_scroll: 0,
            worker_scroll: 0,
            events_scroll: 0,
            events_rows: 10,
            log_filter: crate::tui::model::LogFilter::All,
            screen: Screen::Normal,
            roster: None,
            roster_loading: false,
            roster_error: None,
            llm_models: Vec::new(),
            llm_models_for: String::new(),
            llm_cursor: 0,
            pending: 0,
            next_job_id: 0,
            background_jobs: Vec::new(),
            inflight: Vec::new(),
            last_event_id: None,
            pending_enqueue: None,
            backend_start_outstanding: false,
            start_cancel: None,
            machines_graph: false,
            graph_band: 0,
            graph_cols: 1,
            pending_catchup: None,
            pending_onboard: Vec::new(),
            onboarded: std::collections::HashSet::new(),
            catchup_jobs: Vec::new(),
            command_return: None,
            back: Vec::new(),
            prompt_spent: None,
            theme: Theme::default(),
            mouse_capture: true,
            mouse_toggle: false,
            status: LogLine {
                level: Level::Info,
                wall: bm_proto::now_secs(),
                text: "press ? for help".into(),
            },
            conn: Conn::Unknown,
            tick: 0,
            refreshed: None,
            audition: None,
            locked_lines: HashMap::new(),
            lines: None,
            lines_loading: false,
            sound: None,
            sound_loading: false,
            sound_error: None,
            player: Player::new(),
            cloud: Vec::new(),
            cloud_error: None,
            focused_panel: Panel::Machines,
            hit_regions: Vec::new(),
            last_click: None,
        }
    }

    pub(crate) fn clear_hit_regions(&mut self) {
        self.hit_regions.clear();
    }

    pub(crate) fn add_hit_region(&mut self, area: Rect, target: HitTarget) {
        self.hit_regions.push(HitRegion::new(area, target));
    }

    pub(crate) fn hit_region(&self, x: u16, y: u16) -> Option<HitRegion> {
        self.hit_regions
            .iter()
            .rev()
            .find(|r| {
                x >= r.area.x
                    && x < r.area.x.saturating_add(r.area.width)
                    && y >= r.area.y
                    && y < r.area.y.saturating_add(r.area.height)
            })
            .copied()
    }

    /// Build the audition line index if it is not already here or on its way.
    ///
    /// Called when a screen that can audition opens, so the hundred file opens
    /// happen while the operator is still reading the table rather than after they
    /// press the key. Idempotent: a second call while it is loading does nothing.
    pub(crate) fn ensure_lines(&mut self, job_tx: &tokio::sync::mpsc::UnboundedSender<Job>) {
        if self.lines.is_some() || self.lines_loading {
            return;
        }
        self.lines_loading = true;
        dispatch(
            self,
            job_tx,
            Job::LoadLines {
                layout: self.layout.clone(),
            },
        );
    }

    /// Load the sound-design pools unless they are already here or on their way.
    ///
    /// Called when the editor opens. Unlike the audition line index this is not
    /// once per session: every save changes what is on disk, and the removal
    /// guard is read off this data — a stale copy would offer a remove key for
    /// an entry that has just been referenced, which is the one thing the guard
    /// exists to prevent.
    pub(crate) fn load_sound(&mut self, job_tx: &tokio::sync::mpsc::UnboundedSender<Job>) {
        if self.sound_loading {
            self.set_status(Level::Warn, "pool reload already running — watch events");
            return;
        }
        self.sound_loading = true;
        self.sound_error = None;
        dispatch(
            self,
            job_tx,
            Job::LoadSounds {
                layout: self.layout.clone(),
            },
        );
    }

    /// Finish an audition: play what came back, and always leave a status.
    ///
    /// Every path here sets a status, including the ones that play nothing. The
    /// "auditioning…" line is written when the op is *dispatched* and this is
    /// the only thing that ever replaces it — so a path that returns without
    /// touching the bar leaves the operator watching a render that finished a
    /// minute ago, which is the one state the bar can never leave by itself.
    fn play_audition(&mut self, ok: bool, audio_b64: Option<String>) {
        if !ok {
            // The op's own failure text is already in the event pane; the bar's
            // job here is to stop claiming the audition is still in flight.
            self.set_status(Level::Error, "audition failed — see the events pane");
            return;
        }
        let Some(b64) = audio_b64 else {
            // What an *older inductor* answers with: it has no audio field at
            // all. The TUI cannot tell that from a render that produced nothing,
            // so it says both — and the fix is a restart, not a retry.
            self.set_status(
                Level::Warn,
                "the inductor sent no audio — restart it (older build?)",
            );
            return;
        };
        let wav = match B64.decode(b64.as_bytes()) {
            Ok(w) => w,
            Err(e) => {
                self.set_status(
                    Level::Error,
                    format!("undecodable audio from the inductor: {e}"),
                );
                return;
            }
        };
        let kb = wav.len() / 1024;
        match self.player.play_bytes(&wav) {
            Ok(()) => self.set_status(Level::Info, format!("playing {kb} KB")),
            // The render worked and the speaker did not. Naming that split is
            // the whole message.
            Err(e) => self.set_status(Level::Error, e),
        }
    }

    pub(crate) fn push_log(&mut self, mut line: LogLine) {
        // Absolute paths are the machine's, not the pane's: shorten them to
        // this checkout's own terms before the line is stored, so a worker's
        // `/Volumes/…/engines/pocket/models` reads as `./engines/pocket/models`.
        // The message still says exactly what failed — only the prefix it was
        // wrapped in changes.
        line.text = shorten_paths(&self.layout.root, &line.text);
        // A new line must not shove the operator's reading position away. While
        // the view is scrolled back, the same line stays on screen and the
        // distance grows by one — so "N back" is always the honest distance
        // from the live tail, and the cap on the distance is the buffer's own
        // depth (the oldest line kept), not a number the keys invented.
        let held = self.events_scroll > 0;
        while self.events.len() >= EVENT_CAP {
            self.events.pop_front();
        }
        self.events.push_back(line);
        if held {
            // Eviction from the front doesn't move the held line's distance
            // from the tail; only the new arrival does. If the held line was
            // itself evicted, the clamp lands us at the top, which is the only
            // honest answer.
            self.events_scroll = (self.events_scroll + 1).min(self.events.len());
        }
    }

    /// The farthest the Logs pane may scroll: the whole buffer. The buffer is
    /// capped at [`EVENT_CAP`], but the scroll writes used to be unbounded —
    /// PgUp past the cap left the title claiming "852 line(s) back" against a
    /// 500-line buffer (and a keyboard repeat would walk it into the
    /// thousands). Every scroll write clamps through this, so the number in
    /// the title always names lines that exist.
    pub(crate) fn max_events_scroll(&self) -> usize {
        self.events.len()
    }

    /// Raise `events_scroll` (scroll toward older lines), clamped to the
    /// buffer. The one doorway every "go older" write goes through.
    pub(crate) fn scroll_events_older(&mut self, by: usize) {
        self.events_scroll = (self.events_scroll + by).min(self.max_events_scroll());
    }

    /// Lower `events_scroll` (scroll toward newer lines), clamped at 0.
    /// The one doorway every "go newer" write goes through.
    pub(crate) fn scroll_events_newer(&mut self, by: usize) {
        self.events_scroll = self.events_scroll.saturating_sub(by);
    }

    pub(crate) fn log_at(&mut self, level: Level, text: impl Into<String>) {
        self.push_log(LogLine {
            level,
            wall: bm_proto::now_secs(),
            text: text.into(),
        });
    }

    /// The worker rows the Workers pane will actually draw, and nothing else.
    ///
    /// **The layout sizes this pane from this number**, so it has to be the
    /// same set the renderer draws — stale beats and ghost rows included in the
    /// count would reserve rows for rows that never appear, which is exactly
    /// the too-tall pane this replaced. One definition, used by both, is the
    /// only way that stays true when either filter changes.
    pub(crate) fn live_workers(&self) -> Vec<&bm_proto::Heartbeat> {
        let now = bm_proto::now_secs();
        live_beats(&self.beats, now)
            .into_iter()
            .filter(|b| beat_backed(&self.machines, b))
            .collect()
    }

    /// The same set as identities: the worker ids the *ledger* may treat as
    /// alive.
    ///
    /// Two shapes because two questions are asked of the same two predicates.
    /// The pane draws rows and needs the beats themselves; the ledger's
    /// `abandoned` facet and its release keys only need to know **who** is
    /// answering, and a set answers that in one lookup per holder — which is
    /// what a filter over five thousand rows wants. Both read `live_beats` and
    /// `beat_backed`, so the pane and the facet cannot come to disagree about
    /// whether a box is gone.
    pub(crate) fn live_worker_ids(&self) -> std::collections::BTreeSet<String> {
        crate::tui::model::live_worker_ids(&self.beats, &self.machines, bm_proto::now_secs())
    }

    /// Step back one layer: the screen under this one, or the dashboard.
    ///
    /// The dashboard is the floor, so an `Esc` that runs off the bottom of the
    /// stack closes rather than doing nothing — which is what the screens that
    /// are only ever opened from the dashboard did before the stack existed,
    /// and what they must keep doing.
    ///
    /// `command_return` is the fallback for a prompt opened outside a key
    /// handler (a test, or anything that sets a screen directly): it is the
    /// same value the stack would have held, recorded by the opener itself.
    pub(crate) fn back_out(&mut self) -> Screen {
        let target = self.back.pop().or_else(|| self.command_return.take());
        self.command_return = None;
        target.unwrap_or(Screen::Normal)
    }

    pub(crate) fn set_status(&mut self, level: Level, text: impl Into<String>) {
        self.status = LogLine {
            level,
            wall: bm_proto::now_secs(),
            text: text.into(),
        };
    }

    /// Colour-aware style. Mono (the theme, not a flag) drops the hue but
    /// keeps the bold; the state word is always rendered too, so nothing
    /// depends on colour alone.
    pub(crate) fn colour(&self) -> bool {
        self.theme != Theme::Mono
    }

    pub(crate) fn style(&self, c: Color) -> Style {
        style_of(self.colour(), c)
    }

    pub(crate) fn style_bold(&self, c: Color) -> Style {
        style_bold_of(self.colour(), c)
    }
}

/// Rewrite the absolute prefixes a log line carries so the Events pane reads in
/// this checkout's own terms.
///
/// Worker and backend messages name files absolutely
/// (`/Volumes/…/engines/pocket/models`). The dashboard knows exactly one root,
/// so it becomes `.`, and the home directory becomes `~` — the venv, the model
/// caches and the HF token all live there. A path under neither is another
/// machine's fact and is left as it was written.
fn shorten_paths(root: &std::path::Path, text: &str) -> String {
    let mut out = text.to_string();
    let root = root.to_string_lossy();
    // A one-character root (`/`) would rewrite every path in the line to `.`;
    // refuse it rather than mangle the message.
    if root.len() > 1 {
        out = out.replace(root.as_ref(), ".");
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = home.to_string_lossy();
        if home.len() > 1 {
            out = out.replace(home.as_ref(), "~");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::shorten_paths;
    use std::path::Path;

    #[test]
    fn only_this_root_and_home_are_shortened() {
        assert_eq!(
            shorten_paths(Path::new("/repo"), "Error: /repo/engines/pocket/models"),
            "Error: ./engines/pocket/models"
        );
        // A path under neither is another machine's and is left alone. (The
        // test avoids `$HOME` on purpose: rewriting it is the same branch and
        // asserting on an env-derived string is not worth the flake.)
        assert_eq!(
            shorten_paths(Path::new("/repo"), "/elsewhere/x"),
            "/elsewhere/x"
        );
        // A bare `/` root must not turn every path into `.`.
        assert_eq!(shorten_paths(Path::new("/"), "/a/b"), "/a/b");
    }
}
