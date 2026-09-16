//! Help overlay.
use crate::tui::{app::App, style::centered_padded};
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};

pub(crate) fn draw_help(f: &mut ratatui::Frame, app: &App, scroll: usize) {
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
        (
            "K",
            "task ledger: every task, its failure detail, and a re-queue key",
        ),
        (
            "i",
            "inspect the selected machine (probe output, capabilities)",
        ),
        (
            "J / :jobs",
            "TUI background jobs: running/queued, elapsed time, activity",
        ),
        ("R", "system overview: preview everything"),
        ("PgUp PgDn", "scroll the log   (G returns to newest)"),
        ("r", "refresh now"),
        ("?", "this help"),
        (
            "C",
            "toggle colour (state names are always shown, so nothing depends on colour)",
        ),
        ("q", "quit"),
    ] {
        lines.push(Line::from(vec![
            Span::styled(format!("  {k:<12}"), app.style(Color::Cyan)),
            Span::raw(v.to_string()),
        ]));
    }

    section(&mut lines, "The command line");
    for v in [
        "`:` opens the command line: `:m`, `:B`, `:X`, or words like :reconcile, :backend, :stop",
        "or :quit. Every action runs from here — no single key can fire",
        "anything destructive, so a stray keypress is always safe.",
        "Actions that need more input (add machine, provision, translate, crawl)",
        "open their normal prompt or confirm after Enter.",
    ] {
        lines.push(Line::from(Span::styled(format!("  {v}"), dim)));
    }
    section(&mut lines, "Commands");
    for (k, v) in [
        (":a  :add", "add a machine by IP or hostname"),
        (
            ":A  :sample",
            "pool a clip — tags from the filename, enrolled locally",
        ),
        (
            ":N  :named",
            "a `path as Name` voice — manual assignment only",
        ),
        (":p  :provision", "provision the selected machine"),
        (
            ":P  :reprovision",
            "re-provision it, forcing past the skip-if-configured check",
        ),
        (
            ":d  :drop",
            "drop the selected machine from the cluster registry",
        ),
        (
            ":t  :translate",
            "enqueue crawl + digest for a chapter range",
        ),
        (
            ":c  :crawl",
            "save the URL template, then probe-crawl one chapter",
        ),
        (
            ":v  :voices",
            "re-read the roster, enforce the accent policy, refill gaps",
        ),
        (
            ":s  :swap",
            "repoint one character — destructive, see below",
        ),
        (
            ":S  :cast",
            "cast overview: every speaker × voice, read-only",
        ),
        (":e  :eta", "estimate the remaining wall-clock time"),
        (":u  :retry", "requeue every shelved task — strikes reset"),
        (
            ":m  :reconcile",
            "fold duplicates — asks first; certain folds apply, ambiguous only listed",
        ),
        (
            ":B  :backend",
            "backend up now, machines provision in background and join as ready",
        ),
        (
            ":X  :stop",
            "stop everything everywhere: local backend plus workers on all machines",
        ),
    ] {
        lines.push(Line::from(vec![
            Span::styled(format!("  {k:<16}"), app.style(Color::Cyan)),
            Span::raw(v.to_string()),
        ]));
    }
    lines.push(Line::from(Span::styled(
        "  backend logs live in .bm/inductor.log and .bm/agent.log",
        Style::default().fg(Color::DarkGray),
    )));

    section(&mut lines, "Task ledger (K)");
    for v in [
        "Filter with a few letters, Enter for the full failure reason.",
        "u retries the highlighted row; F force re-runs it. Both are direct",
        "keys here — this screen is read-only navigation otherwise.",
    ] {
        lines.push(Line::from(Span::styled(format!("  {v}"), dim)));
    }

    section(&mut lines, "Voice picker (:s) and cast overview (:S)");
    for v in [
        "Step 1 picks a character, step 2 picks a voice. :S shows the whole",
        "cast at once, read-only — swapping happens only in the picker.",
        "Type to filter. Accents are ignored, so \"thai son\" finds \"Thái Sơn\".",
        "Movement is arrow keys only, so letters reach the filter — except",
        "`t`/`T` on step 2 and the cast overview, which audition instead.",
        "Every voice is listed with gender, accent, language and style, plus whether",
        "it is already in use and whether the accent policy permits it.",
        "Pooled samples show their tags (pool: young, female) — type one to filter.",
        "Enter advances or applies; Esc goes back one step.",
    ] {
        lines.push(Line::from(Span::styled(format!("  {v}"), dim)));
    }

    section(
        &mut lines,
        "Auditioning a voice (picker step 2 and cast overview)",
    );
    for v in [
        "`t`, `T` and `^T` audition and nothing else — the two letters don't",
        "filter on these two screens (step 1 still types everything). `t`",
        "plays the held line with the current voice, from cache only: zero",
        "synthesis. A miss plays nothing and names the render key. In the",
        "picker `t` never follows the cursor; in the cast overview it follows",
        "the highlighted speaker.",
        "`T` renders that same held line with the pointed voice: the one",
        "deliberate generation, and the only way to hear two voices on the",
        "same sentence before either is assigned.",
        "`^T` renders another line with the pointed voice; the chosen one",
        "is shown above the list.",
        "Enter on a voice locks that sentence: later auditions keep it instead of",
        "another random pick. None of these assign anything — only the confirm",
        "after Enter changes the cast.",
        "The line index is read once per session (a hundred scripts) and starts",
        "building when the screen opens, not when you press the key.",
        "Playback is afplay, one sample at a time; a new one stops the last.",
        "The audio comes back from the inductor as bytes, so it is written to a",
        "single temp file that each audition overwrites and exiting removes. The",
        "repo is never touched, and the file sits next to the speaker even when",
        "the inductor is on another box.",
    ] {
        lines.push(Line::from(Span::styled(format!("  {v}"), dim)));
    }

    section(&mut lines, "Notes");
    for v in [
        "Swap voice deletes only that speaker's cached segments, drops the stale",
        "mp3s and requeues render + merge. Every other character keeps its cache.",
        "VieNeu presets are Central/South only — Northern voices are rejected by",
        "policy. Enrolled clones always pass, because they were vetted on enrolment.",
        "Below 100x30 the Tasks pane folds into the footer so Logs keeps its rows;",
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
    f.render_widget(Paragraph::new(lines).block(block).scroll((offset, 0)), area);
}
