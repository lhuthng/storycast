//! `:` commands: names for keys, direct runs for gated operator actions.
use crate::tui::input::runconfig::run_preview;
use crate::tui::{
    app::App,
    input::{audition, dispatch, dispatch_op},
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
    Mix,
    Sound,
    Rerender,
    Remerge,
    ShutdownWhenIdle,
    AuditionCurrent,
    AuditionTry,
    AuditionAnother,
}

/// One `:`-addressable command: its single-char form, its words, and its
/// one-line explanation.
///
/// This table is the whole word layer — parsing (`command_key`) and `:help`
/// both read it, so a word and its explanation cannot drift apart. The
/// canonical word comes first; the rest are aliases. `desc` is `None` for
/// the read-only navigations, which `:help` covers in their own sections.
///
/// Deliberate non-aliases, because the words are taken: `:swap` stays the
/// voice picker and `:sample` stays pool-a-clip, so the auditions are
/// `:current` / `:try` / `:another` instead.
pub(crate) struct Word {
    /// `:x` single-char form, if the command has one.
    pub key: Option<char>,
    /// Canonical word first, then aliases. Matched case-insensitively.
    pub names: &'static [&'static str],
    /// One line for `:help`, or `None` when another section explains it.
    pub desc: Option<&'static str>,
    pub cmd: Command,
}

pub(crate) static WORDS: &[Word] = &[
    Word { key: Some('a'), names: &["add"], desc: Some("add a machine by IP or hostname"), cmd: Command::AddMachine },
    Word { key: Some('A'), names: &["sample"], desc: Some("pool a clip — tags from the filename, enrolled locally"), cmd: Command::AddSample },
    Word { key: Some('N'), names: &["named"], desc: Some("a `path as Name` voice — manual assignment only"), cmd: Command::AddNamed },
    Word { key: Some('p'), names: &["provision", "prov"], desc: Some("provision the selected machine"), cmd: Command::Provision { force: false } },
    Word { key: Some('P'), names: &["reprovision", "reprov"], desc: Some("re-provision it, forcing past the skip-if-configured check"), cmd: Command::Provision { force: true } },
    Word { key: Some('d'), names: &["drop", "remove"], desc: Some("drop the selected machine from the cluster registry"), cmd: Command::DropMachine },
    Word { key: Some('t'), names: &["translate"], desc: Some("enqueue crawl + digest for a chapter range"), cmd: Command::Translate },
    Word { key: Some('c'), names: &["crawl"], desc: Some("save the URL template, then probe-crawl one chapter"), cmd: Command::CrawlSetup },
    Word { key: Some('v'), names: &["voices"], desc: Some("re-read the roster, enforce the accent policy, refill gaps"), cmd: Command::Voices },
    Word { key: Some('s'), names: &["swap"], desc: Some("repoint one character — destructive, see below"), cmd: Command::SwapVoice },
    Word { key: Some('S'), names: &["cast"], desc: Some("cast overview: every speaker × voice, read-only"), cmd: Command::Cast },
    Word { key: Some('e'), names: &["eta"], desc: Some("estimate the remaining wall-clock time"), cmd: Command::Eta },
    Word { key: Some('u'), names: &["retry"], desc: Some("requeue every shelved task — strikes reset"), cmd: Command::Retry },
    Word { key: Some('m'), names: &["reconcile"], desc: Some("fold duplicates — asks first; certain folds apply, ambiguous only listed"), cmd: Command::Reconcile },
    Word { key: Some('B'), names: &["backend"], desc: Some("backend up now, machines provision in background and join as ready"), cmd: Command::Backend },
    Word { key: None, names: &["mix"], desc: Some("story speed and fx/music/inject volumes — requeues every merge"), cmd: Command::Mix },
    Word { key: None, names: &["sound", "sounds", "pools"], desc: Some("the three clip pools: add, edit, retune, remove"), cmd: Command::Sound },
    Word { key: None, names: &["remerge"], desc: Some("requeue every merge — render cache kept, no confirm"), cmd: Command::Remerge },
    Word { key: None, names: &["rerender"], desc: Some("requeue every render + merge — full re-speak, asks first"), cmd: Command::Rerender },
    Word { key: None, names: &["shutdown-when-idle", "drain"], desc: Some("workers exit on their own once the queue drains — restart with :B"), cmd: Command::ShutdownWhenIdle },
    Word { key: Some('X'), names: &["stop"], desc: Some("stop everything everywhere: local backend plus workers on all machines"), cmd: Command::Stop },
    Word { key: None, names: &["sshkey"], desc: None, cmd: Command::SshKey },
    Word { key: None, names: &["sshuser"], desc: None, cmd: Command::SshUser },
    Word { key: None, names: &["sshport"], desc: None, cmd: Command::SshPort },
    Word { key: Some('q'), names: &["quit", "exit", "q"], desc: None, cmd: Command::Key(KeyCode::Char('q')) },
    Word { key: None, names: &["inspect"], desc: None, cmd: Command::Key(KeyCode::Char('i')) },
    Word { key: None, names: &["tasks"], desc: None, cmd: Command::Key(KeyCode::Char('K')) },
    Word { key: None, names: &["jobs"], desc: None, cmd: Command::Key(KeyCode::Char('J')) },
    Word { key: None, names: &["refresh"], desc: None, cmd: Command::Key(KeyCode::Char('r')) },
    Word { key: None, names: &["colour", "color"], desc: None, cmd: Command::Key(KeyCode::Char('C')) },
    Word { key: None, names: &["run"], desc: None, cmd: Command::Key(KeyCode::Char('R')) },
    Word { key: None, names: &["newest"], desc: None, cmd: Command::Key(KeyCode::Char('G')) },
    Word { key: None, names: &["help"], desc: None, cmd: Command::Key(KeyCode::Char('?')) },
    Word { key: None, names: &["current", "cur"], desc: Some("play the held line with the current voice, from cache only"), cmd: Command::AuditionCurrent },
    Word { key: None, names: &["try", "test"], desc: Some("render the held line with the pointed voice"), cmd: Command::AuditionTry },
    Word { key: None, names: &["another", "change", "next"], desc: Some("render another line with the pointed voice"), cmd: Command::AuditionAnother },
];

/// `:` command line → the command. A single character is a command key
/// (`:m` is reconcile); longer words are the readable form, aliases
/// included (`:prov` is provision, `:drain` is shutdown-when-idle).
/// Unknown input stays an error in the prompt.
pub(crate) fn command_key(input: &str) -> Option<Command> {
    let word = input.trim();
    if word.chars().count() == 1 {
        let c = word.chars().next().filter(|c| *c != ':')?;
        if let Some(w) = WORDS.iter().find(|w| w.key == Some(c)) {
            return Some(w.cmd);
        }
        // Read-only keys keep their Normal-mode arms, so the command
        // presses the key and every context behaves like it was typed.
        return Some(Command::Key(KeyCode::Char(c)));
    }
    let lower = word.to_ascii_lowercase();
    WORDS
        .iter()
        .find(|w| w.names.iter().any(|n| *n == lower))
        .map(|w| w.cmd)
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
            None => app.set_status(Level::Warn, "no machine selected — :add adds one first"),
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
        Command::Mix => {
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::Mix,
                "Mix — story speed and layer volumes",
                "as <speed 0.5-2.0> <fx 0-2> <music 0-2> [inject 0-2], e.g. 1.25 1.0 1.0 1.0 (0 mutes). \
                 These are the whole layers; per-sound trims and the pools themselves are :sound",
                &crate::tui::input::runconfig::mix_prefill(app),
            ));
        }
        Command::Sound => {
            // The editor is a screen, not a prompt: it reads three registries
            // and the scripts to know what is safe to remove, which is a
            // background load rather than something to do between keystrokes.
            app.screen = Screen::Sound(crate::tui::sound::SoundView::new());
            app.load_sound(job_tx);
        }
        Command::Rerender => {
            // Full re-speak: worth one Enter, like every other destructive
            // action. Mix-only changes belong on `:mix`, which keeps the
            // render cache.
            app.screen = Screen::Confirm(Confirm::rerender());
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
        Command::Remerge => {
            // Same blast radius as `:mix` (finished mp3s rebuild from cache),
            // so no confirm — unlike `:rerender`, nothing is deleted for good.
            dispatch_op(
                app,
                job_tx,
                http,
                OpRequest {
                    op: Op::Remerge,
                    ..Default::default()
                },
            );
            app.set_status(Level::Info, "requeueing every merge — render cache kept");
        }
        Command::AuditionCurrent => {
            audition::audition_word(app, job_tx, http, audition::AuditionKind::Current);
        }
        Command::AuditionTry => {
            audition::audition_word(app, job_tx, http, audition::AuditionKind::Pointed { reroll: false });
        }
        Command::AuditionAnother => {
            audition::audition_word(app, job_tx, http, audition::AuditionKind::Pointed { reroll: true });
        }
        Command::ShutdownWhenIdle => {
            // Graceful and reversible (nothing deleted, `:B` brings workers
            // back), so no confirm — but command-line only, never a key.
            dispatch_op(
                app,
                job_tx,
                http,
                OpRequest {
                    op: Op::ShutdownWhenIdle,
                    ..Default::default()
                },
            );
            app.set_status(Level::Info, "shutdown armed — workers exit once the queue drains");
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
