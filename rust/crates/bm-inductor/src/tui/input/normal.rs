//! Normal mode: operator keys. Destructive actions live behind `:`.
use crate::tui::{
    app::App,
    input::{dispatch, runconfig::run_preview},
    jobs::Job,
    screen::{Confirm, ConfirmAction, Screen, TasksView, TextKind, TextPrompt},
    style::{theme_next, Conn, Level, Theme},
};
use crossterm::event::{KeyCode, KeyEvent};
use std::sync::{atomic::AtomicBool, Arc};

pub(crate) async fn normal_key(
    app: &mut App,
    key: KeyEvent,
    http: &reqwest::Client,
    job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
) -> bool {
    match key.code {
        KeyCode::Char('q') => {
            if app.pending > 0 {
                app.screen = Screen::Confirm(Confirm {
                    title: "Quit with work in flight?".into(),
                    danger: true,
                    body: vec![
                        format!("{} background job(s) are still running.", app.pending),
                        "The inductor keeps working without the TUI, but you will lose".into(),
                        "the event log and any in-flight result.".into(),
                    ],
                    action: ConfirmAction::Quit,
                });
            } else {
                return true;
            }
        }
        KeyCode::Char('?') => app.screen = Screen::Help { scroll: 0 },
        // Hand the mouse back to the terminal.
        //
        // While mouse reporting is on, the terminal gives every drag to us
        // instead of treating it as a selection, so an error message cannot be
        // highlighted and copied — which is the one thing anybody wants to do
        // with an error. This is the escape hatch, and it is a toggle rather
        // than a one-way door because clicking panes is also worth having.
        //
        // **`M`, not `m`**: `m` is the documented alias for `:m` (reconcile),
        // and taking a key that already means something is how a dashboard
        // grows two spellings for one action. Uppercase `M` is free in both the
        // bare-key map and the command aliases, and the two sit next to each
        // other on the keyboard on purpose.
        KeyCode::Char('M') => {
            app.mouse_capture = !app.mouse_capture;
            // The handler has no terminal; the loop owns it and applies this.
            app.mouse_toggle = true;
            app.set_status(
                Level::Info,
                if app.mouse_capture {
                    "mouse on — click panes · m again to select and copy text"
                } else {
                    "mouse off — drag to select and copy · m again for click-to-select"
                },
            );
        }
        KeyCode::Char('f') => {
            app.focused_panel = app.focused_panel.next_visible(app.machines_graph);
            app.set_status(
                Level::Info,
                format!(
                    "focus: {} — click a pane or use f to cycle",
                    app.focused_panel.label()
                ),
            );
        }
        KeyCode::Char(':') => {
            // Command mode: every operator action behind a prompt, so a stray
            // keypress can never provision, reconcile or stop anything.
            // Words first (`:reconcile`), single letters still work (`:m`).
            app.command_return = Some(Screen::Normal);
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::Command,
                ":",
                "command — a word (:add :prov :reconcile :quit) or a letter (:m :B :X)",
                "",
            ));
            app.set_status(Level::Info, "command mode — Enter runs it, Esc closes");
        }
        KeyCode::Char('c') => {
            // The crawl view. Lowercase, because uppercase C is the palette
            // cycle and the two must never trade places silently.
            app.screen = Screen::Crawl { scroll: 0 };
        }
        KeyCode::Char('C') => {
            // The theme cycle: default → dim → mono. The palette itself is
            // resolved by `style_of`/`themed`, so flipping the theme here is
            // the whole change — no pane repaints specially.
            let to = theme_next();
            app.theme = Theme::ALL
                .iter()
                .find(|t| t.label() == to)
                .copied()
                .unwrap_or_default();
            app.set_status(
                Level::Info,
                format!("theme {} (C cycles)", app.theme.label()),
            );
        }
        KeyCode::Char('r') => {
            app.refresh(http).await;
            app.set_status(Level::Ok, "refreshed");
        }
        // Draw the Machines pane as the hub-and-spoke picture, or back to the
        // table. A view preference only — it touches nothing about the cluster,
        // and it is not persisted, so a dashboard opened later is the table it
        // expects. The table stays the complete list: the graph is the glance,
        // and it says how many boxes it did not fit rather than dropping them.
        KeyCode::Char('g') => {
            app.machines_graph = !app.machines_graph;
            // The rack draws every box with the worker on it, so the Workers
            // pane is not shown at all in that mode — and focus must not be
            // left on a pane that has just left the screen.
            if app.machines_graph && app.focused_panel == crate::tui::app::Panel::Workers {
                app.focused_panel = crate::tui::app::Panel::Machines;
            }
            let (level, msg) = if app.machines_graph {
                (
                    Level::Info,
                    "machines as a rack — ↑↓ across it, ←→ along · g for the table",
                )
            } else {
                (Level::Info, "machines as a table — g for the rack")
            };
            app.set_status(level, msg);
        }
        // Walk the rack sideways. The console is anchored at the left, so the
        // boxes are what moves — and the arrows are the graph's only, because
        // in the table the cursor moves down a list and there is nothing here
        // for a sideways press to mean.
        KeyCode::Left if app.machines_graph => app.selected = app.selected.saturating_sub(1),
        KeyCode::Right if app.machines_graph => {
            app.selected = (app.selected + 1).min(app.machines.len().saturating_sub(1))
        }
        // Up and down walk a *row* of the rack — a whole band of servers — and
        // a single machine in the table. The step is the pane's own width, which
        // the drawer publishes; a key handler cannot know it.
        KeyCode::Up | KeyCode::Char('k') => {
            let step = if app.machines_graph {
                app.graph_cols
            } else {
                1
            };
            app.selected = app.selected.saturating_sub(step);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            let step = if app.machines_graph {
                app.graph_cols
            } else {
                1
            };
            app.selected = (app.selected + step).min(app.machines.len().saturating_sub(1));
        }
        KeyCode::Home => app.selected = 0,
        KeyCode::End => app.selected = app.machines.len().saturating_sub(1),
        KeyCode::PageUp => {
            // A page is a page: the draw publishes the pane's row count, so
            // one press is one screenful and the walk back to the newest line
            // costs exactly what the walk out did (or one `G`). The old
            // fixed 5 made the buffer a hundred presses each way.
            app.scroll_events_older(app.events_rows.max(1));
        }
        KeyCode::PageDown => {
            app.scroll_events_newer(app.events_rows.max(1));
        }
        KeyCode::Char('G') => {
            app.events_scroll = 0;
            app.set_status(Level::Info, "event log pinned to newest");
        }
        KeyCode::Char('i') => match app.selected_machine() {
            None => app.set_status(Level::Warn, "no machine selected"),
            Some(m) => app.screen = Screen::Machine(m.addr.clone()),
        },
        // The work policy for the selected box: which stages it may run and in
        // what order. Read-only keys open screens directly; this one edits
        // scheduling, but it is per-machine and reversible, so it sits beside
        // `i` rather than behind a `:` confirmation.
        KeyCode::Char('P') => match app.selected_machine() {
            None => app.set_status(
                Level::Warn,
                "no machine selected — the policy editor works on one box",
            ),
            Some(m) => {
                let label = crate::tui::model::machine_label(&m);
                app.screen = Screen::Policy(crate::tui::screen::PolicyView::new(
                    m.addr.clone(),
                    label,
                    m.effective_task_policy(),
                ));
                app.set_status(
                    Level::Info,
                    "work policy — ↑↓ move · Space grab · Enter toggle · Esc close",
                );
            }
        },
        // Park the selected box, or wake it: no new work, and its sidecar is
        // let go so the 2.85 GB it holds goes back to the machine.
        //
        // On the key rather than behind a `:` confirmation because it is the
        // most reversible thing here — one press undoes it, nothing is lost, and
        // a task already running is allowed to finish. That last part is why
        // this is not `X`: the stop flow is a verdict about the run, this is a
        // pause on one box, and confusing the two is how somebody parks a box
        // expecting the cluster to stop.
        KeyCode::Char('z') => match app.selected_machine() {
            None => app.set_status(Level::Warn, "no machine selected"),
            Some(m) => {
                let (addr, label) = (m.addr.clone(), crate::tui::model::machine_label(&m));
                // The intent is known now, but the pane renders what the
                // inductor last reported — so the status line says what was
                // asked for, and the state column says what is true once a poll
                // has come back. They can disagree for under a second; a
                // status line that claimed more than that would be the lie.
                let (level, msg) = if m.accepting_work {
                    (
                        Level::Info,
                        format!(
                            "parking {label} — takes nothing new, finishes what it is on, drops its sidecar"
                        ),
                    )
                } else {
                    (
                        Level::Ok,
                        format!("{label} woken — takes work again on its next ask"),
                    )
                };
                app.set_status(level, msg);
                dispatch(
                    app,
                    job_tx,
                    Job::SetAccepting {
                        api: app.api.clone(),
                        http: http.clone(),
                        addr,
                        accepting_work: !m.accepting_work,
                    },
                );
            }
        },
        // The digest manager. Unlike `P` it needs no machine: it is about the
        // *book*, not a box — every chapter the library knows, and a manual
        // two-round digest for one of them.
        //
        // The chapter list comes from the ledger the panes already hold rather
        // than a scan — plus the one next chapter past it, when its text is on
        // disk. A manual report creates its own digest row (see `complete`),
        // so that next chapter can land; anything further ahead cannot, and is
        // refused at open time to keep bible deltas landing in order.
        KeyCode::Char('D') => {
            let mut chapters: Vec<u32> = app.tasks.iter().map(|t| t.chapter).collect();
            chapters.sort_unstable();
            chapters.dedup();
            let next = chapters.last().copied().unwrap_or(0) + 1;
            if !chapters.contains(&next) && app.layout.chapter_txt(next).is_file() {
                chapters.push(next);
            }
            let total = chapters.len();
            app.screen = Screen::Digest(crate::tui::screen::DigestView::new(chapters));
            app.set_status(
                Level::Info,
                format!(
                    "digest manager — {total} chapters · ↑↓ move · Enter open · f hide digested · Esc close"
                ),
            );
        }
        KeyCode::Char('B') => {
            // Backend up now, boxes join in background — a second press
            // while the first sequence runs would provision everything
            // twice, so it is refused instead of queued.
            if app.backend_start_outstanding {
                app.set_status(Level::Warn, "backend start already running — watch events");
            } else {
                let cfg = run_preview(app);
                let cancel = Arc::new(AtomicBool::new(false));
                app.start_cancel = Some(cancel.clone());
                app.backend_start_outstanding = true;
                dispatch(
                    app,
                    job_tx,
                    Job::StartBackend {
                        layout: app.layout.clone(),
                        api: app.api.clone(),
                        api_up: app.conn == Conn::Up,
                        start: cfg.start,
                        count: cfg.count,
                        enqueue: false,
                        machines: app.effective_machines(),
                        cancel,
                        settings_key: app.ssh_defaults().key,
                    },
                );
                app.set_status(
                    Level::Info,
                    "starting backend now — boxes join in background; watch events",
                );
            }
        }
        KeyCode::Char('R') => {
            app.screen = Screen::Run;
            if app.roster.is_none() {
                app.load_roster(job_tx, http);
            }
        }
        // The task ledger. Capital K so the lowercase `k` can stay "move up" —
        // and so it reads as the sibling of `R` (run screen) and `S` (cast).
        KeyCode::Char('K') => {
            app.screen = Screen::Tasks(TasksView::new());
        }
        // The background jobs (what the footer's "N job(s) running" actually
        // is). Tab is the spelling the footer advertises — it is the one
        // free top-row key and "flip to the other side of the dashboard" is
        // what Tab already means in the sound editor — with `J` kept as the
        // mnemonic alias (sibling of `K`, capital so lowercase `j` stays
        // "move down").
        KeyCode::Tab | KeyCode::Char('J') => {
            let previous = Box::new(app.screen.clone());
            app.screen = Screen::Jobs {
                scroll: 0,
                previous,
            };
        }
        // Operator commands fire from the `:` line only: a stray keypress
        // must never provision, reconcile or stop anything. This arm catches
        // every gated key before the fallthrough swallows it silently.
        KeyCode::Char(c)
            if matches!(
                c,
                'a' | 'A'
                    | 'N'
                    | 'p'
                    | 'd'
                    | 't'
                    | 'c'
                    | 'v'
                    | 's'
                    | 'S'
                    | 'e'
                    | 'u'
                    | 'm'
                    | 'B'
                    | 'X'
                    | 'w'
                    | 'o'
                    | 'l'
            ) =>
        {
            app.set_status(
                Level::Warn,
                format!("use ':{c}' — operator commands live on the command line"),
            );
        }
        _ => {}
    }
    false
}
