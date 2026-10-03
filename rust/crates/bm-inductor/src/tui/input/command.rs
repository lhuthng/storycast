//! `:` commands: names for keys, direct runs for gated operator actions.
use crate::tui::input::runconfig::run_preview;
use crate::tui::{
    app::App,
    input::{audition, dispatch, dispatch_op},
    jobs::Job,
    model::{busy_on, instance_addresses, is_live_state},
    screen::{
        CastView, CloudView, Confirm, ConfirmAction, Picker, Screen, ScriptView, TextKind,
        TextPrompt,
    },
    style::{Conn, Level},
};
use bm_proto::{Op, OpRequest, Stage};
use crossterm::event::KeyCode;
use std::sync::{atomic::AtomicBool, Arc};

/// What a `:` command line request actually runs. Read-only commands map to
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Adopt chapter text the operator supplies — the manual half of crawling.
    Import,
    Voices,
    SwapVoice,
    Cast,
    /// Stop offering digest work on every machine, remembering what each box had.
    DigestOff,
    /// Put each machine's snapshotted digest policy back — **not** "digest on":
    DigestOn,
    Eta,
    /// Requeue shelved work: everything, one chapter, or one task. `stage`
    FixSpeaker {
        chapter: u32,
        segment: usize,
        expect: String,
        speaker: String,
    },
    /// Fold characters into one by hand: the first name survives (keeps its
    Merge {
        survivor: String,
        absorbed: Vec<String>,
    },
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
    /// GitHub `owner/name` hosting the baked model artifact: save-only, like
    ModelsRelease,
    /// Set `packs_release`: the repo a box fetches the profile pack from.
    PacksRelease,
    /// How many of one chapter's takes a single render offer carries. Saved to
    RenderBatch,
    /// The selected box's TTS sidecar thread count (`None` opens the prompt,
    TtsThreads {
        threads: Option<Option<u16>>,
    },
    Mix,
    Sound,
    Rerender,
    Remerge,
    /// Start or stop distributing work (`:go` / `:hold`).
    Dispatch {
        go: bool,
    },
    ShutdownWhenIdle,
    /// Drop the queued exclusive write (`:xdrop`), or the whole line with
    ExclusiveCancel {
        route: Option<String>,
    },
    /// Open the script inspection window (`:script`).
    Script,
    /// The workspace prompt, opened by `:ws <name>` with the line already in it.
    Workspace {
        prefill: String,
    },
    /// `:ws` with nothing after it: the books, with arrows. The prompt it used
    WorkspacePick,
    Profile,
    /// LLM providers: keys, endpoints, models, and which one digests.
    Llm,
    AuditionCurrent,
    AuditionTry,
    AuditionAnother,
    /// Show what the EC2 account holds. Read-only: launches nothing.
    AwsPool,
    /// Store the app's IAM user from the console's CSV. Setup, once.
    AwsLogin,
    /// Read the account into the pool definition. Setup, once — and safe to
    AwsDiscover,
    /// Launch `count` EC2 boxes and link what comes back into the registry.
    AwsUp {
        count: u32,
    },
    /// Terminate the live boxes the Cloud view is showing. Destructive, so it
    AwsDown {
        force: bool,
    },
}

/// One `:`-addressable command: its single-char form, its words, and its
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
    Word { key: Some('i'), names: &["import"], desc: Some("adopt a chapter from a file — `:import 34 /tmp/ch34.txt`"), cmd: Command::Import },
    Word { key: Some('v'), names: &["voices"], desc: Some("re-read the roster and refill gaps"), cmd: Command::Voices },
    Word { key: Some('s'), names: &["swap"], desc: Some("repoint one character — destructive, see below"), cmd: Command::SwapVoice },
    Word { key: Some('S'), names: &["cast"], desc: Some("cast overview: every speaker × voice, read-only"), cmd: Command::Cast },
    Word { key: Some('e'), names: &["eta"], desc: Some("estimate the remaining wall-clock time"), cmd: Command::Eta },
    Word { key: Some('u'), names: &["retry"], desc: Some("requeue every shelved task — strikes reset; `:retry 24` narrows to one chapter, `:retry render 24` to one task"), cmd: Command::Retry { stage: None, chapter: None } },
    Word { key: None, names: &["speaker"], desc: Some("re-attribute one segment: `:speaker 18 67 \"Thanh Sơn lão tổ\" \"Dịch Phong\"` — segment is 1-based, the two names are checked, quotes for spaces; re-speaks only the takes the edit reached"), cmd: Command::FixSpeaker { chapter: 0, segment: 0, expect: String::new(), speaker: String::new() } },
    Word { key: None, names: &["script"], desc: Some("script inspection: every digested chapter, its segments with their speakers, and s to re-point one — the guided form of :speaker"), cmd: Command::Script },
    Word { key: Some('m'), names: &["reconcile"], desc: Some("fold duplicates — asks first; certain folds apply, ambiguous only listed"), cmd: Command::Reconcile },
    Word { key: None, names: &["merge"], desc: Some("fold characters by hand: `:merge \"Survivor\" \"Absorbed\"…` — first name keeps its voice, the rest join its proper_aliases; scripts rewritten, losers re-rendered; asks first"), cmd: Command::Merge { survivor: String::new(), absorbed: Vec::new() } },
    Word { key: Some('B'), names: &["backend"], desc: Some("backend up now, machines provision in background and join as ready"), cmd: Command::Backend },
    Word { key: None, names: &["mix"], desc: Some("story speed and fx/music/inject volumes — requeues every merge"), cmd: Command::Mix },
    Word { key: None, names: &["sound", "sounds", "pools"], desc: Some("the three clip pools: add, edit, retune, remove"), cmd: Command::Sound },
    Word { key: None, names: &["remerge"], desc: Some("requeue every merge — render cache kept, no confirm"), cmd: Command::Remerge },
    Word { key: None, names: &["rerender"], desc: Some("requeue every render + merge — full re-speak, asks first"), cmd: Command::Rerender },
    Word { key: None, names: &["shutdown-when-idle", "drain"], desc: Some("workers exit on their own once the queue drains — restart with :B"), cmd: Command::ShutdownWhenIdle },
    Word { key: None, names: &["xdrop"], desc: Some("drop the queued exclusive write (a swap/merge/remix waiting for the cluster to quiet) — :xdrop swap-voice drops only that kind"), cmd: Command::ExclusiveCancel { route: None } },
    Word { key: None, names: &["go"], desc: Some("start distributing: armed here and now, and the remainder of the range queued — a process comes up held, so a restart never resumes on its own"), cmd: Command::Dispatch { go: true } },
    Word { key: None, names: &["hold"], desc: Some("stop distributing: what is in flight finishes, nothing new is offered — `:go` to resume; `:drain` is the other thing (workers exit)"), cmd: Command::Dispatch { go: false } },
    Word { key: None, names: &["workspace", "ws"], desc: Some("list, switch or create a workspace — one per book; only with the cluster stopped"), cmd: Command::WorkspacePick },
    Word { key: None, names: &["profile"], desc: Some("list, load or pack a genre profile — loading replaces assets/ + prompts/, so only with the cluster stopped"), cmd: Command::Profile },
    Word { key: None, names: &["login"], desc: Some("store the IAM user's key from the console's accessKeys.csv — setup, once"), cmd: Command::AwsLogin },
    Word { key: None, names: &["discover"], desc: Some("read the account into `.bm/aws.json`: AMI, subnet, group, keypair, instance profile"), cmd: Command::AwsDiscover },
    Word { key: Some('l'), names: &["pool", "aws", "cloud"], desc: Some("what the EC2 account holds — launches nothing"), cmd: Command::AwsPool },
    Word { key: Some('L'), names: &["llm", "model", "models"], desc: Some("LLM providers: add a key, set the endpoint and model, switch the active one — same as L"), cmd: Command::Llm },
    Word { key: Some('w'), names: &["up", "launch"], desc: Some("launch EC2 boxes and link them into the cluster — spends money"), cmd: Command::AwsUp { count: 1 } },
    Word { key: Some('o'), names: &["down", "terminate"], desc: Some("terminate the live EC2 boxes — destructive; asks first, refuses while a render is in flight"), cmd: Command::AwsDown { force: false } },
    Word { key: Some('X'), names: &["stop"], desc: Some("stop everything everywhere: local backend plus workers on all machines"), cmd: Command::Stop },
    Word { key: None, names: &["sshkey"], desc: None, cmd: Command::SshKey },
    Word { key: None, names: &["sshuser"], desc: None, cmd: Command::SshUser },
    Word { key: None, names: &["sshport"], desc: None, cmd: Command::SshPort },
    Word { key: None, names: &["advertise", "adv"], desc: Some("the address workers dial back on — set it when they are off the LAN"), cmd: Command::Advertise },
    Word { key: None, names: &["release", "modelsrelease"], desc: Some("GitHub owner/name whose releases hold the model artifact, so a box fetches the weights from a CDN instead of your uplink (empty = push)"), cmd: Command::ModelsRelease },
    Word { key: None, names: &["packrelease", "packsrelease"], desc: Some("GitHub owner/name whose releases hold the profile pack, so a box fetches assets/ from a CDN instead of your uplink — the tag comes from the loaded profile's version (empty = push)"), cmd: Command::PacksRelease },
    Word { key: None, names: &["batch", "renderbatch"], desc: Some("how many of one chapter's takes one render offer carries (default 5)"), cmd: Command::RenderBatch },
    Word { key: None, names: &["threads", "ttsthreads"], desc: Some("the selected box's TTS sidecar threads — `:threads 8` sets 1-64 directly, `:threads clear` restores the sidecar's own default, bare opens the prompt; the box's sidecar restarts on its next render"), cmd: Command::TtsThreads { threads: None } },
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
pub(crate) fn command_key(input: &str) -> Option<Command> {
    let word = input.trim();
    if word.chars().count() == 1 {
        let c = word.chars().next().filter(|c| *c != ':')?;
        if let Some(w) = WORDS.iter().find(|w| w.key == Some(c)) {
            return Some(w.cmd.clone());
        }
        // Read-only keys keep their Normal-mode arms, so the command
        return Some(Command::Key(KeyCode::Char(c)));
    }
    // Commands that take an argument: `:up 3`, `down force`, and the retry
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
            "threads" | "ttsthreads" if !rest.is_empty() => {
                return Some(Command::TtsThreads {
                    threads: Some(parse_threads_arg(rest[0])?),
                });
            }
            // Its own splitter, because a speaker name is almost always two or
            "speaker" if !rest.is_empty() => {
                let args = split_args(&rest.join(" "));
                let [chapter, segment, expect, speaker] = args.as_slice() else {
                    return None;
                };
                let chapter = chapter.parse::<u32>().ok().filter(|c| *c > 0)?;
                let segment = segment.parse::<usize>().ok().filter(|s| *s > 0)?;
                if expect.is_empty() || speaker.is_empty() {
                    return None;
                }
                return Some(Command::FixSpeaker {
                    chapter,
                    segment,
                    expect: expect.clone(),
                    speaker: speaker.clone(),
                });
            }
            // A name is almost always two or three words, so the arguments
            "ws" | "workspace" if rest.is_empty() => return Some(Command::WorkspacePick),
            // `:ws <name>` used to arrive here as a bare `:ws`: the argument
            "ws" | "workspace" => {
                return Some(Command::Workspace {
                    prefill: rest.join(" "),
                });
            }
            "merge" if !rest.is_empty() => {
                let args = split_args(&rest.join(" "));
                let [survivor, absorbed @ ..] = args.as_slice() else {
                    return None;
                };
                if survivor.trim().is_empty()
                    || absorbed.is_empty()
                    || absorbed.iter().any(|a| a.trim().is_empty())
                {
                    return None;
                }
                return Some(Command::Merge {
                    survivor: survivor.clone(),
                    absorbed: absorbed.to_vec(),
                });
            }
            _ => {}
        }
    }
    let lower = word.to_ascii_lowercase();
    WORDS
        .iter()
        .find(|w| w.names.iter().any(|n| *n == lower))
        .map(|w| w.cmd.clone())
}

/// Split a command's arguments on whitespace, except inside double quotes, so
pub(crate) fn split_args(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut started = false;
    for c in input.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            c if c.is_whitespace() && !quoted => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            c => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(cur);
    }
    out
}

/// `:retry <chapter>` / `:retry <stage> <chapter>` → the command that names
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

/// `:threads <n>` / `:threads clear` → the count it names, if valid.
fn parse_threads_arg(s: &str) -> Option<Option<u16>> {
    match s.to_ascii_lowercase().as_str() {
        "clear" | "default" | "auto" | "none" | "-" => Some(None),
        n => {
            let v: u16 = n.parse().ok()?;
            (1..=64).contains(&v).then_some(Some(v))
        }
    }
}
/// A stage name as the task ledger spells it (`crawl`, `digest`, `render`,
fn stage_by_name(s: &str) -> Option<Stage> {
    Stage::ALL
        .into_iter()
        .find(|st| st.as_str().eq_ignore_ascii_case(s))
}

/// A bounded, one-entry list of in-flight tasks for a dialog line.
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

pub(crate) mod exec;

pub(crate) use exec::*;
