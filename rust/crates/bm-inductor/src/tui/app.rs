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
    pub(crate) layout: bm_core::Layout,
    /// The profile the live `assets/` + `prompts/` tree claims to be, read from
    pub(crate) profile: Option<bm_core::profile::Binding>,
    /// Shared HTTP client for the inductor API.
    pub(crate) http: reqwest::Client,
    pub(crate) machines: Vec<Machine>,
    /// Draw the Machines pane as a hub-and-spoke picture instead of the table.
    pub(crate) machines_graph: bool,
    /// The Machines rack's window: which band of servers it starts at, and how
    pub(crate) graph_band: usize,
    pub(crate) graph_cols: usize,
    pub(crate) beats: Vec<Heartbeat>,
    pub(crate) tasks: Vec<Task>,
    pub(crate) counts: serde_json::Value,
    /// Stats pane data: per-worker per-stage completions plus per-stage
    pub(crate) stats: WorkerStats,
    /// Whether the cluster is distributing, and the range it stands at, from
    pub(crate) dispatch: Option<Dispatch>,
    pub(crate) settings: Option<serde_json::Value>,
    pub(crate) events: VecDeque<LogLine>,
    pub(crate) selected: usize,
    pub(crate) machine_scroll: usize,
    /// Scroll offset for the live worker list; the pane sizes itself around
    pub(crate) worker_scroll: usize,
    /// 0 = pinned to the newest event; N = N rows scrolled back.
    pub(crate) events_scroll: usize,
    /// The Logs pane's content height in rows, published by the draw and read
    pub(crate) events_rows: usize,
    pub(crate) screen: Screen,
    pub(crate) roster: Option<Roster>,
    pub(crate) roster_loading: bool,
    pub(crate) roster_error: Option<String>,
    /// Models the last `f` fetch listed, and which provider they are for.
    pub(crate) llm_models: Vec<String>,
    pub(crate) llm_models_for: String,
    /// Cursor to restore on the `L` screen after a `k`/`u`/`m` prompt closes.
    pub(crate) llm_cursor: usize,
    /// Jobs in flight, for the "working…" indicator and duplicate suppression.
    pub(crate) pending: usize,
    pub(crate) next_job_id: u64,
    pub(crate) background_jobs: Vec<BackgroundJob>,
    pub(crate) inflight: Vec<String>,
    /// Highest scheduler event id already folded into `events`. `None` until the
    pub(crate) last_event_id: Option<u64>,
    /// A chapter range to enqueue once the inductor answers. Set when `B`
    pub(crate) pending_enqueue: Option<(u32, u32)>,
    /// A backend start sequence is in flight: refuses a second `B`/`R` start.
    pub(crate) backend_start_outstanding: bool,
    /// Cancel flag for the catch-up provisions a `B` start handed out, set
    pub(crate) start_cancel: Option<Arc<AtomicBool>>,
    /// The boxes a `B` start left to catch up, and the flag that stops them.
    pub(crate) pending_catchup: Option<(Vec<bm_proto::Machine>, Arc<AtomicBool>)>,
    /// Boxes a launch orphaned that the account has just given an address to,
    pub(crate) pending_onboard: Vec<bm_proto::Machine>,
    /// Addresses an onboard job has already been handed out for.
    pub(crate) onboarded: std::collections::HashSet<String>,
    /// Ids of the catch-up provisions a `B` handed out that are still running.
    pub(crate) catchup_jobs: Vec<u64>,
    /// Screen a `:` command returns to after it runs: commands fire in the
    pub(crate) command_return: Option<Screen>,
    /// The screens `Esc` steps back through, outermost first: one entry per
    pub(crate) back: Vec<Screen>,
    /// Set by the `:` line as it hands control to the screen its command runs
    pub(crate) prompt_spent: Option<Screen>,
    /// The active palette. Replaces the old `colour: bool`: mono is now one
    pub(crate) theme: Theme,
    /// Whether the terminal is reporting mouse events to us.
    pub(crate) mouse_capture: bool,
    /// Set by the key handler, consumed by the event loop, which is the only
    pub(crate) mouse_toggle: bool,
    pub(crate) status: LogLine,
    pub(crate) conn: Conn,
    pub(crate) tick: u64,
    pub(crate) refreshed: Option<Instant>,
    /// Voice whose audition render is in flight, if any.
    pub(crate) audition: Option<String>,
    /// Sentences locked by Enter-pick, per character. Picking a voice locks
    pub(crate) locked_lines: HashMap<String, crate::tui::audition::AuditionLine>,
    /// `speaker -> their lines`, from `data/script-*.json`. Built once per
    pub(crate) lines: Option<std::collections::HashMap<String, Vec<String>>>,
    pub(crate) lines_loading: bool,
    /// The three sound-design pools, the scene map and what each entry is
    pub(crate) sound: Option<SoundData>,
    pub(crate) sound_loading: bool,
    pub(crate) sound_error: Option<String>,
    /// The speaker on this desk. Owns the audio process, not the audio.
    pub(crate) player: Player,
    /// What the EC2 account holds, from the last `describe-instances`. Empty
    pub(crate) cloud: Vec<AwsInstance>,
    pub(crate) cloud_error: Option<String>,
    /// The Logs pane filter, stepped with `←/→`. TUI-local: polls never reset it.
    pub(crate) log_filter: crate::tui::model::LogFilter,
    /// The pane highlighted by keyboard focus or a mouse click.
    pub(crate) focused_panel: Panel,
    /// Rebuilt on every frame; mouse hit testing is therefore resize-safe.
    pub(crate) hit_regions: Vec<HitRegion>,
    /// Coordinates and time of the last left click, used only for activation
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
    fn play_audition(&mut self, ok: bool, audio_b64: Option<String>) {
        if !ok {
            // The op's own failure text is already in the event pane; the bar's
            self.set_status(Level::Error, "audition failed — see the events pane");
            return;
        }
        let Some(b64) = audio_b64 else {
            // What an *older inductor* answers with: it has no audio field at
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
            Err(e) => self.set_status(Level::Error, e),
        }
    }

    pub(crate) fn push_log(&mut self, mut line: LogLine) {
        // Absolute paths are the machine's, not the pane's: shorten them to
        line.text = shorten_paths(&self.layout.root, &line.text);
        // A new line must not shove the operator's reading position away. While
        let held = self.events_scroll > 0;
        while self.events.len() >= EVENT_CAP {
            self.events.pop_front();
        }
        self.events.push_back(line);
        if held {
            // Eviction from the front doesn't move the held line's distance
            self.events_scroll = (self.events_scroll + 1).min(self.events.len());
        }
    }

    /// The farthest the Logs pane may scroll: the whole buffer. The buffer is
    pub(crate) fn max_events_scroll(&self) -> usize {
        self.events.len()
    }

    /// Raise `events_scroll` (scroll toward older lines), clamped to the
    pub(crate) fn scroll_events_older(&mut self, by: usize) {
        self.events_scroll = (self.events_scroll + by).min(self.max_events_scroll());
    }

    /// Lower `events_scroll` (scroll toward newer lines), clamped at 0.
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
    pub(crate) fn live_workers(&self) -> Vec<&bm_proto::Heartbeat> {
        let now = bm_proto::now_secs();
        live_beats(&self.beats, now)
            .into_iter()
            .filter(|b| beat_backed(&self.machines, b))
            .collect()
    }

    /// The same set as identities: the worker ids the *ledger* may treat as
    pub(crate) fn live_worker_ids(&self) -> std::collections::BTreeSet<String> {
        crate::tui::model::live_worker_ids(&self.beats, &self.machines, bm_proto::now_secs())
    }

    /// Step back one layer: the screen under this one, or the dashboard.
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
fn shorten_paths(root: &std::path::Path, text: &str) -> String {
    let mut out = text.to_string();
    let root = root.to_string_lossy();
    // A one-character root (`/`) would rewrite every path in the line to `.`;
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
        assert_eq!(
            shorten_paths(Path::new("/repo"), "/elsewhere/x"),
            "/elsewhere/x"
        );
        // A bare `/` root must not turn every path into `.`.
        assert_eq!(shorten_paths(Path::new("/"), "/a/b"), "/a/b");
    }
}
