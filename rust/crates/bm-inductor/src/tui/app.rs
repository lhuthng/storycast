//! The dashboard state: what the poller fills and every pane reads.
use crate::tui::EVENT_CAP;
use crate::tui::{
    audio::Player,
    input::dispatch,
    jobs::{fetch_state, BackgroundJob, DoneKind, Ev, Job},
    model::{cast_rows, parse_stats, registry_machines, CastRow, WorkerStats},
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
    pub(crate) fn next(self) -> Self {
        const PANELS: [Panel; 4] = [
            Panel::Machines,
            Panel::Workers,
            Panel::Events,
            Panel::Footer,
        ];
        let i = PANELS.iter().position(|p| *p == self).unwrap_or(0);
        PANELS[(i + 1) % PANELS.len()]
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
    pub(crate) profile: Option<bm_core::profile::Pointer>,
    /// Shared HTTP client for the inductor API.
    pub(crate) http: reqwest::Client,
    pub(crate) machines: Vec<Machine>,
    pub(crate) beats: Vec<Heartbeat>,
    pub(crate) tasks: Vec<Task>,
    pub(crate) counts: serde_json::Value,
    /// Stats pane data: per-worker per-stage completions plus per-stage
    /// task averages, for the matrix and the TUI-side ETA.
    pub(crate) stats: WorkerStats,
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
    /// The active palette. Replaces the old `colour: bool`: mono is now one
    /// of three themes, and `C` cycles all of them. `colour()` is the boolean
    /// the panes already read — false exactly when the theme is `Mono` — so
    /// "no bold/fg for terminals that cannot show it" keeps working.
    pub(crate) theme: Theme,
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
            settings: None,
            events: VecDeque::with_capacity(EVENT_CAP),
            selected: 0,
            machine_scroll: 0,
            worker_scroll: 0,
            events_scroll: 0,
            events_rows: 10,
            screen: Screen::Normal,
            roster: None,
            roster_loading: false,
            roster_error: None,
            pending: 0,
            next_job_id: 0,
            background_jobs: Vec::new(),
            inflight: Vec::new(),
            last_event_id: None,
            pending_enqueue: None,
            backend_start_outstanding: false,
            start_cancel: None,
            pending_catchup: None,
            catchup_jobs: Vec::new(),
            command_return: None,
            theme: Theme::default(),
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

    pub(crate) fn push_log(&mut self, line: LogLine) {
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

    /// The settings actually in force: the live ones while the inductor
    /// answers, else this workspace's own file, else the compiled defaults.
    ///
    /// **One precedence for the whole dashboard**, because the alternative is
    /// what actually happened: `setting_u32` read the live payload only, so on a
    /// cold start a prompt showed the compiled `10` over a workspace file that
    /// said `6` — and a compiled-in default has to read differently from a
    /// number somebody chose. `run_preview` had the right precedence and the
    /// accessors did not, which is the "same thing configured in six places"
    /// defect in miniature: two answers to one question.
    ///
    /// A `Value` rather than a typed `Settings` because the key-based accessors
    /// read arbitrary keys, and a struct cannot answer for a key it does not
    /// have. The cost is a file read when the backend is down — which is what
    /// the run screen already did on every frame, so this adds no new work to
    /// the draw loop.
    pub(crate) fn effective_settings(&self) -> serde_json::Value {
        if let Some(live) = &self.settings {
            return live.clone();
        }
        if !self.layout.root.as_os_str().is_empty() {
            // A missing *or* malformed file both land on the defaults, the same
            // rule `Settings::load` applies.
            if let Ok(v) = bm_core::read_json::<serde_json::Value>(&self.layout.settings()) {
                return v;
            }
        }
        serde_json::to_value(bm_core::config::Settings::default()).unwrap_or_default()
    }

    pub(crate) fn setting_u32(&self, key: &str, default: u32) -> u32 {
        self.effective_settings()
            .get(key)
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
            .unwrap_or(default)
    }

    pub(crate) fn setting_f64(&self, key: &str, default: f64) -> f64 {
        self.effective_settings()
            .get(key)
            .and_then(|v| v.as_f64())
            .unwrap_or(default)
    }

    pub(crate) fn setting_str(&self, key: &str, default: &str) -> String {
        self.effective_settings()
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or(default)
            .to_string()
    }

    /// App-wide ssh defaults from [`App::effective_settings`] (see
    /// `SshDefaults`). Deserializing the `ssh` subtree keeps one source for the
    /// defaults — a missing or partial subtree parses as defaults, like the
    /// file itself.
    ///
    /// This is read by `:add`'s prefill **and** by `Job::StartBackend`'s
    /// `settings_key`, so a cold start now binds a machine with the key the file
    /// names instead of silently passing none and letting ssh decide.
    pub(crate) fn ssh_defaults(&self) -> bm_core::config::SshDefaults {
        self.effective_settings()
            .get("ssh")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default()
    }

    pub(crate) fn selected_machine(&self) -> Option<Machine> {
        self.machines.get(self.selected).cloned()
    }

    /// Machines to act on for B/R/X: the live registry when the inductor
    /// answers, the on-disk registry when it doesn't. A fresh TUI against a
    /// dead inductor has an empty list — defaulting to local-only there is how
    /// B silently drops remote boxes, so the files (machines.json config +
    /// ledger runtime, no liveness needed) stand in instead.
    pub(crate) fn effective_machines(&self) -> Vec<Machine> {
        if !self.machines.is_empty() {
            return self.machines.clone();
        }
        registry_machines(&self.layout)
    }

    /// Any screen other than the dashboard. Used by the size guard to say when
    /// a dialog is still open, and by the "is anything pending" checks.
    pub(crate) fn dialog_open(&self) -> bool {
        !matches!(self.screen, Screen::Normal)
    }

    /// The cast overview's rows, or an empty list before the roster arrives.
    pub(crate) fn cast_rows(&self) -> Vec<CastRow> {
        self.roster.as_ref().map(cast_rows).unwrap_or_default()
    }

    /// Kick off a roster fetch unless one is already in flight.
    pub(crate) fn load_roster(
        &mut self,
        job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
        http: &reqwest::Client,
    ) {
        // Singleton: every R press queues a ~35s job behind the serial worker,
        // so spamming it wedges the picker for minutes instead of hurrying it.
        if self.roster_loading {
            self.set_status(Level::Warn, "roster reload already running — watch events");
            return;
        }
        self.roster_loading = true;
        self.roster_error = None;
        dispatch(
            self,
            job_tx,
            Job::LoadRoster {
                api: self.api.clone(),
                http: http.clone(),
                layout: self.layout.clone(),
            },
        );
    }

    /// Re-read everything that depends on *which* workspace or profile is
    /// active, after a `:workspace` switch or a `:profile` load lands.
    ///
    /// The layout, the profile pointer and every cached file index belong to
    /// the old one until this runs — and none of it is visible from here: the
    /// switch happened in a background job, on disk. Dropping the caches is
    /// what makes the next read go to the new tree instead of showing the old
    /// book's lines and pools under the new book's name.
    pub(crate) fn relayout(
        &mut self,
        job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
        http: &reqwest::Client,
    ) {
        if self.layout.root.as_os_str().is_empty() {
            return;
        }
        let (layout, problem) = bm_core::Layout::resolve_or_root(&self.layout.root);
        self.layout = layout;
        self.profile = bm_core::profile::read_pointer(&self.layout.root).ok();
        // Cached file indexes: all of them belong to the workspace that was.
        self.lines = None;
        self.lines_loading = false;
        self.sound = None;
        self.sound_loading = false;
        self.roster = None;
        self.roster_loading = false;
        self.locked_lines.clear();
        // The polled view is per-workspace too, and a switch happens with the
        // inductor *down* — so nothing will refresh it. Left alone, the footer
        // would keep reporting the previous book's engine and chapter range and
        // the Tasks pane its chapters: the exact lie this whole change is
        // about. Blank is honest; the next `:B` fills them.
        self.tasks.clear();
        self.counts = serde_json::Value::Null;
        self.settings = None;
        match &problem {
            Some(e) => self.log_at(Level::Warn, format!("workspace pointer: {e}")),
            None => self.log_at(
                Level::Info,
                format!(
                    "now on workspace {} · profile {}",
                    crate::tui::model::workspace_label(&self.layout),
                    crate::tui::model::profile_label(self.profile.as_ref()),
                ),
            ),
        }
        self.load_roster(job_tx, http);
    }

    pub(crate) fn machine_by_addr(&self, addr: &str) -> Option<&Machine> {
        self.machines.iter().find(|m| m.addr == addr)
    }

    /// One blocking snapshot. Only the startup path and the `r` key use this;
    /// the steady state is the background poller in `run_loop`, so a slow
    /// inductor can never freeze the drawing loop.
    pub(crate) async fn refresh(&mut self, http: &reqwest::Client) {
        let outcome = fetch_state(http, &self.api).await;
        match outcome {
            Ok(v) => self.apply_state(v),
            Err(e) => self.state_failed(e),
        }
    }

    /// Fold one `/api/state` payload into the screen.
    pub(crate) fn apply_state(&mut self, v: serde_json::Value) {
        let mut machines: Vec<Machine> =
            serde_json::from_value(v.get("machines").cloned().unwrap_or_default())
                .unwrap_or_default();
        let mut beats: Vec<Heartbeat> =
            serde_json::from_value(v.get("beats").cloned().unwrap_or_default()).unwrap_or_default();
        let mut tasks: Vec<Task> =
            serde_json::from_value(v.get("tasks").cloned().unwrap_or_default()).unwrap_or_default();
        // The API serialises HashMaps, whose iteration order is not
        // stable. Without sorting, every refresh reshuffles the rows
        // and the cursor silently lands on a different machine.
        machines.sort_by(|a, b| a.addr.cmp(&b.addr));
        beats.sort_by(|a, b| a.worker_id.cmp(&b.worker_id));
        tasks.sort_by_key(|t| (t.chapter, t.stage));
        self.machines = machines;
        self.beats = beats;
        self.tasks = tasks;
        self.counts = v.get("counts").cloned().unwrap_or_default();
        self.stats = parse_stats(v.get("stats"));
        self.settings = v.get("settings").cloned();
        self.ingest_events(v.get("events"));
        if self.selected >= self.machines.len() {
            self.selected = self.machines.len().saturating_sub(1);
        }
        self.refreshed = Some(Instant::now());
        if self.conn != Conn::Up {
            if matches!(self.conn, Conn::Down(_)) {
                self.log_at(Level::Ok, "inductor reachable again");
            }
            self.conn = Conn::Up;
        }
    }

    /// Record a failed poll. The message is only logged on the *transition* into
    /// being down: a dead inductor would otherwise fill the pane with the same
    /// line every 800 ms. Warn, not Error — a down inductor at startup is the
    /// normal cold start (the message itself names `:B`), not a failure.
    pub(crate) fn state_failed(&mut self, e: String) {
        if self.conn != Conn::Down(e.clone()) {
            self.log_at(Level::Warn, e.clone());
        }
        self.conn = Conn::Down(e);
    }

    /// Append scheduler events the inductor has not shown us yet.
    ///
    /// Task failures, successes, lease expiries and operator actions all arrive
    /// here, which is what makes a worker's digest failure visible in the TUI at
    /// all: the worker only reports to the inductor, and this is the bridge.
    pub(crate) fn ingest_events(&mut self, events: Option<&serde_json::Value>) {
        let Some(list) = events.and_then(|v| v.as_array()) else {
            return;
        };
        let mut fresh: Vec<crate::state::EventRecord> = Vec::new();
        for item in list {
            match serde_json::from_value::<crate::state::EventRecord>(item.clone()) {
                Ok(rec) => fresh.push(rec),
                Err(_) => continue,
            }
        }
        let newest = fresh.iter().map(|e| e.id).max();
        // A restarted inductor begins its ids at 0 again. Without this reset the
        // new history would look "old" and be swallowed forever.
        if let (Some(newest), Some(last)) = (newest, self.last_event_id) {
            if newest < last {
                self.last_event_id = None;
                self.log_at(Level::Info, "inductor restarted — event stream reset");
            }
        }
        for rec in fresh {
            if self
                .last_event_id
                .map(|last| rec.id <= last)
                .unwrap_or(false)
            {
                continue;
            }
            self.last_event_id = Some(rec.id);
            // The record's own `ts` is epoch seconds on the inductor's clock;
            // close enough to local time for a pane stamp (same LAN, same day).
            self.push_log(LogLine {
                level: level_from_str(&rec.level),
                wall: rec.ts,
                text: rec.text,
            });
        }
    }

    pub(crate) fn apply(&mut self, ev: Ev) {
        match ev {
            Ev::JobStarted(id) => {
                if let Some(job) = self.background_jobs.iter_mut().find(|job| job.id == id) {
                    job.started.get_or_insert_with(Instant::now);
                    job.activity = "running".into();
                }
            }
            Ev::JobProgress { id, text } => {
                if let Some(job) = self.background_jobs.iter_mut().find(|job| job.id == id) {
                    job.activity = text;
                }
            }
            Ev::JobFinished(id) => {
                self.background_jobs.retain(|job| job.id != id);
                // The last catch-up provision finishing is what ends the `B`
                // start sequence now — see `catchup_jobs`.
                if let Some(i) = self.catchup_jobs.iter().position(|j| *j == id) {
                    self.catchup_jobs.remove(i);
                    if self.catchup_jobs.is_empty() {
                        self.backend_start_outstanding = false;
                    }
                }
            }
            Ev::Log(l) => self.push_log(l),
            Ev::Roster(Ok(r)) => {
                self.roster_loading = false;
                self.roster_error = None;
                self.log_at(
                    Level::Ok,
                    format!(
                        "roster: {} voices · {} speakers · {} ({})",
                        r.voices.len(),
                        r.characters.len(),
                        r.cast.len(),
                        r.source
                    ),
                );
                self.roster = Some(r);
            }
            Ev::Roster(Err(e)) => {
                self.roster_loading = false;
                self.roster_error = Some(e.clone());
                self.log_at(Level::Error, format!("roster: {e}"));
            }
            // The inductor's answer to a manual digest, shown either way. The
            // screen already told the operator it was reporting, so it has to be
            // able to say what came back — including "no", which is the one
            // outcome a silent success would hide.
            Ev::ManualDigest(Ok(line)) => {
                self.log_at(Level::Ok, format!("manual digest: {line}"));
            }
            Ev::ManualDigest(Err(e)) => {
                self.log_at(Level::Error, format!("manual digest: {e}"));
            }
            Ev::DigestPolicy(Ok(msg)) => self.log_at(Level::Ok, msg),
            Ev::DigestPolicy(Err(e)) => self.log_at(Level::Error, format!("digest policy: {e}")),
            // The `B` job started a backend: run this range on the first live
            // refresh. Stored, not sent, because the inductor is still booting.
            Ev::BackendLive { start, count } => {
                self.pending_enqueue = Some((start, count));
                self.log_at(
                    Level::Info,
                    format!("ch{start}×{count} will enqueue once live"),
                );
            }
            // The `B` job stopped at the backend and handed us the boxes that
            // still need work, so the catch-up is one visible job per box
            // instead of a loop inside "start backend". Stored, not dispatched
            // here: only `dispatch` can allocate a job id.
            Ev::CatchUp { machines, cancel } => {
                self.pending_catchup = Some((machines, cancel));
            }
            Ev::MachineUpdate { addr, state, note } => {
                if let Some(m) = self.machines.iter_mut().find(|m| m.addr == addr) {
                    m.state = state;
                    m.note = note;
                }
            }
            Ev::Lines(res) => {
                self.lines_loading = false;
                match res {
                    Ok(index) => {
                        self.log_at(
                            Level::Info,
                            format!("{} speaker(s) indexed for audition lines", index.len()),
                        );
                        self.lines = Some(index);
                    }
                    // Not fatal: the sample audition still works, only the
                    // "exact line" half needs a script. Say which half is out.
                    Err(e) => {
                        self.set_status(
                            Level::Warn,
                            format!("audition lines unavailable: {e} — :current (rendered segments) still plays"),
                        );
                    }
                }
            }
            Ev::Sounds(res) => {
                self.sound_loading = false;
                match res {
                    Ok(data) => {
                        let counts: Vec<String> = bm_core::audio_pool::PoolKind::ALL
                            .iter()
                            .map(|k| format!("{} {}", data.pools[k].len(), k.label()))
                            .collect();
                        self.log_at(
                            Level::Info,
                            format!("sound design loaded: {}", counts.join(" · ")),
                        );
                        self.sound_error = None;
                        self.sound = Some(*data);
                    }
                    // Not fatal to the TUI, fatal to the editor: the screen
                    // renders the reason rather than an empty pool, because an
                    // empty pool and an unreadable one look the same and only
                    // one of them is safe to edit.
                    Err(e) => {
                        self.sound = None;
                        self.sound_error = Some(e.clone());
                        self.log_at(Level::Error, format!("sound design: {e}"));
                    }
                }
            }
            // A fresh account listing. A failed read clears the rows and
            // records the reason: showing the previous account as if it were
            // still true is the one wrong answer here.
            Ev::Cloud(Ok(instances)) => {
                self.cloud_error = None;
                self.cloud = instances;
            }
            Ev::Cloud(Err(e)) => {
                self.cloud.clear();
                self.cloud_error = Some(e);
            }
            // A poller snapshot, applied the moment it arrives: nothing here
            // waits on the network, which is what keeps the drawing loop moving
            // even when the inductor is slow to answer.
            Ev::State(Ok(v)) => self.apply_state(v),
            Ev::State(Err(e)) => self.state_failed(e),
            Ev::Done(kind) => {
                self.pending = self.pending.saturating_sub(1);
                match kind {
                    DoneKind::StartDone => {
                        // The start *job* is over; the sequence it began may not
                        // be. `catchup_jobs` holds the boxes it handed out, so
                        // the flag follows them rather than this event — which
                        // is what a `B` press now means, and it is why a second
                        // `B` is still refused while boxes are joining.
                        //
                        // `start_cancel` deliberately survives this. It used to
                        // be cleared here because `StartDone` was the end of the
                        // catch-up; it is now the moment the catch-up *starts*,
                        // so clearing it would leave `X` with no way to stop the
                        // provisions the start just handed out. Its life is from
                        // a `B` press until `X` consumes it or the next `B`
                        // replaces it — and a flag nobody sets is inert.
                        self.backend_start_outstanding = !self.catchup_jobs.is_empty();
                    }
                    DoneKind::RosterDone => self.roster_loading = false,
                    DoneKind::LinesDone => self.lines_loading = false,
                    DoneKind::SoundsDone => self.sound_loading = false,
                    DoneKind::Op {
                        op,
                        key,
                        ok,
                        voice,
                        audio_b64,
                        line_speaker,
                        line_text,
                    } => {
                        self.inflight.retain(|k| *k != key);
                        if op == Op::PreviewVoice || op == Op::Segment {
                            self.audition = None;
                            if ok {
                                if let Screen::Pick(p) = &mut self.screen {
                                    if let Some(v) = voice {
                                        if !p.previewed.contains(&v) {
                                            p.previewed.push(v);
                                        }
                                    }
                                }
                                // A served segment names its sentence: hold it
                                // so the operator is comparing voices on words
                                // they can see, and so T renders this
                                // exact line rather than another random pick.
                                if op == Op::Segment {
                                    if let (Some(speaker), Some(text)) = (line_speaker, line_text) {
                                        let line = crate::tui::audition::AuditionLine {
                                            character: speaker,
                                            text,
                                        };
                                        match &mut self.screen {
                                            Screen::Cast(v) => v.line = Some(line),
                                            Screen::Pick(p) => p.line = Some(line),
                                            _ => {}
                                        }
                                    }
                                }
                            }
                            self.play_audition(ok, audio_b64);
                        }
                        // A successful swap rewrites the cast, so the picker's
                        // copy is stale from this moment on.
                        if op == Op::SwapVoice && ok {
                            self.roster = None;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}
