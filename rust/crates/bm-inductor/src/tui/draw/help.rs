//! Help overlay.
use crate::tui::{app::App, style::centered_padded};
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
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
            "P",
            "work policy: which stages the selected box may run, in priority order",
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
            "cycle the palette: default → dim → mono (state words are always shown, so nothing depends on colour)",
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
        "`:` opens the command line: words like :add :reconcile :backend :stop",
        "or :quit (letters like :m :B :X still work). Every action runs from",
        "here — no single key can fire anything destructive, so a stray",
        "keypress is always safe.",
        "Actions that need more input (add machine, provision, translate, crawl)",
        "open their normal prompt or confirm after Enter.",
    ] {
        lines.push(Line::from(Span::styled(format!("  {v}"), dim)));
    }
    section(&mut lines, "Commands");
    // Rendered from the word table, so a word and its explanation cannot
    // drift apart — add the word (and its aliases) in input/command.rs and
    // it shows up here with its own line.
    for w in crate::tui::input::command::WORDS
        .iter()
        .filter(|w| w.desc.is_some())
    {
        let mut head = match w.key {
            Some(k) => format!(":{k}  :{}", w.names[0]),
            None => format!(":{}", w.names[0]),
        };
        for a in &w.names[1..] {
            head.push_str(&format!(" :{a}"));
        }
        lines.push(Line::from(vec![
            Span::styled(format!("  {head:<30}"), app.style(Color::Cyan)),
            Span::raw(w.desc.unwrap_or("").to_string()),
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
        "R requeues every merge (render cache kept); E re-renders everything,",
        "asking first. Capitals, so lowercase keeps typing into the filter.",
    ] {
        lines.push(Line::from(Span::styled(format!("  {v}"), dim)));
    }

    section(&mut lines, "Voice picker (:swap) and cast overview (:cast)");
    for v in [
        "Step 1 picks a character, step 2 picks a voice. :cast shows the",
        "whole cast at once, read-only — swapping happens only in the picker.",
        "Step 2 and the overview open in audition focus (see below); step 1",
        "types every letter. Accents are ignored, so \"thai son\" finds",
        "\"Thái Sơn\".",
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
        "Three keys — `t`, `T`, `^T` — or three words — `:current`, `:try`,",
        "`:another` — and none of them assign anything. `t` plays the held",
        "line with the current voice, from cache only: zero synthesis. A",
        "miss plays nothing and names the render key. In the picker `t`",
        "never follows the cursor; in the cast overview it follows the",
        "highlighted speaker.",
        "`T` renders that same held line with the pointed voice: the one",
        "deliberate generation, and the only way to hear two voices on the",
        "same sentence before either is assigned. Inductor down: this box",
        "synthesizes it instead, so no worker needs to be on.",
        "`^T` renders another line with the pointed voice; the chosen one",
        "is shown above the list.",
        "Keys or filter, never both: both screens open in audition focus,",
        "where `t`/`T`/`^T` play and any other letter focuses the filter",
        "instead. While the filter is focused every letter types — `t`/`T`",
        "included — and the keys go quiet (the words still audition from",
        "the command line). `^R` focuses explicitly; `Esc` blurs back to",
        "the audition keys.",
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

    section(&mut lines, "Sound design (:sound)");
    for v in [
        "Three layers, one tab each: effects (place beds, picked by the scene",
        "map's rules), music (tracks, picked by the palette) and injects (spot",
        "effects the script places by name). ←→ or Tab switches tab.",
        "a adds an entry, e edits the whole entry, l retunes its own level, and",
        "d removes it. The line you type is key=value and is prefilled with the",
        "values actually in force: name=, files=, tags=, plus the fields the",
        "layer has — looped on effects, mode/hold/dur_s on injects. An inject's",
        "dur_s is re-probed from the clip rather than typed, because it is a",
        "fact about the clip and a typed number drifts from the file.",
        "REMOVE IS DISABLED FOR ANYTHING STILL IN USE. A scene names tags, not",
        "sounds, so a sound the scene map can still reach would go quiet rather",
        "than fail if it were dropped — the status column and the line under the",
        "table name every rule, palette value or chapter that still reaches the",
        "highlighted entry, and the action bar marks d as unavailable. Clear the",
        "reference (scene-map.json, or re-digest the script) and it becomes",
        "removable. Removing never deletes a clip: the registry is the pool.",
        "A level here is the third rung of a ladder: the sound's own trim × the",
        "layer's master (shown in the header) × the :mix volume. An absent level",
        "is 1.0, so a pool written before the field existed mixes as it did.",
        "The registries are hand-formatted and their _note is documentation, so",
        "an edit rewrites only the entry it touched and leaves the rest of the",
        "file byte for byte. The merge runs here and picks the change up at",
        "once; a worker running a digest uses the copy it was provisioned with,",
        "so re-provision a box whose pools have changed.",
    ] {
        lines.push(Line::from(Span::styled(format!("  {v}"), dim)));
    }

    section(&mut lines, "Machines, Workers and Stats (dashboard)");
    for v in [
        "Machines reads `machine · kind · ip`: the box's handle, where it came",
        "from (aws for an EC2-launched box, rmt for one reached by ssh, local for",
        "this host), and the address to reach it. The `policy` column is the",
        "box's work order at a glance (`M>R>D>C`; a lower-case letter is off).",
        "Workers lists every live box: alias, machine, stage, chapter,",
        "progress, cpu %, ram % + used GiB (a dash until the agent measures).",
        "Stats sits beside Tasks: rows are workers, columns the four stages,",
        "each number completed tasks of that stage on that worker. The eta",
        "column is measured here, not reported — the stage's median task",
        "duration scaled by the beat's unworked fraction, a dash with no",
        "history yet. Both panes are full-tier only; the compact tier keeps",
        "Logs readable instead.",
    ] {
        lines.push(Line::from(Span::styled(format!("  {v}"), dim)));
    }

    section(&mut lines, "Work policy (P)");
    for v in [
        "Each machine picks its own next task. The list shows the four stages —",
        "merge, render, digest, crawl — most-preferred first, all enabled by",
        "default. The scheduler takes the first *enabled* stage that has work on",
        "that box, and falls to the next when it has none.",
        "↑↓ move the cursor; Space picks a row up and the arrows carry it up or",
        "down the order, Space drops it; Enter toggles a stage on or off. Every",
        "change saves at once, so there is no unsaved state to lose on Esc.",
        "A stage is also gated by what the box can actually do: a worker without",
        "ffmpeg reports no `merge` capability, so merge stays off there until",
        "provisioning installs ffmpeg (or you install it and re-provision).",
    ] {
        lines.push(Line::from(Span::styled(format!("  {v}"), dim)));
    }

    section(&mut lines, "Outside the TUI");
    for v in [
        "The AWS *console* is the only thing the dashboard cannot stand in for:",
        "creating the IAM user, its access key, the SSH keypair and the security",
        "group are AWS's own browser pages. Everything the tool owns is here —",
        "`:` :login stores the console's CSV, :discover reads the account into",
        ".bm/aws.json, :up launches and links, :pool shows the account, :down",
        "terminates, :prov onboards. An EC2 public IP changes on every stop/start",
        "and spot relaunch — a drifted box is now re-pointed automatically when",
        "the account is read (:pool, and once at startup), matched by instance id;",
        ":relink is still there to force it by hand. Each repair is logged.",
        "No step of an AWS pool needs a terminal —",
        "the CLI still keeps `aws up --dry-run`, a no-call preview of the launch.",
        "While the console work is not done yet, `:login` and `:discover` say what",
        "is missing rather than failing obscurely.",
    ] {
        lines.push(Line::from(Span::styled(format!("  {v}"), dim)));
    }

    section(&mut lines, "Notes");
    for v in [
        "Swap voice deletes only that speaker's cached segments, drops the stale",
        "mp3s and requeues render + merge. Every other character keeps its cache.",
        "Enrolled clones always pass, because they were vetted on enrolment.",
        "Below 100x30 the Tasks pane folds into the footer so Logs keeps its rows;",
        "below 76x20 the dashboard is replaced by a size notice, because a clipped",
        "dashboard is worse than an honest one.",
        "Jobs run in the background: the interface never blocks, and a second copy of",
        "the same operation is refused while the first is still in flight.",
    ] {
        lines.push(Line::from(Span::styled(format!("  {v}"), dim)));
    }

    let block = super::pane_block(app, "Help — Esc or ? to close · ↑↓ scroll");
    let inner_h = area.height.saturating_sub(2) as usize;
    let max = lines.len().saturating_sub(inner_h);
    let offset = scroll.min(max) as u16;
    f.render_widget(Paragraph::new(lines).block(block).scroll((offset, 0)), area);
}
