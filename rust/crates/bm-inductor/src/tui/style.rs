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
/// ANSI hues; the others exist because a dashboard that is only legible on
/// one terminal is a dashboard half its operators cannot read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Theme {
    /// The stock colours every pane already used.
    #[default]
    Default,
    /// Sunken hues for dark terminals: same structure, less glare.
    Dim,
    /// Black/white plus intensity, for e-ink, colour-blind operators and
    /// terminals that mangle the palette — the state word always carries the
    /// meaning, so nothing depends on colour alone.
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
    /// pane that needs a colour asks the theme rather than hard-coding it,
    /// which is what made retuning "the reds" a five-file hunt.
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
/// the help box all draw with it, which is what makes the chrome read as one
/// surface rather than five unrelated boxes.
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
/// closures can call it alongside `style_of`.
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
    /// same offset on every line, which is what makes a pane of timestamps
    /// readable at all. The old mixed-width glyphs (`·`, `OK`, `ERROR`) left
    /// the message ragged by construction.
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
    /// Relative uptime was unreadable next to multi-day ledger history —
    /// local time is what an operator correlates against everything else.
    pub(crate) wall: u64,
    pub(crate) text: String,
}

/// Local `HH:MM:SS` for a pane stamp. Unparseable input (including 0, the
/// never-logged sentinel) reads as dashes, never as 1970.
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
///
/// Anything unrecognised reads as info rather than as an error: a newer inductor
/// with a level this build has never heard of must not paint the log red.
pub(crate) fn level_from_str(s: &str) -> Level {
    match s {
        "ok" => Level::Ok,
        "warn" => Level::Warn,
        "error" => Level::Error,
        _ => Level::Info,
    }
}

/// Reachability of the inductor. Kept apart from `status` so a transient
/// network blip never overwrites the result of the last operator action.
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
        // booting. Red here would make a fresh pool look broken, which is the
        // misreading the state exists to prevent.
        "running" | "assigned" | "probing" | "provisioning" | "initializing" | "awaiting-ip" => {
            Color::Yellow
        }
        "offline" | "failed" | "shelved" | "error" => Color::Red,
        "pending" => Color::Gray,
        // Parked by the operator. Grey rather than yellow: it is neither a wait
        // (nothing is coming) nor a fault (nothing is wrong), and the two hues
        // already mean those. Named explicitly instead of falling through, so a
        // later state cannot inherit this one's colour by landing on `_`.
        "relaxed" => Color::Gray,
        _ => Color::Gray,
    }
}

/// Pipeline stages get their own hues so the Workers pane reads at a glance.
/// Previously the stage column reused the *task-state* palette, which no stage
/// name ever matched — the column was permanently grey.
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
///
/// The stage it is working on, so a cluster mid-render is a wall of cyan and a
/// digest is a wall of magenta — the Workers pane's reading carried over, not a
/// second one invented. A fault outranks it, exactly as in the table's `state`
/// column: a broken box is red whatever it was doing when it broke.
///
/// Idle and never-contacted are **not** the same grey. A box that is up and has
/// nothing to do is the cluster working; one that has never answered is the
/// thing an operator is looking for, and wearing the same colour would make it
/// the thing they stop looking for.
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
/// worker id. Raw ids (`host-pid`) are meaningless to an operator and change
/// on every restart; the alias is arbitrary but stable for the same id, so a
/// box is recognisable at a glance. Display-only — the protocol, ledger and
/// affinity still use the raw id.
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
/// racers are grinding the same row. One cell, so the column stays narrow.
pub(crate) fn worker_racing(t: &bm_proto::Task) -> String {
    let base = worker_name(t.assigned_to.as_deref());
    if t.racers.is_empty() {
        base
    } else {
        format!("{} +{} racing", base, t.racers.len())
    }
}

/// `last_seen` is 0 for a machine that has never reported. Subtracting it from
/// now produced a ~56-year uptime; say "never" instead.
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
/// `state_since`.
///
/// Worth a line of its own because the state alone does not say whether a box
/// is *just* booting or has been stuck for twenty minutes, and those two want
/// opposite reactions. `—` when the record was never stamped (it predates the
/// field), so a missing clock reads as unknown rather than as "0s ago".
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
/// paragraph; the table points at it and Enter shows it in full.
pub(crate) fn why_label(detail: &str) -> String {
    let first = detail.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    if first.is_empty() {
        return "—".into();
    }
    bm_core::util::head_chars(first.trim(), 120)
}

/// Leading speaker token of a log line, if any: `[192.168.2.2] …` and
/// `localhost-4578: …` qualify; plain sentences (`reconcile: …`) do not — a
/// bare word before a colon is message text, not a speaker. The alias
/// prefixes the untouched line, so monochrome mode loses colour but no
/// information.
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
/// (see `Theme`), so `C` cycles Default → Dim → Mono and the palette lives in
/// one place instead of being hard-coded across the panes.
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
/// colour-blind read the text, never the hue — but the dot gives the eye an
/// anchor: ● running/healthy, ◐ transitional, ✗ failed, ○ pending.
pub(crate) fn state_glyph_cell(colour: bool, text: &str) -> Line<'static> {
    let glyph = match text {
        "online" | "done" | "configured" | "ok" => "● ",
        // `initializing` joins the in-progress family: a box booting is on its
        // way, not missing. The hollow circle stays for "nothing known".
        "running" | "assigned" | "probing" | "provisioning" | "initializing" | "awaiting-ip" => {
            "◐ "
        }
        "offline" | "failed" | "shelved" | "error" => "✗ ",
        // See `state_color`: parked is the hollow circle — nothing running,
        // nothing wrong, nothing pending.
        "relaxed" => "○ ",
        _ => "○ ",
    };
    Line::from(Span::styled(
        format!("{glyph}{text}"),
        style_of(colour, state_color(text)),
    ))
}

/// Braille spinner frames, stepped by the ~200 ms tick. Shown beside the
/// footer's job count so "work is happening" is visible even when the job is
/// quiet — a static "1 job(s) running" read as stuck text.
pub(crate) fn spinner(tick: u64) -> char {
    const FRAMES: [char; 8] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠇'];
    FRAMES[tick as usize % FRAMES.len()]
}

/// The live-indicator pulse: a two-step dot that breathes once per second at
/// the poll cadence. Subtle on purpose — steady means healthy, and anything
/// louder would compete with real warnings.
pub(crate) fn pulse(tick: u64) -> char {
    const FRAMES: [char; 8] = ['●', '●', '●', '◉', '○', '◉', '●', '●'];
    FRAMES[(tick / 4) as usize % FRAMES.len()]
}

/// Background of the selected row: a faint tint, not `REVERSED` — reversing
/// the row inverted the state colours, so a green `online` read as a dark
/// pill and the task ledger lost its severity tint on the cursor line.
pub(crate) fn selection_bg() -> Color {
    themed(Color::Indexed(236))
}

/// A completed task of that stage, for the Stats matrix: the live hues when
/// the theme offers them, dim grey otherwise. A number and its stage column
/// share a hue so the matrix reads by column as well as by row.
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
/// the light shade. Ten columns of `█░` can only move in 10% steps; these
/// partial glyphs make the same width read in ~2% steps, so a bar that is
/// "almost done" looks almost done.
pub(crate) fn bar(frac: f32, width: usize) -> String {
    const EIGHTHS: [char; 7] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉'];
    let frac = frac.clamp(0.0, 1.0) as f64;
    let exact = frac * width as f64;
    let full = exact.floor() as usize;
    let rest = exact - full as f64;
    let mut out = String::with_capacity(width * 3);
    out.push_str(&"█".repeat(full));
    if full < width {
        // The remainder in eighths, rounded; zero remainder is the light
        // shade, never a stray one-eighth tick.
        if rest <= 0.0 {
            out.push('░');
        } else {
            let eighths = ((rest * 8.0).round() as usize).clamp(1, 7);
            out.push(EIGHTHS[eighths - 1]);
        }
        out.push_str(&"░".repeat(width - full - 1));
    }
    out
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
/// the border reads as a layout bug rather than a dialog.
pub(crate) fn centered_padded(area: Rect, w: u16, h: u16, pad: u16) -> Rect {
    centered(
        area,
        w.min(area.width.saturating_sub(pad * 2)),
        h.min(area.height.saturating_sub(pad * 2)),
    )
}

/// The chapter range from the inductor's settings, for the footer: `start`
/// and `count` are what the prompts prefill and what `B` reconciles, so they
/// stay on screen instead of living in a file nobody opens.
pub(crate) fn range_label(settings: &Option<serde_json::Value>) -> Option<String> {
    let s = settings.as_ref()?;
    let start = s.get("start").and_then(|v| v.as_u64())? as u32;
    let count = s.get("count").and_then(|v| v.as_u64())? as u32;
    if count == 0 {
        return None;
    }
    Some(format!("chapters {start}–{}", start + count - 1))
}
