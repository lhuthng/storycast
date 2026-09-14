//! Live cluster dashboard: machines, workers, tasks, events.
//!
//! Read-only against the inductor API except for explicit operator keys.
//! Slow work (provisioning) runs on background tasks; the UI never blocks.

use bm_core::Layout;
use bm_proto::{Heartbeat, Machine, Op, OpRequest, Task};
use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout as RLayout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Row, Table},
    Terminal,
};
use std::collections::VecDeque;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq)]
enum InputKind {
    AddMachine,
    Translate,
    SwapVoice,
}

struct App {
    api: String,
    machines: Vec<Machine>,
    beats: Vec<Heartbeat>,
    tasks: Vec<Task>,
    counts: serde_json::Value,
    events: VecDeque<String>,
    selected: usize,
    input: Option<(InputKind, String, String)>, // kind, prompt, buffer
    status: String,
    tick: u64,
}

impl App {
    fn new(api: &str) -> Self {
        App {
            api: api.trim_end_matches('/').to_string(),
            machines: Vec::new(),
            beats: Vec::new(),
            tasks: Vec::new(),
            counts: serde_json::Value::Null,
            events: VecDeque::with_capacity(200),
            selected: 0,
            input: None,
            status: "r refresh · q quit".into(),
            tick: 0,
        }
    }

    fn log(&mut self, line: String) {
        if self.events.len() >= 200 {
            self.events.pop_front();
        }
        self.events.push_back(line);
    }

    async fn refresh(&mut self, http: &reqwest::Client) {
        let url = format!("{}/api/state", self.api);
        match http.get(&url).send().await {
            Ok(r) => match r.json::<serde_json::Value>().await {
                Ok(v) => {
                    self.machines = serde_json::from_value(v.get("machines").cloned().unwrap_or_default())
                        .unwrap_or_default();
                    self.beats = serde_json::from_value(v.get("beats").cloned().unwrap_or_default())
                        .unwrap_or_default();
                    self.tasks = serde_json::from_value(v.get("tasks").cloned().unwrap_or_default())
                        .unwrap_or_default();
                    self.counts = v.get("counts").cloned().unwrap_or_default();
                    if self.selected >= self.machines.len().max(1) {
                        self.selected = 0;
                    }
                }
                Err(e) => self.status = format!("bad state payload: {e}"),
            },
            Err(e) => self.status = format!("inductor unreachable: {e}"),
        }
    }

    fn selected_machine(&self) -> Option<Machine> {
        self.machines.get(self.selected).cloned()
    }
}

fn bar(frac: f32, width: usize) -> String {
    let fill = (frac.clamp(0.0, 1.0) * width as f32).round() as usize;
    format!("{}{}", "█".repeat(fill), "░".repeat(width.saturating_sub(fill)))
}

fn state_color(s: &str) -> Color {
    match s {
        "online" | "done" => Color::Green,
        "running" | "assigned" => Color::Yellow,
        "offline" | "failed" | "shelved" => Color::Red,
        "provisioning" | "probing" => Color::Cyan,
        _ => Color::Gray,
    }
}

fn cell(text: String) -> Line<'static> {
    Line::from(text)
}

fn state_cell(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        Style::default().fg(state_color(text)),
    ))
}

fn draw(f: &mut ratatui::Frame, app: &App) {
    let root = RLayout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(8),
            Constraint::Length(9),
            Constraint::Length(5),
            Constraint::Min(6),
            Constraint::Length(2),
        ])
        .split(f.area());

    // Machines pane.
    let mrows: Vec<Row> = app
        .machines
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let mut row = Row::new(vec![
                cell(m.id.clone()),
                cell(m.addr.clone()),
                cell(m.role.clone()),
                state_cell(m.state.as_str()),
                cell(m.tts_url.clone().unwrap_or_else(|| "-".into())),
                cell(format!("{}s", bm_proto::now_secs().saturating_sub(m.last_seen))),
            ]);
            if i == app.selected {
                row = row.style(Style::default().add_modifier(Modifier::REVERSED));
            }
            row
        })
        .collect();
    let mtable = Table::new(
        mrows,
        [Constraint::Length(14), Constraint::Length(15), Constraint::Length(8),
         Constraint::Length(13), Constraint::Length(22), Constraint::Length(8)],
    )
    .header(Row::new(vec!["id", "addr", "role", "state", "tts", "seen"]).style(Style::default().add_modifier(Modifier::BOLD)))
    .block(Block::default().borders(Borders::ALL).title("Machines"));
    f.render_widget(mtable, root[0]);

    // Workers pane (live heartbeats).
    let wrows: Vec<Row> = app
        .beats
        .iter()
        .map(|b| {
            let st = b.stage.map(|s| s.as_str().to_string()).unwrap_or_else(|| "-".into());
            let ch = b.chapter.map(|c| c.to_string()).unwrap_or_else(|| "-".into());
            Row::new(vec![
                cell(b.worker_id.clone()),
                cell(b.hostname.clone()),
                state_cell(&st),
                cell(ch),
                cell(format!("{} {:>3}%", bar(b.progress, 10), (b.progress * 100.0) as u32)),
                cell(b.activity.clone()),
                cell(b.eta_secs.map(|e| format!("{}m", e / 60)).unwrap_or_else(|| "-".into())),
            ])
        })
        .collect();
    let wtable = Table::new(
        wrows,
        [Constraint::Length(14), Constraint::Length(14), Constraint::Length(8),
         Constraint::Length(5), Constraint::Length(17), Constraint::Min(20), Constraint::Length(6)],
    )
    .header(Row::new(vec!["worker", "machine", "stage", "ch", "progress", "activity", "eta"]).style(Style::default().add_modifier(Modifier::BOLD)))
    .block(Block::default().borders(Borders::ALL).title("Workers"));
    f.render_widget(wtable, root[1]);

    // Tasks pane.
    let mut tlines = vec![];
    if let Some(obj) = app.counts.as_object() {
        let mut stages: Vec<&String> = obj.keys().collect();
        stages.sort();
        for st in stages {
            let c = &obj[st.as_str()];
            let done = c.get("done").and_then(|v| v.as_u64()).unwrap_or(0);
            let total: u64 = c.as_object().map(|m| m.values().filter_map(|v| v.as_u64()).sum()).unwrap_or(0);
            let shelved = c.get("shelved").and_then(|v| v.as_u64()).unwrap_or(0);
            tlines.push(Line::from(vec![
                Span::styled(format!("{st:8}"), Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(format!("{done}/{total} done")),
                Span::styled(
                    if shelved > 0 { format!(" · shelved: {shelved}") } else { String::new() },
                    Style::default().fg(Color::Red),
                ),
            ]));
        }
    } else {
        tlines.push(Line::from("no task data (is the inductor up?)"));
    }
    // Shelved chapters named explicitly.
    let mut shelved: Vec<String> = app
        .tasks
        .iter()
        .filter(|t| format!("{:?}", t.state) == "Shelved")
        .map(|t| format!("{}:{}", t.stage, t.chapter))
        .collect();
    shelved.sort();
    shelved.dedup();
    if !shelved.is_empty() {
        tlines.push(Line::from(Span::styled(
            format!("shelved: {}", shelved.join(" ")),
            Style::default().fg(Color::Red),
        )));
    }
    f.render_widget(
        Paragraph::new(tlines).block(Block::default().borders(Borders::ALL).title("Tasks")),
        root[2],
    );

    // Events pane.
    let items: Vec<ListItem> = app.events.iter().map(|e| ListItem::new(e.as_str())).collect();
    f.render_widget(
        List::new(items).block(Block::default().borders(Borders::ALL).title("Events")),
        root[3],
    );

    // Footer: status or input.
    let footer = match &app.input {
        Some((_, prompt, buf)) => Line::from(vec![
            Span::styled(prompt.clone(), Style::default().fg(Color::Yellow)),
            Span::raw(buf.clone()),
            Span::raw("▌"),
        ]),
        None => Line::from(vec![
            Span::raw("a add · p provision · t translate · c crawl setup · v voices · s swap · e eta · r refresh · q quit    "),
            Span::styled(app.status.clone(), Style::default().fg(Color::Cyan)),
        ]),
    };
    f.render_widget(Paragraph::new(footer), root[4]);
}

fn submit_input(app: &mut App, http: &reqwest::Client, kind: InputKind, buf: String) -> Option<ProvisionJob> {
    match kind {
        InputKind::AddMachine => {
            let addr = buf.trim().to_string();
            if addr.is_empty() {
                app.status = "empty address".into();
                return None;
            }
            let m = Machine::new(&addr, "thang", 22, None, "worker");
            let api = app.api.clone();
            let http = http.clone();
            Some(ProvisionJob::AddMachine { api, http, m })
        }
        InputKind::Translate => {
            let mut it = buf.split_whitespace();
            let (start, count) = (it.next().and_then(|s| s.parse().ok()).unwrap_or(21),
                                  it.next().and_then(|s| s.parse().ok()).unwrap_or(80));
            let api = app.api.clone();
            let http = http.clone();
            Some(ProvisionJob::Op {
                api,
                http,
                req: OpRequest { op: Op::Translate, start: Some(start), count: Some(count), ..Default::default() },
            })
        }
        InputKind::SwapVoice => {
            let mut it = buf.split_whitespace();
            let (character, voice) = (it.next().unwrap_or("").to_string(), it.next().unwrap_or("").to_string());
            if character.is_empty() || voice.is_empty() {
                app.status = "usage: <character> <voice>".into();
                return None;
            }
            let api = app.api.clone();
            let http = http.clone();
            Some(ProvisionJob::Op {
                api,
                http,
                req: OpRequest { op: Op::SwapVoice, character: Some(character), voice: Some(voice), ..Default::default() },
            })
        }
    }
}

/// Background work the TUI must never block on.
enum ProvisionJob {
    /// Long SSH/rsync flow executed off the UI task.
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
    Op {
        api: String,
        http: reqwest::Client,
        req: OpRequest,
    },
}

async fn run_job(job: ProvisionJob, events: tokio::sync::mpsc::UnboundedSender<String>) {
    match job {
        ProvisionJob::Provision { layout_root, machine, force } => {
            let addr = machine.addr.clone();
            let layout = bm_core::Layout::new(&layout_root);
            let out = tokio::task::spawn_blocking(move || {
                crate::provision_machine(&layout, &machine.addr, &machine.ssh_user, machine.ssh_port, machine.ssh_key.clone(), force)
            })
            .await;
            match out {
                Ok(lines) => {
                    for l in lines {
                        let _ = events.send(l);
                    }
                    let _ = events.send(format!("[{addr}] provision finished"));
                }
                Err(e) => {
                    let _ = events.send(format!("[{addr}] provision task failed: {e}"));
                }
            }
        }
        ProvisionJob::AddMachine { api, http, m } => {
            let addr = m.addr.clone();
            match http.post(format!("{api}/api/machines")).json(&m).send().await {
                Ok(_) => {
                    let _ = events.send(format!("machine {addr} added — press p to provision"));
                }
                Err(e) => {
                    let _ = events.send(format!("add {addr} failed: {e}"));
                }
            }
        }
        ProvisionJob::Op { api, http, req } => {
            let name = req.op.as_str().to_string();
            match http.post(format!("{api}/api/op")).json(&req).send().await {
                Ok(r) => match r.json::<bm_proto::OpResult>().await {
                    Ok(res) => {
                        let _ = events.send(format!("op {name}: {}", res.message));
                    }
                    Err(e) => {
                        let _ = events.send(format!("op {name}: bad result: {e}"));
                    }
                },
                Err(e) => {
                    let _ = events.send(format!("op {name} failed: {e}"));
                }
            }
        }
    }
}

fn op_now(
    api: &str,
    http: &reqwest::Client,
    req: OpRequest,
) -> ProvisionJob {
    ProvisionJob::Op { api: api.to_string(), http: http.clone(), req }
}

pub async fn run(api: &str, layout: Layout) -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let result = run_loop(api, layout, &mut terminal).await;
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
    let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(10)).build()?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<ProvisionJob>();
    // Background worker: at most one provision at a time (they fight over ssh).
    tokio::spawn(async move {
        while let Some(job) = job_rx.recv().await {
            run_job(job, tx.clone()).await;
        }
    });
    app.refresh(&http).await;
    loop {
        terminal.draw(|f| draw(f, &app))?;
        // Drain background events without blocking the UI.
        while let Ok(line) = rx.try_recv() {
            app.log(line);
        }
        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                match &mut app.input {
                    Some((kind, _, buf)) => match key.code {
                        KeyCode::Esc => {
                            app.input = None;
                            app.status = "cancelled".into();
                        }
                        KeyCode::Enter => {
                            let kind = *kind;
                            let buf = std::mem::take(buf);
                            app.input = None;
                            if let Some(job) = submit_input(&mut app, &http, kind, buf) {
                                let _ = job_tx.send(job);
                            }
                        }
                        KeyCode::Backspace => {
                            buf.pop();
                        }
                        KeyCode::Char(c) => {
                            buf.push(c);
                        }
                        _ => {}
                    },
                    None => match key.code {
                        KeyCode::Char('q') => break,
                        KeyCode::Char('r') => app.refresh(&http).await,
                        KeyCode::Up => {
                            app.selected = app.selected.saturating_sub(1);
                        }
                        KeyCode::Down => {
                            app.selected = (app.selected + 1).min(app.machines.len().saturating_sub(1));
                        }
                        KeyCode::Char('a') => {
                            app.input = Some((InputKind::AddMachine, "addr: ".into(), String::new()));
                        }
                        KeyCode::Char('p') => {
                            if let Some(m) = app.selected_machine() {
                                app.log(format!("[{}] provisioning in background…", m.addr));
                                let _ = job_tx.send(ProvisionJob::Provision {
                                    layout_root: layout.root.clone(),
                                    machine: m,
                                    force: false,
                                });
                            } else {
                                app.status = "no machine selected".into();
                            }
                        }
                        KeyCode::Char('t') => {
                            app.input = Some((InputKind::Translate, "start count: ".into(), String::new()));
                        }
                        KeyCode::Char('c') => {
                            let _ = job_tx.send(op_now(&app.api, &http, OpRequest { op: Op::CrawlSetup, ..Default::default() }));
                        }
                        KeyCode::Char('v') => {
                            let _ = job_tx.send(op_now(&app.api, &http, OpRequest { op: Op::Voices, ..Default::default() }));
                        }
                        KeyCode::Char('s') => {
                            app.input = Some((InputKind::SwapVoice, "character voice: ".into(), String::new()));
                        }
                        KeyCode::Char('e') => {
                            let _ = job_tx.send(op_now(&app.api, &http, OpRequest { op: Op::Eta, ..Default::default() }));
                        }
                        _ => {}
                    },
                }
            }
        }
        app.tick += 1;
        if app.tick.is_multiple_of(4) {
            app.refresh(&http).await;
        }
    }
    Ok(())
}
