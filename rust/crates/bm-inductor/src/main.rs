//! Inductor: control API + scheduler. Workers report facts; this decides.

mod api;
mod aws_ops;
mod backend;
mod dispatch;
mod manual;
mod roster;
mod segments;
mod state;
mod tui;
mod tunnel;

use bm_core::{config::Settings, Layout};
use bm_proto::Machine;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "bm-inductor",
    about = "Cluster orchestrator for the novel pipeline"
)]
struct Cli {
    /// Repo root (discovered when omitted: `rust/Cargo.toml` in a checkout,
    /// `.bm/profile` on a provisioned worker).
    #[arg(long, global = true)]
    root: Option<std::path::PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the control API + scheduler.
    Serve {
        /// Control API port.
        #[arg(long, default_value = "8901")]
        port: u16,
        /// Bind address (127.0.0.1 for solo, 0.0.0.0 when remote workers join).
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        /// Chapter range to reconcile on startup.
        #[arg(long, default_value = "1")]
        start: u32,
        #[arg(long, default_value = "1")]
        count: u32,
    },
    /// Onboard one machine by address: probe, push what's missing, verify.
    Provision {
        /// Linked box name (from `link`); skips retyping addr/user/key.
        #[arg(long)]
        r#box: Option<String>,
        /// Machine address (IP or hostname). Required unless --box is given.
        #[arg(long)]
        addr: Option<String>,
        /// SSH user.
        #[arg(long, default_value = "thang")]
        user: String,
        /// SSH port.
        #[arg(long, default_value = "22")]
        port: u16,
        /// SSH key path.
        #[arg(long)]
        key: Option<String>,
        /// Inductor API port (for post-provision registration).
        #[arg(long, default_value = "8901")]
        api_port: u16,
        /// Rebuild even when already configured.
        #[arg(long)]
        force: bool,
    },
    /// Live cluster dashboard (talks to a running inductor API).
    Tui {
        /// Inductor API base URL.
        #[arg(long, default_value = "http://127.0.0.1:8901")]
        api: String,
        /// Print one plain-text snapshot and exit instead of drawing the
        /// dashboard. Needs no terminal, so it works with screen readers,
        /// `watch(1)` and shell pipelines.
        #[arg(long)]
        once: bool,
    },
    /// Voice roster maintenance. Local only — touches no worker.
    Roster {
        #[command(subcommand)]
        cmd: RosterCmd,
    },
    /// Segment inventory across the cluster: per chapter per machine, diffed
    /// against what the scripts+cast actually require. Report-only by default;
    /// `--collect` pulls what remotes hold that this inductor lacks.
    Segments {
        /// Only these machines (address or link name), instead of all linked.
        #[arg(long)]
        from: Vec<String>,
        /// Pull remote-held segments the inductor lacks into `data/audio/`.
        #[arg(long)]
        collect: bool,
        /// Delete local files no chapter expects (stale voices). DESTRUCTIVE:
        /// prints every path as it deletes. Without this flag the command
        /// only reports.
        #[arg(long)]
        prune: bool,
        /// Report what `--collect` would pull, and write nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Rewrite written-out non-verbal sounds into engine tags across every
    /// script (`Ha ha ha!` → `[cười]`), and requeue the chapters it touches.
    /// Posts to a running inductor (refused while workers are mid-play).
    /// Report-only with `--dry-run`.
    Retag {
        /// Inductor API base URL.
        #[arg(long, default_value = "http://127.0.0.1:8901")]
        api: String,
        /// Show every change without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Ask the analyzer for ONE chapter and print what it said.
    ///
    /// This is the digest stage's question with none of its consequences: no
    /// render is queued, no merge runs, no ledger task is touched, and the
    /// bible is not merged. Nothing is written at all unless `--write` is
    /// passed, and even then it is only the script file — the same function the
    /// digest worker calls, so what you read here is exactly what a run would
    /// have produced.
    Digest {
        /// Chapter number (needs `data/chapters/chNN.txt`).
        chapter: u32,
        /// Analyzer to use. Default: the `analyzer` value in `.bm/settings.json`.
        #[arg(long)]
        analyzer: Option<String>,
        /// Also write `data/script-NN.json`. Nothing else happens: caches are
        /// NOT invalidated and nothing is requeued, so a hand-written script
        /// can disagree with segments already on disk.
        #[arg(long)]
        write: bool,
        /// Print the whole script object instead of a one-line summary.
        #[arg(long)]
        json: bool,
    },
    /// Backup digestor: run the digest here, while the cluster's digest has no
    /// quota, and hand every accepted chapter to a running inductor.
    ///
    /// **The worker's own digest, not a second one.** Round 1 renders
    /// `build_attribution_prompt`, round 2 renders `build_staging_prompt`, and
    /// the answers are checked by the same validators the automatic path uses —
    /// the same flow the TUI's `:digest` manager drives by clipboard, with a
    /// model standing where the operator's pastes would be.
    ///
    /// Each finished chapter is reported to `POST /api/complete` under the
    /// reserved `operator` id, so the ledger, the bible and the cast move
    /// exactly as they do for a worker, and a race with a box grinding the same
    /// chapter resolves the way the manual digest's does: the report wins.
    ///
    /// A running inductor is a **precondition**, as it is for `tui`: the report
    /// is the only thing that makes a digest count, so the backend is checked
    /// first and a missing one is named with what to enter instead of waited on.
    Backup {
        /// First chapter. Default: the one after the last digested chapter —
        /// the only chapter a digest may start from, since each chapter's bible
        /// delta lands on its predecessor's.
        start: Option<u32>,
        /// Last chapter, inclusive. Default: the last chapter the ledger knows
        /// about, so a bare `backup` carries on to the end of the book.
        #[arg(long)]
        through: Option<u32>,
        /// Which service to call. Default: read off `--api` (a URL containing
        /// `openrouter` or `googleapis`), then the API key in the environment
        /// (`sk-or-` → openrouter, `AIza` → gemini), then settings.
        #[arg(long)]
        analyzer: Option<String>,
        /// The model service's base URL — where the two digest calls go. This is
        /// **not** the inductor: the report target is this machine's own control
        /// API, `127.0.0.1:<control_port>`, unless `--inductor` says otherwise.
        #[arg(long, default_value = "https://openrouter.ai/api/v1")]
        api: String,
        /// Where the finished chapters are reported. Default: this machine's
        /// inductor, on the port in settings. Rarely needs saying.
        #[arg(long)]
        inductor: Option<String>,
        /// The model to answer with, on whichever service the key names.
        #[arg(long)]
        model: Option<String>,
        /// How many times to re-ask a round the gate refused, with the
        /// validator's own complaint attached. The worker's path allows itself
        /// one repair; a backup digestor is a person-or-model with more
        /// patience and a chapter nobody else is racing, so it is worth more.
        /// 0 asks once and reports the refusal.
        #[arg(long, default_value_t = 3)]
        retries: u32,
        /// Analyze and print, but report nothing — a dry run of the prompts.
        #[arg(long)]
        dry_run: bool,
    },
    /// Check a link before you build a workspace around it: one request, and a
    /// verdict on whether a crawl of that page would produce a chapter.
    ///
    /// **Paste a URL and find out in a second**, rather than finding out from
    /// ten workers failing at once. A site behind a bot check costs a cluster an
    /// afternoon to discover; this costs one request.
    ///
    /// Reads the active workspace's `crawl.user_agent` and `crawl.headers` when
    /// they are set, so a check goes out exactly as a real crawl would — a
    /// session cookie you are relying on is part of what is being checked, and
    /// `bm-inductor check` is how you confirm the cookie still works. Writes
    /// nothing, and exits non-zero when the page is not a chapter, so a setup
    /// script can gate on it.
    Check {
        /// The URL to check. A chapter, not a book index: the question is whether
        /// a *chapter* comes back.
        url: String,
        /// Per-request timeout.
        #[arg(long, default_value_t = 30)]
        timeout: u64,
    },
    /// Link a machine by name: remembers how to reach it so `provision --box`
    /// needs no flags. Writes `.bm/machines.json`, which is ignored.
    Link {
        /// Short handle, e.g. `box-1`.
        #[arg(long)]
        name: String,
        /// Machine address (IP or hostname).
        #[arg(long)]
        addr: String,
        /// SSH user.
        #[arg(long, default_value = "thang")]
        user: String,
        /// SSH port.
        #[arg(long, default_value = "22")]
        port: u16,
        /// SSH key path.
        #[arg(long)]
        key: Option<String>,
    },
    /// Workspaces: one directory per book under `workspaces/`, holding its
    /// settings, ledger, data and output. Switching only moves the
    /// `.bm/active-workspace` pointer — nothing is wiped, nothing is mixed.
    /// Local only — touches no worker.
    Workspace {
        #[command(subcommand)]
        cmd: WorkspaceCmd,
    },
    /// AWS worker pool: the IAM user this app runs as, the definition written
    /// once in `.bm/aws.json`, and what the account actually holds. Local only
    /// — starts nothing.
    Aws {
        #[command(subcommand)]
        cmd: AwsCmd,
    },
}

#[derive(Subcommand)]
enum AwsCmd {
    /// Print the pool definition, what is still missing, and the one-off setup
    /// commands it needs. Reads no network.
    Show,
    /// Print the least-privilege policy for this app's IAM user, and the
    /// commands that create that user and attach it. Reads no network.
    ///
    /// The policy is the tracked `aws-policy.json` — one document, used by both
    /// this command and `aws iam put-user-policy`, so what you read and what
    /// you install cannot drift apart.
    Policy,
    /// Write a `.bm/aws.json` template to fill in. Refuses to overwrite one
    /// that exists unless --force.
    Init {
        /// Overwrite an existing pool definition.
        #[arg(long)]
        force: bool,
    },
    /// List the boxes this tool started, straight from the account.
    ///
    /// The credential and region check: if this answers, `up` can too.
    Ls,
    /// Store the credentials of the IAM user created for this app, then verify
    /// them against the account.
    ///
    /// **An IAM user, and nothing else.** The identity is checked with
    /// `sts get-caller-identity` and refused unless it is a `:user/` ARN, so a
    /// root key or an assumed role cannot be stored — those are the identities
    /// a dedicated user exists to replace.
    ///
    /// The secret is read from stdin and **never** taken as an argument:
    /// `argv` is visible in `ps` on every box it was typed on. Piped input
    /// works, so a script can do
    /// `printf '%s\\n' "$SECRET" | bm-inductor aws login --access-key-id AKIA…`.
    Login {
        #[command(flatten)]
        args: aws_ops::LoginArgs,
    },
    /// Fill in the pool fields a console page cannot hand you as a copy-paste:
    /// the AMI, the default network, and the instance profile.
    ///
    /// Everything it learns is written into `.bm/aws.json` **and printed**, so
    /// what was chosen stays visible and reviewable instead of being
    /// re-resolved on every launch. Read-only calls; writes nothing outside
    /// `.bm/`. Run it once, after `aws login`.
    Discover {
        #[command(flatten)]
        args: aws_ops::DiscoverArgs,
    },
    /// Launch boxes and leave them running, tagged, ready to provision.
    ///
    /// `--dry-run` prints the exact `aws ec2 run-instances` call and stops —
    /// the review step before anything costs money.
    Up {
        /// How many. Refused if it would take the pool past `max_workers`.
        #[arg(long, default_value = "1")]
        count: u32,
        /// Print the call instead of making it.
        #[arg(long)]
        dry_run: bool,
    },
    /// Terminate the boxes carrying our marker tag.
    ///
    /// Resolves the ids from the tag first and prints them, so the destructive
    /// step is always over a list someone could read.
    Down {
        /// Print what would be terminated instead of doing it.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum WorkspaceCmd {
    /// Create `workspaces/<name>/` with default settings stamped to the
    /// loaded profile, and switch to it.
    New {
        /// Workspace name, e.g. `beyond-myriads`.
        name: String,
    },
    /// Switch the pointer to an existing workspace. Data follows the
    /// directory, so selecting never wipes.
    Use {
        /// Workspace name.
        name: String,
    },
    /// List workspaces, marking the active one.
    List,
}

#[derive(Subcommand)]
enum RosterCmd {
    /// Rewrite the cast files to store catalogue keys instead of display names,
    /// keeping a `.bak` of each.
    ///
    /// Not a prerequisite for anything: the reader accepts both forms and the
    /// writer keys the file on its next save. This does it now, and shows what
    /// changed.
    MigrateCast {
        /// Report what would change and write nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Add a sample clip to the voice pool: copies it under `refs/`, takes its
    /// tags from the filename (`young-female-4.mp3` → young, female), registers
    /// it in `voice-pool.json`, maps it in `voices.json` so the next provision
    /// enrolls it on workers — and enrolls it into this machine's own store
    /// right away when it has one, so renders use it immediately.
    AddSample {
        /// Clip to add (mp3/wav/m4a/ogg/flac).
        path: std::path::PathBuf,
        /// Override the filename tags: `--tags young,female`.
        #[arg(long, value_delimiter = ',')]
        tags: Vec<String>,
        /// Voice name to register under (default: the file stem). Without
        /// `--tags` the voice stays private: assignable by hand, never
        /// auto-rolled. `refs/narrator.mp3 --name Narrator` voices as `Narrator`.
        #[arg(long)]
        name: Option<String>,
    },
}

/// What one provision run concluded.
///
/// `ready` and `reachable` are separate on purpose, and the difference is the
/// one that matters to an operator staring at a box that will not come up.
/// `!reachable` means ssh never answered, so *nothing* was learned: no
/// platform, no binary pushed, no stamp read. The failure is a network fact,
/// not a provisioning one — and a caller that already knows the box was
/// launched seconds ago can keep saying "still booting" instead of "broken".
pub struct ProvisionOutcome {
    /// The post-provision probe says the box runs this exact agent build with
    /// TTS — the gate a start hides behind.
    pub ready: bool,
    /// ssh answered at all.
    pub reachable: bool,
    /// The log, one line per step, prefixed with the address.
    pub lines: Vec<String>,
    /// Why the run stopped, when it stopped before a step could log it —
    /// a missing local build, an unreadable profile pointer. Carried rather
    /// than recovered by grepping `lines`: the dashboard used to scan the log
    /// for the words "missing"/"not found"/"failed", and every pre-flight
    /// message that did not use one of them ("no TTS sidecar binary for …")
    /// left the machine pane reporting a bare `provision INCOMPLETE` and
    /// telling the operator to retry the same click forever.
    pub stop: Option<String>,
}

/// A run that stopped before any step: the message is logged *and* carried,
/// so the log and the machine pane cannot disagree about why.
fn stopped(
    log: &mut bm_core::provision::LiveLog,
    addr: &str,
    why: impl std::fmt::Display,
    reachable: bool,
) -> ProvisionOutcome {
    let why = why.to_string();
    log.push(format!("[{addr}] {why}"));
    ProvisionOutcome {
        ready: false,
        reachable,
        stop: Some(why),
        lines: std::mem::take(&mut log.lines),
    }
}

/// Blocking provision run shared by the CLI and the TUI background task.
///
/// `live` streams each log line to the TUI event pane as it happens (slow
/// steps read as progress, not a stall); `None` keeps collect-only for the
/// CLI, which prints everything at the end.
pub fn provision_machine(
    layout: &Layout,
    addr: &str,
    user: &str,
    port: u16,
    key: Option<String>,
    force: bool,
    live: Option<tokio::sync::mpsc::UnboundedSender<String>>,
) -> ProvisionOutcome {
    if bm_core::is_local_node(addr) {
        // No mirror to fill: the local worker runs in place from this repo —
        // prompts, assets, models and binaries are read where they stand, so
        // syncing a `~/bm-worker` copy would only spend disk and let a stale
        // copy fail the readiness gate below. Launching the worker stays the
        // caller's job (`:B` catch-up, `make agent`).
        return ProvisionOutcome {
            ready: true,
            reachable: true,
            stop: None,
            lines: vec![format!(
                "[{addr}] local machine — runs from the repo, nothing to provision"
            )],
        };
    }
    use bm_core::provision::{provision, LiveLog, Ssh};
    let mut log = LiveLog::new(live.clone());
    let probe_ssh = Ssh {
        target: format!("{user}@{addr}"),
        port,
        key: key.clone(),
        local: bm_core::is_local_node(addr),
    };
    let pre = probe_ssh.probe();
    log.push(format!("[{addr}] {}", pre.summary()));
    // Unreachable means nothing downstream can run: no platform was learned
    // (os/arch stay empty — the old flow continued and failed confusingly on
    // "no agent binary for /"), no binary can be pushed, no worker launched.
    // Name the network cause and stop.
    if !pre.reachable {
        return stopped(
            &mut log,
            addr,
            format!(
                "cannot provision: the box never answered ssh ({}) — check it is up, and that its address is reachable from here (an EC2 private IP like 172.31.x.x is only routable from inside the VPC; the pool prefers public IPs)",
                pre.note
            ),
            false,
        );
    }
    let pointer = match bm_core::profile::read_pointer(&layout.root) {
        Ok(p) => p,
        Err(_) => {
            return stopped(
                &mut log,
                addr,
                "no local profile loaded — load one first (`:profile` in the dashboard; `tools/profile.sh fetch/unpack <name>`) — workers verify it at startup",
                true,
            );
        }
    };
    log.push(format!(
        "[{addr}] profile: {} ({})",
        pointer.name,
        &pointer.hash[..12.min(pointer.hash.len())]
    ));
    let binary = match agent_binary_for(pre.os.as_str(), pre.arch.as_str(), layout) {
        Ok(b) => b,
        Err(e) => return stopped(&mut log, addr, e, true),
    };
    log.push(format!("[{addr}] agent binary: {}", binary.display()));
    let tts = match tts_binary_for(pre.os.as_str(), pre.arch.as_str(), layout, &mut log, addr) {
        Ok(b) => b,
        Err(e) => return stopped(&mut log, addr, e, true),
    };
    log.push(format!("[{addr}] tts sidecar: {}", tts.display()));
    let mut m = Machine::new(addr, user, port, key, "worker");
    m.tts_url = Some("http://127.0.0.1:8818".into());
    let (after, mut flow) = provision(
        &m,
        layout,
        &binary,
        &tts,
        tts_runtime_dir(pre.os.as_str(), pre.arch.as_str(), layout).as_deref(),
        env!("CARGO_PKG_VERSION"),
        force,
        Some(pre),
        live,
    );
    // Already streamed live inside `provision` — collect silently here.
    log.lines.append(&mut flow);
    ProvisionOutcome {
        ready: after.configured(env!("CARGO_PKG_VERSION")),
        reachable: true,
        stop: None,
        lines: log.lines,
    }
}

/// A re-provision must not reset the operator's work policy: the machine
/// `cmd_provision` registers is fresh (`task_policy: None`), and both
/// registration paths persist it — the live POST and the ledger fallback.
/// Carry the stored policy forward so re-provisioning keeps the order and
/// toggles from the policy panel.
fn carry_task_policy(m: &mut Machine, layout: &Layout) {
    if m.task_policy.is_none() {
        m.task_policy = bm_core::provision::load_boxes(&layout.machines())
            .iter()
            .find(|b| b.addr == m.addr)
            .and_then(|b| b.task_policy.clone());
    }
}

fn check_bins() -> anyhow::Result<()> {
    for bin in ["ssh", "rsync", "ffmpeg"] {
        let found = std::env::var_os("PATH")
            .map(|paths| std::env::split_paths(&paths).any(|d| d.join(bin).is_file()))
            .unwrap_or(false);
        if !found {
            anyhow::bail!(
                "{bin} not found on PATH: provisioning needs ssh/rsync, merging needs ffmpeg"
            );
        }
    }
    Ok(())
}

async fn cmd_serve(
    layout: Layout,
    settings: Settings,
    port: u16,
    bind: &str,
    start: u32,
    count: u32,
) -> anyhow::Result<()> {
    // `Inner` takes the layout; the dispatcher needs its own handle on it.
    let drive_layout = layout.clone();
    let drive_root = drive_layout.root.clone();
    let mut inner = state::Inner::new(layout, settings);
    // No profile, no run. A drifted live tree is adopted by verify
    // (a `:sound` retune), not refused — only a missing pointer or an
    // empty live tree stops us before touching the ledger.
    let pointer = bm_core::profile::verify(&inner.layout.root)?;
    println!(
        "profile {} ({})",
        pointer.name,
        &pointer.hash[..12.min(pointer.hash.len())]
    );
    // The cluster token, generated on first use and then stable across
    // restarts: a worker that outlived a restart must not be locked out, and
    // provisioning copies this file to every box it onboards. Only a
    // fingerprint is printed — the log is not a place for a secret.
    let token = bm_core::token::load_or_create(&inner.layout.root)?;
    println!("cluster token {}", &token[..8.min(token.len())]);
    inner.load_ledger();
    inner.check_profile()?;
    inner.reconcile(start, count);
    let shared = std::sync::Arc::new(tokio::sync::Mutex::new(inner));
    // Lease reaper: expired leases return to the pool, no strike.
    let reaper = shared.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            let (expired, never_came_up) = {
                let mut inner = reaper.lock().await;
                // Boot deadline first, then leases: a box that never answered
                // has no leases to expire, and one lock covers both.
                let expired = inner.reap();
                let never_came_up = inner.expire_initializing();
                (expired, never_came_up)
            };
            for id in expired {
                println!("[inductor] lease expired, requeued {id} (no strike)");
            }
            for line in never_came_up {
                println!("[inductor] {line} — marked error");
            }
        }
    });
    // The driving half. Nothing dials this process, so if this loop is not
    // running, no work moves at all — the workers are servers waiting to be
    // asked, and this is the only thing that asks.
    let driving = shared.clone();
    tokio::spawn(async move { dispatch::run(driving, drive_layout).await });
    // The reverse tunnels: one ssh client per remote box, forwarding the
    // box's own loopback hook port to this API. A worker uses it only when
    // this process has gone silent on every normal channel (see
    // `bm-agent/src/hook.rs`) — holding it open costs one idle ssh per box,
    // and gives a finished stage a road home when the uplink blips mid-task.
    tokio::spawn(tunnel::supervise(shared.clone(), port));
    // Auto-relink once at startup: an EC2 box that cycled while this inductor
    // was down is sitting in the registry at an address that no longer answers.
    // Best-effort — no account, no creds or an offline CLI only means the
    // repairs wait for the next `:pool` refresh.
    {
        let relink_shared = shared.clone();
        let relink_root = drive_root.clone();
        tokio::spawn(async move {
            let root = relink_root;
            let pool = tokio::task::spawn_blocking(move || crate::aws_ops::pool(&root)).await;
            if let Ok(Ok((_cfg, instances))) = pool {
                let mut inner = relink_shared.lock().await;
                for line in inner.relink_drifted(&instances) {
                    inner.push_event("info", line.clone());
                    println!("[inductor] {line}");
                }
            }
        });
    }
    let app = api::router(shared);
    let addr = format!("{bind}:{port}");
    println!("inductor on http://{addr}");
    axum::serve(
        tokio::net::TcpListener::bind(&addr).await?,
        app.into_make_service(),
    )
    .await?;
    Ok(())
}

/// Pick the binaries matching the target platform. `os`/`arch` are the probe's
/// normalized values, in Rust's spelling (`linux`/`macos`, `x86_64`/`aarch64`).
///
/// A box identical to this machine runs the native build — which is also how a
/// macOS worker gets its binary with no cross toolchain involved. Anything
/// else needs its cross build present; a missing one is a build error naming
/// the exact command, never a guess that ships the wrong executable.
///
/// Pick the TTS sidecar binary for the target platform, building a cross
/// target on demand — the same bargain [`agent_binary_for`] strikes, with one
/// difference: this one is a **release** build (bm-tts's hot loop is a
/// hand-written SIMD matvec, and a debug build gives all of that back), so the
/// build is minutes rather than seconds and the log says so before it starts.
///
/// A staged binary that predates its sources is *used*, with a warning. The
/// agent's freshness rule forces a rebuild because the version gate then ships
/// a stale agent forever; the sidecar carries no version, so the honest fix
/// there is to say the box is running an older sidecar, not to silently spend
/// a multi-minute release build on every `:prov` because someone edited
/// bm-core. `make tts` rebuilds it whenever you want.
fn tts_binary_for(
    os: &str,
    arch: &str,
    layout: &Layout,
    log: &mut bm_core::provision::LiveLog,
    addr: &str,
) -> anyhow::Result<std::path::PathBuf> {
    match tts_binary_staged(os, arch, layout) {
        Ok(b) => {
            if tts_is_stale(&b, layout) {
                log.push(format!(
                    "[{addr}] staged bm-tts is older than its sources — this box runs the last sidecar built; `make tts` rebuilds it"
                ));
            }
            Ok(b)
        }
        Err(staged) => {
            // The cross candidates are buildable, and a `:prov` clicked in the
            // dashboard should heal the gap rather than send the operator to a
            // shell.
            if let Some(cand) = buildable_tts_candidates(os, arch, layout)
                .into_iter()
                .next()
            {
                log.push(format!(
                    "[{addr}] no TTS sidecar built yet — cross-building it now (release build, several minutes; the first one also fetches the ONNX Runtime)"
                ));
                build_tts_binary(&cand, layout)?;
                return Ok(cand);
            }
            Err(staged)
        }
    }
}

/// The pick among sidecar binaries already on disk. Platform-pure, no side
/// effects — the piece tests can exercise without a toolchain, and the error
/// [`tts_binary_for`] falls back from.
fn tts_binary_staged(os: &str, arch: &str, layout: &Layout) -> anyhow::Result<std::path::PathBuf> {
    for cand in tts_candidates(os, arch, layout) {
        if cand.is_file() {
            return Ok(cand);
        }
    }
    anyhow::bail!(
        "no TTS sidecar binary for {os}/{arch} at {} (linux/x86_64: `make tts`; linux/aarch64: `cargo zigbuild --release --target aarch64-unknown-linux-gnu -p bm-tts` after staging its runtime)",
        tts_candidates(os, arch, layout)
            .into_iter()
            .map(|c| c.display().to_string())
            .collect::<Vec<_>>()
            .join(" or ")
    )
}

/// True when a staged sidecar is older than the workspace sources it was built
/// from. A warning, never a rebuild — see [`tts_binary_for`] for why.
fn tts_is_stale(bin: &std::path::Path, layout: &Layout) -> bool {
    !staged_is_fresh_against(bin, &["crates/bm-tts/src"], layout)
}

/// The sidecar targets this host can actually cross-build: exactly one.
///
/// linux/x86_64, because it is the target whose ONNX Runtime `make runtime`
/// stages — a build that cannot find its runtime library is a build that
/// cannot happen, and a *wrong* runtime would link a binary that dies on the
/// box. The native candidate is out (it exists exactly when this host *is* the
/// target, and `ort`'s own `download-binaries` covers that case without a
/// cross toolchain), and linux/aarch64 keeps the manual command in the error
/// until a `runtime-aarch64` target exists to stage its library.
fn buildable_tts_candidates(os: &str, arch: &str, layout: &Layout) -> Vec<std::path::PathBuf> {
    if (os, arch) != ("linux", "x86_64") {
        return Vec::new();
    }
    tts_candidates(os, arch, layout)
        .into_iter()
        .take(1)
        .collect()
}

/// Cross-build the sidecar into the exact path a candidate names, staging the
/// shared ONNX Runtime it links against first.
///
/// The runtime is staged by shelling out to `make runtime` rather than
/// reimplemented here: the URL and the sha256 that pins it live in the
/// Makefile, and a second copy of a checksum is a checksum that will rot.
fn build_tts_binary(cand: &std::path::Path, layout: &Layout) -> anyhow::Result<()> {
    let target = cross_target_of(cand)?;
    let rust_dir = workspace_dir_above_target(cand)?;
    // The runtime goes where `tts_runtime_dir` will look for it, so the box
    // gets pushed the library this binary actually links against.
    let runtime = rust_dir.join("target").join("ort-linux-x64");
    // A staging failure is not fatal on its own: a runtime already sitting
    // there is all the build needs, and the build's own linker error is the
    // honest report if it is not. Keep the reason and move on.
    let stage_note = stage_onnx_runtime(layout, &runtime)
        .err()
        .map(|e| e.to_string());
    let mut cmd = std::process::Command::new("cargo-zigbuild");
    for tool in ["zig", "cargo-zigbuild"] {
        if tool_on_path(tool).is_none() {
            anyhow::bail!(
                "{tool} not found: the TTS cross-build needs it (`cargo install cargo-zigbuild`; zig from `brew install zig` or https://ziglang.org/download){}",
                stage_note
                    .map(|n| format!("; staging the ONNX Runtime also failed: {n}"))
                    .unwrap_or_default()
            );
        }
    }
    let shim_dir = tool_on_path("cargo-zigbuild")
        .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
        .expect("probed above");
    cmd.args([
        "zigbuild",
        "--release",
        "--target",
        &target,
        "-p",
        "bm-tts",
        "--bin",
        "bm-tts",
        "--manifest-path",
    ])
    .arg(rust_dir.join("Cargo.toml"))
    .env("ORT_LIB_LOCATION", &runtime)
    .env("ORT_PREFER_DYNAMIC_LINK", "1")
    // A GUI launch (or a desktop shortcut) inherits a PATH without
    // `~/.cargo/bin`, and the linker is looked up by name from there.
    .env("PATH", path_with_shim(shim_dir));
    let out = cmd
        .output()
        .map_err(|e| anyhow::anyhow!("running cargo-zigbuild: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail: String = stderr
            .lines()
            .filter(|l| !l.trim().is_empty())
            .rev()
            .take(12)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n  ");
        anyhow::bail!("cross build of bm-tts for {target} failed:\n  {tail}");
    }
    if !cand.is_file() {
        anyhow::bail!(
            "cross build reported success but {} is still missing",
            cand.display()
        )
    }
    Ok(())
}

/// `make runtime`, unless the shared library is already staged. Idempotent in
/// the Makefile too — this only saves the round trip of spawning it.
fn stage_onnx_runtime(layout: &Layout, dir: &std::path::Path) -> anyhow::Result<()> {
    if dir.join("libonnxruntime.so").is_file() && dir.join("libonnxruntime.so.1").is_file() {
        return Ok(());
    }
    let out = std::process::Command::new("make")
        .arg("-C")
        .arg(&layout.root)
        .arg("runtime")
        .output()
        .map_err(|e| anyhow::anyhow!("running `make runtime`: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!(
            "staging the ONNX Runtime failed: {}",
            bm_core::util::head_chars(err.trim(), 300)
        );
    }
    Ok(())
}

/// Where `make runtime` staged the shared ONNX Runtime to push alongside the
/// sidecar — `None` where the sidecar is self-contained (macOS links its
/// runtime statically, so no `.so` travels).
fn tts_runtime_dir(os: &str, arch: &str, layout: &Layout) -> Option<std::path::PathBuf> {
    match (os, arch) {
        ("linux", "x86_64") => Some(layout.root.join("rust/target/ort-linux-x64")),
        ("linux", "aarch64") => Some(layout.root.join("rust/target/ort-linux-aarch64")),
        _ => None,
    }
}

fn agent_binary_for(os: &str, arch: &str, layout: &Layout) -> anyhow::Result<std::path::PathBuf> {
    match agent_binary_staged(os, arch, layout) {
        Ok(b) => Ok(b),
        Err(staged) => {
            // The cross targets are cheap to produce on demand (a debug
            // `bm-agent` links no C toolchain, so `zig cc` needs no runtime
            // staged), and a `:prov` clicked in the dashboard should heal the
            // gap itself rather than send the operator to a shell. Only the
            // cross candidates are buildable — the native fallback exists
            // exactly when this host is the target, so `cargo build` already
            // ran and a miss means something is wrong beyond a missing build.
            // Cross targets only, and at most one per platform — the build
            // either succeeds (returning the candidate) or its error is the
            // provision failure.
            if let Some(cand) = buildable_agent_candidates(os, arch, layout)
                .into_iter()
                .next()
            {
                build_agent_binary(&cand)?;
                return Ok(cand);
            }
            Err(staged)
        }
    }
}

/// The pick among binaries already on disk. Platform-pure, no side effects —
// the piece tests can exercise without a toolchain. A missing cross build is
// an error naming the platform; [`agent_binary_for`] may still build it.
//
// A present file is only picked when it postdates the sources it was built
// from: bumping the version (or touching agent code) without rebuilding left
// 0.2.3 on disk while the inductor demanded 0.2.4, and `:prov` shipped the
// stale build forever. A stale file reads as missing so the caller builds it.
fn agent_binary_staged(
    os: &str,
    arch: &str,
    layout: &Layout,
) -> anyhow::Result<std::path::PathBuf> {
    for cand in agent_candidates(os, arch, layout) {
        if cand.is_file() && staged_is_fresh(&cand, layout) {
            return Ok(cand);
        }
    }
    anyhow::bail!(
        "no agent binary for {os}/{arch} at {} (linux/x86_64 is cross-built; linux/aarch64: `cargo zigbuild --target aarch64-unknown-linux-gnu -p bm-agent`; macOS: provision from a same-arch Mac so the native build matches)",
        agent_candidates(os, arch, layout)
            .into_iter()
            .map(|c| c.display().to_string())
            .collect::<Vec<_>>()
            .join(" or ")
    )
}

/// Cross candidates only — the native build is never something we can conjure
/// here: it exists exactly when this host *is* the target, so a miss there is
/// not a missing cross toolchain but a broken workspace.
fn buildable_agent_candidates(os: &str, arch: &str, layout: &Layout) -> Vec<std::path::PathBuf> {
    let native = layout.root.join("rust/target/debug/bm-agent");
    agent_candidates(os, arch, layout)
        .into_iter()
        .filter(|c| c != &native)
        .collect()
}

/// True when no workspace source the agent builds from is newer than the
/// staged binary. The agent's in-workspace deps are `bm-core` and `bm-proto`;
/// third-party crates come from the registry lockfile, which a version bump
/// already invalidates through the rebuild it forces.
fn staged_is_fresh(bin: &std::path::Path, layout: &Layout) -> bool {
    staged_is_fresh_against(
        bin,
        &[
            "crates/bm-agent/src",
            "crates/bm-core/src",
            "crates/bm-proto/src",
        ],
        layout,
    )
}

/// The same question for a different set of sources — the sidecar is built
/// from its own crate, not the agent's, and measuring it against the agent's
/// dirs would call it stale on every unrelated edit.
fn staged_is_fresh_against(bin: &std::path::Path, dirs: &[&str], layout: &Layout) -> bool {
    let built = match std::fs::metadata(bin).and_then(|m| m.modified()) {
        Ok(t) => t,
        Err(_) => return false,
    };
    for dir in dirs {
        if sources_newer_than(&layout.root.join("rust").join(dir), built) {
            return false;
        }
    }
    true
}

fn sources_newer_than(dir: &std::path::Path, built: std::time::SystemTime) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return false;
    };
    for e in rd.filter_map(|e| e.ok()) {
        let p = e.path();
        if p.is_dir() {
            if sources_newer_than(&p, built) {
                return true;
            }
        } else if p.extension().and_then(|x| x.to_str()) == Some("rs")
            && std::fs::metadata(&p)
                .and_then(|m| m.modified())
                .map(|t| t > built)
                .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

/// Build the agent binary into the exact path a candidate names, so the
/// provision flow that asked for it can pick the file straight up. The target
/// triple is the candidate's grandparent directory (`…/target/<triple>/debug`) —
/// one spelling, one source. Output is captured: the caller's log gets the tail
/// on failure, and the dashboard never has cargo's progress spew landing
/// mid-redraw.
fn build_agent_binary(cand: &std::path::Path) -> anyhow::Result<()> {
    let target = cross_target_of(cand)?;
    let rust_dir = workspace_dir_above_target(cand)?;
    for tool in ["zig", "cargo-zigbuild"] {
        if tool_on_path(tool).is_none() {
            anyhow::bail!(
                "{tool} not found: the linux agent cross-build needs it (`cargo install cargo-zigbuild`; zig from `brew install zig` or https://ziglang.org/download)"
            );
        }
    }
    let mut cmd = std::process::Command::new(tool_on_path("cargo-zigbuild").expect("probed above"));
    // The rustup shim dir is missing from a non-login shell's PATH, and `zig cc`
    // is looked up by name from there — so the dir goes on the PATH of the
    // build, not just into the existence check.
    let shim_dir = tool_on_path("cargo-zigbuild")
        .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
        .expect("probed above");
    let out = cmd
        .args([
            "zigbuild",
            "--target",
            &target,
            "-p",
            "bm-agent",
            "--manifest-path",
        ])
        .arg(rust_dir.join("Cargo.toml"))
        .env("PATH", path_with_shim(shim_dir))
        .output()
        .map_err(|e| anyhow::anyhow!("running cargo-zigbuild: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail: String = stderr
            .lines()
            .filter(|l| !l.trim().is_empty())
            .rev()
            .take(12)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n  ");
        anyhow::bail!("cross build of bm-agent for {target} failed:\n  {tail}");
    }
    if !cand.is_file() {
        anyhow::bail!(
            "cross build reported success but {} is still missing",
            cand.display()
        )
    }
    Ok(())
}

/// This process's PATH with `dir` in front — the shape a build needs when the
/// linker lives in a rustup shim dir the environment forgot.
fn path_with_shim(dir: std::path::PathBuf) -> std::ffi::OsString {
    let old = std::env::var_os("PATH").unwrap_or_default();
    let mut dirs = vec![dir];
    dirs.extend(std::env::split_paths(&old));
    std::env::join_paths(dirs).unwrap_or(old)
}

/// Where a build tool actually is, looking in `~/.cargo/bin` as well as PATH:
/// a rustup shim dir is missing from a non-login shell's PATH — the same trap
/// the Makefile's CARGO fallback covers — and the dashboard is often launched
/// from somewhere that has no PATH at all. Returns the full path, because a
/// check that accepts a tool the exec cannot find is a check that lies.
fn tool_on_path(tool: &str) -> Option<std::path::PathBuf> {
    let dirs = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default();
    if let Some(found) = dirs.iter().map(|d| d.join(tool)).find(|c| c.is_file()) {
        return Some(found);
    }
    std::env::var("HOME")
        .ok()
        .map(|h| std::path::Path::new(&h).join(".cargo/bin").join(tool))
        .filter(|c| c.is_file())
}

/// The cross target a candidate names: its grandparent directory under
/// `target/` (`…/target/<triple>/debug/bm-agent`). A separate pure function so
/// the inference is testable without running a toolchain — caught live: the
/// first version took the *parent* and handed zigbuild `debug`.
fn cross_target_of(cand: &std::path::Path) -> anyhow::Result<String> {
    cand.parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("cannot infer target from {}", cand.display()))
}

/// Walk up from a candidate binary to the workspace directory — the candidate
/// lives under `<repo>/rust/target/<triple>/debug`, so `target`'s parent is
/// the dir holding `Cargo.toml`, wherever the layout root actually is.
fn workspace_dir_above_target(cand: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
    cand.ancestors()
        .find(|a| {
            a.file_name().is_some_and(|n| n == "target")
                && a.parent()
                    .is_some_and(|p| p.file_name().is_some_and(|n| n == "rust"))
        })
        .and_then(|a| a.parent())
        .map(|p| p.to_path_buf())
        .ok_or_else(|| anyhow::anyhow!("cannot locate the rust workspace above {}", cand.display()))
}

/// Cross builds first (they target older glibc and run anywhere), then the
/// native build when this machine *is* the target platform.
fn agent_candidates(os: &str, arch: &str, layout: &Layout) -> Vec<std::path::PathBuf> {
    let dir = layout.root.join("rust/target");
    let mut cands = Vec::new();
    match (os, arch) {
        ("linux", "x86_64") => cands.push(dir.join("x86_64-unknown-linux-gnu/debug/bm-agent")),
        ("linux", "aarch64") => cands.push(dir.join("aarch64-unknown-linux-gnu/debug/bm-agent")),
        _ => {}
    }
    if os == std::env::consts::OS && arch == std::env::consts::ARCH {
        cands.push(dir.join("debug/bm-agent"));
    }
    cands
}

/// Same order as the agent: cross first, native when this machine matches.
fn tts_candidates(os: &str, arch: &str, layout: &Layout) -> Vec<std::path::PathBuf> {
    let dir = layout.root.join("rust/target");
    let mut cands = Vec::new();
    match (os, arch) {
        ("linux", "x86_64") => cands.push(dir.join("x86_64-unknown-linux-gnu/release/bm-tts")),
        ("linux", "aarch64") => cands.push(dir.join("aarch64-unknown-linux-gnu/release/bm-tts")),
        _ => {}
    }
    if os == std::env::consts::OS && arch == std::env::consts::ARCH {
        cands.push(dir.join("release/bm-tts"));
    }
    cands
}

/// `workspace` — one directory per book. Creating switches to it; selecting
/// only moves the pointer, so data is never wiped and ledgers never mix
/// (the serve gate still refuses a ledger bound to another profile).
///
/// Returns the lines to show rather than printing them: the CLI prints, the
/// dashboard logs them into its event pane, and both are the same operation.
pub(crate) fn workspace_cmd(
    root: &std::path::Path,
    cmd: WorkspaceCmd,
) -> anyhow::Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    let dir = |name: &str| root.join("workspaces").join(name);
    let valid = |name: &str| {
        !name.is_empty()
            && name != "."
            && name != ".."
            && !name.contains('/')
            && !name.contains('\0')
    };
    match cmd {
        WorkspaceCmd::New { name } => {
            if !valid(&name) {
                anyhow::bail!("bad workspace name {name:?}");
            }
            if dir(&name).exists() {
                anyhow::bail!(
                    "workspace {name:?} already exists — `workspace use {name}` to select it"
                );
            }
            for d in ["data/chapters", "data/audio", "output"] {
                std::fs::create_dir_all(dir(&name).join(d))?;
            }
            // A workspace is born bound to the loaded profile, so its first
            // run cannot mix genres. No profile loaded yet is not an error —
            // the serve gate names it when it matters.
            let mut settings = Settings::default();
            match bm_core::profile::read_pointer(root) {
                Ok(p) => settings.profile = p,
                Err(_) => {
                    out.push(
                        "note: no profile loaded — `:profile` in the dashboard, or `tools/profile.sh fetch/unpack <name>`, first"
                            .into(),
                    )
                }
            }
            settings.save(&dir(&name).join("settings.json"))?;
            std::fs::create_dir_all(root.join(".bm"))?;
            std::fs::write(Layout::active_workspace_file(root), format!("{name}\n"))?;
            out.push(format!("workspace {name} created and selected"));
            Ok(out)
        }
        WorkspaceCmd::Use { name } => {
            if !dir(&name).is_dir() {
                anyhow::bail!("no workspace {name:?} under workspaces/");
            }
            std::fs::create_dir_all(root.join(".bm"))?;
            std::fs::write(Layout::active_workspace_file(root), format!("{name}\n"))?;
            out.push(format!("workspace {name} selected"));
            Ok(out)
        }
        WorkspaceCmd::List => {
            let active = std::fs::read_to_string(Layout::active_workspace_file(root))
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            let mut names: Vec<String> = std::fs::read_dir(root.join("workspaces"))
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default();
            names.sort();
            if names.is_empty() {
                out.push("no workspaces (this root is the implicit default)".into());
            }
            for n in names {
                out.push(format!("{} {n}", if n == active { "*" } else { " " }));
            }
            Ok(out)
        }
    }
}
/// The AWS pool: the app's IAM user, the definition, and what the account
/// holds.
///
/// Returns the lines to show, like [`workspace_cmd`] — the CLI prints them and
/// the dashboard could log the same operation. `policy` and `show` read no
/// network at all; `login` verifies against the account; `up`/`down` are the
/// two that spend money and destroy things.
fn aws_cmd(root: &std::path::Path, cmd: AwsCmd) -> anyhow::Result<Vec<String>> {
    use bm_core::provision::{
        describe_image_args, instance_line, parse_instances, run_instances_args, terminate_args,
        AwsConfig,
    };
    let path = root.join(".bm").join("aws.json");
    let mut out: Vec<String> = Vec::new();
    match cmd {
        AwsCmd::Init { force } => {
            out.push(seed_pool(root, &path, force)?);
            out.push(String::new());
            // Only two things have to be typed, and neither is a lookup: the
            // identity (console work, then `aws login`) and the region.
            // Everything else `discover` reads off the account.
            out.push("Then, in order:".into());
            out.push("  bm-inductor aws login --csv ~/Downloads/accessKeys.csv".into());
            out.push("  bm-inductor aws discover --region eu-central-1 \\".into());
            out.push("      --pem ~/Downloads/storycast.pem".into());
            out.push(String::new());
            out.push("`discover` resolves the AMI, the keypair, the instance profile and".into());
            out.push("the default subnet and security group, prints each one, and writes".into());
            out.push("them here — so nothing has to be looked up by hand and nothing is".into());
            out.push("re-resolved on the next launch. `aws show` lists whatever is still".into());
            out.push("missing.".into());
            out.push(String::new());
            out.push("Add these when the account cannot choose for you. Each is checked".into());
            out.push("before it is written and kept once set, so a typo costs a sentence".into());
            out.push("rather than a failed launch:".into());
            out.push("  --instance-profile <name>   the account holds more than one".into());
            out.push(
                "  --security-group <sg-…>     the default group is shared — use your own".into(),
            );
            out.push(
                "  --subnet <subnet-…>         the default subnet is one AZ of several".into(),
            );
            out.push(String::new());
            out.push("It also reads the chosen security group's inbound rules and says so".into());
            out.push("when none admits you: a closed port makes ssh HANG, it does not".into());
            out.push("refuse, so the symptom is silence.".into());
            out.push(String::new());
            out.push("`region` is the only thing you must decide. `bucket` is the only".into());
            out.push("other field worth typing (leave it empty to rsync the assets from".into());
            out.push("this machine).".into());
            out.push(String::new());
            // The identity is not "whatever you already have on this machine".
            // Naming the user here matters: `init` is the first command anyone
            // runs, and the old text told them to grant EC2 to their own
            // identity, which is the thing this replaces.
            out.push("The identity this runs as is an IAM user created for the app —".into());
            out.push("created entirely in the AWS console:".into());
            out.push(
                "  bm-inductor aws policy    # the policy, and the commands that create it".into(),
            );
            out.push("  step by step: docs/AWS-IAM-USER.md".into());
            out.push(String::new());
            // Console work, all of it — and named here rather than assumed,
            // because the console is where the account gets set up.
            out.push("In the console, alongside the user:".into());
            out.push("  the SSH keypair — EC2 → Key pairs → Create key pair, and keep".into());
            out.push("  the downloaded .pem; `aws discover --pem` puts it where the".into());
            out.push("  boxes expect it (.bm/aws/<region>.pem, 0600)".into());
            out.push("  a security group — EC2 → Security Groups → Create, SSH inbound".into());
            out.push("  only. The account's default group is shared with everything else".into());
            out.push("  in the default VPC, so a rule on it is a rule on all of that too.".into());
            out.push("  Name yours with `--security-group` (docs/AWS-IAM-USER.md step 6)".into());
            out.push("  a bucket — S3 → Create bucket, only if you publish the asset".into());
            out.push("  plane; leave `bucket` empty and every box rsyncs from here".into());
            out.push(String::new());
            out.push("Creating, tagging and terminating boxes is money and destruction — those are `aws up` / `aws down`, next.".into());
            Ok(out)
        }
        AwsCmd::Policy => {
            // The tracked document is the single source: `aws policy` prints it
            // and the guide says to install it with `--policy-document`, so
            // there is one policy rather than a JSON block in a doc and an
            // action list in the code that quietly disagree.
            let path = root.join(bm_core::provision::aws_credentials::POLICY_FILE);
            let text = std::fs::read_to_string(&path)
                .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
            // Refuse to print something that is not a policy: pasted into IAM
            // it fails with a message about the *document*, not about this.
            let doc: serde_json::Value = serde_json::from_str(&text)
                .map_err(|e| anyhow::anyhow!("{} is not valid JSON: {e}", path.display()))?;
            if doc.get("Statement").and_then(|s| s.as_array()).is_none() {
                anyhow::bail!(
                    "{} has no `Statement` array — it does not look like a policy document",
                    path.display()
                );
            }
            out.push(format!(
                "the policy for this app's IAM user — {} (tracked)",
                path.display()
            ));
            out.push(String::new());
            out.push(text.trim_end().to_string());
            out.push(String::new());
            out.push("Two placeholders to replace: <ACCOUNT_ID> and <WORKER_ROLE>".into());
            out.push("  (the role the boxes assume — `aws init` names the profile).".into());
            out.push(String::new());
            out.push("Create the user and attach it — as an account admin, once:".into());
            out.push("  aws iam create-user --user-name storycast-operator".into());
            out.push("  aws iam put-user-policy --user-name storycast-operator \\".into());
            out.push(
                "      --policy-name storycast-operator --policy-document file://<the file above, placeholders replaced>"
                    .into(),
            );
            out.push("  aws iam create-access-key --user-name storycast-operator   # secret is shown once".into());
            out.push(String::new());
            out.push("Then, as the operator:".into());
            out.push("  bm-inductor aws login --access-key-id <that key id>".into());
            out.push("  bm-inductor aws ls       # the check: reaches the API as that user".into());
            out.push(String::new());
            out.push("Full walkthrough, including the console path: docs/AWS-IAM-USER.md".into());
            Ok(out)
        }
        AwsCmd::Login { args } => {
            // Prompting is a terminal affordance and stays here: a CLI has a
            // hidden stdin, the dashboard does not. The verify-then-write order
            // lives in `aws_ops::login`, shared with the TUI, which hands over
            // the console's CSV instead of typing a secret.
            let aws_ops::LoginArgs { access_key_id, csv } = args;
            let (key_id, secret) = match csv {
                // Passed through when a CSV is present, so the shared check
                // refuses the two answers rather than silently preferring one.
                Some(_) => (access_key_id, None),
                None => {
                    let key_id = match access_key_id {
                        Some(k) => k,
                        None => ask("AWS access key id: ")?,
                    };
                    let secret = ask_secret("AWS secret access key (not echoed): ")?;
                    (Some(key_id), Some(secret))
                }
            };
            out.extend(aws_ops::login(root, csv, key_id, secret)?);
            Ok(out)
        }
        AwsCmd::Discover { args } => {
            out.extend(aws_ops::discover(root, args)?);
            Ok(out)
        }
        AwsCmd::Show => {
            let cfg = AwsConfig::load_layered(root);
            // Which identity is in force, named before anything else: "why
            // can't it launch" is usually the IAM user — missing, or not the
            // one the operator thinks — and the answer should not require
            // running a command that spends money.
            out.push(bm_core::provision::aws_credentials::source(root).describe(root));
            if !path.exists() {
                out.push(format!(
                    "no pool defined yet — `aws init` writes {}",
                    path.display()
                ));
            }
            out.push(cfg.summary());
            // The key file is named after the region, so there is nothing
            // meaningful to print before one is set — a path ending in `.pem`
            // with no name in it reads as a missing file rather than as a
            // missing region.
            if cfg.region.trim().is_empty() {
                out.push("private key: (no region yet — the file is named after it)".into());
            } else {
                let key = root.join(cfg.key_file());
                out.push(format!(
                    "private key: {} ({})",
                    key.display(),
                    if key.is_file() {
                        "present"
                    } else {
                        "MISSING — the box cannot be reached without it"
                    }
                ));
            }
            if let Ok(p) = bm_core::profile::read_pointer(root) {
                match bm_core::provision::profile_object(&cfg.bucket, &p.hash) {
                    Some(obj) => out.push(format!("asset plane: {obj}")),
                    None => out.push(
                        "asset plane: not published — every box would take a 668 MB upload from this machine".into(),
                    ),
                }
            }
            let missing = cfg.missing();
            if missing.is_empty() {
                // "Ready" means ready to *launch*, which is what `missing()`
                // knows about — and the firewall is deliberately not part of it,
                // because this command reads no network. Said out loud, because
                // a box behind a closed port launches perfectly and then does
                // nothing, and this is the line someone will read before
                // spending money.
                out.push(
                    "ready: nothing missing — the firewall is not checked here; `aws discover` checks it"
                        .into(),
                );
            } else {
                out.push(String::new());
                out.push(format!("{} thing(s) still to set:", missing.len()));
                for m in missing {
                    out.push(format!("  - {m}"));
                }
            }
            Ok(out)
        }
        AwsCmd::Ls => {
            out.extend(aws_ops::pool_lines(root)?);
            Ok(out)
        }
        AwsCmd::Up { count, dry_run } => {
            let cfg = AwsConfig::load_layered(root);
            let missing = cfg.missing();
            let hash = bm_core::profile::read_pointer(root)
                .map(|p| p.hash)
                .unwrap_or_default();
            if dry_run {
                // No call at all, and that is the point: a dry run has to work
                // *before* the account is set up, which is exactly when the
                // permissions are missing and when seeing the call matters
                // most. The two lookups the real path needs are shown rather
                // than performed.
                out.push("--dry-run: no call is made and nothing is launched.".into());
                out.push(format!(
                    "would run: aws {}",
                    run_instances_args(&cfg, count, "<the AMI's root device>", &hash).join(" ")
                ));
                out.push(format!(
                    "  after resolving it: aws {}",
                    describe_image_args(&cfg, cfg.image().unwrap_or("<AMI>")).join(" ")
                ));
                out.push(format!(
                    "  and counting what is live: aws ec2 describe-instances --filters Name=tag-key,Values={}",
                    cfg.tag_key
                ));
                if !missing.is_empty() {
                    out.push(String::new());
                    out.push(format!(
                        "{} thing(s) to set before the real run:",
                        missing.len()
                    ));
                    for m in missing {
                        out.push(format!("  - {m}"));
                    }
                }
                return Ok(out);
            }
            let (_, lines, _) = aws_ops::launch(root, count)?;
            out.extend(lines);
            Ok(out)
        }
        AwsCmd::Down { dry_run } => {
            let cfg = AwsConfig::load_layered(root);
            if cfg.region.trim().is_empty() {
                anyhow::bail!("no region set — `aws init`, then fill it in");
            }
            let json = aws_cli_instances(root, &cfg.region, &cfg.tag_key)?;
            let found = parse_instances(&json, &cfg.tag_key).ok_or_else(|| {
                anyhow::anyhow!(
                    "the aws CLI answered something unexpected — not terminating on a guess"
                )
            })?;
            let live: Vec<_> = found
                .iter()
                .filter(|i| matches!(i.state.as_str(), "pending" | "running" | "stopping"))
                .collect();
            if live.is_empty() {
                out.push(format!(
                    "nothing to terminate (no live box carries {})",
                    cfg.tag_key
                ));
                return Ok(out);
            }
            out.push(format!("{} box(es) carry {}:", live.len(), cfg.tag_key));
            for i in &live {
                out.push(format!("  {}", instance_line(i)));
            }
            let ids: Vec<String> = live.iter().map(|i| i.id.clone()).collect();
            if dry_run {
                out.push(String::new());
                out.push(format!(
                    "--dry-run: nothing terminated. Would run: aws {}",
                    terminate_args(&cfg.region, &ids).join(" ")
                ));
                return Ok(out);
            }
            out.extend(aws_ops::terminate(root, &ids)?);
            Ok(out)
        }
    }
}

/// Write `.bm/aws.json` from the tracked template, once.
///
/// Split out of `init` rather than duplicated, because `discover` may be the
/// first command anyone runs and must not be a second implementation of the
/// same seeding.
pub(crate) fn seed_pool(
    root: &std::path::Path,
    path: &std::path::Path,
    force: bool,
) -> anyhow::Result<String> {
    if path.exists() && !force {
        anyhow::bail!(
            "{} already exists — edit it, or pass --force to replace it",
            path.display()
        );
    }
    let template = root.join(bm_core::provision::DEFAULT_FILE);
    // Seed from the tracked template when it is there, so the file the operator
    // edits carries the `_note`s explaining each field rather than a bare
    // struct dump. The notes are ignored on read — serde drops what the struct
    // does not name — and the round trip proves a hand-edited template that has
    // drifted from the shape cannot seed a broken config.
    if template.is_file() {
        let text = std::fs::read_to_string(&template)?;
        serde_json::from_str::<bm_core::provision::AwsConfig>(&text)
            .map_err(|e| anyhow::anyhow!("{} does not parse: {e}", template.display()))?;
        bm_core::util::atomic_write(path, &text)?;
        Ok(format!(
            "wrote {} (from {})",
            path.display(),
            template.display()
        ))
    } else {
        bm_core::provision::AwsConfig::default().save(path)?;
        Ok(format!("wrote {}", path.display()))
    }
}

/// The local pool document, as raw JSON.
///
/// Refuses to start from an empty document when the file exists but does not
/// parse: quietly writing `{}` back over it would discard every field the
/// operator had already set, and the file is theirs.
pub(crate) fn read_pool_doc(path: &std::path::Path) -> anyhow::Result<serde_json::Value> {
    if !path.exists() {
        return Ok(serde_json::json!({}));
    }
    let text = std::fs::read_to_string(path)?;
    let doc: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("{} does not parse: {e}", path.display()))?;
    if !doc.is_object() {
        anyhow::bail!("{} is not a JSON object", path.display());
    }
    Ok(doc)
}

/// Set one key in a pool document, creating the objects on the way down.
pub(crate) fn set_json(doc: &mut serde_json::Value, path: &[&str], value: serde_json::Value) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut cur = doc;
    for key in parents {
        if !cur.is_object() {
            *cur = serde_json::json!({});
        }
        let Some(obj) = cur.as_object_mut() else {
            return;
        };
        cur = obj
            .entry((*key).to_string())
            .or_insert_with(|| serde_json::json!({}));
    }
    if !cur.is_object() {
        *cur = serde_json::json!({});
    }
    if let Some(obj) = cur.as_object_mut() {
        obj.insert((*last).to_string(), value);
    }
}

/// One `aws` call whose answer is a single value, or nothing.
///
/// `None` means "the call worked and there was nothing to find" — an account
/// with no default subnet, a region with no keypairs. For `discover` that is an
/// answer to report rather than a failure, which is why it is not
/// [`aws_cli_text`], where an empty answer is a refusal.
pub(crate) fn aws_cli_opt(
    root: &std::path::Path,
    args: &[String],
) -> anyhow::Result<Option<String>> {
    let text = aws_cli_raw(root, args)?.trim().to_string();
    if text.is_empty() || text == "None" {
        return Ok(None);
    }
    Ok(Some(text))
}

/// `aws ec2 describe-instances`, filtered to the boxes we tagged.
///
/// The filter is `tag-key`, not a value: it matches every box we started
/// whatever profile it was built for, and it cannot match a stranger's
/// instances.
pub(crate) fn aws_cli_instances(
    root: &std::path::Path,
    region: &str,
    tag_key: &str,
) -> anyhow::Result<String> {
    aws_cli_raw(
        root,
        &[
            "ec2".into(),
            "describe-instances".into(),
            "--region".into(),
            region.into(),
            "--filters".into(),
            format!("Name=tag-key,Values={tag_key}"),
            "--filters".into(),
            "Name=instance-state-name,Values=pending,running,stopping,stopped".into(),
            "--output".into(),
            "json".into(),
        ],
    )
}

/// Run one `aws` subcommand and hand back stdout, or an error naming the fix.
///
/// Shared by `ls`/`up`/`down` so the three cannot disagree about what a missing
/// CLI, missing credentials, or a policy that forbids the call means. Those are
/// all normal first-run states, so each gets a sentence naming the fix instead
/// of a raw exit status.
/// Ask for one visible line. Used for the key id, which is an identifier
/// rather than a secret and hiding it only makes a typo harder to spot.
fn ask(prompt: &str) -> anyhow::Result<String> {
    use std::io::{BufRead, Write};
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

/// Ask for one line without echoing it when stdin is a terminal.
///
/// `stty -echo` rather than an `rpassword` dependency — one call, Unix-only,
/// the same reasoning the cluster token uses for `/dev/urandom`. Piped input
/// is read plainly, which is what makes `printf '%s\n' "$SECRET" | …` work.
///
/// If `stty` is missing the echo is simply not suppressed; the value is still
/// read correctly, and nothing is written anywhere it should not be.
fn ask_secret(prompt: &str) -> anyhow::Result<String> {
    use std::io::{BufRead, IsTerminal, Write};
    let tty = std::io::stdin().is_terminal();
    print!("{prompt}");
    std::io::stdout().flush()?;
    if tty {
        let _ = std::process::Command::new("stty").arg("-echo").status();
    }
    let mut line = String::new();
    let read = std::io::stdin().lock().read_line(&mut line);
    if tty {
        let _ = std::process::Command::new("stty").arg("echo").status();
        println!();
    }
    read?;
    Ok(line.trim().to_string())
}

pub(crate) fn aws_cli_raw(root: &std::path::Path, args: &[String]) -> anyhow::Result<String> {
    // Every AWS call this tool makes goes through here, and this is the line
    // that makes "the app runs as the IAM user you created for it" true rather
    // than aspirational: no user stored, no call made. Never a fallback.
    bm_core::provision::aws_credentials::require(root)?;
    aws_cli_with(root, args, &[])
}

/// Run one `aws` subcommand with credentials supplied for this call alone.
///
/// Only `aws login` uses it, and it has to: the identity must be proven — and
/// proven to be an IAM *user* — before there is a file to point the CLI at.
///
/// The shadowing variables are stripped either way. Env-var keys outrank a
/// shared credentials file, so a stray `AWS_ACCESS_KEY_ID` exported in the
/// shell would otherwise win silently while `aws show` reported the IAM user.
pub(crate) fn aws_cli_with(
    root: &std::path::Path,
    args: &[String],
    supplied: &[(String, String)],
) -> anyhow::Result<String> {
    let mut cmd = std::process::Command::new("aws");
    cmd.args(args);
    for k in bm_core::provision::aws_credentials::SHADOWING_ENV {
        cmd.env_remove(k);
    }
    for (k, v) in bm_core::provision::aws_credentials::cli_env(root) {
        cmd.env(k, v);
    }
    for (k, v) in supplied {
        cmd.env(k, v);
    }
    let out = match cmd.output() {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => anyhow::bail!(
            "the `aws` CLI is not on PATH — install the AWS CLI v2, or run `aws show` for the definition alone"
        ),
        Err(e) => anyhow::bail!("could not run the aws CLI: {e}"),
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        // Three first-run states, three different fixes. The middle one is the
        // one that wastes an afternoon: the credentials *are* found, so nothing
        // says "credentials" — the call is simply not allowed, and the account
        // and user in the message are the only clue about which policy to edit.
        let hint = if err.contains("Unable to locate credentials") {
            " — the stored key was not accepted; re-run `bm-inductor aws login` (docs/AWS-IAM-USER.md)"
        } else if err.contains("UnauthorizedOperation") || err.contains("not authorized") {
            " — this IAM user is not allowed to make this call; `bm-inductor aws policy` prints the policy it needs"
        } else if err.contains("InvalidClientTokenId") || err.contains("ExpiredToken") {
            " — the stored key is stale or deleted; `bm-inductor aws login` with a fresh one"
        } else {
            ""
        };
        anyhow::bail!(
            "aws {} failed{hint}: {}",
            args.first().map(String::as_str).unwrap_or("?"),
            bm_core::util::head_chars(err.trim(), 300)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// One `--query`-driven value: a single line, trimmed. `None` and empty are
/// both "the call succeeded and answered nothing", which for a root device name
/// is a refusal rather than a default.
pub(crate) fn aws_cli_text(root: &std::path::Path, args: &[String]) -> anyhow::Result<String> {
    let text = aws_cli_raw(root, args)?.trim().to_string();
    if text.is_empty() || text == "None" {
        anyhow::bail!("aws {} answered nothing", args.join(" "));
    }
    Ok(text)
}

/// Rewrite interjections into engine tags via the live inductor API.
async fn cmd_retag(api: &str, dry_run: bool) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?;
    let res: bm_proto::OpResult = client
        .post(format!("{}/api/op", api.trim_end_matches('/')))
        .json(&bm_proto::OpRequest {
            op: bm_proto::Op::Retag,
            dry_run: Some(dry_run),
            ..Default::default()
        })
        .send()
        .await?
        .json()
        .await?;
    println!("{}", res.message);
    if !res.ok {
        anyhow::bail!("retag refused");
    }
    Ok(())
}

async fn cmd_provision(
    layout: Layout,
    addr: String,
    user: String,
    port: u16,
    key: Option<String>,
    api_port: u16,
    force: bool,
) -> anyhow::Result<()> {
    // The blocking SSH/rsync flow runs off the async runtime; registration
    // afterwards needs the live API client.
    let mut m = Machine::new(&addr, &user, port, key.clone(), "worker");
    m.tts_url = Some("http://127.0.0.1:8818".into());
    carry_task_policy(&mut m, &layout);
    let out = tokio::task::spawn_blocking({
        let (layout, addr, user) = (layout.clone(), addr.clone(), user.clone());
        move || provision_machine(&layout, &addr, &user, port, key, force, None)
    })
    .await?;
    for line in &out.lines {
        println!("{line}");
    }
    if !out.ready {
        println!("[{addr}] provision INCOMPLETE — fix the errors above and run it again");
    }
    // Register the machine so the scheduler sees it: prefer the live API,
    // fall back to merging the ledger file (safe only when no inductor runs —
    // the API attempt failing is exactly that signal).
    let api = format!("http://127.0.0.1:{api_port}/api/machines");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    match client.post(&api).json(&m).send().await {
        Ok(r) if r.status().is_success() => println!("[{}] registered with live inductor", addr),
        _ => {
            // No live inductor: merge into the ledger file (safe only when no
            // inductor runs — the API attempt failing is exactly that signal).
            // New shape is `machine_state` (runtime); a pre-migration file
            // still carrying the `machines` array gets both, so the boot
            // migration sees one coherent story.
            let path = layout.bm_state().join("ledger.json");
            let mut doc: serde_json::Value = std::fs::read_to_string(&path)
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or(serde_json::json!({"tasks": [], "machine_state": {}}));
            let (_, rt) = bm_core::provision::split_machine(&m, "");
            if let Some(st) = doc.get_mut("machine_state").and_then(|v| v.as_object_mut()) {
                st.insert(addr.clone(), serde_json::to_value(&rt)?);
            }
            if let Some(ms) = doc.get_mut("machines").and_then(|v| v.as_array_mut()) {
                ms.retain(|x| x.get("addr").and_then(|a| a.as_str()) != Some(addr.as_str()));
                ms.push(serde_json::to_value(&m)?);
            }
            std::fs::create_dir_all(layout.bm_state())?;
            bm_core::atomic_write(&path, &serde_json::to_string_pretty(&doc)?)?;
            // Config side: a provision is a bind, so the box lands in
            // machines.json under its stored name (or the address, first time).
            let boxes_path = layout.machines();
            let name = bm_core::provision::load_boxes(&boxes_path)
                .iter()
                .find(|b| b.addr == addr)
                .map(|b| b.name.clone())
                .unwrap_or_else(|| addr.clone());
            let (bxo, _) = bm_core::provision::split_machine(&m, &name);
            bm_core::provision::save_box(&boxes_path, &bxo)?;
            println!(
                "[{}] recorded in ledger file (no live inductor found)",
                addr
            );
        }
    }
    Ok(())
}

/// Add one clip to the sample pool, reporting what the filename suggested.
fn cmd_roster_add_sample(
    layout: &Layout,
    path: &std::path::Path,
    tags: Vec<String>,
    name: Option<String>,
) -> anyhow::Result<()> {
    let tags = if tags.is_empty() { None } else { Some(tags) };
    for line in bm_core::pool::add_sample(&layout.root, path, tags, name)? {
        println!("{line}");
    }
    Ok(())
}

/// Report a `migrate-cast` run: what changed, what could not, and where the
/// backup went.
fn cmd_roster_migrate_cast(layout: &Layout, dry_run: bool) -> anyhow::Result<()> {
    let runs = roster::migrate_cast(layout, dry_run)?;
    if runs.is_empty() {
        println!("no cast files under {}", layout.data().display());
        return Ok(());
    }
    for r in runs {
        let outcome = if r.written {
            ", rewritten"
        } else if dry_run {
            " [dry run]"
        } else if r.changed.is_empty() {
            ", already keyed"
        } else {
            ", unchanged"
        };
        println!(
            "{} ({}) — {} entries, {} keyed{}",
            r.path.display(),
            r.engine,
            r.entries,
            r.keyed,
            outcome
        );
        for (character, old, new) in &r.changed {
            println!("  {character}: {old} -> {new}");
        }
        // Never silent: an entry with no key is a clone, or a voice the
        // catalogue has dropped, and the operator is the one who can tell which.
        for line in &r.unmigratable {
            println!("  no catalogue key, left as a name: {line}");
        }
        if r.written {
            println!("  backup: {}", r.backup().display());
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // The dashboard and `workspace` are the management plane: they must open
    // on a root whose pointer is *stale*, because re-pointing is exactly how
    // that gets repaired. Everything else resolves first and refuses. The
    // problem is carried, not swallowed — the dashboard prints it, and
    // `workspace list` shows which names are still there.
    let manages = matches!(&cli.cmd, Cmd::Workspace { .. } | Cmd::Tui { .. });
    let (layout, pointer_problem) = match cli.root {
        Some(r) if manages => Layout::resolve_or_root(r),
        Some(r) => (Layout::resolve(r)?, None),
        None if manages => Layout::resolve_or_root(Layout::find_root()?),
        None => (Layout::discover()?, None),
    };
    bm_core::config::load_dotenv(&layout.root.join(".env"));
    let settings = Settings::load(&layout.settings());
    // `roster` and `workspace` are local file work: requiring ssh/rsync/ffmpeg
    // to rewrite JSON would make them unusable on exactly the machine that
    // needs them. Same for `digest`, which is one HTTP call to an analyzer
    // and touches no worker.
    if !matches!(
        &cli.cmd,
        Cmd::Roster { .. }
            | Cmd::Digest { .. }
            | Cmd::Backup { .. }
            | Cmd::Workspace { .. }
            | Cmd::Aws { .. }
    ) {
        check_bins()?;
    }
    match cli.cmd {
        Cmd::Serve {
            port,
            bind,
            start,
            count,
        } => cmd_serve(layout, settings, port, &bind, start, count).await,
        Cmd::Provision {
            r#box,
            addr,
            user,
            port,
            key,
            api_port,
            force,
        } => {
            // A linked box fills every flag it stored; explicit flags win for
            // the rest. Neither is an error until both are missing an address.
            let linked = r#box
                .as_deref()
                .map(|name| {
                    bm_core::provision::load_boxes(&layout.machines())
                        .into_iter()
                        .find(|b| b.name == name)
                        .ok_or_else(|| {
                            anyhow::anyhow!("no linked box {name:?} (see `link --help`)")
                        })
                })
                .transpose()?;
            let addr = addr
                .or_else(|| linked.as_ref().map(|b| b.addr.clone()))
                .ok_or_else(|| anyhow::anyhow!("provision needs --box or --addr"))?;
            // clap's defaults must not shadow a linked value: only an
            // explicitly passed flag wins over the box.
            let user = if user != "thang" {
                user
            } else {
                linked.as_ref().map(|b| b.user.clone()).unwrap_or(user)
            };
            let port = if port != 22 {
                port
            } else {
                linked.as_ref().map(|b| b.port).unwrap_or(port)
            };
            let key = key
                .or_else(|| linked.as_ref().and_then(|b| b.key.clone()))
                .or_else(|| settings.ssh.key.clone());
            cmd_provision(layout, addr, user, port, key, api_port, force).await
        }
        Cmd::Segments {
            from,
            collect,
            prune,
            dry_run,
        } => {
            // Separate paths: the report hashes every local byte (~100s on a
            // full store in debug builds), while prune only lists names.
            // Neither piggybacks on the other.
            if prune {
                segments::cmd_prune(&layout, &settings, !dry_run)
            } else {
                segments::cmd_segments(&layout, &settings, &from, collect, dry_run)
            }
        }
        Cmd::Retag { api, dry_run } => cmd_retag(&api, dry_run).await,
        Cmd::Digest {
            chapter,
            analyzer,
            write,
            json,
        } => {
            cmd_digest(
                &layout,
                &settings,
                chapter,
                analyzer.as_deref(),
                write,
                json,
            )
            .await
        }
        Cmd::Backup {
            start,
            through,
            analyzer,
            api,
            inductor,
            model,
            retries,
            dry_run,
        } => {
            cmd_backup(
                &layout,
                settings,
                BackupOpts {
                    start,
                    through,
                    analyzer,
                    model,
                    model_api: api,
                    inductor,
                    retries,
                    dry_run,
                },
            )
            .await
        }
        Cmd::Link {
            name,
            addr,
            user,
            port,
            key,
        } => {
            let bxo = bm_core::provision::LinkedBox {
                name: name.clone(),
                addr,
                user,
                port,
                key,
                role: "worker".into(),
                task_policy: None,
            };
            bm_core::provision::save_box(&layout.machines(), &bxo)?;
            println!("linked {name} -> {}", layout.machines().display());
            Ok(())
        }
        Cmd::Tui { api, once } => {
            // A stale pointer is why this dashboard opened on the root: say so
            // before the alternate screen takes the terminal, or the operator
            // sees the wrong book's panes with no explanation.
            if let Some(problem) = &pointer_problem {
                eprintln!("warning: {problem}");
            }
            if once {
                tui::snapshot(&api).await
            } else {
                tui::run(&api, layout).await
            }
        }
        Cmd::Roster { cmd } => match cmd {
            RosterCmd::MigrateCast { dry_run } => cmd_roster_migrate_cast(&layout, dry_run),
            RosterCmd::AddSample { path, tags, name } => {
                cmd_roster_add_sample(&layout, &path, tags, name)
            }
        },
        Cmd::Workspace { cmd } => {
            for line in workspace_cmd(&layout.root, cmd)? {
                println!("{line}");
            }
            Ok(())
        }
        Cmd::Aws { cmd } => {
            for line in aws_cmd(&layout.root, cmd)? {
                println!("{line}");
            }
            Ok(())
        }
        Cmd::Check { url, timeout } => cmd_check(settings.clone(), url, timeout).await,
    }
}

/// The link check, on a blocking thread.
///
/// **The crawl HTTP client cannot be built or dropped inside a tokio task**, and
/// this is the same constraint the provider works under: `reqwest::blocking`
/// owns a runtime of its own. `spawn_blocking` keeps it off the async worker and
/// keeps that runtime out of the way — without it the command panics on drop
/// before it prints anything.
async fn cmd_check(settings: Settings, url: String, timeout: u64) -> anyhow::Result<()> {
    tokio::task::spawn_blocking(move || cmd_check_blocking(&settings, &url, timeout))
        .await
        .map_err(|e| anyhow::anyhow!("the link check panicked: {e}"))?
}

fn cmd_check_blocking(settings: &Settings, url: &str, timeout: u64) -> anyhow::Result<()> {
    let opts = bm_core::crawl::probe::Options {
        // The workspace's own agent and headers, so a check goes out exactly as
        // a crawl would — including a session cookie that is part of the setup
        // being validated.
        user_agent: settings.crawl.user_agent.clone(),
        headers: settings.crawl.headers.clone(),
        timeout_secs: timeout,
    };
    let check = bm_core::crawl::probe::probe(url, &opts)?;

    println!("{}", check.url);
    if check.final_url != check.url {
        println!("  -> {}", check.final_url);
    }
    println!(
        "  HTTP {}  ·  {} bytes  ·  {} bytes of prose",
        check.status, check.bytes, check.text_bytes
    );
    if !check.guess.is_empty() {
        println!("  title: {}", check.guess);
    }
    let mark = if check.verdict.crawlable() {
        "ok"
    } else if check.retryable() {
        "blocked (retryable)"
    } else {
        "blocked"
    };
    println!("  {mark}: {}", check.detail);

    // A recognised site gets its crawler named, whether the check passed or not:
    // the person who is about to paste this URL into `settings.json` is the one
    // who needs to know which script to put there, and a check that says "ok"
    // without it has left the actual work undone.
    if let Some(site) = bm_core::crawl::for_url(url) {
        print!("{}", bm_core::crawl::known::note(site));
    }

    // A non-`Ok` verdict is a **failed check**, so a script can gate on it — but
    // the reason is always printed first, because "exit 1" on its own helps
    // nobody choose between a cookie, a browser user agent, and a different site.
    if !check.verdict.crawlable() {
        if check.verdict == bm_core::crawl::probe::Verdict::Cloudflare {
            // Say what is actually true, because the plausible-sounding wrong
            // answer here costs an afternoon of someone's time.
            println!(
                "\n  Cloudflare is refusing this client, and nothing in this repo\n  \
                 bypasses that: no TLS-fingerprint spoofing, no browser engine,\n  \
                 no challenge solver. The crawler speaks HTTP/1.1 with rustls and\n  \
                 a header-shaped request, and some sites refuse that on the\n  \
                 fingerprint alone.\n\n  \
                 Two things are worth trying, in this order:\n    \
                 1. a real browser user agent — the default \"Mozilla/5.0\" is\n       \
                 thin, and \"crawl\".\"user_agent\" is a one-line change;\n    \
                 2. a session cookie: open the page in a browser, solve the\n       \
                 challenge, copy the cf_clearance cookie into\n       \
                 \"crawl\".\"headers\", and run this command again to confirm it."
            );
        }
        std::process::exit(1);
    }
    Ok(())
}

/// `digest` — the analyzer's answer for one chapter, and nothing else.
///
/// Deliberately not routed through the control API: a request to a running
/// inductor is a request to the *pipeline*, and the whole point of this command
/// is to ask the question without waking it. It reads the chapter text and the
/// bible, calls [`bm_core::digest::analyze_chapter`] — the same function the
/// digest worker calls — and prints the result.
async fn cmd_digest(
    layout: &Layout,
    settings: &Settings,
    chapter: u32,
    analyzer: Option<&str>,
    write: bool,
    json: bool,
) -> anyhow::Result<()> {
    let analyzer = analyzer.unwrap_or(settings.analyzer.as_str()).to_string();
    let txt = layout.chapter_txt(chapter);
    if !txt.is_file() {
        anyhow::bail!(
            "no chapter text at {} — crawl it first (`serve`), or pass a chapter that exists",
            txt.display()
        );
    }
    let bible = bm_core::digest::load_bible(&layout.bible());
    let mut progress = |_f: f32, s: String| eprintln!("{s}");
    let out = bm_core::digest::analyze_chapter(
        layout,
        chapter,
        &bible,
        settings,
        &analyzer,
        &mut progress,
    )
    .await?;

    for line in &out.log {
        eprintln!("{line}");
    }
    for w in &out.warnings {
        eprintln!("WARN: {w}");
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&out.script)?);
    } else {
        let sounds = out
            .script
            .get("segments")
            .and_then(|s| s.as_array())
            .map(|segs| {
                segs.iter()
                    .filter(|i| bm_core::util::is_sound_item(i))
                    .count()
            })
            .unwrap_or(0);
        let lines = out.segments.saturating_sub(sounds);
        println!(
            "ch{chapter} via {analyzer}: {lines} lines, {sounds} sound items, {} warnings",
            out.warnings.len()
        );
    }
    if write {
        let path = layout.script(chapter);
        bm_core::atomic_write(&path, &serde_json::to_string_pretty(&out.script)?)?;
        eprintln!(
            "wrote {} — caches NOT invalidated and nothing requeued, so segments on disk \
             may no longer match this script; `serve` reconciles that when you next run it",
            path.display()
        );
    }
    Ok(())
}

/// The backup runner's options — bundled so one call carries the whole request.
struct BackupOpts {
    start: Option<u32>,
    through: Option<u32>,
    analyzer: Option<String>,
    model: Option<String>,
    /// The **model service's** base URL, not the inductor's.
    model_api: String,
    /// Where the accepted chapters are reported. Defaults to this machine.
    inductor: Option<String>,
    /// Re-asks per refused round. See `--retries`.
    retries: u32,
    dry_run: bool,
}

/// `backup` — be the digestor while the cluster's analyzer has no quota.
///
/// Chapters are digested **in order**, and the run stops at the first failure:
/// each chapter's bible delta lands on top of its predecessor's, so skipping
/// ahead would merge deltas out of order. Nothing is written here — the
/// inductor is the single writer of the bible and the script, and it does that
/// when the report lands.
async fn cmd_backup(
    layout: &Layout,
    mut settings: Settings,
    opts: BackupOpts,
) -> anyhow::Result<()> {
    let BackupOpts {
        start,
        through,
        analyzer,
        model,
        model_api,
        inductor,
        retries,
        dry_run,
    } = opts;
    // **The two addresses are different things, and the difference is the whole
    // point of this command's arguments.** `model_api` is where the two digest
    // calls go — a model service. The report goes to an *inductor*, which is
    // this machine's own control API: it is where the ledger and the bible
    // live, exactly as it is for `make tui`.
    let api = inductor.unwrap_or_else(|| format!("http://127.0.0.1:{}", settings.control_port));
    let model_api = model_api.trim_end_matches('/').to_string();
    let analyzer = match analyzer {
        Some(a) => a.to_string(),
        // The address says which service it is; the key says so when the
        // address does not say (a gateway at a name of its own). So the
        // operator passes an API, a key and a model, and never a transport.
        None if model_api.contains("openrouter") => "openrouter".to_string(),
        None if model_api.contains("googleapis") => "gemini".to_string(),
        None if std::env::var("OPENROUTER_API_KEY").is_ok_and(|k| k.starts_with("sk-or-")) => {
            "openrouter".to_string()
        }
        None if std::env::var("GEMINI_API_KEY").is_ok_and(|k| k.starts_with("AIza")) => {
            "gemini".to_string()
        }
        None => settings.analyzer.clone(),
    };
    // The model and the endpoint land on whichever fields the chosen service
    // reads, so one `--model` and one `--api` cover every backend.
    if let Some(model) = model.filter(|m| !m.trim().is_empty()) {
        match analyzer.as_str() {
            "openrouter" => settings.openrouter_model = model,
            "gemini" => settings.analyze_models = vec![model],
            "opencode" => settings.opencode_model = model,
            "local" => settings.local_model = model,
            other => anyhow::bail!("unknown analyzer {other:?}"),
        }
    }
    if analyzer == "openrouter" {
        settings.openrouter_url = model_api;
    }
    // Where a digest may begin is not a free choice: the deltas have to land in
    // chapter order, so the only legal start is the chapter after the last one
    // with a script on disk. Asking for anything else would merge this book's
    // bible out of order, which is why the guess is the default rather than a
    // number the operator has to remember.
    let start = match start {
        Some(n) => n,
        None => {
            let mut n = 1u32;
            while layout.digested(n) {
                n += 1;
            }
            n
        }
    };
    let last = match through {
        Some(t) => t,
        // To the end of the **book the ledger knows about** — the highest
        // chapter that has a digest row. Not "every chapter file on disk":
        // `data/chapters/` can hold text for a range this run never enqueued
        // (200 files against a 100-chapter ledger), and digesting those would
        // invent a book nobody asked for. A bare `backup` is "carry on with the
        // book", not "do one chapter and stop".
        None => {
            let from_ledger = bm_core::read_json::<serde_json::Value>(&layout.ledger())
                .ok()
                .and_then(|doc| {
                    doc.get("tasks")
                        .and_then(|t| t.as_array())
                        .and_then(|tasks| {
                            tasks
                                .iter()
                                .filter(|t| {
                                    t.get("stage").and_then(|s| s.as_str()) == Some("digest")
                                })
                                .filter_map(|t| t.get("chapter").and_then(|c| c.as_u64()))
                                .max()
                        })
                })
                .map(|max| max as u32);
            match from_ledger {
                Some(max) => max,
                None => {
                    let mut n = start;
                    while layout.chapter_txt(n + 1).is_file() {
                        n += 1;
                    }
                    n
                }
            }
        }
    };
    if last < start {
        anyhow::bail!("--through {last} is before ch{start}");
    }
    if !layout.chapter_txt(start).is_file() {
        anyhow::bail!(
            "ch{start} has no chapter text at {} — crawl it first",
            layout.chapter_txt(start).display()
        );
    }
    let endpoint = match analyzer.as_str() {
        "openrouter" => settings.openrouter_url.clone(),
        "local" => settings.ollama_url.clone(),
        "opencode" => format!("the {analyzer} CLI"),
        _ => "https://generativelanguage.googleapis.com".to_string(),
    };
    eprintln!(
        "backup digest: ch{start}..ch{last} via {analyzer} at {endpoint} → reporting to {api}"
    );
    let http = reqwest::Client::new();
    // The backend is a precondition, the same way it is for `make tui`: named
    // in a second, before the first chapter, so a missing inductor costs a
    // message instead of a whole range.
    manual::require_inductor(&api, &http)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    let digested = |n: u32| layout.digested(n);

    let mut done = 0u32;
    for n in start..=last {
        let mut next =
            manual::open(layout, n, &digested).map_err(|e| anyhow::anyhow!("ch{n}: {e}"))?;
        let mut cast: Option<serde_json::Value> = None;

        loop {
            let (round, prompt) = match &next {
                manual::Next::Prompt { round, text, .. } => (*round, text.clone()),
                manual::Next::Done(outcome) => {
                    if dry_run {
                        println!(
                            "ch{n}: {} segments (dry run, not reported)",
                            outcome.segments
                        );
                    } else {
                        let line = manual::report_or_stop(
                            &api,
                            &http,
                            n,
                            &outcome.script,
                            &outcome.delta,
                            format!("digest ch{n} by backup"),
                        )
                        .await
                        .map_err(|e| anyhow::anyhow!("ch{n}: {e}"))?;
                        println!("ch{n}: {line}");
                    }
                    done += 1;
                    break;
                }
            };
            // Round 1's validated cast rides on round 2's prompt; it is what
            // round 2 was rendered against and what its answer is checked
            // against, so it has to be carried forward.
            if let Some(c) = next.cast() {
                cast = Some(c.clone());
            }

            eprintln!(
                "ch{n}: {} prompt ready ({} bytes) via {analyzer}",
                round.as_str(),
                prompt.len()
            );
            // A refused round is re-asked with the validator's own words, and
            // the complaint is the instruction: each retry is a different ask
            // because the previous one is named in it. Bounded, because a model
            // that cannot satisfy the gate is a chapter to look at — but not
            // after a single refusal, which is what cost ch79.
            let mut accepted = None;
            let mut complaint = String::new();
            for attempt in 0..=retries {
                let asked = if attempt == 0 {
                    prompt.clone()
                } else {
                    eprintln!(
                        "ch{n}: {} answer refused ({complaint}) — repair {attempt}/{retries}",
                        round.as_str()
                    );
                    manual::repair_prompt(&prompt, &complaint)
                };
                let answer = manual::ask(&asked, &analyzer, &settings)
                    .await
                    .map_err(|e| anyhow::anyhow!("ch{n} round {}: {e}", round.as_str()))?;
                match manual::advance(layout, n, round, &answer, cast.as_ref()) {
                    Ok(step) => {
                        accepted = Some(step);
                        break;
                    }
                    Err(e) => {
                        complaint = e;
                        if attempt == retries {
                            anyhow::bail!(
                                "ch{n} round {} refused {attempts} time(s); last: {complaint}",
                                round.as_str(),
                                attempts = attempt + 1
                            );
                        }
                    }
                }
            }
            next = accepted.expect("the loop either lands or bails");
        }
    }
    if dry_run {
        println!("{done} chapter(s) digested, nothing reported (--dry-run)");
    } else {
        println!("{done} chapter(s) reported to {api}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_new_use_list_roundtrip() {
        let dir = std::env::temp_dir().join(format!("bm-workspace{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // Create switches to it, stamping the loaded profile (none here).
        workspace_cmd(
            &dir,
            WorkspaceCmd::New {
                name: "demo".into(),
            },
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(Layout::active_workspace_file(&dir)).unwrap(),
            "demo\n"
        );
        assert!(dir.join("workspaces/demo/settings.json").is_file());
        // Creating twice is an error, not a wipe.
        assert!(workspace_cmd(
            &dir,
            WorkspaceCmd::New {
                name: "demo".into()
            }
        )
        .is_err());
        // Selecting a missing workspace is an error, not a creation.
        assert!(workspace_cmd(
            &dir,
            WorkspaceCmd::Use {
                name: "gone".into()
            }
        )
        .is_err());
        workspace_cmd(
            &dir,
            WorkspaceCmd::Use {
                name: "demo".into(),
            },
        )
        .unwrap();
        let listed = workspace_cmd(&dir, WorkspaceCmd::List).unwrap();
        assert!(listed.iter().any(|l| l == "* demo"), "{listed:?}");
        // A loaded profile stamps new workspaces at creation.
        std::fs::create_dir_all(dir.join(".bm")).unwrap();
        std::fs::write(dir.join(".bm/profile"), r#"{"name":"xianxia","hash":"h1"}"#).unwrap();
        workspace_cmd(
            &dir,
            WorkspaceCmd::New {
                name: "second".into(),
            },
        )
        .unwrap();
        let settings: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join("workspaces/second/settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(settings["profile"]["name"], "xianxia");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn provisioning_localhost_syncs_nothing_and_reports_ready() {
        // The local worker runs in place from the repo, so there is no
        // mirror to fill — and crucially no readiness gate to fail: a
        // missing (or stale) `~/bm-worker` must never park localhost in
        // Error during `:B` catch-up.
        let dir = std::env::temp_dir().join(format!("bm-local-prov{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let layout = bm_core::Layout::new(&dir);
        for addr in ["127.0.0.1", "localhost", "::1"] {
            let out = provision_machine(&layout, addr, "thang", 22, None, false, None);
            assert!(out.ready, "{addr} must always be ready");
            assert!(out.reachable, "{addr} is local — ssh is never involved");
            assert_eq!(out.lines.len(), 1, "{:?}", out.lines);
            assert!(
                out.lines[0].contains("nothing to provision"),
                "{}",
                out.lines[0]
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reprovision_keeps_the_stored_work_policy() {
        // A fresh `Machine` carries `task_policy: None`, and both
        // registration paths persist it — so without the carry, every
        // re-provision silently reset the policy panel's order/toggles.
        let dir = std::env::temp_dir().join(format!("bm-policy{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let layout = bm_core::Layout::new(&dir);
        let policy = vec![
            bm_proto::TaskPref {
                stage: bm_proto::Stage::Merge,
                enabled: true,
            },
            bm_proto::TaskPref {
                stage: bm_proto::Stage::Digest,
                enabled: true,
            },
            bm_proto::TaskPref {
                stage: bm_proto::Stage::Crawl,
                enabled: true,
            },
            bm_proto::TaskPref {
                stage: bm_proto::Stage::Render,
                enabled: true,
            },
        ];
        bm_core::provision::save_box(
            &layout.machines(),
            &bm_core::provision::LinkedBox {
                name: "box-1".into(),
                addr: "192.0.2.1".into(),
                user: "fixture".into(),
                port: 2222,
                key: None,
                role: "worker".into(),
                task_policy: Some(policy.clone()),
            },
        )
        .unwrap();
        let mut m = Machine::new("192.0.2.1", "fixture", 2222, None, "worker");
        carry_task_policy(&mut m, &layout);
        assert_eq!(m.task_policy, Some(policy));
        // Unknown box: nothing to carry, stays default.
        let mut fresh = Machine::new("192.0.2.2", "fixture", 2222, None, "worker");
        carry_task_policy(&mut fresh, &layout);
        assert_eq!(fresh.task_policy, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn binary_routing_serves_each_platform_its_own_build() {
        // A fixture root holding one binary per platform: routing must pick
        // the file matching the probe, and refuse — not guess — the rest.
        let dir = std::env::temp_dir().join(format!("bm-routing{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let layout = bm_core::Layout::new(&dir);
        let touch = |rel: &str| {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"fake").unwrap();
            p
        };
        let cross_agent = touch("rust/target/x86_64-unknown-linux-gnu/debug/bm-agent");
        assert_eq!(
            agent_binary_for("linux", "x86_64", &layout).unwrap(),
            cross_agent
        );
        assert_eq!(
            tts_runtime_dir("linux", "x86_64", &layout).unwrap(),
            dir.join("rust/target/ort-linux-x64")
        );
        // Nothing staged for linux/arm64: a build error naming the platform,
        // never another platform's binary. (The full `agent_binary_for` would
        // go on to cross-build that target on demand — the staged lookup is
        // the pure, toolchain-free half that is testable here.)
        let err = agent_binary_staged("linux", "aarch64", &layout).unwrap_err();
        assert!(err.to_string().contains("linux/aarch64"), "{err}");
        // The on-demand build only ever targets cross candidates: the native
        // build exists exactly when this host is the target, so a miss there
        // means a broken workspace, not a missing cross toolchain.
        let native = layout.root.join("rust/target/debug/bm-agent");
        assert!(
            !buildable_agent_candidates(std::env::consts::OS, std::env::consts::ARCH, &layout)
                .contains(&native)
        );
        // A foreign platform always has cross candidates to build.
        let foreign_arch = if std::env::consts::ARCH == "x86_64" {
            "aarch64"
        } else {
            "x86_64"
        };
        assert!(
            !buildable_agent_candidates("linux", foreign_arch, &layout).is_empty(),
            "a foreign linux target must have cross candidates to build"
        );
        // This host's own platform falls back to the native build — asserted
        // on the candidate list (not the pick) so the test holds on any host:
        // on linux/x86_64 the cross file above would otherwise win first.
        let native = touch("rust/target/debug/bm-agent");
        assert_eq!(
            agent_candidates(std::env::consts::OS, std::env::consts::ARCH, &layout)
                .last()
                .unwrap(),
            &native
        );
        // The macOS sidecar is self-contained: no runtime travels with it.
        assert!(tts_runtime_dir("macos", "aarch64", &layout).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_sidecar_is_picked_from_disk_or_named_in_the_error() {
        let dir = std::env::temp_dir().join(format!("bm-tts-stage{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let layout = bm_core::Layout::new(&dir);
        // Nothing staged: the error has to name the platform and the way to
        // fix it, because that string is what the machine pane shows.
        let err = tts_binary_staged("linux", "x86_64", &layout).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("linux/x86_64"), "{msg}");
        assert!(msg.contains("make tts"), "{msg}");
        // Staged: picked, and only for the platform that built it.
        let cross = dir.join("rust/target/x86_64-unknown-linux-gnu/release/bm-tts");
        std::fs::create_dir_all(cross.parent().unwrap()).unwrap();
        std::fs::write(&cross, b"fake").unwrap();
        assert_eq!(
            tts_binary_staged("linux", "x86_64", &layout).unwrap(),
            cross
        );
        assert!(tts_binary_staged("linux", "aarch64", &layout).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_linux_x86_64_gets_an_on_demand_sidecar_build() {
        // The build stages its ONNX Runtime through `make runtime`, which
        // knows one target. Offering aarch64 here would link a binary against
        // the wrong library, and offering the native candidate would build a
        // sidecar whose runtime is never pushed to the box.
        let dir = std::env::temp_dir().join(format!("bm-tts-build{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let layout = bm_core::Layout::new(&dir);
        let cands = buildable_tts_candidates("linux", "x86_64", &layout);
        assert_eq!(
            cands,
            vec![dir.join("rust/target/x86_64-unknown-linux-gnu/release/bm-tts")]
        );
        for (os, arch) in [
            ("linux", "aarch64"),
            ("macos", "aarch64"),
            ("windows", "x86_64"),
        ] {
            assert!(
                buildable_tts_candidates(os, arch, &layout).is_empty(),
                "{os}/{arch} must not be offered a build this host cannot finish"
            );
        }
        // Same on a linux/x86_64 host: the native `target/release/bm-tts` is
        // never the candidate, so the built artifact is the one provisioning
        // pushes and the one `make tts` produces.
        assert!(!cands.contains(&dir.join("rust/target/release/bm-tts")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_old_sidecar_is_reported_but_never_silently_rebuilt() {
        // The agent rebuilds a stale staged binary because the version gate
        // would otherwise ship it forever. The sidecar has no such gate, and a
        // release cross-build costs minutes — so a stale one warns instead,
        // and only `make tts` spends the time.
        let dir = std::env::temp_dir().join(format!("bm-tts-stale{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let layout = bm_core::Layout::new(&dir);
        let src = dir.join("rust/crates/bm-tts/src/lib.rs");
        std::fs::create_dir_all(src.parent().unwrap()).unwrap();
        std::fs::write(&src, b"fn main() {}").unwrap();
        let bin = dir.join("rust/target/x86_64-unknown-linux-gnu/release/bm-tts");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(&bin, b"fake").unwrap();
        assert!(
            !tts_is_stale(&bin, &layout),
            "built after the source: fresh"
        );
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(&src, b"fn main() { /* newer */ }").unwrap();
        assert!(tts_is_stale(&bin, &layout), "older than the source: warn");
        // A source the sidecar does not build from is not a reason to call it
        // stale — the agent's dirs would otherwise do it on every edit.
        let _ = std::fs::remove_dir_all(src.parent().unwrap());
        std::fs::create_dir_all(dir.join("rust/crates/bm-agent/src")).unwrap();
        std::fs::write(
            dir.join("rust/crates/bm-agent/src/main.rs"),
            b"fn main() { /* newer */ }",
        )
        .unwrap();
        assert!(!tts_is_stale(&bin, &layout));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stopped_run_carries_why_it_stopped() {
        // The pane used to print a bare "provision INCOMPLETE" for every
        // pre-flight failure, because the reason was recovered by grepping
        // the log. It is carried instead, so the log and the pane can never
        // disagree.
        let mut log = bm_core::provision::LiveLog::new(None);
        let out = stopped(
            &mut log,
            "10.0.0.1",
            "no TTS sidecar binary for linux/x86_64",
            true,
        );
        assert!(!out.ready);
        assert!(out.reachable);
        assert_eq!(
            out.stop.as_deref(),
            Some("no TTS sidecar binary for linux/x86_64")
        );
        assert_eq!(out.lines.len(), 1);
        assert_eq!(
            out.lines[0],
            "[10.0.0.1] no TTS sidecar binary for linux/x86_64"
        );
    }

    #[test]
    fn a_staged_onnx_runtime_skips_the_make() {
        // The fixture root has no Makefile, so `make -C <root> runtime` can
        // only fail: an `Ok` here is proof the staged library short-circuits
        // the spawn, and an `Err` is proof the guard is really looking at
        // both names rather than trusting one.
        let dir = std::env::temp_dir().join(format!("bm-ort{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let layout = bm_core::Layout::new(&dir);
        let ort = dir.join("rust/target/ort-linux-x64");
        std::fs::create_dir_all(&ort).unwrap();
        std::fs::write(ort.join("libonnxruntime.so"), b"x").unwrap();
        assert!(
            stage_onnx_runtime(&layout, &ort).is_err(),
            "one of the two names is not a staged runtime"
        );
        std::fs::write(ort.join("libonnxruntime.so.1"), b"x").unwrap();
        assert!(stage_onnx_runtime(&layout, &ort).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_staged_agent_older_than_its_sources_rebuilds() {
        // 0.2.3 on disk while 0.2.4 is demanded: a stale staged file reads as
        // missing so the caller cross-builds instead of shipping it forever.
        let dir = std::env::temp_dir().join(format!("bm-staged-fresh{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let layout = bm_core::Layout::new(&dir);
        let src = dir.join("rust/crates/bm-agent/src/main.rs");
        std::fs::create_dir_all(src.parent().unwrap()).unwrap();
        let bin = dir.join("rust/target/x86_64-unknown-linux-gnu/debug/bm-agent");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, b"fake").unwrap();
        // The source must land strictly after the binary: one sleep so the
        // comparison never ties on a coarse clock.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(&src, b"fake newer").unwrap();
        assert!(
            !staged_is_fresh(&bin, &layout),
            "older-than-sources must rebuild"
        );
        assert!(agent_binary_staged("linux", "x86_64", &layout).is_err());
        // No sources at all (a bare fixture, like the routing test above)
        // reads as fresh — only newer sources veto.
        let _ = std::fs::remove_dir_all(dir.join("rust/crates"));
        assert!(staged_is_fresh(&bin, &layout));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cross_build_infers_target_and_workspace_from_the_candidate_path() {
        // The target triple is the candidate's grandparent (`…/<triple>/debug`)
        // and the workspace manifest sits a fixed number of levels above — both
        // read from the path, so there is one spelling of each and no drift.
        // Caught live: the first version took the parent and handed zigbuild
        // `debug`, which it rightly refused.
        let cand =
            std::path::Path::new("/repo/rust/target/x86_64-unknown-linux-gnu/debug/bm-agent");
        assert_eq!(cross_target_of(cand).unwrap(), "x86_64-unknown-linux-gnu");
        assert_eq!(
            workspace_dir_above_target(cand).unwrap(),
            std::path::Path::new("/repo/rust")
        );
    }
}
