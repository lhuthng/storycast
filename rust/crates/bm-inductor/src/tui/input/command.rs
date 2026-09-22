//! `:` commands: names for keys, direct runs for gated operator actions.
use crate::tui::input::runconfig::run_preview;
use crate::tui::{
    app::App,
    input::{audition, dispatch, dispatch_op},
    jobs::Job,
    model::{busy_on, instance_addresses, is_live_state},
    screen::{CastView, CloudView, Confirm, ConfirmAction, Picker, Screen, TextKind, TextPrompt},
    style::{Conn, Level},
};
use bm_proto::{Op, OpRequest, Stage};
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
    Relink,
    Provision {
        force: bool,
    },
    DropMachine,
    Translate,
    CrawlSetup,
    Voices,
    SwapVoice,
    Cast,
    /// Stop offering digest work on every machine, remembering what each box had.
    DigestOff,
    /// Put each machine's snapshotted digest policy back — **not** "digest on":
    /// a box that had it off stays off.
    DigestOn,
    Eta,
    /// Requeue shelved work: everything, one chapter, or one task. `stage`
    /// and `chapter` together name one task; `chapter` alone narrows to that
    /// chapter; neither is the whole ledger.
    Retry {
        stage: Option<Stage>,
        chapter: Option<u32>,
    },
    Reconcile,
    Backend,
    Stop,
    SshKey,
    SshUser,
    SshPort,
    Advertise,
    /// How many of one chapter's takes a single render offer carries. Saved to
    /// this workspace's settings; applies to offers made from then on.
    RenderBatch,
    Mix,
    Sound,
    Rerender,
    Remerge,
    ShutdownWhenIdle,
    Workspace,
    Profile,
    AuditionCurrent,
    AuditionTry,
    AuditionAnother,
    /// Show what the EC2 account holds. Read-only: launches nothing.
    AwsPool,
    /// Store the app's IAM user from the console's CSV. Setup, once.
    AwsLogin,
    /// Read the account into the pool definition. Setup, once — and safe to
    /// re-run, since every field already set is kept.
    AwsDiscover,
    /// Launch `count` EC2 boxes and link what comes back into the registry.
    AwsUp {
        count: u32,
    },
    /// Terminate the live boxes the Cloud view is showing. Destructive, so it
    /// asks first; `force` skips the in-flight guard, not the confirmation.
    AwsDown {
        force: bool,
    },
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
    Word { key: None, names: &["relink"], desc: Some("re-point a drifted EC2 box at its current public IP — matched by instance id, then :prov"), cmd: Command::Relink },
    Word { key: Some('t'), names: &["translate"], desc: Some("enqueue crawl + digest for a chapter range"), cmd: Command::Translate },
    Word { key: Some('c'), names: &["crawl"], desc: Some("save the URL template, then probe-crawl one chapter"), cmd: Command::CrawlSetup },
    Word { key: Some('v'), names: &["voices"], desc: Some("re-read the roster, enforce the accent policy, refill gaps"), cmd: Command::Voices },
    Word { key: Some('s'), names: &["swap"], desc: Some("repoint one character — destructive, see below"), cmd: Command::SwapVoice },
    Word { key: Some('S'), names: &["cast"], desc: Some("cast overview: every speaker × voice, read-only"), cmd: Command::Cast },
    Word { key: Some('e'), names: &["eta"], desc: Some("estimate the remaining wall-clock time"), cmd: Command::Eta },
    Word { key: Some('u'), names: &["retry"], desc: Some("requeue every shelved task — strikes reset; `:retry 24` narrows to one chapter, `:retry render 24` to one task"), cmd: Command::Retry { stage: None, chapter: None } },
    Word { key: Some('m'), names: &["reconcile"], desc: Some("fold duplicates — asks first; certain folds apply, ambiguous only listed"), cmd: Command::Reconcile },
    Word { key: Some('B'), names: &["backend"], desc: Some("backend up now, machines provision in background and join as ready"), cmd: Command::Backend },
    Word { key: None, names: &["mix"], desc: Some("story speed and fx/music/inject volumes — requeues every merge"), cmd: Command::Mix },
    Word { key: None, names: &["sound", "sounds", "pools"], desc: Some("the three clip pools: add, edit, retune, remove"), cmd: Command::Sound },
    Word { key: None, names: &["remerge"], desc: Some("requeue every merge — render cache kept, no confirm"), cmd: Command::Remerge },
    Word { key: None, names: &["rerender"], desc: Some("requeue every render + merge — full re-speak, asks first"), cmd: Command::Rerender },
    Word { key: None, names: &["shutdown-when-idle", "drain"], desc: Some("workers exit on their own once the queue drains — restart with :B"), cmd: Command::ShutdownWhenIdle },
    Word { key: None, names: &["workspace", "ws"], desc: Some("list, switch or create a workspace — one per book; only with the cluster stopped"), cmd: Command::Workspace },
    Word { key: None, names: &["profile"], desc: Some("list, load or pack a genre profile — loading replaces assets/ + prompts/, so only with the cluster stopped"), cmd: Command::Profile },
    Word { key: None, names: &["login"], desc: Some("store the IAM user's key from the console's accessKeys.csv — setup, once"), cmd: Command::AwsLogin },
    Word { key: None, names: &["discover"], desc: Some("read the account into `.bm/aws.json`: AMI, subnet, group, keypair, instance profile"), cmd: Command::AwsDiscover },
    Word { key: Some('l'), names: &["pool", "aws", "cloud"], desc: Some("what the EC2 account holds — launches nothing"), cmd: Command::AwsPool },
    Word { key: Some('w'), names: &["up", "launch"], desc: Some("launch EC2 boxes and link them into the cluster — spends money"), cmd: Command::AwsUp { count: 1 } },
    Word { key: Some('o'), names: &["down", "terminate"], desc: Some("terminate the live EC2 boxes — destructive; asks first, refuses while a render is in flight"), cmd: Command::AwsDown { force: false } },
    Word { key: Some('X'), names: &["stop"], desc: Some("stop everything everywhere: local backend plus workers on all machines"), cmd: Command::Stop },
    Word { key: None, names: &["sshkey"], desc: None, cmd: Command::SshKey },
    Word { key: None, names: &["sshuser"], desc: None, cmd: Command::SshUser },
    Word { key: None, names: &["sshport"], desc: None, cmd: Command::SshPort },
    Word { key: None, names: &["advertise", "adv"], desc: Some("the address workers dial back on — set it when they are off the LAN"), cmd: Command::Advertise },
    Word { key: None, names: &["batch", "renderbatch"], desc: Some("how many of one chapter's takes one render offer carries (default 10)"), cmd: Command::RenderBatch },
    Word { key: Some('q'), names: &["quit", "exit", "q"], desc: None, cmd: Command::Key(KeyCode::Char('q')) },
    Word { key: None, names: &["inspect"], desc: None, cmd: Command::Key(KeyCode::Char('i')) },
    Word { key: None, names: &["policy"], desc: Some("per-machine work policy: which stages the selected box may run, in priority order"), cmd: Command::Key(KeyCode::Char('P')) },
    Word { key: None, names: &["digest"], desc: Some("digest manager: every chapter, and a manual two-round digest by clipboard for one of them"), cmd: Command::Key(KeyCode::Char('D')) },
    Word { key: None, names: &["off"], desc: Some("DIGEST POLICY: stop offering digest work on every machine — each box's own policy is saved first, so `:on` puts back what it had"), cmd: Command::DigestOff },
    Word { key: None, names: &["on"], desc: Some("digest policy: restore every machine to the policy it had before `:off` — a box whose digest was already off stays off"), cmd: Command::DigestOn },
    Word { key: None, names: &["tasks"], desc: None, cmd: Command::Key(KeyCode::Char('K')) },
    Word { key: None, names: &["jobs"], desc: None, cmd: Command::Key(KeyCode::Char('J')) },
    Word { key: None, names: &["refresh"], desc: None, cmd: Command::Key(KeyCode::Char('r')) },
    Word { key: None, names: &["colour", "color"], desc: None, cmd: Command::Key(KeyCode::Char('C')) },
    Word { key: None, names: &["theme"], desc: Some("cycle the palette: default → dim → mono — the word always carries the state"), cmd: Command::Key(KeyCode::Char('C')) },
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
    // Commands that take an argument: `:up 3`, `down force`, and the retry
    // scopes (`:retry`, `:retry 24`, `:retry render 24`). A bad argument is
    // `None`, which keeps the prompt open with "unknown command" rather than
    // running the wrong thing at the wrong scope.
    let mut parts = word.split_whitespace();
    if let Some(head) = parts.next() {
        let rest: Vec<&str> = parts.collect();
        match head.to_ascii_lowercase().as_str() {
            "up" => {
                let count = match rest.first() {
                    None => 1,
                    Some(s) => s.parse::<u32>().ok()?,
                };
                return (count > 0).then_some(Command::AwsUp { count });
            }
            "down" if !rest.is_empty() => {
                return rest[0]
                    .eq_ignore_ascii_case("force")
                    .then_some(Command::AwsDown { force: true });
            }
            "retry" | "u" if !rest.is_empty() => return retry_scope(&rest),
            _ => {}
        }
    }
    let lower = word.to_ascii_lowercase();
    WORDS
        .iter()
        .find(|w| w.names.iter().any(|n| *n == lower))
        .map(|w| w.cmd)
}

/// `:retry <chapter>` / `:retry <stage> <chapter>` → the command that names
/// them. `rest` is already non-empty.
///
/// A stage on its own is refused rather than widened to every chapter of that
/// stage: `:remerge` and `:rerender` already mean exactly that, and a typo
/// should not reach them. A chapter of 0, an unknown stage name and a third
/// argument are all `None`, which leaves the prompt open.
fn retry_scope(rest: &[&str]) -> Option<Command> {
    let (stage, chapter) = match rest {
        [chapter] => (None, *chapter),
        [stage, chapter] => (Some(stage_by_name(stage)?), *chapter),
        _ => return None,
    };
    Some(Command::Retry {
        stage,
        chapter: Some(chapter.parse::<u32>().ok().filter(|n| *n > 0)?),
    })
}

/// A stage name as the task ledger spells it (`crawl`, `digest`, `render`,
/// `merge`). Single letters are deliberately not accepted: they are live keys
/// on other screens, so `:u r 24` reads as a typo rather than as a scope.
fn stage_by_name(s: &str) -> Option<Stage> {
    Stage::ALL
        .into_iter()
        .find(|st| st.as_str().eq_ignore_ascii_case(s))
}

/// A bounded, one-entry list of in-flight tasks for a dialog line.
///
/// **Why it is bounded at all.** `Confirm`'s height is `body.len() + 5`, one
/// entry per body line — but the paragraph *wraps*, so a single long entry costs
/// visual lines the height calculation never counted and pushes the
/// `Enter / y confirm` hint out of the box. That was fine while a box held one
/// task; with `render_batch` a single box can hold sixty-four, and a forced
/// `:down` would then render a dialog with no visible keys.
///
/// **Bounded by width, not by count.** Ids vary in length (`merge:7` against
/// `render:42:11`), so "show six" is not a width — a count bound that looks
/// right for one chapter overflows for another. The budget here is the dialog's
/// own usable width, which is the actual constraint, and the trailer is
/// recomputed as names are added because its width depends on how many are left.
///
/// The count is always exact — only the *names* are elided, and the number left
/// out is stated, so nothing is hidden. The ledger's event lines cap themselves
/// the same way (`reap` shows the first eight).
pub(crate) fn busy_summary(busy: &[String]) -> String {
    /// The dialog is 76 wide with two borders; leave a little slack.
    const WIDTH: usize = 68;
    let full = busy.join(", ");
    if full.len() <= WIDTH {
        return full;
    }
    let mut shown = 0usize;
    let mut out = String::new();
    for (i, id) in busy.iter().enumerate() {
        let left = busy.len() - i - 1;
        let candidate = if shown == 0 {
            id.clone()
        } else {
            format!("{out}, {id}")
        };
        let trailer = if left == 0 {
            0
        } else {
            format!(", … +{left} more").len()
        };
        if candidate.len() + trailer > WIDTH {
            break;
        }
        out = candidate;
        shown += 1;
    }
    let left = busy.len() - shown;
    if left == 0 {
        out
    } else if shown == 0 {
        // Not even one name fits; the count is the whole answer.
        format!("… +{left} more")
    } else {
        format!("{out}, … +{left} more")
    }
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
        // Digest off / on, cluster-wide. **Off takes the snapshot; on is the only
        // thing that can put it back**, so the two are one feature and neither
        // touches a box's policy without the other being able to undo it.
        Command::DigestOff | Command::DigestOn => {
            let restore = matches!(cmd, Command::DigestOn);
            if app.machines.is_empty() {
                app.set_status(Level::Warn, "no machines known — nothing to switch");
                return;
            }
            if restore {
                // A restore with no snapshot would post an empty policy to every
                // box, which reads as "the default list" — i.e. digest *on*
                // everywhere. That is the opposite of what was asked, so refuse
                // and name the editor instead.
                let path = app.layout.bm_state().join("digest-suspend.json");
                if !path.is_file() {
                    app.set_status(
                        Level::Warn,
                        "no snapshot to restore — `:off` was not run from here; use `P` per box",
                    );
                    return;
                }
            }
            let machines: Vec<(String, Option<Vec<bm_proto::TaskPref>>)> = app
                .machines
                .iter()
                .map(|m| (m.addr.clone(), m.task_policy.clone()))
                .collect();
            let count = machines.len();
            dispatch(
                app,
                job_tx,
                Job::DigestPolicy {
                    api: app.api.clone(),
                    http: http.clone(),
                    layout: app.layout.clone(),
                    machines,
                    restore,
                },
            );
            app.set_status(
                Level::Info,
                if restore {
                    format!("restoring each box's digest policy… ({count} machine(s))")
                } else {
                    format!("turning digest off everywhere… ({count} machine(s))")
                },
            );
        }
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
                            "Force ignores the skip-if-configured check and re-sends the".into()
                        } else {
                            "Already-configured machines are detected and skipped, so this is"
                                .into()
                        },
                        if force {
                            "sidecar and its weights. That is the slow path.".into()
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
        Command::Relink => match app.selected_machine() {
            None => app.set_status(Level::Warn, "no machine selected"),
            Some(m) => {
                app.screen = Screen::Confirm(Confirm {
                    title: "Relink to the box's current address".into(),
                    danger: false,
                    body: vec![
                        format!("{} no longer answers — EC2 public IPs change on every", m.addr),
                        "stop/start and spot relaunch.".into(),
                        String::new(),
                        "The account is read, the box is matched by its EC2 instance id,".into(),
                        "and the registry entry is re-pointed at the address it carries".into(),
                        "now. Nothing is pushed; :prov afterwards onboards it.".into(),
                    ],
                    action: ConfirmAction::RelinkMachine {
                        old_addr: m.addr.clone(),
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
        Command::Advertise => {
            // Prefilled with what is in force — including the `127.0.0.1`
            // sentinel, which reads as "unset" rather than as an address.
            let cur = app.setting_str("advertise", "127.0.0.1");
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::Advertise,
                "Advertised address for workers",
                "host or host:port as the *workers* see this machine — needed when they are \
                 off the LAN (a cloud box cannot reach a NAT'd 192.168.x.x). Empty restores the guess.",
                &cur,
            ));
        }
        Command::RenderBatch => {
            // Prefilled from `run_preview` — the *same* precedence the run
            // screen displays: the live settings while the backend answers, the
            // workspace's own file while it does not, the compiled default when
            // neither exists. `App::setting_u32` reads the live settings only,
            // so on a cold start it would show the compiled 10 over a saved 6 —
            // and a compiled-in default has to read differently from a number
            // somebody chose. Sharing the helper is also what stops the prompt
            // and the screen disagreeing about what is in force.
            let cur = run_preview(app).render_batch;
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::RenderBatch,
                "Render batch — takes per offer",
                "how many of ONE chapter's takes a single render offer carries, 1-64. \
                 Each take still settles on its own ledger row; this only decides how many \
                 travel together, so a worker pays one round trip per batch instead of one \
                 per segment. Saved to this workspace's settings.",
                &cur.to_string(),
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
        Command::Workspace => {
            // Prefilled with what is in force, like every other prompt: the
            // operator sees the active name before editing it, and an empty
            // line lists instead of switching.
            let cur = crate::tui::model::workspace_label(&app.layout);
            let initial = if cur == "default" { "" } else { cur.as_str() };
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::Workspace,
                "Workspace — one directory per book",
                "<name> to switch · `new <name>` to create and switch · empty to list. \
                 Only with the cluster stopped (:X): the ledger, settings and data all move.",
                initial,
            ));
        }
        Command::Profile => {
            let cur = crate::tui::model::profile_label(app.profile.as_ref());
            let initial = if cur == "none" { "" } else { cur.as_str() };
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::Profile,
                "Profile — the genre bundle behind assets/ + prompts/",
                "<name> to load · `pack <name>` to bundle the live tree · empty to list. \
                 Loading replaces the live tree every worker reads, so only with the cluster stopped (:X).",
                initial,
            ));
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
        Command::Retry { stage, chapter } => {
            dispatch_op(
                app,
                job_tx,
                http,
                OpRequest {
                    op: Op::Retry,
                    stage,
                    chapter,
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
            audition::audition_word(
                app,
                job_tx,
                http,
                audition::AuditionKind::Pointed { reroll: false },
            );
        }
        Command::AuditionAnother => {
            audition::audition_word(
                app,
                job_tx,
                http,
                audition::AuditionKind::Pointed { reroll: true },
            );
        }
        Command::AwsLogin => {
            // The console's download is the whole prompt. Prefilled with where
            // it actually lands, because the secret must never be typed on a
            // screen — this is the only login route a dashboard can offer.
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::AwsLogin,
                "AWS login — the IAM user this app runs as",
                "the console's accessKeys.csv: IAM → Users → storycast-operator → Security \
                 credentials → Access keys → Create access key → Download .csv file. \
                 It carries both halves, so no secret is typed here.",
                "~/Downloads/accessKeys.csv",
            ));
        }
        Command::AwsDiscover => {
            // Prefilled with the region already in force, so the common re-run
            // is one keypress. The name-and-path flags are left out on purpose:
            // an omitted field is kept from the pool, and prefilling them would
            // suggest they had to be retyped.
            let cfg = bm_core::provision::AwsConfig::load_layered(&app.layout.root);
            let initial = if cfg.region.trim().is_empty() {
                String::new()
            } else {
                format!("--region {}", cfg.region.trim())
            };
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::AwsDiscover,
                "AWS discover — read the account into .bm/aws.json",
                "--region <r> [--pem <path>] [--instance-profile <name>] \
                 [--security-group sg-…] [--subnet subnet-…] [--ami ami-…] [--force]. \
                 Everything already set is kept, so re-running is safe; --force only \
                 replaces a `.pem` that is a different key.",
                &initial,
            ));
        }
        Command::AwsPool => {
            // Open the view first, then fill it: the screen renders its own
            // "reading…" state, so the operator sees the command was taken.
            app.screen = Screen::Cloud(CloudView::new());
            dispatch(
                app,
                job_tx,
                Job::AwsPool {
                    root: app.layout.root.clone(),
                    api: app.api.clone(),
                    http: http.clone(),
                },
            );
            app.set_status(Level::Info, "reading the EC2 account…");
        }
        Command::AwsUp { count } => {
            // No confirmation: the CLI's review step is `aws up --dry-run`, and
            // the launch prints its own argv into the event pane before it runs.
            // The cap in `.bm/aws.json` is the guard that matters here.
            dispatch(
                app,
                job_tx,
                Job::AwsUp {
                    root: app.layout.root.clone(),
                    api: app.api.clone(),
                    http: http.clone(),
                    count,
                },
            );
            app.set_status(
                Level::Info,
                format!("launching {count} box(es) — watch events"),
            );
        }
        Command::AwsDown { force } => {
            if app.cloud.is_empty() {
                app.set_status(Level::Warn, "no cloud listing yet — run :pool first");
                return;
            }
            let ids: Vec<String> = app
                .cloud
                .iter()
                .filter(|i| is_live_state(&i.state))
                .map(|i| i.id.clone())
                .collect();
            if ids.is_empty() {
                app.set_status(
                    Level::Info,
                    "nothing to terminate — no live box carries the marker tag",
                );
                return;
            }
            // The guard the CLI does not have: a box killed mid-render loses
            // that render, and TTS is stochastic — it cannot be reproduced from
            // the same inputs, so the loss is not recoverable from the ledger.
            let addrs = instance_addresses(&app.cloud);
            let busy = busy_on(&app.beats, &app.tasks, &addrs, bm_proto::now_secs());
            if !busy.is_empty() && !force {
                app.set_status(
                    Level::Warn,
                    format!(
                        "refused: {} task(s) in flight on those boxes — `:down force` to kill a render mid-flight",
                        busy.len()
                    ),
                );
                return;
            }
            let mut body = vec![
                format!("Terminate {} EC2 instance(s):", ids.len()),
                String::new(),
            ];
            for id in &ids {
                body.push(format!("  {id}"));
            }
            body.push(String::new());
            body.push("Terminated boxes cannot be restarted; a replacement is `:up`.".into());
            body.push("Their registry entries stay until you `:drop` them.".into());
            if !busy.is_empty() {
                body.push(String::new());
                // Bounded: one box can hold a whole batch of takes, and the
                // dialog counts body *entries* while the text wraps.
                body.push(format!(
                    "FORCED past {} in-flight task(s): {}",
                    busy.len(),
                    busy_summary(&busy)
                ));
            }
            app.screen = Screen::Confirm(Confirm {
                title: format!("Terminate {} box(es)?", ids.len()),
                danger: true,
                body,
                action: ConfirmAction::AwsDown { ids },
            });
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
            app.set_status(
                Level::Info,
                "shutdown armed — workers exit once the queue drains",
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
