//! The dashboard state: what the poller fills and every pane reads.
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, atomic::AtomicBool};
use std::time::Instant;
use bm_proto::{Heartbeat, Machine, Op, Roster, Task};
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use ratatui::style::{Color, Style};
use crate::tui::EVENT_CAP;
use crate::tui::{
    audio::Player,
    input::dispatch,
    jobs::{DoneKind, Ev, Job, fetch_state},
    model::{CastRow, cast_rows, registry_machines},
    screen::Screen,
    style::{Conn, Level, LogLine, level_from_str, style_bold_of, style_of},
};

pub(crate) struct App {
    pub(crate) api: String,
    /// Repo root — a provision job needs a `Layout` to run against.
    pub(crate) layout_root: std::path::PathBuf,
    /// Shared HTTP client for the inductor API.
    pub(crate) http: reqwest::Client,
    pub(crate) machines: Vec<Machine>,
    pub(crate) beats: Vec<Heartbeat>,
    pub(crate) tasks: Vec<Task>,
    pub(crate) counts: serde_json::Value,
    pub(crate) settings: Option<serde_json::Value>,
    pub(crate) events: VecDeque<LogLine>,
    pub(crate) selected: usize,
    pub(crate) machine_scroll: usize,
    /// 0 = pinned to the newest event; N = N rows scrolled back.
    pub(crate) events_scroll: usize,
    pub(crate) screen: Screen,
    pub(crate) roster: Option<Roster>,
    pub(crate) roster_loading: bool,
    pub(crate) roster_error: Option<String>,
    /// Jobs in flight, for the "working…" indicator and duplicate suppression.
    /// One key per op *instance* (see `op_key`), so retrying chapter 3 does not
    /// block retrying chapter 4 — but pressing the same key twice does.
    pub(crate) pending: usize,
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
    /// A backend start sequence is in flight: refuses a second `B`/`R` start,
    /// cleared when the sequence reports `DoneKind::StartDone`.
    pub(crate) backend_start_outstanding: bool,
    /// Cancel flag for the in-flight start's catch-up loop, set
    /// synchronously by `X` (the stop job itself still queues behind).
    pub(crate) start_cancel: Option<Arc<AtomicBool>>,
    /// Screen a `:` command returns to after it runs: commands fire in the
    /// context they were typed in, so `:F` in the task list retries the
    /// highlighted row instead of losing it.
    pub(crate) command_return: Option<Screen>,
    pub(crate) colour: bool,
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
    /// The speaker on this desk. Owns the audio process, not the audio.
    pub(crate) player: Player,
}

impl App {
    pub(crate) fn new(api: &str) -> Self {
        App {
            api: api.trim_end_matches('/').to_string(),
            layout_root: std::path::PathBuf::new(),
            http: reqwest::Client::new(),
            machines: Vec::new(),
            beats: Vec::new(),
            tasks: Vec::new(),
            counts: serde_json::Value::Null,
            settings: None,
            events: VecDeque::with_capacity(EVENT_CAP),
            selected: 0,
            machine_scroll: 0,
            events_scroll: 0,
            screen: Screen::Normal,
            roster: None,
            roster_loading: false,
            roster_error: None,
            pending: 0,
            inflight: Vec::new(),
            last_event_id: None,
            pending_enqueue: None,
            backend_start_outstanding: false,
            start_cancel: None,
            command_return: None,
            colour: true,
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
            player: Player::new(),
        }
    }

    /// Build the audition line index if it is not already here or on its way.
    ///
    /// Called when a screen that can audition opens, so the hundred file opens
    /// happen while the operator is still reading the table rather than after they
    /// press the key. Idempotent: a second call while it is loading does nothing.
    pub(crate) fn ensure_lines(
        &mut self,
        job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
    ) {
        if self.lines.is_some() || self.lines_loading {
            return;
        }
        self.lines_loading = true;
        dispatch(
            self,
            job_tx,
            Job::LoadLines { layout_root: self.layout_root.clone() },
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
        while self.events.len() >= EVENT_CAP {
            self.events.pop_front();
        }
        self.events.push_back(line);
    }

    pub(crate) fn log_at(&mut self, level: Level, text: impl Into<String>) {
        self.push_log(LogLine { level, wall: bm_proto::now_secs(), text: text.into() });
    }

    pub(crate) fn set_status(&mut self, level: Level, text: impl Into<String>) {
        self.status = LogLine {
            level,
            wall: bm_proto::now_secs(),
            text: text.into(),
        };
    }

    /// Colour-aware style. `C` disables colour for monochrome terminals and
    /// for operators who cannot separate the state hues; the state word is
    /// always rendered too, so nothing depends on colour alone.
    pub(crate) fn style(&self, c: Color) -> Style {
        style_of(self.colour, c)
    }

    pub(crate) fn style_bold(&self, c: Color) -> Style {
        style_bold_of(self.colour, c)
    }

    pub(crate) fn setting_u32(&self, key: &str, default: u32) -> u32 {
        self.settings
            .as_ref()
            .and_then(|s| s.get(key))
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
            .unwrap_or(default)
    }

    pub(crate) fn setting_str(&self, key: &str, default: &str) -> String {
        self.settings
            .as_ref()
            .and_then(|s| s.get(key))
            .and_then(|v| v.as_str())
            .unwrap_or(default)
            .to_string()
    }

    /// App-wide ssh defaults from the live settings (see `SshDefaults`).
    /// Deserializing the `ssh` subtree keeps one source for the defaults —
    /// a missing or partial subtree parses as defaults, like the file itself.
    pub(crate) fn ssh_defaults(&self) -> bm_core::config::SshDefaults {
        self.settings
            .as_ref()
            .and_then(|s| s.get("ssh"))
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
        registry_machines(&self.layout_root)
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
                layout_root: self.layout_root.clone(),
            },
        );
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
            serde_json::from_value(v.get("beats").cloned().unwrap_or_default())
                .unwrap_or_default();
        let mut tasks: Vec<Task> =
            serde_json::from_value(v.get("tasks").cloned().unwrap_or_default())
                .unwrap_or_default();
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
    /// line every 800 ms.
    pub(crate) fn state_failed(&mut self, e: String) {
        if self.conn != Conn::Down(e.clone()) {
            self.log_at(Level::Error, e.clone());
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
            if self.last_event_id.map(|last| rec.id <= last).unwrap_or(false) {
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
            // The `B` job started a backend: run this range on the first live
            // refresh. Stored, not sent, because the inductor is still booting.
            Ev::BackendLive { start, count } => {
                self.pending_enqueue = Some((start, count));
                self.log_at(Level::Info, format!("ch{start}×{count} will enqueue once live"));
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
                            format!("audition lines unavailable: {e} — t (rendered segments) still plays"),
                        );
                    }
                }
            }
            // A poller snapshot, applied the moment it arrives: nothing here
            // waits on the network, which is what keeps the drawing loop moving
            // even when the inductor is slow to answer.
            Ev::State(Ok(v)) => self.apply_state(v),
            Ev::State(Err(e)) => self.state_failed(e),
            Ev::Done(kind) => {
                self.pending = self.pending.saturating_sub(1);
                match kind {
                    DoneKind::StartDone => self.backend_start_outstanding = false,
                    DoneKind::Op { op, key, ok, voice, audio_b64, line_speaker, line_text } => {
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
                                        let line = crate::tui::audition::AuditionLine { character: speaker, text };
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
