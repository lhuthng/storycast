//! Live cluster dashboard: machines, workers, tasks, events.
//!
//! The TUI owns no state beyond the screen. It reads `/api/state` and
//! `/api/roster`, and every operator key pushes a command onto a channel that a
//! background task executes — so provisioning, roster fetches and voice
//! previews never freeze the interface.
//!
//! Two rules hold throughout:
//!
//! * **No silent defaults.** Every prompt is prefilled with the value actually
//!   in force, and every argument is echoed before it is submitted.
//! * **No blank panes.** Each pane renders an explicit empty, loading or error
//!   state that says what to do next.

use bm_core::Layout;
use bm_proto::{Heartbeat, Machine, Op, OpRequest, Roster, Task, TaskState, VoiceInfo};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout as RLayout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Row, Table, Wrap},
    Terminal,
};
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Event scrollback depth. Older lines fall off the top.
const EVENT_CAP: usize = 500;
/// `/api/state` poll period, in 200 ms ticks.
const REFRESH_TICKS: u64 = 4;

// --- responsive layout ------------------------------------------------------

/// Hard floor. Below this the dashboard is not merely cramped, it is
/// misleading: table columns clip mid-word and a reversed-cursor row can look
/// like a different row than the one selected. Rather than render a lie, say
/// what is wrong and what to do.
const MIN_W: u16 = 76;
const MIN_H: u16 = 20;

/// Above this the full five-pane dashboard fits without squeezing Events,
/// which is the one pane that must stay readable.
const FULL_W: u16 = 100;
const FULL_H: u16 = 32;

/// How much room the terminal has.
///
/// Three tiers rather than a single pass/fail threshold: an 80×24 terminal is
/// the default on most setups, so refusing to draw at 100×32 would blank the
/// dashboard for almost everyone. Compact keeps every pane that carries live
/// state and folds only the Tasks summary into the footer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Size {
    /// Below `MIN_W` × `MIN_H` — draw the guard panel and nothing else.
    TooSmall,
    /// Usable, but the Tasks pane is collapsed into the footer.
    Compact,
    /// Everything fits.
    Full,
}

fn size_class(w: u16, h: u16) -> Size {
    if w < MIN_W || h < MIN_H {
        Size::TooSmall
    } else if w < FULL_W || h < FULL_H {
        Size::Compact
    } else {
        Size::Full
    }
}

/// Pane heights, per tier. Named rather than inlined so the compile-time guard
/// below and the renderer cannot drift apart.
const FULL_MACHINES_H: u16 = 8;
const FULL_WORKERS_H: u16 = 8;
const FULL_TASKS_H: u16 = 7;
const FULL_EVENTS_MIN_H: u16 = 5;
const FULL_FOOTER_H: u16 = 3;

const COMPACT_MACHINES_H: u16 = 6;
const COMPACT_WORKERS_H: u16 = 6;
const COMPACT_EVENTS_MIN_H: u16 = 4;
const COMPACT_FOOTER_H: u16 = 4;

/// Key hints, on two lines each.
///
/// A single line was 161 characters, so it was clipped on *every* terminal —
/// and the part that fell off the right-hand end held the least guessable keys.
/// The compact tier gets shorter labels because it has 76 columns to work with;
/// every key is described in full on the help screen, which `?` opens.
const KEYS_FULL: [&str; 2] = [
    "a add · p provision · d drop · i inspect · t translate · c crawl-setup",
    "P force · v voices · s swap-voice · S cast · e eta · r refresh · ? help · C colour · q quit",
];
const KEYS_COMPACT: [&str; 2] = [
    "a add · p prov · d drop · i info · t translate · c crawl",
    "v voices · s swap · S cast · e eta · r refresh · ? help · q quit",
];

/// Compact-tier column widths. The full tier has slack and keeps its widths
/// inline; these are the ones that must fit inside `MIN_W`, so they are named
/// and checked while compiling.
///
/// `id, addr, role, state, seen` — the `tts` column is dropped.
const COMPACT_MACHINE_COLS: [u16; 5] = [16, 15, 8, 13, 8];
/// `worker, stage, ch, progress, activity, eta` — `machine` is dropped.
const COMPACT_WORKER_COLS: [u16; 6] = [14, 8, 5, 17, 16, 8];

/// Column widths for the cast table, in two sets.
///
/// The dashboard floor is 76 columns, but the wide cast table needs 92 — so on
/// the terminal sizes where the dashboard is *most* useful the table would be
/// squeezed and every column clipped together. The narrow set drops `gender`
/// (the picker shows it in full) so the speaker, the voice and the verdict stay
/// readable. `lang` is absent from both: it is the constant `vi-VN` and so
/// carries no information, and it lives in the overlay title instead.
///
/// `speaker, voice, gender, accent, status`
const CAST_COLS_WIDE: [u16; 5] = [24, 20, 7, 14, 27];
/// `speaker, voice, accent, status`
const CAST_COLS_NARROW: [u16; 4] = [18, 16, 13, 23];

/// Sum of a column list, in a form `const` evaluation accepts.
const fn cols(xs: &[u16]) -> u16 {
    let mut i = 0;
    let mut total = 0;
    while i < xs.len() {
        total += xs[i];
        i += 1;
    }
    total
}

/// Display width of a hint line. Every glyph in these strings occupies one
/// column, so counting UTF-8 lead bytes is exact — and `str::chars().count()`
/// is not available in a `const` context.
const fn width_of(s: &str) -> usize {
    let b = s.as_bytes();
    let mut i = 0;
    let mut n = 0;
    while i < b.len() {
        if b[i] & 0xC0 != 0x80 {
            n += 1;
        }
        i += 1;
    }
    n
}

// Proved at compile time: a widened column, a taller pane or one more key hint
// must not silently start clipping on the smallest terminal of its tier.
const _: () = assert!(
    COMPACT_MACHINES_H + COMPACT_WORKERS_H + COMPACT_EVENTS_MIN_H + COMPACT_FOOTER_H <= MIN_H,
    "the compact tier must fit inside MIN_H"
);
const _: () = assert!(
    FULL_MACHINES_H + FULL_WORKERS_H + FULL_TASKS_H + FULL_EVENTS_MIN_H + FULL_FOOTER_H <= FULL_H,
    "the full tier must fit inside FULL_H"
);
const _: () = assert!(
    cols(&COMPACT_MACHINE_COLS) + 2 <= MIN_W,
    "compact machines columns plus borders must fit MIN_W"
);
const _: () = assert!(
    cols(&COMPACT_WORKER_COLS) + 2 <= MIN_W,
    "compact workers columns plus borders must fit MIN_W"
);
const _: () = assert!(width_of(KEYS_FULL[0]) <= FULL_W as usize, "key line 1 overflows the full tier");
const _: () = assert!(width_of(KEYS_FULL[1]) <= FULL_W as usize, "key line 2 overflows the full tier");
const _: () = assert!(width_of(KEYS_COMPACT[0]) <= MIN_W as usize, "key line 1 overflows compact");
const _: () = assert!(width_of(KEYS_COMPACT[1]) <= MIN_W as usize, "key line 2 overflows compact");
const _: () = assert!(
    cols(&CAST_COLS_NARROW) + 4 <= MIN_W,
    "the narrow cast table plus two sets of borders must fit the smallest terminal"
);

// --- severity ---------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Info,
    Ok,
    Warn,
    Error,
}

impl Level {
    fn color(self) -> Color {
        match self {
            Level::Info => Color::Gray,
            Level::Ok => Color::Green,
            Level::Warn => Color::Yellow,
            Level::Error => Color::Red,
        }
    }

    fn glyph(self) -> &'static str {
        match self {
            Level::Info => "·",
            Level::Ok => "✓",
            Level::Warn => "!",
            Level::Error => "✗",
        }
    }
}

#[derive(Debug, Clone)]
struct LogLine {
    level: Level,
    /// Offset from TUI start. Deliberately relative: the repo takes no clock
    /// dependency, and a monotonic stamp is what you actually correlate a
    /// provisioning run against.
    at: Duration,
    text: String,
}

fn stamp(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("+{}h{:02}m", s / 3600, (s % 3600) / 60)
    } else {
        format!("+{:02}:{:02}", s / 60, s % 60)
    }
}

/// Reachability of the inductor. Kept apart from `status` so a transient
/// network blip never overwrites the result of the last operator action.
#[derive(Debug, Clone, PartialEq)]
enum Conn {
    Unknown,
    Up,
    Down(String),
}

// --- screens ----------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextKind {
    AddMachine,
    AddSample,
    Translate,
    CrawlTemplate,
}

/// A single-line editor with a real cursor. The old prompt could only append
/// and backspace; a mistyped address meant starting over.
#[derive(Debug, Clone)]
struct TextPrompt {
    kind: TextKind,
    title: String,
    hint: String,
    buf: String,
    /// Cursor position in *characters*, never bytes — the data is Vietnamese.
    cursor: usize,
}

impl TextPrompt {
    fn new(kind: TextKind, title: &str, hint: &str, initial: &str) -> Self {
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

    fn len(&self) -> usize {
        self.buf.chars().count()
    }

    fn byte_at(&self, char_idx: usize) -> usize {
        self.buf
            .char_indices()
            .nth(char_idx)
            .map(|(b, _)| b)
            .unwrap_or(self.buf.len())
    }

    fn insert(&mut self, c: char) {
        let b = self.byte_at(self.cursor);
        self.buf.insert(b, c);
        self.cursor += 1;
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let (a, b) = (self.byte_at(self.cursor - 1), self.byte_at(self.cursor));
        self.buf.replace_range(a..b, "");
        self.cursor -= 1;
    }

    fn delete(&mut self) {
        if self.cursor >= self.len() {
            return;
        }
        let (a, b) = (self.byte_at(self.cursor), self.byte_at(self.cursor + 1));
        self.buf.replace_range(a..b, "");
    }

    fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.len());
    }

    fn home(&mut self) {
        self.cursor = 0;
    }

    fn end(&mut self) {
        self.cursor = self.len();
    }

    fn kill_to_start(&mut self) {
        let b = self.byte_at(self.cursor);
        self.buf.replace_range(..b, "");
        self.cursor = 0;
    }

    fn kill_word(&mut self) {
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
    fn split(&self) -> (String, String) {
        let b = self.byte_at(self.cursor);
        (self.buf[..b].to_string(), self.buf[b..].to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickStage {
    Character,
    Voice,
}

#[derive(Debug, Clone)]
struct Picker {
    stage: PickStage,
    /// Chosen in step 1; empty until then.
    character: String,
    filter: String,
    cursor: usize,
    scroll: usize,
    /// Voice with an audition in flight, if any.
    previewing: Option<String>,
    /// Voices auditioned this session, so the operator can tell them apart
    /// from ones merely read about.
    previewed: Vec<String>,
}

impl Picker {
    fn new() -> Self {
        Picker {
            stage: PickStage::Character,
            character: String::new(),
            filter: String::new(),
            cursor: 0,
            scroll: 0,
            previewing: None,
            previewed: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
enum ConfirmAction {
    Quit,
    Provision { addr: String, force: bool },
    DropMachine { addr: String },
    SwapVoice { character: String, voice: String },
}

#[derive(Debug, Clone)]
struct Confirm {
    title: String,
    body: Vec<String>,
    action: ConfirmAction,
    danger: bool,
}

/// Read-only overview of the whole cast: who speaks with what, which voices
/// are shared, and which assignments the accent policy would reject.
///
/// The picker can only answer "what is this one character's voice"; this
/// answers "is the cast healthy", which previously meant reading the cast file
/// by hand. `Enter` hands the highlighted speaker to the picker's step 2, so
/// the overview is a starting point for a fix rather than just a report.
#[derive(Debug, Clone)]
struct CastView {
    cursor: usize,
    scroll: usize,
    filter: String,
}

impl CastView {
    fn new() -> Self {
        CastView { cursor: 0, scroll: 0, filter: String::new() }
    }
}

#[derive(Debug, Clone)]
enum Screen {
    Normal,
    Help { scroll: usize },
    Text(TextPrompt),
    Pick(Picker),
    Cast(CastView),
    Confirm(Confirm),
    /// Machine detail, keyed by address so a refresh can never retarget it.
    Machine(String),
}

// --- background jobs --------------------------------------------------------

#[derive(Debug)]
enum Job {
    /// Long SSH/rsync flow, executed off the UI task.
    Provision {
        layout_root: std::path::PathBuf,
        machine: Machine,
        force: bool,
    },
    AddMachine {
        api: String,
        http: reqwest::Client,
        m: Machine,
    },
    /// Local file work: copy a clip into `refs/`, tag it from its filename,
    /// register it in the pool and in `voices.json`. Needs no inductor.
    AddSample {
        layout_root: std::path::PathBuf,
        path: String,
        name: Option<String>,
    },
    /// Deregister a machine. Idempotent, so it needs no confirmation beyond
    /// the one the operator already gave.
    DropMachine {
        api: String,
        http: reqwest::Client,
        addr: String,
    },
    Op {
        api: String,
        http: reqwest::Client,
        req: OpRequest,
    },
    LoadRoster {
        api: String,
        http: reqwest::Client,
    },
}

enum DoneKind {
    Op { op: Op, ok: bool, voice: Option<String> },
    Other,
}

enum Ev {
    Log(LogLine),
    Roster(Result<Roster, String>),
    Done(DoneKind),
}

// --- app --------------------------------------------------------------------

struct App {
    api: String,
    /// Repo root — a provision job needs a `Layout` to run against.
    layout_root: std::path::PathBuf,
    /// Shared HTTP client for the inductor API.
    http: reqwest::Client,
    started: Instant,
    machines: Vec<Machine>,
    beats: Vec<Heartbeat>,
    tasks: Vec<Task>,
    counts: serde_json::Value,
    settings: Option<serde_json::Value>,
    events: VecDeque<LogLine>,
    selected: usize,
    machine_scroll: usize,
    /// 0 = pinned to the newest event; N = N rows scrolled back.
    events_scroll: usize,
    screen: Screen,
    roster: Option<Roster>,
    roster_loading: bool,
    roster_error: Option<String>,
    /// Jobs in flight, for the "working…" indicator and duplicate suppression.
    pending: usize,
    inflight: Vec<Op>,
    colour: bool,
    status: LogLine,
    conn: Conn,
    tick: u64,
    refreshed: Option<Instant>,
}

impl App {
    fn new(api: &str) -> Self {
        App {
            api: api.trim_end_matches('/').to_string(),
            layout_root: std::path::PathBuf::new(),
            http: reqwest::Client::new(),
            started: Instant::now(),
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
            colour: true,
            status: LogLine {
                level: Level::Info,
                at: Duration::ZERO,
                text: "press ? for help".into(),
            },
            conn: Conn::Unknown,
            tick: 0,
            refreshed: None,
        }
    }

    fn push_log(&mut self, line: LogLine) {
        while self.events.len() >= EVENT_CAP {
            self.events.pop_front();
        }
        self.events.push_back(line);
    }

    fn log_at(&mut self, level: Level, text: impl Into<String>) {
        let at = self.started.elapsed();
        self.push_log(LogLine { level, at, text: text.into() });
    }

    fn set_status(&mut self, level: Level, text: impl Into<String>) {
        self.status = LogLine {
            level,
            at: self.started.elapsed(),
            text: text.into(),
        };
    }

    /// Colour-aware style. `C` disables colour for monochrome terminals and
    /// for operators who cannot separate the state hues; the state word is
    /// always rendered too, so nothing depends on colour alone.
    fn style(&self, c: Color) -> Style {
        style_of(self.colour, c)
    }

    fn style_bold(&self, c: Color) -> Style {
        style_bold_of(self.colour, c)
    }

    fn setting_u32(&self, key: &str, default: u32) -> u32 {
        self.settings
            .as_ref()
            .and_then(|s| s.get(key))
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
            .unwrap_or(default)
    }

    fn setting_str(&self, key: &str, default: &str) -> String {
        self.settings
            .as_ref()
            .and_then(|s| s.get(key))
            .and_then(|v| v.as_str())
            .unwrap_or(default)
            .to_string()
    }

    fn selected_machine(&self) -> Option<Machine> {
        self.machines.get(self.selected).cloned()
    }

    /// Any screen other than the dashboard. Used by the size guard to say when
    /// a dialog is still open, and by the "is anything pending" checks.
    fn dialog_open(&self) -> bool {
        !matches!(self.screen, Screen::Normal)
    }

    /// The cast overview's rows, or an empty list before the roster arrives.
    fn cast_rows(&self) -> Vec<CastRow> {
        self.roster.as_ref().map(cast_rows).unwrap_or_default()
    }

    /// Kick off a roster fetch unless one is already in flight.
    fn load_roster(
        &mut self,
        job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
        http: &reqwest::Client,
    ) {
        self.roster_loading = true;
        self.roster_error = None;
        dispatch(
            self,
            job_tx,
            Job::LoadRoster {
                api: self.api.clone(),
                http: http.clone(),
            },
        );
    }

    fn machine_by_addr(&self, addr: &str) -> Option<&Machine> {
        self.machines.iter().find(|m| m.addr == addr)
    }

    async fn refresh(&mut self, http: &reqwest::Client) {
        let url = format!("{}/api/state", self.api);
        let outcome = match http.get(&url).send().await {
            Ok(r) => match r.json::<serde_json::Value>().await {
                Ok(v) => Ok(v),
                Err(e) => Err(format!("bad state payload: {e}")),
            },
            Err(e) => Err(format!("inductor unreachable at {}: {e}", self.api)),
        };
        match outcome {
            Ok(v) => {
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
            Err(e) => {
                if self.conn != Conn::Down(e.clone()) {
                    self.log_at(Level::Error, e.clone());
                }
                self.conn = Conn::Down(e);
            }
        }
    }

    fn apply(&mut self, ev: Ev) {
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
            Ev::Done(kind) => {
                self.pending = self.pending.saturating_sub(1);
                if let DoneKind::Op { op, ok, voice } = kind {
                    self.inflight.retain(|o| *o != op);
                    if let Screen::Pick(p) = &mut self.screen {
                        p.previewing = None;
                        if op == Op::PreviewVoice && ok {
                            if let Some(v) = voice {
                                if !p.previewed.contains(&v) {
                                    p.previewed.push(v);
                                }
                            }
                        }
                    }
                    // A successful swap rewrites the cast, so the picker's copy
                    // is stale from this moment on.
                    if op == Op::SwapVoice && ok {
                        self.roster = None;
                    }
                }
            }
        }
    }
}

// --- formatting helpers -----------------------------------------------------

fn bar(frac: f32, width: usize) -> String {
    let fill = (frac.clamp(0.0, 1.0) * width as f32).round() as usize;
    format!("{}{}", "█".repeat(fill), "░".repeat(width.saturating_sub(fill)))
}

fn state_color(s: &str) -> Color {
    match s {
        "online" | "done" | "configured" => Color::Green,
        "running" | "assigned" | "probing" | "provisioning" => Color::Yellow,
        "offline" | "failed" | "shelved" | "error" => Color::Red,
        "pending" => Color::Gray,
        _ => Color::Gray,
    }
}

/// Pipeline stages get their own hues so the Workers pane reads at a glance.
/// Previously the stage column reused the *task-state* palette, which no stage
/// name ever matched — the column was permanently grey.
fn stage_color(s: &str) -> Color {
    match s {
        "crawl" => Color::Blue,
        "digest" => Color::Magenta,
        "render" => Color::Cyan,
        "merge" => Color::Green,
        _ => Color::Gray,
    }
}

/// `last_seen` is 0 for a machine that has never reported. Subtracting it from
/// now produced a ~56-year uptime; say "never" instead.
fn seen_label(m: &Machine) -> String {
    if m.last_seen == 0 {
        return "never".into();
    }
    let d = bm_proto::now_secs().saturating_sub(m.last_seen);
    if d < 60 {
        format!("{d}s")
    } else if d < 3600 {
        format!("{}m", d / 60)
    } else {
        format!("{}h", d / 3600)
    }
}

fn gender_label(g: &str) -> &str {
    match g {
        "male" => "male",
        "female" => "female",
        "neutral" => "neutral",
        _ => "—",
    }
}

fn dash_if_empty(s: &str) -> &str {
    if s.trim().is_empty() {
        "—"
    } else {
        s
    }
}

/// Fold Vietnamese diacritics to ASCII so a filter of `thai son` matches
/// `Thái Sơn`. Without it, filtering a Vietnamese cast means typing exact
/// diacritics on every keystroke.
fn fold_char(c: char) -> char {
    match c {
        'à' | 'á' | 'ạ' | 'ả' | 'ã' | 'â' | 'ầ' | 'ấ' | 'ậ' | 'ẩ' | 'ẫ' | 'ă' | 'ằ' | 'ắ'
        | 'ặ' | 'ẳ' | 'ẵ' => 'a',
        'À' | 'Á' | 'Ạ' | 'Ả' | 'Ã' | 'Â' | 'Ầ' | 'Ấ' | 'Ậ' | 'Ẩ' | 'Ẫ' | 'Ă' | 'Ằ' | 'Ắ'
        | 'Ặ' | 'Ẳ' | 'Ẵ' => 'a',
        'è' | 'é' | 'ẹ' | 'ẻ' | 'ẽ' | 'ê' | 'ề' | 'ế' | 'ệ' | 'ể' | 'ễ' => 'e',
        'È' | 'É' | 'Ẹ' | 'Ẻ' | 'Ẽ' | 'Ê' | 'Ề' | 'Ế' | 'Ệ' | 'Ể' | 'Ễ' => 'e',
        'ì' | 'í' | 'ị' | 'ỉ' | 'ĩ' => 'i',
        'Ì' | 'Í' | 'Ị' | 'Ỉ' | 'Ĩ' => 'i',
        'ò' | 'ó' | 'ọ' | 'ỏ' | 'õ' | 'ô' | 'ồ' | 'ố' | 'ộ' | 'ổ' | 'ỗ' | 'ơ' | 'ờ' | 'ớ'
        | 'ợ' | 'ở' | 'ỡ' => 'o',
        'Ò' | 'Ó' | 'Ọ' | 'Ỏ' | 'Õ' | 'Ô' | 'Ồ' | 'Ố' | 'Ộ' | 'Ổ' | 'Ỗ' | 'Ơ' | 'Ờ' | 'Ớ'
        | 'Ợ' | 'Ở' | 'Ỡ' => 'o',
        'ù' | 'ú' | 'ụ' | 'ủ' | 'ũ' | 'ư' | 'ừ' | 'ứ' | 'ự' | 'ử' | 'ữ' => 'u',
        'Ù' | 'Ú' | 'Ụ' | 'Ủ' | 'Ũ' | 'Ư' | 'Ừ' | 'Ứ' | 'Ự' | 'Ử' | 'Ữ' => 'u',
        'ỳ' | 'ý' | 'ỵ' | 'ỷ' | 'ỹ' => 'y',
        'Ỳ' | 'Ý' | 'Ỵ' | 'Ỷ' | 'Ỹ' => 'y',
        'đ' => 'd',
        'Đ' => 'd',
        c => c.to_ascii_lowercase(),
    }
}

fn fold(s: &str) -> String {
    s.chars().map(fold_char).collect()
}

fn matches(filter: &str, haystack: &str) -> bool {
    let f = fold(filter.trim());
    f.is_empty() || fold(haystack).contains(&f)
}

// --- selection helpers ------------------------------------------------------

fn filtered_characters(app: &App, filter: &str) -> Vec<String> {
    match &app.roster {
        None => Vec::new(),
        Some(r) => r
            .characters
            .iter()
            .filter(|c| matches(filter, c))
            .cloned()
            .collect(),
    }
}

fn filtered_voices(app: &App, filter: &str) -> Vec<VoiceInfo> {
    match &app.roster {
        None => Vec::new(),
        Some(r) => r
            .voices
            .iter()
            .filter(|v| {
                matches(filter, &v.name)
                    || matches(filter, &v.gender)
                    || matches(filter, &v.accent)
                    || matches(filter, &v.style)
            })
            .cloned()
            .collect(),
    }
}

/// Characters currently speaking with `voice`.
fn users_of(cast: &BTreeMap<String, String>, voice: &str) -> Vec<String> {
    cast.iter()
        .filter(|(_, v)| v.as_str() == voice)
        .map(|(k, _)| k.clone())
        .collect()
}

// --- cast overview ----------------------------------------------------------

/// How one assignment sits against the accent policy.
///
/// `Blocked` and `Unknown` are deliberately distinct. A voice the roster lists
/// and the policy rejects is a *decision*; a voice the roster has never heard
/// of means the cast is stale, or the roster fell back to the offline table
/// because the sidecar is down. Reporting the second as the first would send
/// an operator hunting for a policy problem that does not exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// Assignable — the policy permits it, or it is an enrolled clone.
    Ok,
    /// The roster lists it and the accent policy rejects it.
    Blocked,
    /// Not in the roster at all.
    Unknown,
    /// No assignment yet.
    Unassigned,
}

/// One speaker's line in the cast overview.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CastRow {
    character: String,
    /// Empty when the speaker has no assignment yet.
    voice: String,
    gender: String,
    accent: String,
    /// The voice's style, used only as a filter key — the picker is where the
    /// catalogue is read, and the table has no room for a seventh column.
    style: String,
    /// The roster lists this voice at all.
    in_roster: bool,
    allowed: bool,
    enrolled: bool,
    /// Other speakers sharing this voice, sorted. Never counts unassigned
    /// speakers as sharing the empty voice.
    shared_with: Vec<String>,
}

impl CastRow {
    fn unassigned(&self) -> bool {
        self.voice.is_empty()
    }

    fn verdict(&self) -> Verdict {
        if self.voice.is_empty() {
            Verdict::Unassigned
        } else if self.allowed || self.enrolled {
            Verdict::Ok
        } else if self.in_roster {
            Verdict::Blocked
        } else {
            Verdict::Unknown
        }
    }

    /// A voice carrying more than one character is not an error — `Adam` alone
    /// voices eleven speakers here — but it is the thing worth noticing.
    fn shared(&self) -> bool {
        !self.shared_with.is_empty()
    }
}

/// Flatten a roster into one row per speaker.
///
/// Pure, so the duplicate and policy logic is testable without a terminal.
fn cast_rows(roster: &Roster) -> Vec<CastRow> {
    let meta: BTreeMap<&str, &VoiceInfo> =
        roster.voices.iter().map(|v| (v.name.as_str(), v)).collect();

    // The two sets are not always equal: a cast file can name a speaker no
    // script mentions any more, and `characters` can name one with no
    // assignment. The overview must show both.
    let mut names: Vec<String> = roster.characters.clone();
    for k in roster.cast.keys() {
        if !names.iter().any(|n| n == k) {
            names.push(k.clone());
        }
    }
    // `Narrator` is the one speaker whose position carries meaning — it is the
    // fallback voice — so it leads; everything else is alphabetical.
    names.sort_by_key(|n| (n != "Narrator", n.clone()));

    names
        .iter()
        .map(|name| {
            let voice = roster.cast.get(name).cloned().unwrap_or_default();
            let v = meta.get(voice.as_str()).copied();
            let shared_with = if voice.is_empty() {
                Vec::new()
            } else {
                let mut s: Vec<String> = roster
                    .cast
                    .iter()
                    .filter(|(k, val)| val.as_str() == voice && k.as_str() != name.as_str())
                    .map(|(k, _)| k.clone())
                    .collect();
                s.sort();
                s
            };
            CastRow {
                character: name.clone(),
                voice,
                gender: v.map(|x| x.gender.clone()).unwrap_or_default(),
                accent: v.map(|x| x.accent.clone()).unwrap_or_default(),
                style: v.map(|x| x.style.clone()).unwrap_or_default(),
                in_roster: v.is_some(),
                allowed: v.map(|x| x.allowed).unwrap_or(false),
                enrolled: v.map(|x| x.enrolled).unwrap_or(false),
                shared_with,
            }
        })
        .collect()
}

/// Diacritic-insensitive filter over the speaker, their voice, and that voice's
/// style, so `duc tri` finds everyone voiced by `Đức Trí` and `tin tuc` finds
/// every newsreader.
fn filtered_cast_rows(rows: &[CastRow], filter: &str) -> Vec<CastRow> {
    if filter.trim().is_empty() {
        return rows.to_vec();
    }
    rows.iter()
        .filter(|r| {
            matches(filter, &r.character)
                || matches(filter, &r.voice)
                || matches(filter, &r.style)
        })
        .cloned()
        .collect()
}

fn clamp_scroll(cursor: usize, scroll: &mut usize, len: usize, height: usize) {
    if height == 0 {
        *scroll = 0;
        return;
    }
    if cursor < *scroll {
        *scroll = cursor;
    } else if cursor >= *scroll + height {
        *scroll = cursor + 1 - height;
    }
    let max = len.saturating_sub(height);
    if *scroll > max {
        *scroll = max;
    }
}

// --- drawing ----------------------------------------------------------------

fn cell(text: String) -> Line<'static> {
    Line::from(text)
}

/// Colour-aware style as a free function, so render closures capture a plain
/// `bool` instead of the whole `App` — which the panes are also borrowing.
fn style_of(colour: bool, c: Color) -> Style {
    if colour {
        Style::default().fg(c)
    } else {
        Style::default()
    }
}

fn style_bold_of(colour: bool, c: Color) -> Style {
    if colour {
        Style::default().fg(c).add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    }
}

fn state_cell(colour: bool, text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        style_of(colour, state_color(text)),
    ))
}

/// A pane's "nothing here yet" body: centred, dim, and always actionable.
fn empty_body(lines: Vec<String>) -> Paragraph<'static> {
    let text: Vec<Line> = lines
        .into_iter()
        .map(|l| Line::from(Span::styled(l, Style::default().fg(Color::DarkGray))))
        .collect();
    Paragraph::new(text)
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true })
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

/// `centered`, but never flush against the frame edge. An overlay that touches
/// the border reads as a layout bug rather than a dialog.
fn centered_padded(area: Rect, w: u16, h: u16, pad: u16) -> Rect {
    centered(
        area,
        w.min(area.width.saturating_sub(pad * 2)),
        h.min(area.height.saturating_sub(pad * 2)),
    )
}

/// One-line task roll-up, rendered in the footer when the terminal is too short
/// for the Tasks pane. Collapsing the pane must not lose the numbers.
fn task_rollup(counts: &serde_json::Value, colour: bool) -> Line<'static> {
    let dim = Style::default().fg(Color::DarkGray);
    let Some(obj) = counts.as_object() else {
        return Line::from(Span::styled("tasks: waiting for the inductor…", dim));
    };
    if obj.is_empty() {
        return Line::from(Span::styled("tasks: none queued — press t to enqueue a range", dim));
    }
    let (mut done, mut total, mut failed, mut shelved) = (0u64, 0u64, 0u64, 0u64);
    for c in obj.values() {
        let get = |k: &str| c.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
        done += get("done");
        failed += get("failed");
        shelved += get("shelved");
        total += c
            .as_object()
            .map(|m| m.values().filter_map(|v| v.as_u64()).sum::<u64>())
            .unwrap_or(0);
    }
    let open = total.saturating_sub(done).saturating_sub(shelved);
    let mut spans = vec![
        Span::styled("tasks: ", dim),
        Span::styled(format!("{done}/{total} done"), style_of(colour, Color::Green)),
        Span::styled(format!("  · {open} open"), dim),
    ];
    if failed > 0 {
        spans.push(Span::styled(
            format!("  · {failed} failed"),
            style_of(colour, Color::Yellow),
        ));
    }
    if shelved > 0 {
        spans.push(Span::styled(
            format!("  · {shelved} shelved"),
            style_of(colour, Color::Red),
        ));
    }
    spans.push(Span::styled("   · resize for per-stage detail", dim));
    Line::from(spans)
}

/// The size guard: the only thing on screen when the terminal cannot hold the
/// dashboard. It names the requirement, the current size, and the way out.
fn draw_too_small(f: &mut ratatui::Frame, app: &App, area: Rect) {
    // On a sliver there is no room for a bordered box, and a blank screen would
    // be indistinguishable from a hang. One clipped line still says what is
    // wrong, which is the whole point of the guard.
    if area.width < 30 || area.height < 5 {
        f.render_widget(
            Paragraph::new(format!(
                "terminal too small — need {MIN_W}×{MIN_H}, have {}×{}",
                area.width, area.height
            ))
            .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }

    let dim = Style::default().fg(Color::DarkGray);
    let mut lines = vec![
        Line::from(Span::styled(
            "terminal too small for the dashboard",
            app.style_bold(Color::Yellow),
        )),
        Line::from(""),
        Line::from(format!(
            "need at least {MIN_W}×{MIN_H}, have {}×{}",
            area.width, area.height
        )),
        Line::from(""),
        Line::from(Span::styled(
            "resize the window — the dashboard returns on its own",
            dim,
        )),
        Line::from(Span::styled("q quits", dim)),
    ];
    // Say when a dialog is still open underneath: its keys stay live, so an
    // operator who shrank the terminal mid-prompt is not stranded.
    if app.dialog_open() {
        lines.push(Line::from(Span::styled(
            "a dialog is still open — Esc cancels it",
            app.style(Color::Cyan),
        )));
    }
    let box_ = centered_padded(area, 56, lines.len() as u16 + 2, 1);
    f.render_widget(Clear, box_);
    f.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(app.style(Color::Yellow)),
            ),
        box_,
    );
}

fn draw(f: &mut ratatui::Frame, app: &mut App) {
    let area = f.area();
    let size = size_class(area.width, area.height);
    if size == Size::TooSmall {
        // Nothing else is drawn: a clipped dashboard is worse than none.
        draw_too_small(f, app, area);
        return;
    }
    let compact = size == Size::Compact;

    // Compact gives up the Tasks pane — its numbers move to the footer — so
    // that Events keeps rows. Events is the pane that must stay readable.
    let (machines_h, workers_h) = if compact {
        (COMPACT_MACHINES_H, COMPACT_WORKERS_H)
    } else {
        (FULL_MACHINES_H, FULL_WORKERS_H)
    };
    let constraints: Vec<Constraint> = if compact {
        vec![
            Constraint::Length(machines_h),
            Constraint::Length(workers_h),
            Constraint::Min(COMPACT_EVENTS_MIN_H),
            Constraint::Length(COMPACT_FOOTER_H),
        ]
    } else {
        vec![
            Constraint::Length(machines_h),
            Constraint::Length(workers_h),
            Constraint::Length(FULL_TASKS_H),
            Constraint::Min(FULL_EVENTS_MIN_H),
            Constraint::Length(FULL_FOOTER_H),
        ]
    };
    let root = RLayout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    draw_machines(f, app, root[0], compact);
    draw_workers(f, app, root[1], compact);
    if compact {
        draw_events(f, app, root[2]);
        draw_footer(f, app, root[3], true);
    } else {
        draw_tasks(f, app, root[2]);
        draw_events(f, app, root[3]);
        draw_footer(f, app, root[4], false);
    }

    // Overlays paint last and cover everything beneath them. They are rendered
    // even in the compact tier: a confirmation must stay answerable.
    match app.screen.clone() {
        Screen::Help { scroll } => draw_help(f, app, scroll),
        Screen::Text(p) => draw_text_prompt(f, app, &p),
        Screen::Pick(p) => draw_picker(f, app, &p),
        Screen::Cast(v) => draw_cast(f, app, &v),
        Screen::Confirm(c) => draw_confirm(f, app, &c),
        Screen::Machine(addr) => draw_machine_info(f, app, &addr),
        _ => {}
    }
}

fn draw_machines(f: &mut ratatui::Frame, app: &mut App, area: Rect, compact: bool) {
    let disconnected = matches!(app.conn, Conn::Down(_));
    let colour = app.colour;
    let selected = app.selected;
    let title = if disconnected { "Machines — DISCONNECTED" } else { "Machines" };
    let border = if disconnected {
        style_of(colour, Color::Red)
    } else {
        Style::default()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border)
        .title(title);

    if app.machines.is_empty() {
        let mut body = vec!["no machines in the cluster".to_string()];
        match &app.conn {
            Conn::Down(e) => {
                body.push(e.clone());
                body.push("is the inductor running?  make serve".into());
            }
            _ => body.push("press a to add one by IP or hostname".into()),
        }
        f.render_widget(empty_body(body).block(block), area);
        return;
    }

    let height = area.height.saturating_sub(3) as usize;
    let len = app.machines.len();
    clamp_scroll(selected, &mut app.machine_scroll, len, height);
    let start = app.machine_scroll;
    let end = (start + height).min(len);

    let rows: Vec<Row> = app.machines[start..end]
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let idx = start + i;
            let cursor = if idx == selected { "▸ " } else { "  " };
            let mut cells = vec![
                cell(format!("{cursor}{}", m.id)),
                cell(m.addr.clone()),
                cell(m.role.clone()),
                state_cell(colour, m.state.as_str()),
            ];
            // The tts column is the widest and the least urgent; in the compact
            // tier it is the first thing to go, so the remaining columns keep
            // their full width instead of all clipping together.
            if !compact {
                cells.push(cell(m.tts_url.clone().unwrap_or_else(|| "—".into())));
            }
            cells.push(cell(seen_label(m)));
            let mut row = Row::new(cells);
            if idx == selected {
                row = row.style(Style::default().add_modifier(Modifier::REVERSED));
            }
            row
        })
        .collect();

    let mut header = vec!["id", "addr", "role", "state"];
    let mut widths: Vec<Constraint> = if compact {
        // Taken from the constant the compile-time guard checks.
        COMPACT_MACHINE_COLS[..4].iter().map(|w| Constraint::Length(*w)).collect()
    } else {
        vec![
            Constraint::Length(16),
            Constraint::Length(15),
            Constraint::Length(8),
            Constraint::Length(13),
        ]
    };
    if !compact {
        header.push("tts");
        widths.push(Constraint::Length(22));
    }
    header.push("seen");
    widths.push(Constraint::Length(if compact { COMPACT_MACHINE_COLS[4] } else { 8 }));

    let table = Table::new(rows, widths)
        .header(Row::new(header).style(style_bold_of(colour, Color::Gray)))
        .block(block);
    f.render_widget(table, area);
}

fn draw_workers(f: &mut ratatui::Frame, app: &App, area: Rect, compact: bool) {
    let block = Block::default().borders(Borders::ALL).title("Workers");
    if app.beats.is_empty() {
        f.render_widget(
            empty_body(vec![
                "no workers connected".into(),
                "start one with:  bm-agent worker --inductor <this host>".into(),
            ])
            .block(block),
            area,
        );
        return;
    }

    let colour = app.colour;
    let rows: Vec<Row> = app
        .beats
        .iter()
        .map(|b| {
            let st = b.stage.map(|s| s.as_str().to_string()).unwrap_or_else(|| "—".into());
            let ch = b.chapter.map(|c| c.to_string()).unwrap_or_else(|| "—".into());
            let pct = (b.progress.clamp(0.0, 1.0) * 100.0).round() as u32;
            let stage_line = Line::from(Span::styled(
                st.clone(),
                style_of(colour, stage_color(&st)),
            ));
            let mut cells = vec![cell(b.worker_id.clone())];
            // The machine column is derivable from the Machines pane; the
            // activity string is not, so the machine column goes first.
            if !compact {
                cells.push(cell(if b.hostname.is_empty() {
                    b.addr.clone()
                } else {
                    b.hostname.clone()
                }));
            }
            cells.extend([
                stage_line,
                cell(ch),
                cell(format!("{} {:>3}%", bar(b.progress, 10), pct)),
                cell(if b.activity.is_empty() { "—".into() } else { b.activity.clone() }),
                cell(b.eta_secs.map(bm_core::eta::human).unwrap_or_else(|| "—".into())),
            ]);
            Row::new(cells)
        })
        .collect();

    let mut header = vec!["worker"];
    let mut widths: Vec<Constraint> = vec![Constraint::Length(14)];
    if !compact {
        header.push("machine");
        widths.push(Constraint::Length(14));
    }
    header.extend(["stage", "ch", "progress", "activity", "eta"]);
    if compact {
        // From the constant the compile-time guard checks; the activity column
        // is the one that absorbs any slack on a wider terminal.
        widths.extend(COMPACT_WORKER_COLS[1..].iter().enumerate().map(|(i, w)| {
            if i == 3 {
                Constraint::Min(*w)
            } else {
                Constraint::Length(*w)
            }
        }));
    } else {
        widths.extend([
            Constraint::Length(8),
            Constraint::Length(5),
            Constraint::Length(17),
            Constraint::Min(20),
            Constraint::Length(8),
        ]);
    }

    let table = Table::new(rows, widths)
        .header(Row::new(header).style(style_bold_of(colour, Color::Gray)))
        .block(block);
    f.render_widget(table, area);
}

fn draw_tasks(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title("Tasks");
    let mut lines: Vec<Line> = Vec::new();

    match app.counts.as_object() {
        None => {
            lines.push(Line::from(Span::styled(
                "waiting for the inductor…",
                Style::default().fg(Color::DarkGray),
            )));
        }
        Some(obj) if obj.is_empty() => {
            lines.push(Line::from(Span::styled(
                "no tasks queued",
                Style::default().fg(Color::DarkGray),
            )));
            lines.push(Line::from(Span::styled(
                "press t to enqueue a chapter range",
                Style::default().fg(Color::DarkGray),
            )));
        }
        Some(obj) => {
            let mut stages: Vec<&String> = obj.keys().collect();
            stages.sort();
            for st in stages {
                let c = &obj[st.as_str()];
                let get = |k: &str| c.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
                let done = get("done");
                let shelved = get("shelved");
                let failed = get("failed");
                let total: u64 = c
                    .as_object()
                    .map(|m| m.values().filter_map(|v| v.as_u64()).sum())
                    .unwrap_or(0);
                let open = total.saturating_sub(done).saturating_sub(shelved);
                let mut spans = vec![
                    Span::styled(format!("{st:8}"), app.style_bold(stage_color(st))),
                    Span::raw(format!("{done}/{total} done")),
                    Span::styled(
                        format!("  · {open} open"),
                        Style::default().fg(Color::DarkGray),
                    ),
                ];
                if failed > 0 {
                    spans.push(Span::styled(
                        format!("  · {failed} failed"),
                        app.style(Color::Yellow),
                    ));
                }
                if shelved > 0 {
                    spans.push(Span::styled(
                        format!("  · {shelved} shelved"),
                        app.style(Color::Red),
                    ));
                }
                lines.push(Line::from(spans));
            }
        }
    }

    let mut shelved: Vec<String> = app
        .tasks
        .iter()
        .filter(|t| t.state == TaskState::Shelved)
        .map(|t| format!("{}:{}", t.stage, t.chapter))
        .collect();
    shelved.sort();
    shelved.dedup();
    if !shelved.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("shelved: {}", shelved.join(" ")),
            app.style(Color::Red),
        )));
    }

    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_events(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title("Events");
    if app.events.is_empty() {
        f.render_widget(
            empty_body(vec!["nothing has happened yet".into()]).block(block),
            area,
        );
        return;
    }

    let height = area.height.saturating_sub(2) as usize;
    let total = app.events.len();
    // Anchor to the newest line: events are appended at the back, and a list
    // rendered from the front hides exactly the lines you just caused.
    let offset = app.events_scroll.min(total.saturating_sub(height.min(total)));
    let end = total - offset;
    let start = end.saturating_sub(height);

    let colour = app.colour;
    let lines: Vec<Line> = app
        .events
        .iter()
        .skip(start)
        .take(end - start)
        .map(|l| {
            Line::from(vec![
                Span::styled(
                    format!("[{}] ", stamp(l.at)),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(format!("{} ", l.level.glyph()), style_of(colour, l.level.color())),
                Span::styled(l.text.clone(), style_of(colour, l.level.color())),
            ])
        })
        .collect();

    let title = if offset > 0 {
        format!("Events — {offset} line(s) back · G for newest")
    } else {
        "Events".to_string()
    };
    let block = block.title(title);
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_footer(f: &mut ratatui::Frame, app: &App, area: Rect, compact: bool) {
    let dim = Style::default().fg(Color::DarkGray);
    // Two lines: one was 161 characters and clipped on every terminal, losing
    // exactly the keys nobody can guess.
    let keys: Vec<Line> = if compact { KEYS_COMPACT } else { KEYS_FULL }
        .iter()
        .map(|k| Line::from(Span::styled(*k, dim)))
        .collect();

    // The status line always reports, in order: what just happened, how many
    // jobs are still running, whether the inductor is reachable.
    let mut spans = vec![
        Span::styled(
            format!("{} ", app.status.level.glyph()),
            app.style(app.status.level.color()),
        ),
        Span::styled(app.status.text.clone(), app.style(app.status.level.color())),
    ];
    if app.pending > 0 {
        spans.push(Span::styled(
            format!("   ⏳ {} job(s) running", app.pending),
            app.style(Color::Yellow),
        ));
    }
    match &app.conn {
        Conn::Up => {
            let ago = app
                .refreshed
                .map(|t| t.elapsed().as_secs())
                .unwrap_or(0);
            spans.push(Span::styled(
                format!("   ● live ({ago}s ago)"),
                app.style(Color::Green),
            ));
        }
        Conn::Down(_) => spans.push(Span::styled("   ● disconnected", app.style(Color::Red))),
        Conn::Unknown => spans.push(Span::styled("   ● connecting…", app.style(Color::Yellow))),
    }
    if !app.colour {
        spans.push(Span::styled("   [mono]", Style::default().fg(Color::DarkGray)));
    }
    if let Some(engine) = app.settings.as_ref().and_then(|s| s.get("engine")).and_then(|e| e.as_str()) {
        spans.push(Span::styled(
            format!("   engine: {engine}"),
            Style::default().fg(Color::DarkGray),
        ));
    }

    let mut lines = keys;
    lines.push(Line::from(spans));
    if compact {
        // The Tasks pane is gone in this tier; the roll-up takes its place so
        // the counts are never simply missing.
        lines.push(task_rollup(&app.counts, app.colour));
    }
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::NONE)),
        area,
    );
}

fn draw_help(f: &mut ratatui::Frame, app: &App, scroll: usize) {
    let area = centered_padded(f.area(), 84, 32, 1);
    f.render_widget(Clear, area);

    let bold = app.style_bold(Color::White);
    let dim = Style::default().fg(Color::DarkGray);
    let mut lines: Vec<Line> = Vec::new();
    let section = |lines: &mut Vec<Line>, name: &str| {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(name.to_string(), bold)));
    };

    section(&mut lines, "Navigation");
    for (k, v) in [
        ("↑ ↓  k j", "move the machine cursor"),
        ("PgUp PgDn", "scroll the event log   (G returns to newest)"),
        ("r", "refresh now"),
        ("?", "this help"),
        ("C", "toggle colour (state names are always shown, so nothing depends on colour)"),
        ("q", "quit"),
    ] {
        lines.push(Line::from(vec![
            Span::styled(format!("  {k:<12}"), app.style(Color::Cyan)),
            Span::raw(v.to_string()),
        ]));
    }

    section(&mut lines, "Cluster");
    for (k, v) in [
        ("a", "add a machine by IP or hostname"),
        ("p", "provision the selected machine"),
        ("P", "re-provision it, forcing past the skip-if-configured check"),
        ("d", "drop the selected machine from the cluster registry"),
        ("i", "inspect the selected machine (probe output, capabilities)"),
    ] {
        lines.push(Line::from(vec![
            Span::styled(format!("  {k:<12}"), app.style(Color::Cyan)),
            Span::raw(v.to_string()),
        ]));
    }

    section(&mut lines, "Pipeline operations");
    for (k, v) in [
        ("t  translate", "enqueue crawl + digest for a chapter range"),
        ("c  crawl-setup", "save the URL template, then probe-crawl one chapter"),
        ("v  voices", "re-read the roster, enforce the accent policy, refill gaps"),
        ("A  add-sample", "pool a clip from refs/ — tags come from the filename"),
        ("s  swap-voice", "repoint one character — destructive, see below"),
        ("S  cast", "every speaker × voice, flagging shared voices and policy problems"),
        ("e  eta", "estimate the remaining wall-clock time"),
    ] {
        lines.push(Line::from(vec![
            Span::styled(format!("  {k:<14}"), app.style(Color::Cyan)),
            Span::raw(v.to_string()),
        ]));
    }

    section(&mut lines, "Voice picker (s) and cast overview (S)");
    for v in [
        "Step 1 picks a character, step 2 picks a voice. S shows the whole cast",
        "at once, and Enter there jumps straight to step 2 for that speaker.",
        "Type to filter. Accents are ignored, so \"thai son\" finds \"Thái Sơn\".",
        "Movement is arrow keys only, so every letter reaches the filter.",
        "Every voice is listed with gender, accent, language and style, plus whether",
        "it is already in use and whether the accent policy permits it.",
        "Tab auditions the highlighted voice into data/previews/<voice>.wav.",
        "Pooled samples show their tags (pool: young, female) — type one to filter.",
        "Enter advances or applies; Esc goes back one step.",
    ] {
        lines.push(Line::from(Span::styled(format!("  {v}"), dim)));
    }

    section(&mut lines, "Notes");
    for v in [
        "Swap voice deletes only that speaker's cached segments, drops the stale",
        "mp3s and requeues render + merge. Every other character keeps its cache.",
        "VieNeu presets are Central/South only — Northern voices are rejected by",
        "policy. Enrolled clones always pass, because they were vetted on enrolment.",
        "Below 100x30 the Tasks pane folds into the footer so Events keeps its rows;",
        "below 76x20 the dashboard is replaced by a size notice, because a clipped",
        "dashboard is worse than an honest one.",
        "Jobs run in the background: the interface never blocks, and a second copy of",
        "the same operation is refused while the first is still in flight.",
    ] {
        lines.push(Line::from(Span::styled(format!("  {v}"), dim)));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.style(Color::Cyan))
        .title("Help — Esc or ? to close · ↑↓ scroll");
    let inner_h = area.height.saturating_sub(2) as usize;
    let max = lines.len().saturating_sub(inner_h);
    let offset = scroll.min(max) as u16;
    f.render_widget(
        Paragraph::new(lines).block(block).scroll((offset, 0)),
        area,
    );
}

fn draw_picker(f: &mut ratatui::Frame, app: &mut App, picker: &Picker) {
    let area = centered(f.area(), 96, 24);
    f.render_widget(Clear, area);

    let step = match picker.stage {
        PickStage::Character => "step 1 of 2 — choose a character",
        PickStage::Voice => "step 2 of 2 — choose a voice",
    };
    let title = match picker.stage {
        PickStage::Character => format!("Swap voice · {step}"),
        PickStage::Voice => format!("Swap voice · {step} · for “{}”", picker.character),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.style(Color::Cyan))
        .title(title);

    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 4 {
        return;
    }
    let rows = RLayout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // filter
            Constraint::Length(1), // provenance / policy
            Constraint::Min(1),    // list
            Constraint::Length(2), // hints
        ])
        .split(inner);

    // Filter line.
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("filter: ", Style::default().fg(Color::DarkGray)),
            Span::styled(picker.filter.clone(), app.style(Color::White)),
            Span::styled("▌", app.style(Color::Cyan)),
        ])),
        rows[0],
    );

    // Provenance: never let a fallback roster masquerade as the live one.
    let provenance = match (&app.roster, &app.roster_error, app.roster_loading) {
        (_, _, true) => Line::from(Span::styled(
            "loading roster from the inductor…",
            app.style(Color::Yellow),
        )),
        (_, Some(e), _) => Line::from(Span::styled(
            format!("roster unavailable: {e}   (Esc to close, R to retry)"),
            app.style(Color::Red),
        )),
        (Some(r), _, _) => {
            let (label, colour) = if r.source == "live" {
                ("live roster", Color::Green)
            } else {
                ("OFFLINE roster — metadata may be incomplete", Color::Yellow)
            };
            Line::from(vec![
                Span::styled(format!("{label} · engine {}   ", r.engine), app.style(colour)),
                Span::styled(r.policy_note.clone(), Style::default().fg(Color::DarkGray)),
            ])
        }
        (None, None, _) => Line::from(Span::styled(
            "roster not loaded — press R",
            app.style(Color::Yellow),
        )),
    };
    f.render_widget(Paragraph::new(provenance), rows[1]);

    let height = rows[2].height as usize;
    let colour = app.colour;
    // Precomputed so the row closures below capture plain data rather than a
    // borrow of `app`, which is also being borrowed for the roster itself.
    let cast: BTreeMap<String, String> = app
        .roster
        .as_ref()
        .map(|r| r.cast.clone())
        .unwrap_or_default();
    let meta: BTreeMap<String, VoiceInfo> = app
        .roster
        .as_ref()
        .map(|r| r.voices.iter().map(|v| (v.name.clone(), v.clone())).collect())
        .unwrap_or_default();
    match picker.stage {
        PickStage::Character => {
            let list = filtered_characters(app, &picker.filter);
            if list.is_empty() {
                let msg = if app.roster.is_none() {
                    "no roster yet".to_string()
                } else if picker.filter.trim().is_empty() {
                    "no speakers known yet — run t (translate) or v (voices) first".to_string()
                } else {
                    format!(
                        "no speaker matches “{}” — Enter accepts it as a new character",
                        picker.filter.trim()
                    )
                };
                f.render_widget(empty_body(vec![msg]).wrap(Wrap { trim: true }), rows[2]);
            } else {
                let mut scroll = picker.scroll;
                clamp_scroll(picker.cursor, &mut scroll, list.len(), height);
                let items: Vec<Line> = list
                    .iter()
                    .enumerate()
                    .skip(scroll)
                    .take(height)
                    .map(|(i, name)| {
                        let selected = i == picker.cursor;
                        let marker = if selected { "▸ " } else { "  " };
                        let current = cast.get(name).cloned().unwrap_or_default();
                        let mut spans = vec![
                            Span::styled(marker.to_string(), style_of(colour, Color::Cyan)),
                            Span::styled(
                                format!("{name:<28}"),
                                if selected {
                                    style_bold_of(colour, Color::White)
                                } else {
                                    Style::default()
                                },
                            ),
                        ];
                        if current.is_empty() {
                            spans.push(Span::styled(
                                "unassigned — v (voices) fills gaps".to_string(),
                                Style::default().fg(Color::DarkGray),
                            ));
                        } else {
                            spans.push(Span::styled(
                                format!("{current:<14}"),
                                style_of(colour, Color::Green),
                            ));
                            if let Some(v) = meta.get(&current) {
                                spans.push(Span::styled(
                                    format!(
                                        "{} · {} · {}",
                                        gender_label(&v.gender),
                                        dash_if_empty(&v.accent),
                                        v.language
                                    ),
                                    Style::default().fg(Color::DarkGray),
                                ));
                            }
                        }
                        Line::from(spans)
                    })
                    .collect();
                f.render_widget(Paragraph::new(items), rows[2]);
            }
        }
        PickStage::Voice => {
            let list = filtered_voices(app, &picker.filter);
            if list.is_empty() {
                f.render_widget(
                    empty_body(vec![
                        "no voice matches that filter".to_string(),
                        "Esc goes back to the character list".to_string(),
                    ])
                    .wrap(Wrap { trim: true }),
                    rows[2],
                );
            } else {
                let mut scroll = picker.scroll;
                clamp_scroll(picker.cursor, &mut scroll, list.len(), height);
                let items: Vec<Line> = list
                    .iter()
                    .enumerate()
                    .skip(scroll)
                    .take(height)
                    .map(|(i, v)| {
                        let selected = i == picker.cursor;
                        let marker = if selected { "▸ " } else { "  " };
                        let users = users_of(&cast, &v.name);
                        let (status, colour_of_status) =
                            if users.contains(&picker.character) {
                                ("current".to_string(), Color::Green)
                            } else if !users.is_empty() {
                                (format!("in use: {}", users.join(", ")), Color::Yellow)
                            } else if !v.allowed {
                                ("blocked by accent policy".to_string(), Color::Red)
                            } else {
                                ("available".to_string(), Color::DarkGray)
                            };
                        let mut spans = vec![
                            Span::styled(marker.to_string(), style_of(colour, Color::Cyan)),
                            Span::styled(
                                format!("{:<14}", v.name),
                                if selected {
                                    style_bold_of(colour, Color::White)
                                } else if v.allowed {
                                    Style::default()
                                } else {
                                    Style::default().fg(Color::DarkGray)
                                },
                            ),
                            Span::styled(
                                format!("{:<8}", gender_label(&v.gender)),
                                Style::default().fg(Color::DarkGray),
                            ),
                            Span::styled(
                                format!("{:<14}", dash_if_empty(&v.accent)),
                                Style::default().fg(Color::DarkGray),
                            ),
                            Span::styled(
                                format!("{:<7}", v.language),
                                Style::default().fg(Color::DarkGray),
                            ),
                            Span::styled(
                                format!("{:<16}", dash_if_empty(&v.style)),
                                Style::default().fg(Color::DarkGray),
                            ),
                        ];
                        if v.enrolled {
                            spans.push(Span::styled("clone ", style_of(colour, Color::Magenta)));
                        }
                        if picker.previewing.as_deref() == Some(v.name.as_str()) {
                            spans.push(Span::styled(
                                "auditioning… ",
                                style_of(colour, Color::Yellow),
                            ));
                        } else if picker.previewed.iter().any(|p| p == &v.name) {
                            spans.push(Span::styled("auditioned ", style_of(colour, Color::Green)));
                        }
                        spans.push(Span::styled(status, style_of(colour, colour_of_status)));
                        Line::from(spans)
                    })
                    .collect();
                f.render_widget(Paragraph::new(items), rows[2]);
            }
        }
    }

    // Hint rows, split by stage so the available keys are always accurate.
    let hints: Vec<Line> = match picker.stage {
        PickStage::Character => vec![
            Line::from(Span::styled(
                "type to filter · ↑↓ move · Enter choose · Esc close · R reload roster",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                "Enter on a non-matching name adds it as a new character",
                Style::default().fg(Color::DarkGray),
            )),
        ],
        PickStage::Voice => vec![
            Line::from(Span::styled(
                "type to filter · ↑↓ move · Enter assign · Tab audition · Esc back",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                "blocked voices are shown for completeness but the inductor will reject them",
                Style::default().fg(Color::DarkGray),
            )),
        ],
    };
    f.render_widget(Paragraph::new(hints), rows[3]);
}

/// The whole cast in one table: speaker, voice, that voice's metadata, and how
/// the assignment stands against the policy and the rest of the cast.
fn draw_cast(f: &mut ratatui::Frame, app: &App, view: &CastView) {
    // In the compact tier the overlay takes the whole screen: a 108-wide table
    // centred in a 76-column terminal loses 32 columns to margins it cannot
    // spare.
    let compact = size_class(f.area().width, f.area().height) == Size::Compact;
    let area = if compact {
        f.area()
    } else {
        centered_padded(f.area(), 108, 30, 2)
    };
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.style(Color::Cyan))
        .title("Cast · vi-VN — Esc to close · Enter picks a new voice for the highlighted speaker");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 4 {
        return;
    }

    let rows_area = RLayout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // summary
            Constraint::Length(1), // filter
            Constraint::Min(1),    // table
            Constraint::Length(2), // hints
        ])
        .split(inner);

    let all = app.cast_rows();
    let list = filtered_cast_rows(&all, &view.filter);

    // Summary: the health of the cast in one line, before any row is read.
    let mut in_use: BTreeMap<&str, usize> = BTreeMap::new();
    for r in &all {
        if !r.voice.is_empty() {
            *in_use.entry(r.voice.as_str()).or_insert(0) += 1;
        }
    }
    let shared_voices = in_use.values().filter(|c| **c > 1).count();
    let unassigned = all.iter().filter(|r| r.unassigned()).count();
    let flagged = all
        .iter()
        .filter(|r| matches!(r.verdict(), Verdict::Blocked | Verdict::Unknown))
        .count();

    let mut summary = vec![
        Span::styled(
            format!("{} speakers", all.len()),
            app.style_bold(Color::White),
        ),
        Span::styled(
            format!("  ·  {} voices in use", in_use.len()),
            Style::default().fg(Color::DarkGray),
        ),
    ];
    if shared_voices > 0 {
        summary.push(Span::styled(
            format!("  ·  {shared_voices} shared"),
            app.style(Color::Yellow),
        ));
    }
    if unassigned > 0 {
        summary.push(Span::styled(
            format!("  ·  {unassigned} unassigned"),
            app.style(Color::Yellow),
        ));
    }
    if flagged > 0 {
        summary.push(Span::styled(
            format!("  ·  {flagged} to fix"),
            app.style_bold(Color::Red),
        ));
    } else if !all.is_empty() {
        summary.push(Span::styled("  ·  all assignments valid", app.style(Color::Green)));
    }
    if let Some(r) = &app.roster {
        // Only when there is room: on a narrow terminal the provenance would
        // push the health summary — the reason the screen exists — off the end.
        if !compact {
            summary.push(Span::styled(
                format!("   [{} · engine {}]", r.source, r.engine),
                Style::default().fg(Color::DarkGray),
            ));
        }
    }
    f.render_widget(Paragraph::new(Line::from(summary)), rows_area[0]);

    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("filter: ", Style::default().fg(Color::DarkGray)),
            Span::styled(view.filter.clone(), app.style(Color::White)),
            Span::styled("▌", app.style(Color::Cyan)),
        ])),
        rows_area[1],
    );

    // Header + two borders are inside `rows_area[2]`; only what is left can
    // hold rows.
    let body = rows_area[2].height.saturating_sub(3) as usize;
    if all.is_empty() {
        let msg = if app.roster.is_none() {
            if app.roster_loading {
                "loading the roster…".to_string()
            } else {
                "roster not loaded — press R".to_string()
            }
        } else {
            "no speakers known yet — run t (translate) or v (voices) first".to_string()
        };
        f.render_widget(empty_body(vec![msg]).wrap(Wrap { trim: true }), rows_area[2]);
    } else if list.is_empty() {
        f.render_widget(
            empty_body(vec![format!(
                "no speaker or voice matches “{}” — Backspace clears it",
                view.filter.trim()
            )])
            .wrap(Wrap { trim: true }),
            rows_area[2],
        );
    } else if body > 0 {
        let colour = app.colour;
        // Pick the column set from the width the table actually gets, not from
        // the terminal: the overlay has its own borders to pay for.
        let table_w = rows_area[2].width.saturating_sub(2);
        let wide = table_w >= cols(&CAST_COLS_WIDE);
        let speaker_w = if wide { CAST_COLS_WIDE[0] } else { CAST_COLS_NARROW[0] } as usize;
        let voice_w = if wide { CAST_COLS_WIDE[1] } else { CAST_COLS_NARROW[1] } as usize;
        let accent_w = if wide { CAST_COLS_WIDE[3] } else { CAST_COLS_NARROW[2] } as usize;

        let mut scroll = view.scroll;
        clamp_scroll(view.cursor, &mut scroll, list.len(), body);
        let rows: Vec<Row> = list
            .iter()
            .enumerate()
            .skip(scroll)
            .take(body)
            .map(|(i, r)| {
                let selected = i == view.cursor;
                let marker = if selected { "▸ " } else { "  " };
                let mut voice_cells = vec![Span::styled(
                    format!(
                        "{:<width$}",
                        dash_if_empty(&r.voice),
                        width = voice_w.saturating_sub(6)
                    ),
                    if r.unassigned() {
                        Style::default().fg(Color::DarkGray)
                    } else {
                        style_of(colour, Color::Green)
                    },
                )];
                if r.enrolled {
                    voice_cells.push(Span::styled(" clone", style_of(colour, Color::Magenta)));
                }
                let (status, status_colour) = match r.verdict() {
                    Verdict::Unassigned => ("unassigned — v fills gaps".to_string(), Color::DarkGray),
                    Verdict::Blocked => ("blocked by the accent policy".to_string(), Color::Red),
                    Verdict::Unknown => ("unknown voice — stale cast?".to_string(), Color::Red),
                    Verdict::Ok if r.shared() => (
                        format!(
                            "shared with {} other{}",
                            r.shared_with.len(),
                            if r.shared_with.len() == 1 { "" } else { "s" }
                        ),
                        Color::Yellow,
                    ),
                    Verdict::Ok => ("ok".to_string(), Color::DarkGray),
                };
                let mut cells = vec![cell(format!(
                    "{marker}{:<width$}",
                    bm_core::util::head_chars(&r.character, speaker_w - 2),
                    width = speaker_w - 2
                ))];
                cells.push(Line::from(voice_cells));
                if wide {
                    cells.push(cell(format!("{:<7}", gender_label(&r.gender))));
                }
                cells.push(cell(format!(
                    "{:<width$}",
                    dash_if_empty(&r.accent),
                    width = accent_w
                )));
                cells.push(Line::from(Span::styled(status, style_of(colour, status_colour))));
                let mut row = Row::new(cells);
                if selected {
                    row = row.style(Style::default().add_modifier(Modifier::REVERSED));
                }
                row
            })
            .collect();

        let title = if list.len() > body {
            format!("Cast — showing {} of {}", body.min(list.len()), list.len())
        } else {
            "Cast".to_string()
        };
        // The status column is the flexible one: it is the only column whose
        // text length varies with the verdict.
        let mut header = vec!["speaker", "voice"];
        let mut widths: Vec<Constraint> = vec![
            Constraint::Length(CAST_COLS_NARROW[0]),
            Constraint::Length(CAST_COLS_NARROW[1]),
        ];
        if wide {
            header.push("gender");
            widths[0] = Constraint::Length(CAST_COLS_WIDE[0]);
            widths[1] = Constraint::Length(CAST_COLS_WIDE[1]);
            widths.push(Constraint::Length(CAST_COLS_WIDE[2]));
        }
        header.push("accent");
        widths.push(Constraint::Length(if wide {
            CAST_COLS_WIDE[3]
        } else {
            CAST_COLS_NARROW[2]
        }));
        header.push("status");
        widths.push(Constraint::Min(if wide {
            CAST_COLS_WIDE[4]
        } else {
            CAST_COLS_NARROW[3]
        }));

        let table = Table::new(rows, widths)
            .header(Row::new(header).style(style_bold_of(colour, Color::Gray)))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray))
                    .title(title),
            );
        f.render_widget(table, rows_area[2]);
    }

    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                "type to filter · ↑↓ move · Enter choose a new voice · Esc close · R reload roster",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                "blocked = the accent policy rejects it · unknown = the roster has never heard of it",
                Style::default().fg(Color::DarkGray),
            )),
        ]),
        rows_area[3],
    );
}

fn draw_text_prompt(f: &mut ratatui::Frame, app: &App, prompt: &TextPrompt) {
    let area = centered(f.area(), 88, 8);
    f.render_widget(Clear, area);
    let (before, after) = prompt.split();
    let body = vec![
        Line::from(Span::styled(
            prompt.hint.clone(),
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
        // What is on screen is exactly what will be submitted — the prompt is
        // the echo, so nothing is sent that the operator did not read back.
        Line::from(vec![
            Span::styled("> ", style_of(app.colour, Color::Cyan)),
            Span::styled(before, style_of(app.colour, Color::White)),
            Span::styled("▌", style_of(app.colour, Color::Cyan)),
            Span::raw(after),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "Enter submit · Esc cancel · ←→ move · Home/End · Ctrl-U clear · Ctrl-W delete word",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    f.render_widget(
        Paragraph::new(body)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(style_of(app.colour, Color::Cyan))
                    .title(prompt.title.clone()),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_confirm(f: &mut ratatui::Frame, app: &App, c: &Confirm) {
    let width = 76.min(f.area().width);
    let height = (c.body.len() as u16 + 5).min(f.area().height);
    let area = centered(f.area(), width, height);
    f.render_widget(Clear, area);

    let colour = if c.danger { Color::Red } else { Color::Cyan };
    let mut lines: Vec<Line> = c
        .body
        .iter()
        .map(|l| Line::from(Span::styled(l.clone(), Style::default())))
        .collect();
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("Enter / y ", app.style(Color::Green)),
        Span::raw("confirm    "),
        Span::styled("Esc / n ", app.style(Color::Red)),
        Span::raw("cancel"),
    ]));

    f.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(app.style(colour))
                    .title(c.title.clone()),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_machine_info(f: &mut ratatui::Frame, app: &App, addr: &str) {
    let area = centered(f.area(), 84, 18);
    f.render_widget(Clear, area);

    let Some(m) = app.machine_by_addr(addr) else {
        f.render_widget(
            Paragraph::new("that machine is no longer in the registry")
                .block(Block::default().borders(Borders::ALL).title("Machine")),
            area,
        );
        return;
    };

    let kv = |k: &str, v: String| {
        Line::from(vec![
            Span::styled(format!("  {k:<14}"), app.style(Color::Cyan)),
            Span::raw(v),
        ])
    };
    let mut lines = vec![
        kv("id", m.id.clone()),
        kv("addr", m.addr.clone()),
        kv("role", m.role.clone()),
        kv("state", m.state.as_str().to_string()),
        kv("ssh", m.ssh_target()),
        kv("ssh port", m.ssh_port.to_string()),
        kv("ssh key", m.ssh_key.clone().unwrap_or_else(|| "default".into())),
        kv("tts", m.tts_url.clone().unwrap_or_else(|| "—".into())),
        kv("last seen", seen_label(m)),
        kv(
            "capabilities",
            if m.capabilities.is_empty() {
                "—".into()
            } else {
                m.capabilities.join(", ")
            },
        ),
        Line::from(""),
        Line::from(Span::styled(
            "  note (probe / provision output)",
            app.style_bold(Color::White),
        )),
    ];
    if m.note.trim().is_empty() {
        lines.push(Line::from(Span::styled(
            "  — nothing recorded yet; press p to provision",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for l in m.note.lines() {
            lines.push(Line::from(format!("  {l}")));
        }
    }

    f.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(app.style(Color::Cyan))
                    .title("Machine — Esc or i to close"),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

// --- background execution ---------------------------------------------------

async fn run_job(job: Job, tx: tokio::sync::mpsc::UnboundedSender<Ev>) {
    let send = |level: Level, text: String| {
        let _ = tx.send(Ev::Log(LogLine {
            level,
            at: Duration::ZERO,
            text,
        }));
    };
    match job {
        Job::Provision { layout_root, machine, force } => {
            let addr = machine.addr.clone();
            let layout = bm_core::Layout::new(&layout_root);
            let out = tokio::task::spawn_blocking(move || {
                crate::provision_machine(
                    &layout,
                    &machine.addr,
                    &machine.ssh_user,
                    machine.ssh_port,
                    machine.ssh_key.clone(),
                    force,
                )
            })
            .await;
            match out {
                Ok(lines) => {
                    for l in lines {
                        send(Level::Info, l);
                    }
                    send(Level::Ok, format!("[{addr}] provision finished"));
                }
                Err(e) => send(Level::Error, format!("[{addr}] provision task failed: {e}")),
            }
            let _ = tx.send(Ev::Done(DoneKind::Other));
        }
        Job::AddMachine { api, http, m } => {
            let addr = m.addr.clone();
            match http.post(format!("{api}/api/machines")).json(&m).send().await {
                Ok(r) if r.status().is_success() => send(
                    Level::Ok,
                    format!("machine {addr} added — press p to provision"),
                ),
                Ok(r) => send(
                    Level::Error,
                    format!("add {addr} rejected: HTTP {}", r.status()),
                ),
                Err(e) => send(Level::Error, format!("add {addr} failed: {e}")),
            }
            let _ = tx.send(Ev::Done(DoneKind::Other));
        }
        Job::AddSample { layout_root, path, name } => {
            match bm_core::pool::add_sample(&layout_root, std::path::Path::new(&path), None, name) {
                Ok(lines) => {
                    for l in lines {
                        send(Level::Ok, l);
                    }
                    send(Level::Info, "pool updated — press R to reload the roster".into());
                }
                Err(e) => send(Level::Error, format!("add-sample {path}: {e:#}")),
            }
            let _ = tx.send(Ev::Done(DoneKind::Other));
        }
        Job::DropMachine { api, http, addr } => {
            let url = format!("{api}/api/machines?addr={}", urlencode(&addr));
            let (level, text) = match http.delete(&url).send().await {
                Ok(r) if r.status().is_success() => {
                    (Level::Ok, format!("dropped {addr} from the registry"))
                }
                Ok(r) => (Level::Error, format!("drop {addr}: HTTP {}", r.status())),
                Err(e) => (Level::Error, format!("drop {addr} failed: {e}")),
            };
            send(level, text);
            let _ = tx.send(Ev::Done(DoneKind::Other));
        }
        Job::Op { api, http, req } => {
            let name = req.op.as_str().to_string();
            let voice = req.voice.clone();
            let op = req.op;
            let ok = match http.post(format!("{api}/api/op")).json(&req).send().await {
                Ok(r) => match r.json::<bm_proto::OpResult>().await {
                    Ok(res) => {
                        let level = if res.ok { Level::Ok } else { Level::Error };
                        send(level, format!("{name}: {}", res.message));
                        res.ok
                    }
                    Err(e) => {
                        send(Level::Error, format!("{name}: bad result: {e}"));
                        false
                    }
                },
                Err(e) => {
                    send(Level::Error, format!("{name} failed: {e}"));
                    false
                }
            };
            let _ = tx.send(Ev::Done(DoneKind::Op { op, ok, voice }));
        }
        Job::LoadRoster { api, http } => {
            let res = match http.get(format!("{api}/api/roster")).send().await {
                Ok(r) => match r.json::<Roster>().await {
                    Ok(roster) => Ok(roster),
                    Err(e) => Err(format!("bad roster payload: {e}")),
                },
                Err(e) => Err(format!("roster request failed: {e}")),
            };
            let _ = tx.send(Ev::Roster(res));
            let _ = tx.send(Ev::Done(DoneKind::Other));
        }
    }
}

fn op_job(api: &str, http: &reqwest::Client, req: OpRequest) -> Job {
    Job::Op {
        api: api.to_string(),
        http: http.clone(),
        req,
    }
}

// --- input ------------------------------------------------------------------

/// Validate and dispatch a submitted text prompt.
///
/// Returns `Err(message)` to keep the prompt open with the problem stated,
/// rather than silently substituting a default.
fn submit_text(app: &mut App, prompt: &TextPrompt) -> Result<Job, String> {
    match prompt.kind {
        TextKind::AddMachine => {
            let addr = prompt.buf.trim().to_string();
            if addr.is_empty() {
                return Err("address is empty — enter an IP or hostname".into());
            }
            if addr.contains(char::is_whitespace) {
                return Err(format!("“{addr}” contains whitespace — one address only"));
            }
            let m = Machine::new(&addr, "thang", 22, None, "worker");
            Ok(Job::AddMachine {
                api: app.api.clone(),
                http: app.http.clone(),
                m,
            })
        }
        TextKind::AddSample => {
            // `refs/trien-chieu.mp3 as Triển Chiêu`: the voice answers to the
            // given name, the tags still come from the filename.
            let (path, name) = match prompt.buf.rsplit_once(" as ") {
                Some((p, n)) if !p.trim().is_empty() && !n.trim().is_empty() => {
                    (p.trim().to_string(), Some(n.trim().to_string()))
                }
                _ => (prompt.buf.trim().to_string(), None),
            };
            if path.is_empty() {
                return Err("path is empty — point at a clip, e.g. ~/dl/young-female-4.mp3".into());
            }
            Ok(Job::AddSample { layout_root: app.layout_root.clone(), path, name })
        }
        TextKind::Translate => {
            let mut it = prompt.buf.split_whitespace();
            let start_raw = it.next();
            let count_raw = it.next();
            let start: u32 = match start_raw {
                Some(s) => s.parse().map_err(|_| format!("start “{s}” is not a chapter number"))?,
                None => return Err("expected: <start> <count>, e.g. 21 80".into()),
            };
            let count: u32 = match count_raw {
                Some(s) => s.parse().map_err(|_| format!("count “{s}” is not a number"))?,
                None => return Err("expected: <start> <count>, e.g. 21 80".into()),
            };
            if count == 0 {
                return Err("count must be at least 1".into());
            }
            Ok(op_job(
                &app.api,
                &app.http,
                OpRequest {
                    op: Op::Translate,
                    start: Some(start),
                    count: Some(count),
                    ..Default::default()
                },
            ))
        }
        TextKind::CrawlTemplate => {
            let template = prompt.buf.trim().to_string();
            if template.is_empty() {
                return Err("URL template is empty".into());
            }
            if !template.contains("{n}") {
                return Err("template must contain {n} — that is where the chapter number goes".into());
            }
            Ok(op_job(
                &app.api,
                &app.http,
                OpRequest {
                    op: Op::CrawlSetup,
                    url_template: Some(template),
                    start: Some(app.setting_u32("start", 21)),
                    ..Default::default()
                },
            ))
        }
    }
}

fn dispatch(app: &mut App, job_tx: &tokio::sync::mpsc::UnboundedSender<Job>, job: Job) {
    if job_tx.send(job).is_ok() {
        app.pending += 1;
    } else {
        app.set_status(Level::Error, "background worker is gone — restart the TUI");
    }
}

/// Fire a singleton op, refusing a duplicate while one is already running.
fn dispatch_op(
    app: &mut App,
    job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
    http: &reqwest::Client,
    req: OpRequest,
) {
    let op = req.op;
    if app.inflight.contains(&op) {
        app.set_status(Level::Warn, format!("{} is already running", op.as_str()));
        return;
    }
    app.inflight.push(op);
    dispatch(app, job_tx, op_job(&app.api, http, req));
}

async fn handle_key(
    app: &mut App,
    key: KeyEvent,
    http: &reqwest::Client,
    job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    // Confirm and Help are modal: they swallow everything but their own keys.
    if let Screen::Confirm(c) = app.screen.clone() {
        match key.code {
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                app.screen = Screen::Normal;
                match c.action {
                    ConfirmAction::Quit => return true,
                    ConfirmAction::Provision { addr, force } => {
                        if let Some(m) = app.machine_by_addr(&addr).cloned() {
                            app.set_status(
                                Level::Info,
                                format!("provisioning {addr} in the background…"),
                            );
                            app.log_at(Level::Info, format!("[{addr}] provisioning started"));
                            dispatch(
                                app,
                                job_tx,
                                Job::Provision {
                                    layout_root: app.layout_root.clone(),
                                    machine: m,
                                    force,
                                },
                            );
                        } else {
                            app.set_status(Level::Warn, format!("{addr} is no longer in the registry"));
                        }
                    }
                    ConfirmAction::DropMachine { addr } => {
                        dispatch(
                            app,
                            job_tx,
                            Job::DropMachine {
                                api: app.api.clone(),
                                http: http.clone(),
                                addr,
                            },
                        );
                    }
                    ConfirmAction::SwapVoice { character, voice } => {
                        dispatch_op(
                            app,
                            job_tx,
                            http,
                            OpRequest {
                                op: Op::SwapVoice,
                                character: Some(character.clone()),
                                voice: Some(voice.clone()),
                                ..Default::default()
                            },
                        );
                        app.set_status(
                            Level::Info,
                            format!("swapping {character} → {voice}…"),
                        );
                    }
                }
            }
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                app.screen = Screen::Normal;
                app.set_status(Level::Info, "cancelled — nothing changed");
            }
            _ => {}
        }
        return false;
    }

    if let Screen::Help { scroll } = app.screen.clone() {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') | KeyCode::Enter => {
                app.screen = Screen::Normal;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                app.screen = Screen::Help { scroll: scroll + 1 };
            }
            KeyCode::Up | KeyCode::Char('k') => {
                app.screen = Screen::Help { scroll: scroll.saturating_sub(1) };
            }
            KeyCode::PageDown => app.screen = Screen::Help { scroll: scroll + 8 },
            KeyCode::PageUp => app.screen = Screen::Help { scroll: scroll.saturating_sub(8) },
            _ => {}
        }
        return false;
    }

    if let Screen::Machine(_) = app.screen {
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char('i') => {
                app.screen = Screen::Normal;
            }
            _ => {}
        }
        return false;
    }

    if let Screen::Text(prompt) = app.screen.clone() {
        let mut p = prompt;
        match key.code {
            KeyCode::Esc => {
                app.screen = Screen::Normal;
                app.set_status(Level::Info, "cancelled — nothing was submitted");
            }
            KeyCode::Enter => match submit_text(app, &p) {
                Ok(job) => {
                    app.set_status(Level::Ok, format!("submitted: {}", p.buf.trim()));
                    app.screen = Screen::Normal;
                    dispatch(app, job_tx, job);
                }
                // Keep the prompt open: the operator's typing is preserved and
                // the problem is stated in place.
                Err(msg) => app.set_status(Level::Error, msg),
            },
            KeyCode::Backspace => p.backspace(),
            KeyCode::Delete => p.delete(),
            KeyCode::Left => p.left(),
            KeyCode::Right => p.right(),
            KeyCode::Home => p.home(),
            KeyCode::End => p.end(),
            KeyCode::Char(c) if ctrl => match c.to_ascii_lowercase() {
                'u' => p.kill_to_start(),
                'w' => p.kill_word(),
                'a' => p.home(),
                'e' => p.end(),
                _ => {}
            },
            KeyCode::Char(c) if !alt => p.insert(c),
            _ => {}
        }
        // Write back the edited prompt — unless an arm above already closed it
        // (Esc / successful submit). Doing this unconditionally re-opened the
        // prompt on every close.
        if matches!(app.screen, Screen::Text(_)) {
            app.screen = Screen::Text(p);
        }
        return false;
    }

    if let Screen::Pick(picker) = app.screen.clone() {
        let mut p = picker;
        match key.code {
            KeyCode::Esc => {
                match p.stage {
                    PickStage::Voice => {
                        p.stage = PickStage::Character;
                        p.filter.clear();
                        p.cursor = 0;
                        p.scroll = 0;
                        app.screen = Screen::Pick(p);
                    }
                    PickStage::Character => {
                        app.screen = Screen::Normal;
                        app.set_status(Level::Info, "cancelled — nothing changed");
                    }
                }
            }
            KeyCode::Char('R') => {
                app.load_roster(job_tx, http);
            }
            KeyCode::Enter => match p.stage {
                PickStage::Character => {
                    let list = filtered_characters(app, &p.filter);
                    let chosen = list
                        .get(p.cursor)
                        .cloned()
                        .unwrap_or_else(|| p.filter.trim().to_string());
                    if chosen.is_empty() {
                        app.set_status(Level::Error, "pick a character, or type a new name first");
                    } else {
                        p.character = chosen;
                        p.stage = PickStage::Voice;
                        p.filter.clear();
                        p.cursor = 0;
                        p.scroll = 0;
                        app.screen = Screen::Pick(p);
                    }
                }
                PickStage::Voice => {
                    let list = filtered_voices(app, &p.filter);
                    match list.get(p.cursor) {
                        None => app.set_status(Level::Error, "no voice selected"),
                        Some(v) => {
                            if !v.allowed {
                                app.set_status(
                                    Level::Warn,
                                    format!(
                                        "{} is blocked by the accent policy — pick another",
                                        v.name
                                    ),
                                );
                            } else {
                                app.screen = Screen::Confirm(Confirm {
                                    title: "Confirm voice swap".into(),
                                    danger: true,
                                    body: vec![
                                        format!("Repoint “{}” from its current voice to “{}”.", p.character, v.name),
                                        String::new(),
                                        "This deletes only that speaker's cached segments, drops the".into(),
                                        "stale mp3s for the affected chapters and requeues render +".into(),
                                        "merge. Every other character keeps its cache.".into(),
                                    ],
                                    action: ConfirmAction::SwapVoice {
                                        character: p.character.clone(),
                                        voice: v.name.clone(),
                                    },
                                });
                            }
                        }
                    }
                }
            },
            KeyCode::Tab if p.stage == PickStage::Voice => {
                let list = filtered_voices(app, &p.filter);
                match list.get(p.cursor) {
                    None => app.set_status(Level::Warn, "nothing to audition"),
                    Some(v) => {
                        if p.previewing.is_some() {
                            app.set_status(Level::Warn, "an audition is already running");
                        } else {
                            p.previewing = Some(v.name.clone());
                            app.set_status(Level::Info, format!("auditioning {}…", v.name));
                            dispatch_op(
                                app,
                                job_tx,
                                http,
                                OpRequest {
                                    op: Op::PreviewVoice,
                                    voice: Some(v.name.clone()),
                                    ..Default::default()
                                },
                            );
                        }
                    }
                }
                app.screen = Screen::Pick(p);
            }
            // Movement is arrows only. `j`/`k` used to move too, which meant a
            // filter for a speaker called "Kiên" silently moved the cursor
            // instead of typing — and nothing on screen said why.
            KeyCode::Up => {
                p.cursor = p.cursor.saturating_sub(1);
                app.screen = Screen::Pick(p);
            }
            KeyCode::Down => {
                p.cursor += 1;
                app.screen = Screen::Pick(p);
            }
            KeyCode::PageUp => {
                p.cursor = p.cursor.saturating_sub(8);
                app.screen = Screen::Pick(p);
            }
            KeyCode::PageDown => {
                p.cursor += 8;
                app.screen = Screen::Pick(p);
            }
            KeyCode::Backspace => {
                p.filter.pop();
                p.cursor = 0;
                p.scroll = 0;
                app.screen = Screen::Pick(p);
            }
            KeyCode::Char(c) if ctrl => {
                match c {
                    'u' => {
                        p.filter.clear();
                        p.cursor = 0;
                        p.scroll = 0;
                    }
                    'r' => {
                        app.load_roster(job_tx, http);
                    }
                    _ => {}
                }
                app.screen = Screen::Pick(p);
            }
            KeyCode::Char(c) if !alt => {
                p.filter.push(c);
                p.cursor = 0;
                p.scroll = 0;
                app.screen = Screen::Pick(p);
            }
            _ => {}
        }
        return false;
    }

    if let Screen::Cast(view) = app.screen.clone() {
        let mut v = view;
        match key.code {
            // Esc closes; `q` is deliberately *not* bound here, so it can be
            // typed into the filter like any other letter.
            KeyCode::Esc => {
                app.screen = Screen::Normal;
            }
            KeyCode::Char('R') => app.load_roster(job_tx, http),
            KeyCode::Enter => {
                let rows = app.cast_rows();
                let list = filtered_cast_rows(&rows, &v.filter);
                match list.get(v.cursor) {
                    None => app.set_status(Level::Error, "no speaker selected"),
                    Some(row) => {
                        // Hand straight to step 2: the overview exists to make a
                        // reassignment, not only to be read.
                        let mut p = Picker::new();
                        p.character = row.character.clone();
                        p.stage = PickStage::Voice;
                        app.set_status(
                            Level::Info,
                            format!("choosing a voice for “{}”", row.character),
                        );
                        app.screen = Screen::Pick(p);
                    }
                }
            }
            KeyCode::Up => {
                v.cursor = v.cursor.saturating_sub(1);
                app.screen = Screen::Cast(v);
            }
            KeyCode::Down => {
                v.cursor += 1;
                app.screen = Screen::Cast(v);
            }
            KeyCode::PageUp => {
                v.cursor = v.cursor.saturating_sub(8);
                app.screen = Screen::Cast(v);
            }
            KeyCode::PageDown => {
                v.cursor += 8;
                app.screen = Screen::Cast(v);
            }
            KeyCode::Home => {
                v.cursor = 0;
                app.screen = Screen::Cast(v);
            }
            KeyCode::End => {
                v.cursor = filtered_cast_rows(&app.cast_rows(), &v.filter).len().saturating_sub(1);
                app.screen = Screen::Cast(v);
            }
            KeyCode::Backspace => {
                v.filter.pop();
                v.cursor = 0;
                v.scroll = 0;
                app.screen = Screen::Cast(v);
            }
            KeyCode::Char(c) if ctrl => {
                if c == 'u' {
                    v.filter.clear();
                    v.cursor = 0;
                    v.scroll = 0;
                }
                app.screen = Screen::Cast(v);
            }
            KeyCode::Char(c) if !alt => {
                v.filter.push(c);
                v.cursor = 0;
                v.scroll = 0;
                app.screen = Screen::Cast(v);
            }
            _ => {}
        }
        return false;
    }

    // Normal mode.
    match key.code {
        KeyCode::Char('q') => {
            if app.pending > 0 {
                app.screen = Screen::Confirm(Confirm {
                    title: "Quit with work in flight?".into(),
                    danger: true,
                    body: vec![format!(
                        "{} background job(s) are still running.",
                        app.pending
                    ),
                    "The inductor keeps working without the TUI, but you will lose".into(),
                    "the event log and any in-flight result.".into()],
                    action: ConfirmAction::Quit,
                });
            } else {
                return true;
            }
        }
        KeyCode::Char('?') => app.screen = Screen::Help { scroll: 0 },
        KeyCode::Char('C') => {
            app.colour = !app.colour;
            let on = if app.colour { "on" } else { "off" };
            app.set_status(Level::Info, format!("colour {on}"));
        }
        KeyCode::Char('r') => {
            app.refresh(http).await;
            app.set_status(Level::Ok, "refreshed");
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.selected = app.selected.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.selected = (app.selected + 1).min(app.machines.len().saturating_sub(1));
        }
        KeyCode::Home => app.selected = 0,
        KeyCode::End => app.selected = app.machines.len().saturating_sub(1),
        KeyCode::PageUp => {
            app.events_scroll = app.events_scroll.saturating_add(3);
        }
        KeyCode::PageDown => {
            app.events_scroll = app.events_scroll.saturating_sub(3);
        }
        KeyCode::Char('G') => {
            app.events_scroll = 0;
            app.set_status(Level::Info, "event log pinned to newest");
        }
        KeyCode::Char('a') => {
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::AddMachine,
                "Add machine",
                "IP or hostname of the box to onboard, e.g. 192.168.2.7",
                "",
            ));
        }
        KeyCode::Char('A') => {
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::AddSample,
                "Add sample voice to the pool",
                "clip path — tags come from the filename; append `as Name` to rename",
                "",
            ));
        }
        KeyCode::Char('p') | KeyCode::Char('P') => {
            let force = matches!(key.code, KeyCode::Char('P'));
            match app.selected_machine() {
                None => app.set_status(
                    Level::Warn,
                    "no machine selected — press a to add one first",
                ),
                Some(m) => {
                    app.screen = Screen::Confirm(Confirm {
                        title: if force {
                            "Re-provision (force)".into()
                        } else {
                            "Provision machine".into()
                        },
                        danger: false,
                        body: vec![
                            format!("Onboard {} over ssh.", m.addr),
                            String::new(),
                            if force {
                                "Force ignores the skip-if-configured check and rebuilds the".into()
                            } else {
                                "Already-configured machines are detected and skipped, so this is".into()
                            },
                            if force {
                                "worker venv when present. That is the slow path.".into()
                            } else {
                                "cheap to run again — it will report why it did nothing.".into()
                            },
                        ],
                        action: ConfirmAction::Provision {
                            addr: m.addr.clone(),
                            force,
                        },
                    });
                }
            }
        }
        KeyCode::Char('d') => match app.selected_machine() {
            None => app.set_status(Level::Warn, "no machine selected"),
            Some(m) => {
                app.screen = Screen::Confirm(Confirm {
                    title: "Drop machine from the registry".into(),
                    danger: true,
                    body: vec![
                        format!("Remove {} from the cluster registry.", m.addr),
                        String::new(),
                        "This forgets the machine. It does not touch anything on the".into(),
                        "remote box, and re-adding it by address is enough to bring it back.".into(),
                    ],
                    action: ConfirmAction::DropMachine { addr: m.addr.clone() },
                });
            }
        },
        KeyCode::Char('i') => match app.selected_machine() {
            None => app.set_status(Level::Warn, "no machine selected"),
            Some(m) => app.screen = Screen::Machine(m.addr.clone()),
        },
        KeyCode::Char('t') => {
            let start = app.setting_u32("start", 21);
            let count = app.setting_u32("count", 80);
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::Translate,
                "Translate — enqueue crawl + digest",
                "chapter range as <start> <count>. Prefilled from the inductor's settings.",
                &format!("{start} {count}"),
            ));
        }
        KeyCode::Char('c') => {
            let current = app.setting_str("url_template", "");
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::CrawlTemplate,
                "Crawl setup — save the URL template",
                "must contain {n}; one chapter is probe-crawled to check the selector",
                &current,
            ));
        }
        KeyCode::Char('v') => {
            dispatch_op(app, job_tx, http, OpRequest { op: Op::Voices, ..Default::default() });
        }
        KeyCode::Char('s') => {
            app.screen = Screen::Pick(Picker::new());
            if app.roster.is_none() {
                app.load_roster(job_tx, http);
            }
        }
        KeyCode::Char('S') => {
            app.screen = Screen::Cast(CastView::new());
            if app.roster.is_none() {
                app.load_roster(job_tx, http);
            }
        }
        KeyCode::Char('e') => {
            dispatch_op(app, job_tx, http, OpRequest { op: Op::Eta, ..Default::default() });
        }
        _ => {}
    }
    false
}

/// Percent-encode the few characters that can appear in an address query.
fn urlencode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            c => c
                .to_string()
                .bytes()
                .map(|b| format!("%{b:02X}"))
                .collect(),
        })
        .collect()
}

// --- entry points -----------------------------------------------------------

pub async fn run(api: &str, layout: Layout) -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let result = run_loop(api, layout, &mut terminal).await;
    // Always restore the terminal, even when the loop returned an error —
    // otherwise a crash leaves the operator in raw mode with no cursor.
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

async fn run_loop(
    api: &str,
    layout: Layout,
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
) -> anyhow::Result<()> {
    let mut app = App::new(api);
    app.layout_root = layout.root.clone();
    app.http = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;
    let http = app.http.clone();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Ev>();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    // Background worker: jobs run one at a time. Provisioning in particular
    // must not run concurrently — the flows fight over ssh.
    tokio::spawn(async move {
        while let Some(job) = job_rx.recv().await {
            run_job(job, tx.clone()).await;
        }
    });

    app.refresh(&http).await;
    loop {
        terminal.draw(|f| draw(f, &mut app))?;
        while let Ok(ev) = rx.try_recv() {
            // Background lines carry no timestamp of their own; stamp them on
            // arrival so the log reads in the order things actually finished.
            let ev = match ev {
                Ev::Log(mut l) if l.at == Duration::ZERO => {
                    l.at = app.started.elapsed();
                    Ev::Log(l)
                }
                other => other,
            };
            app.apply(ev);
        }
        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                // Terminals that report key release would otherwise fire every
                // binding twice.
                if key.kind == KeyEventKind::Press
                    && handle_key(&mut app, key, &http, &job_tx).await
                {
                    break;
                }
            }
        }
        app.tick += 1;
        if app.tick.is_multiple_of(REFRESH_TICKS) {
            app.refresh(&http).await;
        }
    }
    Ok(())
}

/// One plain-text snapshot of the cluster, then exit.
///
/// The TUI needs an alternate screen, colour and a keyboard, which rules it out
/// for screen readers, `watch`, CI and shell pipelines. This is the accessible
/// and scriptable view of exactly the same data.
pub async fn snapshot(api: &str) -> anyhow::Result<()> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;
    let base = api.trim_end_matches('/');
    let v: serde_json::Value = http
        .get(format!("{base}/api/state"))
        .send()
        .await?
        .json()
        .await?;

    let mut machines: Vec<Machine> =
        serde_json::from_value(v.get("machines").cloned().unwrap_or_default()).unwrap_or_default();
    let mut beats: Vec<Heartbeat> =
        serde_json::from_value(v.get("beats").cloned().unwrap_or_default()).unwrap_or_default();
    let mut tasks: Vec<Task> =
        serde_json::from_value(v.get("tasks").cloned().unwrap_or_default()).unwrap_or_default();
    machines.sort_by(|a, b| a.addr.cmp(&b.addr));
    beats.sort_by(|a, b| a.worker_id.cmp(&b.worker_id));
    tasks.sort_by_key(|t| (t.chapter, t.stage));

    println!("cluster @ {base}");
    println!("\nmachines ({})", machines.len());
    if machines.is_empty() {
        println!("  none — add one with: bm-inductor provision --addr <ip>");
    }
    for m in &machines {
        println!(
            "  {:<16} {:<15} {:<13} tts={:<22} seen={}",
            m.id,
            m.addr,
            m.state.as_str(),
            m.tts_url.clone().unwrap_or_else(|| "-".into()),
            seen_label(m)
        );
        if !m.note.trim().is_empty() {
            println!("      note: {}", m.note.replace('\n', " "));
        }
    }

    println!("\nworkers ({})", beats.len());
    if beats.is_empty() {
        println!("  none — start one with: bm-agent worker --inductor <this host>");
    }
    for b in &beats {
        println!(
            "  {:<14} {:<8} ch{:<4} {:>3}%  {:<28} eta={}",
            b.worker_id,
            b.stage.map(|s| s.as_str()).unwrap_or("-"),
            b.chapter.map(|c| c.to_string()).unwrap_or_else(|| "-".into()),
            (b.progress.clamp(0.0, 1.0) * 100.0).round() as u32,
            b.activity,
            b.eta_secs.map(bm_core::eta::human).unwrap_or_else(|| "-".into())
        );
    }

    println!("\ntasks");
    match v.get("counts").and_then(|c| c.as_object()) {
        None => println!("  no task data"),
        Some(obj) if obj.is_empty() => println!("  none queued"),
        Some(obj) => {
            let mut stages: Vec<&String> = obj.keys().collect();
            stages.sort();
            for st in stages {
                let c = &obj[st.as_str()];
                let get = |k: &str| c.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
                let total: u64 = c
                    .as_object()
                    .map(|m| m.values().filter_map(|x| x.as_u64()).sum())
                    .unwrap_or(0);
                println!(
                    "  {st:<8} {}/{} done  {} open  {} failed  {} shelved",
                    get("done"),
                    total,
                    total.saturating_sub(get("done")).saturating_sub(get("shelved")),
                    get("failed"),
                    get("shelved")
                );
            }
        }
    }
    let shelved: Vec<String> = {
        let mut s: Vec<String> = tasks
            .iter()
            .filter(|t| t.state == TaskState::Shelved)
            .map(|t| format!("{}:{}", t.stage, t.chapter))
            .collect();
        s.sort();
        s.dedup();
        s
    };
    if !shelved.is_empty() {
        println!("  shelved: {}", shelved.join(" "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accents_are_folded_so_filters_ignore_diacritics() {
        assert_eq!(fold("Thái Sơn"), "thai son");
        assert_eq!(fold("Đức Trí"), "duc tri");
        assert_eq!(fold("Thục Đoan"), "thuc doan");
        assert_eq!(fold("Lạc Lan Tuyết"), "lac lan tuyet");
        assert!(matches("thai son", "Thái Sơn"));
        assert!(matches("duc", "Đức Trí"));
        assert!(matches("", "anything"), "an empty filter matches everything");
        assert!(!matches("adam", "Thái Sơn"));
    }

    #[test]
    fn neutral_does_not_fold_to_a_female_marker() {
        // Guards the Python/Rust twin of the same bug.
        assert_eq!(fold("neutral"), "neutral");
    }

    #[test]
    fn text_prompt_edits_by_character_not_byte() {
        let mut p = TextPrompt::new(TextKind::AddMachine, "t", "h", "Đức");
        // Three characters, six bytes: a byte-indexed cursor would land inside
        // 'ứ' and panic on the next edit.
        assert_eq!(p.len(), 3);
        assert_eq!(p.cursor, 3);
        p.left();
        assert_eq!(p.cursor, 2, "cursor 2 sits before the third character");
        p.insert('x');
        assert_eq!(p.buf, "Đứxc");
        assert_eq!(p.cursor, 3);
        p.backspace();
        assert_eq!(p.buf, "Đức");
        assert_eq!(p.cursor, 2);
        p.home();
        p.delete();
        assert_eq!(p.buf, "ức");
        p.kill_to_start();
        assert_eq!(p.buf, "ức", "cursor is already at 0, so nothing is cut");
        p.end();
        assert_eq!(p.cursor, 2);
        p.kill_word();
        assert_eq!(p.buf, "");
    }

    #[test]
    fn kill_word_stops_at_a_space() {
        let mut p = TextPrompt::new(TextKind::Translate, "t", "h", "21 80");
        p.kill_word();
        assert_eq!(p.buf, "21 ");
        p.kill_word();
        assert_eq!(p.buf, "");
    }

    #[test]
    fn translate_prompt_rejects_garbage_instead_of_defaulting() {
        let mut app = App::new("http://x");
        let p = TextPrompt::new(TextKind::Translate, "t", "h", "abc 80");
        let err = submit_text(&mut app, &p).unwrap_err();
        assert!(err.contains("not a chapter number"), "{err}");

        let p = TextPrompt::new(TextKind::Translate, "t", "h", "21");
        assert!(submit_text(&mut app, &p).unwrap_err().contains("expected"));

        let p = TextPrompt::new(TextKind::Translate, "t", "h", "21 0");
        assert!(submit_text(&mut app, &p).unwrap_err().contains("at least 1"));

        let p = TextPrompt::new(TextKind::Translate, "t", "h", "21 80");
        assert!(submit_text(&mut app, &p).is_ok());
    }

    #[test]
    fn crawl_template_requires_the_chapter_placeholder() {
        let mut app = App::new("http://x");
        let p = TextPrompt::new(TextKind::CrawlTemplate, "t", "h", "https://x/chuong");
        assert!(submit_text(&mut app, &p).unwrap_err().contains("{n}"));
        let p = TextPrompt::new(TextKind::CrawlTemplate, "t", "h", "https://x/chuong-{n}");
        assert!(submit_text(&mut app, &p).is_ok());
        let p = TextPrompt::new(TextKind::CrawlTemplate, "t", "h", "   ");
        assert!(submit_text(&mut app, &p).unwrap_err().contains("empty"));
    }

    #[test]
    fn add_sample_rejects_an_empty_path() {
        let mut app = App::new("http://x");
        let p = TextPrompt::new(TextKind::AddSample, "t", "h", "  ");
        assert!(submit_text(&mut app, &p).unwrap_err().contains("empty"));
        let p = TextPrompt::new(TextKind::AddSample, "t", "h", "~/dl/young-female-4.mp3");
        assert!(matches!(submit_text(&mut app, &p), Ok(Job::AddSample { .. })));
    }

    #[test]
    fn add_sample_splits_an_as_rename_off_the_path() {
        let mut app = App::new("http://x");
        let p = TextPrompt::new(TextKind::AddSample, "t", "h", "refs/trien-chieu.mp3 as Triển Chiêu");
        match submit_text(&mut app, &p) {
            Ok(Job::AddSample { path, name, .. }) => {
                assert_eq!(path, "refs/trien-chieu.mp3");
                assert_eq!(name.as_deref(), Some("Triển Chiêu"));
            }
            other => panic!("expected an add-sample job, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn text_prompt_closes_on_submit_or_esc_but_stays_open_on_error() {
        let http = reqwest::Client::new();
        let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);

        // Esc closes without dispatching.
        let mut app = App::new("http://x");
        app.screen = Screen::Text(TextPrompt::new(TextKind::AddSample, "t", "h", "x.mp3"));
        handle_key(&mut app, key(KeyCode::Esc), &http, &job_tx).await;
        assert!(matches!(app.screen, Screen::Normal), "Esc must close the prompt");
        assert!(job_rx.try_recv().is_err(), "a cancelled prompt dispatches nothing");

        // A good submit closes and dispatches exactly one job.
        app.screen = Screen::Text(TextPrompt::new(TextKind::AddSample, "t", "h", "x.mp3"));
        handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
        assert!(matches!(app.screen, Screen::Normal), "submit must close the prompt");
        assert!(job_rx.try_recv().is_ok());

        // A bad submit keeps the prompt (and its text) open.
        app.screen = Screen::Text(TextPrompt::new(TextKind::AddSample, "t", "h", "   "));
        handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
        assert!(matches!(app.screen, Screen::Text(_)), "an error must keep the prompt open");
    }

    #[test]
    fn add_machine_rejects_whitespace_addresses() {
        let mut app = App::new("http://x");
        let p = TextPrompt::new(TextKind::AddMachine, "t", "h", "192.168.2.7 extra");
        assert!(submit_text(&mut app, &p).unwrap_err().contains("whitespace"));
        let p = TextPrompt::new(TextKind::AddMachine, "t", "h", "  ");
        assert!(submit_text(&mut app, &p).unwrap_err().contains("empty"));
    }

    #[test]
    fn scroll_clamping_keeps_the_cursor_visible() {
        let mut scroll = 0;
        clamp_scroll(0, &mut scroll, 100, 10);
        assert_eq!(scroll, 0);
        clamp_scroll(15, &mut scroll, 100, 10);
        assert_eq!(scroll, 6, "cursor 15 in a 10-row window starts at 6");
        clamp_scroll(2, &mut scroll, 100, 10);
        assert_eq!(scroll, 2);
        // A short list must not scroll past its end.
        let mut s2 = 5;
        clamp_scroll(0, &mut s2, 3, 10);
        assert_eq!(s2, 0);
    }

    #[test]
    fn seen_label_says_never_rather_than_a_fifty_year_uptime() {
        let mut m = Machine::new("10.0.0.5", "u", 22, None, "worker");
        assert_eq!(seen_label(&m), "never");
        m.last_seen = bm_proto::now_secs().saturating_sub(5);
        assert_eq!(seen_label(&m), "5s");
        m.last_seen = bm_proto::now_secs().saturating_sub(120);
        assert_eq!(seen_label(&m), "2m");
        m.last_seen = bm_proto::now_secs().saturating_sub(7200);
        assert_eq!(seen_label(&m), "2h");
    }

    #[test]
    fn stages_and_states_have_distinct_palettes() {
        // The old build coloured the Workers stage column with the task-state
        // palette, which no stage name matched.
        assert_eq!(stage_color("render"), Color::Cyan);
        assert_eq!(state_color("online"), Color::Green);
        assert_ne!(stage_color("render"), state_color("render"));
    }

    #[test]
    fn users_of_lists_every_character_on_a_voice() {
        let mut cast = BTreeMap::new();
        cast.insert("Narrator".to_string(), "Đức Trí".to_string());
        cast.insert("A".to_string(), "Đức Trí".to_string());
        cast.insert("B".to_string(), "Adam".to_string());
        assert_eq!(users_of(&cast, "Đức Trí").len(), 2);
        assert_eq!(users_of(&cast, "Adam"), vec!["B".to_string()]);
        assert!(users_of(&cast, "Nobody").is_empty());
    }

    #[test]
    fn urlencode_leaves_hostnames_alone_and_escapes_the_rest() {
        assert_eq!(urlencode("192.168.2.7"), "192.168.2.7");
        assert_eq!(urlencode("host name"), "host%20name");
    }

    #[test]
    fn timestamp_switches_units_at_an_hour() {
        assert_eq!(stamp(Duration::from_secs(12)), "+00:12");
        assert_eq!(stamp(Duration::from_secs(192)), "+03:12");
        assert_eq!(stamp(Duration::from_secs(3720)), "+1h02m");
    }

    #[test]
    fn app_starts_on_normal_with_a_hint_not_a_blank_status() {
        let app = App::new("http://127.0.0.1:8901/");
        assert_eq!(app.api, "http://127.0.0.1:8901", "trailing slash is trimmed");
        assert!(matches!(app.screen, Screen::Normal));
        assert!(!app.status.text.is_empty());
        assert_eq!(app.pending, 0);
        assert!(app.colour);
    }

    // --- responsive layout --------------------------------------------------

    #[test]
    fn size_class_picks_a_tier_per_axis() {
        assert_eq!(size_class(120, 40), Size::Full);
        assert_eq!(size_class(FULL_W, FULL_H), Size::Full);
        assert_eq!(size_class(80, 24), Size::Compact, "the common default terminal");
        assert_eq!(size_class(MIN_W, MIN_H), Size::Compact, "the floor is still usable");
        assert_eq!(size_class(60, 24), Size::TooSmall, "too narrow");
        assert_eq!(size_class(120, 10), Size::TooSmall, "too short");
        assert_eq!(size_class(0, 0), Size::TooSmall, "a degenerate area must not divide by zero");
    }

    #[test]
    fn compact_columns_fit_a_minimum_width_terminal() {
        // The same totals the compile-time guards prove; asserted here too so a
        // failure names the pane instead of just refusing to compile.
        let machines = cols(&COMPACT_MACHINE_COLS);
        let workers = cols(&COMPACT_WORKER_COLS);
        assert!(
            machines + 2 <= MIN_W,
            "machines needs {machines}+2 columns, terminal floor is {MIN_W}"
        );
        assert!(
            workers + 2 <= MIN_W,
            "workers needs {workers}+2 columns, terminal floor is {MIN_W}"
        );
    }

    #[test]
    fn compact_layout_fits_the_hard_minimum() {
        let panes = COMPACT_MACHINES_H
            + COMPACT_WORKERS_H
            + COMPACT_EVENTS_MIN_H
            + COMPACT_FOOTER_H;
        assert!(panes <= MIN_H, "compact panes need {panes} rows, floor is {MIN_H}");
        // The full tier must not be tighter than the compact one.
        let full = FULL_MACHINES_H + FULL_WORKERS_H + FULL_TASKS_H + FULL_EVENTS_MIN_H + FULL_FOOTER_H;
        assert!(full <= FULL_H, "full panes need {full} rows, threshold is {FULL_H}");
    }

    #[test]
    fn key_hints_fit_their_tier_without_clipping() {
        // The single 161-character line this replaced was clipped on every
        // terminal, and the lost tail held the least guessable keys.
        for k in KEYS_FULL {
            assert!(width_of(k) <= FULL_W as usize, "{k} is {} columns", width_of(k));
        }
        for k in KEYS_COMPACT {
            assert!(width_of(k) <= MIN_W as usize, "{k} is {} columns", width_of(k));
        }
    }

    #[test]
    fn the_footer_advertises_the_cast_key_in_both_tiers() {
        // Regression guard: at 80 columns `S cast` fell off the clipped tail of
        // the old one-line hint, so the feature was undiscoverable exactly
        // where the terminal was most cramped.
        assert!(KEYS_FULL.iter().any(|k| k.contains("S cast")), "{KEYS_FULL:?}");
        assert!(KEYS_COMPACT.iter().any(|k| k.contains("S cast")), "{KEYS_COMPACT:?}");
    }

    #[test]
    fn task_rollup_survives_a_missing_or_empty_counts_object() {
        let text = |v: &serde_json::Value| -> String {
            task_rollup(v, false).spans.iter().map(|s| s.content.as_ref()).collect()
        };
        assert!(text(&serde_json::Value::Null).contains("waiting"));
        assert!(text(&serde_json::json!({})).contains("none queued"));
    }

    #[test]
    fn task_rollup_totals_every_stage_and_flags_shelved() {
        let counts = serde_json::json!({
            "crawl":  {"done": 3, "failed": 1},
            "render": {"done": 1, "shelved": 2},
        });
        let text: String =
            task_rollup(&counts, false).spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("4/7 done"), "{text}");
        assert!(text.contains("1 open"), "{text}");
        assert!(text.contains("1 failed"), "{text}");
        assert!(text.contains("2 shelved"), "{text}");
    }

    #[test]
    fn task_rollup_hides_zero_failure_and_shelved_counters() {
        let counts = serde_json::json!({"crawl": {"done": 2, "failed": 0, "shelved": 0}});
        let text: String =
            task_rollup(&counts, false).spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("2/2 done"), "{text}");
        assert!(!text.contains("failed"), "a zero counter is noise: {text}");
        assert!(!text.contains("shelved"), "a zero counter is noise: {text}");
    }

    // --- cast overview ------------------------------------------------------

    fn roster_fixture() -> Roster {
        let voice = |name: &str, gender: &str, accent: &str, allowed: bool, enrolled: bool| {
            VoiceInfo {
                // Keys come from the real catalogue, so the fixture cannot drift
                // from what the picker actually receives — and a name the
                // catalogue does not declare keeps an empty key, as a clone does.
                key: bm_core::voices::key_for_name("vieneu", name).unwrap_or_default(),
                name: name.to_string(),
                gender: gender.to_string(),
                accent: accent.to_string(),
                language: "vi-VN".to_string(),
                style: "tin tức".to_string(),
                enrolled,
                allowed,
            }
        };
        Roster {
            engine: "vieneu".into(),
            source: "live".into(),
            voices: vec![
                voice("Đức Trí", "male", "South", true, false),
                voice("Adam", "male", "unknown", false, true),
                voice("Bắc Kỳ", "male", "Northern", false, false),
            ],
            cast: BTreeMap::from([
                ("Narrator".to_string(), "Đức Trí".to_string()),
                ("Kiên".to_string(), "Adam".to_string()),
                ("Vũ".to_string(), "Adam".to_string()),
                ("Lâm".to_string(), "Bắc Kỳ".to_string()),
                ("Hà".to_string(), "Đã Biến Mất".to_string()),
            ]),
            characters: vec![
                "Narrator".into(),
                "Kiên".into(),
                "Vũ".into(),
                "Lâm".into(),
                "Hà".into(),
                "Mới".into(),
            ],
            policy_note: "Central/South only".into(),
        }
    }

    #[test]
    fn cast_rows_put_narrator_first_and_keep_unassigned_speakers() {
        let rows = cast_rows(&roster_fixture());
        assert_eq!(rows[0].character, "Narrator", "Narrator is the fallback voice");
        assert_eq!(rows.len(), 6, "every speaker appears exactly once");
        let moi = rows.iter().find(|r| r.character == "Mới").unwrap();
        assert!(moi.unassigned());
        assert_eq!(moi.verdict(), Verdict::Unassigned);
    }

    #[test]
    fn cast_rows_flag_shared_voices_from_both_sides() {
        let rows = cast_rows(&roster_fixture());
        let by = |n: &str| rows.iter().find(|r| r.character == n).unwrap().clone();
        assert_eq!(by("Kiên").shared_with, vec!["Vũ".to_string()]);
        assert_eq!(by("Vũ").shared_with, vec!["Kiên".to_string()]);
        assert!(by("Kiên").shared());
        assert!(!by("Narrator").shared(), "a sole user of a voice is not flagged");
    }

    #[test]
    fn cast_rows_separate_blocked_from_unknown_and_accept_enrolled_clones() {
        let rows = cast_rows(&roster_fixture());
        let by = |n: &str| rows.iter().find(|r| r.character == n).unwrap().verdict();
        assert_eq!(by("Lâm"), Verdict::Blocked, "listed, and the policy rejects it");
        assert_eq!(by("Hà"), Verdict::Unknown, "the roster has never heard of it");
        assert_eq!(by("Kiên"), Verdict::Ok, "enrolled clones bypass the policy");
        assert_eq!(by("Narrator"), Verdict::Ok);
    }

    #[test]
    fn unassigned_speakers_do_not_count_as_sharing_the_empty_voice() {
        let mut r = roster_fixture();
        r.cast.retain(|k, _| k == "Kiên");
        let rows = cast_rows(&r);
        let unassigned: Vec<&CastRow> = rows.iter().filter(|x| x.unassigned()).collect();
        assert!(unassigned.len() > 1, "the fixture must have several unassigned speakers");
        assert!(unassigned.iter().all(|x| x.shared_with.is_empty()));
    }

    #[test]
    fn cast_rows_filter_by_speaker_voice_or_style_ignoring_diacritics() {
        let rows = cast_rows(&roster_fixture());
        assert_eq!(filtered_cast_rows(&rows, "duc tri").len(), 1, "matches the voice");
        assert_eq!(filtered_cast_rows(&rows, "adam").len(), 2, "both speakers on Adam");
        assert_eq!(filtered_cast_rows(&rows, "kien").len(), 1, "matches the speaker");
        assert_eq!(
            filtered_cast_rows(&rows, "tin tuc").len(),
            4,
            "the style is searchable too, and without diacritics"
        );
        assert_eq!(filtered_cast_rows(&rows, "   ").len(), rows.len(), "a blank filter keeps all");
        assert!(filtered_cast_rows(&rows, "nobody").is_empty());
    }

    #[test]
    fn cast_filter_preserves_row_order_and_never_invents_rows() {
        let rows = cast_rows(&roster_fixture());
        let filtered = filtered_cast_rows(&rows, "adam");
        let order: Vec<&String> = filtered.iter().map(|r| &r.character).collect();
        assert_eq!(order, vec!["Kiên", "Vũ"]);
    }

    #[test]
    fn cast_rows_carry_the_voice_metadata_through() {
        let rows = cast_rows(&roster_fixture());
        let narrator = rows.iter().find(|r| r.character == "Narrator").unwrap();
        assert_eq!(narrator.gender, "male");
        assert_eq!(narrator.accent, "South");
        assert!(narrator.in_roster);
        assert!(narrator.allowed && !narrator.enrolled);
        // A voice the roster does not list carries no metadata at all, so the
        // table renders dashes rather than a fabricated accent.
        let ha = rows.iter().find(|r| r.character == "Hà").unwrap();
        assert!(!ha.in_roster);
        assert!(ha.accent.is_empty() && ha.gender.is_empty());
    }

    // --- rendering ----------------------------------------------------------

    /// Render one frame into an in-memory terminal and flatten it to text.
    ///
    /// The responsive tiers are pure layout, so they can be checked without a
    /// real terminal — which is also the only way to prove the size guard does
    /// not panic on a degenerate area.
    fn render_text(app: &mut App, w: u16, h: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn the_size_guard_replaces_the_dashboard_below_the_floor() {
        let mut app = App::new("http://127.0.0.1:8901");
        let text = render_text(&mut app, 60, 16);
        assert!(text.contains("too small"), "{text}");
        assert!(!text.contains("Machines"), "no clipped panes behind the notice:\n{text}");
        assert!(text.contains("60×16"), "the notice names the actual size:\n{text}");
        assert!(text.contains("76×20"), "and the requirement:\n{text}");
    }

    #[test]
    fn the_size_guard_does_not_panic_on_a_degenerate_area() {
        let mut app = App::new("http://127.0.0.1:8901");
        // Only the first has room for the full notice; the slivers must simply
        // not panic, and must never leak a clipped dashboard.
        for (w, h) in [(60u16, 16u16), (1, 1), (0, 0), (200, 3), (3, 200)] {
            let text = render_text(&mut app, w, h);
            assert!(!text.contains("Machines"), "{w}x{h} rendered panes:\n{text}");
            assert!(!text.contains("Workers"), "{w}x{h} rendered panes:\n{text}");
        }
    }

    #[test]
    fn the_compact_tier_folds_the_tasks_pane_into_the_footer() {
        let mut app = App::new("http://127.0.0.1:8901");
        let text = render_text(&mut app, 80, 24);
        assert!(text.contains("Machines"), "{text}");
        assert!(text.contains("Workers"), "{text}");
        assert!(text.contains("Events"), "Events keeps its pane:\n{text}");
        assert!(!text.contains("┌Tasks"), "the Tasks pane is collapsed:\n{text}");
        assert!(text.contains("tasks:"), "its roll-up takes its place:\n{text}");
    }

    #[test]
    fn the_full_tier_shows_every_pane_and_the_new_key() {
        let mut app = App::new("http://127.0.0.1:8901");
        let text = render_text(&mut app, 140, 44);
        for pane in ["Machines", "Workers", "Tasks", "Events"] {
            assert!(text.contains(pane), "{pane} is missing:\n{text}");
        }
        assert!(text.contains("S cast"), "the cast key is advertised:\n{text}");
    }

    #[test]
    fn an_empty_cluster_says_what_to_do_in_every_tier() {
        let mut app = App::new("http://127.0.0.1:8901");
        for (w, h) in [(80u16, 24u16), (140, 44)] {
            let text = render_text(&mut app, w, h);
            assert!(text.contains("no machines in the cluster"), "{w}x{h}:\n{text}");
            assert!(text.contains("no workers connected"), "{w}x{h}:\n{text}");
            assert!(text.contains("nothing has happened yet"), "{w}x{h}:\n{text}");
        }
    }

    #[test]
    fn the_cast_overview_renders_every_speaker_and_flags_shared_voices() {
        let mut app = App::new("http://127.0.0.1:8901");
        app.roster = Some(roster_fixture());
        app.screen = Screen::Cast(CastView::new());
        let text = render_text(&mut app, 140, 44);
        for speaker in ["Narrator", "Kiên", "Vũ", "Lâm", "Hà", "Mới"] {
            assert!(text.contains(speaker), "{speaker} is missing:\n{text}");
        }
        assert!(text.contains("6 speakers"), "the summary counts them:\n{text}");
        assert!(text.contains("4 voices in use"), "{text}");
        assert!(text.contains("1 shared"), "only Adam is shared:\n{text}");
        assert!(text.contains("2 to fix"), "Lâm and Hà:\n{text}");
        assert!(text.contains("1 unassigned"), "Mới:\n{text}");
        assert!(text.contains("shared with 1 other"), "{text}");
        assert!(text.contains("blocked by the accent policy"), "Lâm is flagged:\n{text}");
        assert!(text.contains("unknown voice — stale cast?"), "Hà is flagged:\n{text}");
        assert!(text.contains("unassigned — v fills gaps"), "Mới is flagged:\n{text}");
    }

    #[test]
    fn the_cast_overview_without_a_roster_offers_the_retry_key() {
        let mut app = App::new("http://127.0.0.1:8901");
        app.screen = Screen::Cast(CastView::new());
        let text = render_text(&mut app, 120, 32);
        assert!(text.contains("roster not loaded — press R"), "{text}");
    }
}
