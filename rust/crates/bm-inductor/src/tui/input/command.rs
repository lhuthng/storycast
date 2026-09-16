//! `:` commands: names for keys, direct runs for gated operator actions.
use crate::tui::input::runconfig::run_preview;
use crate::tui::{
    app::App,
    input::{dispatch, dispatch_op},
    jobs::Job,
    screen::{CastView, Confirm, ConfirmAction, Picker, Screen, TextKind, TextPrompt},
    style::{Conn, Level},
};
use bm_proto::{Op, OpRequest};
use crossterm::event::KeyCode;
use std::sync::{atomic::AtomicBool, Arc};

/// What a `:` command line request actually runs. Read-only commands map to
/// `Key` — their single keys still exist in Normal mode, so `:m`-style
/// recursion presses them as if typed. Operator actions map to the variants
/// below and run directly, because their single keys were removed: a stray
/// keypress must never provision, reconcile or stop anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Command {
    Key(KeyCode),
    AddMachine,
    AddSample,
    AddNamed,
    Provision { force: bool },
    DropMachine,
    Translate,
    CrawlSetup,
    Voices,
    SwapVoice,
    Cast,
    Eta,
    Retry,
    Reconcile,
    Backend,
    Stop,
    SshKey,
    SshUser,
    SshPort,
}

/// `:` command line → the command. A single character is a command key
/// (`:m` is reconcile); longer words are the readable form (`:reconcile`
/// is too). Unknown input stays an error in the prompt.
pub(crate) fn command_key(input: &str) -> Option<Command> {
    let word = input.trim();
    if word.chars().count() == 1 {
        let c = word.chars().next().filter(|c| *c != ':')?;
        return Some(match c {
            'a' => Command::AddMachine,
            'A' => Command::AddSample,
            'N' => Command::AddNamed,
            'p' => Command::Provision { force: false },
            'P' => Command::Provision { force: true },
            'd' => Command::DropMachine,
            't' => Command::Translate,
            'c' => Command::CrawlSetup,
            'v' => Command::Voices,
            's' => Command::SwapVoice,
            'S' => Command::Cast,
            'e' => Command::Eta,
            'u' => Command::Retry,
            'm' => Command::Reconcile,
            'B' => Command::Backend,
            'X' => Command::Stop,
            // Read-only keys keep their Normal-mode arms, so the command
            // presses the key and every context behaves like it was typed.
            _ => Command::Key(KeyCode::Char(c)),
        });
    }
    Some(match word.to_ascii_lowercase().as_str() {
        "quit" => Command::Key(KeyCode::Char('q')),
        "add" => Command::AddMachine,
        "drop" => Command::DropMachine,
        "inspect" => Command::Key(KeyCode::Char('i')),
        "provision" => Command::Provision { force: false },
        "reprovision" => Command::Provision { force: true },
        "translate" => Command::Translate,
        "crawl" => Command::CrawlSetup,
        "retry" => Command::Retry,
        "tasks" => Command::Key(KeyCode::Char('K')),
        "voices" => Command::Voices,
        "swap" => Command::SwapVoice,
        "cast" => Command::Cast,
        "eta" => Command::Eta,
        "reconcile" => Command::Reconcile,
        "refresh" => Command::Key(KeyCode::Char('r')),
        "colour" | "color" => Command::Key(KeyCode::Char('C')),
        "backend" => Command::Backend,
        "run" => Command::Key(KeyCode::Char('R')),
        "stop" => Command::Stop,
        "sshkey" => Command::SshKey,
        "sshuser" => Command::SshUser,
        "sshport" => Command::SshPort,
        "newest" => Command::Key(KeyCode::Char('G')),
        "named" => Command::AddNamed,
        "sample" => Command::AddSample,
        "help" => Command::Key(KeyCode::Char('?')),
        _ => return None,
    })
}

/// Run a `:` operator command. `Command::Key` never arrives here — the caller
/// presses those as a live key so a context (task list, picker) reacts the
/// same as a real keypress. Only the gated actions land in this match, which
/// is exactly the set of things a stray keypress must never do.
pub(crate) fn do_command(
    app: &mut App,
    cmd: Command,
    http: &reqwest::Client,
    job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
) {
    match cmd {
        Command::Key(_) => unreachable!("Command::Key is pressed by the caller"),
        Command::AddMachine => {
            // Bind prompt: `addr [user [port [key]]]`, prefilled from the
            // app-wide ssh defaults. The cursor starts at the front so the
            // address is typed first and the defaults shift right untouched.
            let def = app.ssh_defaults();
            let mut initial = format!("{} {}", def.user, def.port);
            if let Some(k) = &def.key {
                initial.push(' ');
                initial.push_str(k);
            }
            let mut prompt = TextPrompt::new(
                TextKind::AddMachine,
                "Add machine",
                "addr [user [port [key]]] — empty key means ssh decides (agent / ~/.ssh/config)",
                &initial,
            );
            prompt.cursor = 0;
            app.screen = Screen::Text(prompt);
        }
        Command::AddSample => {
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::AddSample,
                "Add pooled sample",
                "clip path, e.g. ~/dl/young-female-4.mp3 — tags come from the filename, voice auto-rolls",
                "",
            ));
        }
        Command::AddNamed => {
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::AddNamed,
                "Add named voice",
                "clip path as Name, e.g. refs/narrator.mp3 as Narrator — manual assignment only, never rotates",
                "",
            ));
        }
        Command::Provision { force } => match app.selected_machine() {
            None => app.set_status(Level::Warn, "no machine selected — :a adds one first"),
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
                            "Already-configured machines are detected and skipped, so this is"
                                .into()
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
        },
        Command::DropMachine => match app.selected_machine() {
            None => app.set_status(Level::Warn, "no machine selected"),
            Some(m) => {
                app.screen = Screen::Confirm(Confirm {
                    title: "Drop machine from the registry".into(),
                    danger: true,
                    body: vec![
                        format!("Remove {} from the cluster registry.", m.addr),
                        String::new(),
                        "This forgets the machine. It does not touch anything on the".into(),
                        "remote box, and re-adding it by address is enough to bring it back."
                            .into(),
                    ],
                    action: ConfirmAction::DropMachine {
                        addr: m.addr.clone(),
                    },
                });
            }
        },
        Command::Translate => {
            let start = app.setting_u32("start", 1);
            let count = app.setting_u32("count", 1);
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::Translate,
                "Translate — enqueue crawl + digest",
                "chapter range as <start> <count>. Prefilled from the inductor's settings.",
                &format!("{start} {count}"),
            ));
        }
        Command::CrawlSetup => {
            let current = app.setting_str("url_template", "");
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::CrawlTemplate,
                "Crawl setup — save the URL template",
                "must contain {n}; one chapter is probe-crawled to check the selector",
                &current,
            ));
        }
        Command::SshKey => {
            let cur = app.ssh_defaults().key.unwrap_or_default();
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::SshKey,
                "Default ssh key",
                "key path for machines bound without one — empty clears it (ssh decides). Must exist.",
                &cur,
            ));
        }
        Command::SshUser => {
            let cur = app.ssh_defaults().user;
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::SshUser,
                "Default ssh user",
                "login for machines bound without one",
                &cur,
            ));
        }
        Command::SshPort => {
            let cur = app.ssh_defaults().port.to_string();
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::SshPort,
                "Default ssh port",
                "port for machines bound without one",
                &cur,
            ));
        }
        Command::Voices => {
            dispatch_op(
                app,
                job_tx,
                http,
                OpRequest {
                    op: Op::Voices,
                    ..Default::default()
                },
            );
        }
        Command::SwapVoice => {
            app.screen = Screen::Pick(Picker::new());
            if app.roster.is_none() {
                app.load_roster(job_tx, http);
            }
            // The audition keys need the line index, and building it takes
            // seconds — start it while the operator is still on step 1.
            app.ensure_lines(job_tx);
        }
        Command::Cast => {
            app.screen = Screen::Cast(CastView::new());
            if app.roster.is_none() {
                app.load_roster(job_tx, http);
            }
            // Start the audition line index now: it is seconds of file reads, and
            // the operator is about to want it.
            app.ensure_lines(job_tx);
        }
        Command::Eta => {
            dispatch_op(
                app,
                job_tx,
                http,
                OpRequest {
                    op: Op::Eta,
                    ..Default::default()
                },
            );
        }
        Command::Retry => {
            dispatch_op(
                app,
                job_tx,
                http,
                OpRequest {
                    op: Op::Retry,
                    ..Default::default()
                },
            );
        }
        Command::Reconcile => {
            // Reconcile rewrites cast + scripts and re-renders losers: worth
            // one Enter, like every other destructive action.
            app.screen = Screen::Confirm(Confirm {
                title: "Fold duplicate characters?".into(),
                danger: false,
                body: vec![
                    "Certain folds (titles, casing, parentheticals) apply at once;".into(),
                    "ambiguous pairs are only listed, never auto-merged.".into(),
                    String::new(),
                    "Cast rewritten, losers re-rendered. Workers keep working.".into(),
                ],
                action: ConfirmAction::Reconcile,
            });
        }
        Command::Backend => {
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
                        layout_root: app.layout_root.clone(),
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
        Command::Stop => {
            let mut remotes: Vec<String> = app
                .effective_machines()
                .iter()
                .map(|m| m.addr.clone())
                .filter(|a| !["127.0.0.1", "localhost", "::1"].contains(&a.as_str()))
                .collect();
            remotes.sort();
            remotes.dedup();
            let mut body = vec![
                "Stop the local backend AND every worker on every machine.".into(),
                "In-flight tasks return to the queue; the ledger keeps".into(),
                "everything, so nothing is lost.".into(),
                String::new(),
            ];
            if remotes.is_empty() {
                body.push("No remote machines registered — local only.".into());
            } else {
                body.push(format!(
                    "Remote boxes swept over ssh: {}.",
                    remotes.join(", ")
                ));
                body.push("Unreachable boxes report and are skipped.".into());
            }
            app.screen = Screen::Confirm(Confirm {
                title: "Stop everything, everywhere?".into(),
                danger: true,
                body,
                action: ConfirmAction::StopBackend,
            });
        }
    }
}
