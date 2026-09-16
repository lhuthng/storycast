//! Colour-aware primitives: severity, cells, empty states, overlays.
use bm_proto::Machine;
use ratatui::{
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};

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

    pub(crate) fn glyph(self) -> &'static str {
        match self {
            Level::Info => "·",
            Level::Ok => "✓",
            Level::Warn => "!",
            Level::Error => "✗",
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

pub(crate) fn bar(frac: f32, width: usize) -> String {
    let fill = (frac.clamp(0.0, 1.0) * width as f32).round() as usize;
    format!(
        "{}{}",
        "█".repeat(fill),
        "░".repeat(width.saturating_sub(fill))
    )
}

pub(crate) fn state_color(s: &str) -> Color {
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
pub(crate) fn stage_color(s: &str) -> Color {
    match s {
        "crawl" => Color::Blue,
        "digest" => Color::Magenta,
        "render" => Color::Cyan,
        "merge" => Color::Green,
        _ => Color::Gray,
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

/// Colour-aware style as a free function, so render closures capture a plain
/// `bool` instead of the whole `App` — which the panes are also borrowing.
pub(crate) fn style_of(colour: bool, c: Color) -> Style {
    if colour {
        Style::default().fg(c)
    } else {
        Style::default()
    }
}

pub(crate) fn style_bold_of(colour: bool, c: Color) -> Style {
    if colour {
        Style::default().fg(c).add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    }
}

pub(crate) fn state_cell(colour: bool, text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        style_of(colour, state_color(text)),
    ))
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
