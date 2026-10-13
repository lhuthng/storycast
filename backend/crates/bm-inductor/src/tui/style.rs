//! Colour-aware primitives: theme, severity, cells, empty states, overlays.
use bm_proto::Machine;
use ratatui::{
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};

// --- theme ----------------------------------------------------------------

/// Which named palette the dashboard draws with. `Default` is the stock
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Theme {
    /// The stock colours every pane already used.
    #[default]
    Default,
    /// Sunken hues for dark terminals: same structure, less glare.
    Dim,
    /// Black/white plus intensity, for e-ink, colour-blind operators and
    Mono,
}

impl Theme {
    /// `C` cycles in this order; the status names the theme it lands on.
    pub(crate) const ALL: [Theme; 3] = [Theme::Default, Theme::Dim, Theme::Mono];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Theme::Default => "default",
            Theme::Dim => "dim",
            Theme::Mono => "mono",
        }
    }

    pub(crate) fn next(self) -> Self {
        let i = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }

    /// The severity hues, per theme. The palette is named once, here — a
    fn color(self, c: Color) -> Color {
        use Color::*;
        match self {
            Theme::Default => match c {
                Gray => Gray,
                DarkGray => DarkGray,
                Green => Green,
                Yellow => Yellow,
                Red => Red,
                Blue => Blue,
                Magenta => Magenta,
                Cyan => Cyan,
                White => White,
                _ => c,
            },
            Theme::Dim => match c {
                Gray => Gray,
                DarkGray => Indexed(240),
                Green => Indexed(108),
                Yellow => Indexed(179),
                Red => Indexed(174),
                Blue => Indexed(110),
                Magenta => Indexed(176),
                Cyan => Indexed(116),
                White => Indexed(252),
                _ => c,
            },
            Theme::Mono => match c {
                Gray | White => White,
                Green | Yellow | Red | Blue | Magenta | Cyan => White,
                DarkGray => DarkGray,
                _ => c,
            },
        }
    }

    /// Accent of the `C`-cycled theme chip and the help border.
    pub(crate) fn accent(self) -> Color {
        match self {
            Theme::Default => Color::Cyan,
            Theme::Dim => Color::Indexed(116),
            Theme::Mono => Color::White,
        }
    }
}

pub(crate) fn theme_label() -> &'static str {
    THEME.with(|t| t.get().label())
}

/// Accent of the active theme: pane borders, the header's workspace chip and
pub(crate) fn theme_accent() -> Color {
    THEME.with(|t| t.get().accent())
}

pub(crate) fn theme_next() -> &'static str {
    THEME.with(|t| {
        let next = t.get().next();
        t.set(next);
        next.label()
    })
}

thread_local! {
    static THEME: std::cell::Cell<Theme> = const { std::cell::Cell::new(Theme::Default) };
}

/// Resolve a stock hue through the active theme. Free function so render
pub(crate) fn themed(c: Color) -> Color {
    THEME.with(|t| t.get().color(c))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Level {
    Info,
    Ok,
    Warn,
    Error,
}

impl Level {
    pub(crate) fn color(self) -> Color {
        match self {
            Level::Info => Color::Gray,
            Level::Ok => Color::Green,
            Level::Warn => Color::Yellow,
            Level::Error => Color::Red,
        }
    }

    /// Fixed-width severity tags: the log's message column starts at the
    pub(crate) fn glyph(self) -> &'static str {
        match self {
            Level::Info => "info",
            Level::Ok => " ok ",
            Level::Warn => "warn",
            Level::Error => "err ",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct LogLine {
    pub(crate) level: Level,
    /// Wall-clock epoch seconds on this machine, for the pane stamp.
    pub(crate) wall: u64,
    pub(crate) text: String,
}

/// Local `HH:MM:SS` for a pane stamp. Unparseable input (including 0, the
pub(crate) fn wall_hms(epoch: u64) -> String {
    chrono::DateTime::from_timestamp(epoch as i64, 0)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "--:--:--".into())
}

/// Map an inductor event level onto the pane's severity.
pub(crate) fn level_from_str(s: &str) -> Level {
    match s {
        "ok" => Level::Ok,
        "warn" => Level::Warn,
        "error" => Level::Error,
        _ => Level::Info,
    }
}

/// Reachability of the inductor. Kept apart from `status` so a transient
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Conn {
    Unknown,
    Up,
    Down(String),
}

pub(crate) fn state_color(s: &str) -> Color {
    match s {
        "online" | "done" | "configured" => Color::Green,
        // `initializing` is a wait, not a fault: a launched box that is still
        "running" | "assigned" | "probing" | "provisioning" | "initializing" | "awaiting-ip" => {
            Color::Yellow
        }
        "offline" | "failed" | "shelved" | "error" => Color::Red,
        "pending" => Color::Gray,
        // Parked by the operator. Grey rather than yellow: it is neither a wait
        "relaxed" => Color::Gray,
        _ => Color::Gray,
    }
}

/// Pipeline stages get their own hues so the Workers pane reads at a glance.
pub(crate) fn stage_color(s: &str) -> Color {
    match s {
        "crawl" => Color::Blue,
        "digest" => Color::Magenta,
        "render" => Color::Cyan,
        "merge" => Color::Green,
        _ => Color::Gray,
    }
}

/// The hue a box's art takes in the Machines rack.
pub(crate) fn machine_tint(work: &str, stage: Option<&str>) -> Color {
    if matches!(work, "offline" | "error") {
        return Color::Red;
    }
    match stage {
        Some(s) => stage_color(s),
        None if work == "unknown" => Color::DarkGray,
        None => Color::Gray,
    }
}

/// Display alias for a worker: a stable animal name + colour derived from the
pub(crate) fn worker_alias(id: &str) -> (&'static str, Color) {
    const ANIMALS: [&str; 16] = [
        "fox", "owl", "bear", "wolf", "hare", "lynx", "otter", "hawk", "deer", "mole", "crane",
        "boar", "seal", "wren", "ibex", "newt",
    ];
    const COLOURS: [Color; 6] = [
        Color::Red,
        Color::Green,
        Color::Yellow,
        Color::Blue,
        Color::Magenta,
        Color::Cyan,
    ];
    let mut h: u64 = 0;
    for b in id.bytes() {
        h = h.wrapping_mul(31).wrapping_add(b as u64);
    }
    (
        ANIMALS[h as usize % ANIMALS.len()],
        COLOURS[h as usize / ANIMALS.len() % COLOURS.len()],
    )
}

/// Alias for an optional assignee, for the uncoloured table cells.
pub(crate) fn worker_name(id: Option<&str>) -> String {
    id.map(|i| worker_alias(i).0.to_string())
        .unwrap_or_else(|| "—".into())
}

/// A task's holders for the table: the primary, plus `+N racing` when digest
pub(crate) fn worker_racing(t: &bm_proto::Task) -> String {
    let base = worker_name(t.assigned_to.as_deref());
    if t.racers.is_empty() {
        base
    } else {
        format!("{} +{} racing", base, t.racers.len())
    }
}

/// `last_seen` is 0 for a machine that has never reported. Subtracting it from
pub(crate) fn seen_label(m: &Machine) -> String {
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

/// How long this machine has held its current state — the visible half of
pub(crate) fn state_age_label(m: &Machine) -> String {
    if m.state_since == 0 {
        return "—".into();
    }
    let d = bm_proto::now_secs().saturating_sub(m.state_since);
    if d < 60 {
        format!("{d}s")
    } else if d < 3600 {
        format!("{}m", d / 60)
    } else {
        format!("{}h", d / 3600)
    }
}

pub(crate) fn gender_label(g: &str) -> &str {
    match g {
        "male" => "male",
        "female" => "female",
        "neutral" => "neutral",
        _ => "—",
    }
}

pub(crate) fn dash_if_empty(s: &str) -> &str {
    if s.trim().is_empty() {
        "—"
    } else {
        s
    }
}

/// The detail's first line, for the table's last column. A full error is a
pub(crate) fn why_label(detail: &str) -> String {
    let first = detail.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    if first.is_empty() {
        return "—".into();
    }
    bm_core::util::head_chars(first.trim(), 120)
}

/// Leading speaker token of a log line, if any: `[192.168.2.2] …` and
pub(crate) fn log_head(text: &str) -> Option<&str> {
    if let Some(rest) = text.strip_prefix('[') {
        if let Some(end) = rest.find("] ") {
            let id = rest[..end].trim();
            if !id.is_empty() {
                return Some(id);
            }
        }
        return None;
    }
    if let Some(pos) = text.find(": ") {
        let head = &text[..pos];
        if (head.contains('-') || head.contains('.')) && !head.chars().any(char::is_whitespace) {
            return Some(head);
        }
    }
    None
}

pub(crate) fn cell(text: String) -> Line<'static> {
    Line::from(text)
}

/// Colour-aware style. `colour` still means "no bold/fg dimming for a
/// terminal that cannot show it"; the *hues* now come from the active theme
pub(crate) fn style_of(colour: bool, c: Color) -> Style {
    if colour {
        Style::default().fg(themed(c))
    } else {
        Style::default()
    }
}

pub(crate) fn style_bold_of(colour: bool, c: Color) -> Style {
    if colour {
        Style::default().fg(themed(c)).add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    }
}

/// A state cell with its leading glyph. The word stays — mono mode and the
pub(crate) fn state_glyph_cell(colour: bool, text: &str) -> Line<'static> {
    let glyph = match text {
        "online" | "done" | "configured" | "ok" => "● ",
        // `initializing` joins the in-progress family: a box booting is on its
        "running" | "assigned" | "probing" | "provisioning" | "initializing" | "awaiting-ip" => {
            "◐ "
        }
        "offline" | "failed" | "shelved" | "error" => "✗ ",
        // See `state_color`: parked is the hollow circle — nothing running,
        "relaxed" => "○ ",
        _ => "○ ",
    };
    Line::from(Span::styled(
        format!("{glyph}{text}"),
        style_of(colour, state_color(text)),
    ))
}

/// Braille spinner frames, stepped by the ~200 ms tick. Shown beside the
pub(crate) fn spinner(tick: u64) -> char {
    const FRAMES: [char; 8] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠇'];
    FRAMES[tick as usize % FRAMES.len()]
}

/// The live-indicator pulse: a two-step dot that breathes once per second at
pub(crate) fn pulse(tick: u64) -> char {
    const FRAMES: [char; 8] = ['●', '●', '●', '◉', '○', '◉', '●', '●'];
    FRAMES[(tick / 4) as usize % FRAMES.len()]
}

/// Background of the selected row: a faint tint, not `REVERSED` — reversing
pub(crate) fn selection_bg() -> Color {
    themed(Color::Indexed(236))
}

/// A completed task of that stage, for the Stats matrix: the live hues when
pub(crate) fn stage_count_cell(colour: bool, stage: &str, n: u64) -> Line<'static> {
    let color = match stage {
        "crawl" | "digest" | "render" | "merge" => {
            if colour {
                themed(stage_color(stage))
            } else {
                Color::DarkGray
            }
        }
        _ => Color::DarkGray,
    };
    Line::from(Span::styled(n.to_string(), Style::default().fg(color)))
}

/// Progress at half-block resolution: full, seven-eighths … one-eighth, then
pub(crate) fn bar_parts(frac: f32, width: usize) -> (String, String) {
    const EIGHTHS: [char; 7] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉'];
    let frac = frac.clamp(0.0, 1.0) as f64;
    let exact = frac * width as f64;
    let full = exact.floor() as usize;
    let mut done = String::with_capacity(width * 3);
    done.push_str(&"█".repeat(full));
    if full >= width {
        return (done, String::new());
    }
    // The remainder in eighths, rounded; a zero remainder is left to the track,
    let rest = exact - full as f64;
    if rest > 0.0 {
        let eighths = ((rest * 8.0).round() as usize).clamp(1, 7);
        done.push(EIGHTHS[eighths - 1]);
    }
    let track = width - done.chars().count();
    (done, "░".repeat(track))
}

/// A pane's "nothing here yet" body: centred, dim, and always actionable.
pub(crate) fn empty_body(lines: Vec<String>) -> Paragraph<'static> {
    let text: Vec<Line> = lines
        .into_iter()
        .map(|l| Line::from(Span::styled(l, Style::default().fg(Color::DarkGray))))
        .collect();
    Paragraph::new(text)
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true })
}

pub(crate) fn centered(area: Rect, w: u16, h: u16) -> Rect {
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
pub(crate) fn centered_padded(area: Rect, w: u16, h: u16, pad: u16) -> Rect {
    centered(
        area,
        w.min(area.width.saturating_sub(pad * 2)),
        h.min(area.height.saturating_sub(pad * 2)),
    )
}

/// The chapter range from the inductor's settings, for the footer: `start`
pub(crate) fn range_label(settings: &Option<serde_json::Value>) -> Option<String> {
    let s = settings.as_ref()?;
    let start = s.get("start").and_then(|v| v.as_u64())? as u32;
    let count = s.get("count").and_then(|v| v.as_u64())? as u32;
    if count == 0 {
        return None;
    }
    Some(format!("chapters {start}–{}", start + count - 1))
}
