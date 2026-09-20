//! Inductor: control API + scheduler. Workers report facts; this decides.

mod api;
mod backend;
mod roster;
mod segments;
mod state;
mod tui;

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
    /// AWS worker pool: the definition written once in `.bm/aws.json`, and what
    /// the account actually holds. Local only — starts nothing.
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

/// Blocking provision run shared by the CLI and the TUI background task.
/// Returns `(ready, lines)`: `ready` is the post-provision probe saying the
/// box runs this exact agent build with TTS python — the gate a start hides
/// behind. Soft failures (voice enrolment, opencode check) only ride the log.
pub fn provision_machine(
    layout: &Layout,
    addr: &str,
    user: &str,
    port: u16,
    key: Option<String>,
    force: bool,
) -> (bool, Vec<String>) {
    if bm_core::is_local_node(addr) {
        // No mirror to fill: the local worker runs in place from this repo —
        // prompts, assets, models and binaries are read where they stand, so
        // syncing a `~/bm-worker` copy would only spend disk and let a stale
        // copy fail the readiness gate below. Launching the worker stays the
        // caller's job (`:B` catch-up, `make agent`).
        return (
            true,
            vec![format!(
                "[{addr}] local machine — runs from the repo, nothing to provision"
            )],
        );
    }
    use bm_core::provision::{provision, Ssh};
    let mut log = Vec::new();
    let probe_ssh = Ssh {
        target: format!("{user}@{addr}"),
        port,
        key: key.clone(),
        local: bm_core::is_local_node(addr),
    };
    let pre = probe_ssh.probe();
    log.push(format!("[{addr}] {}", pre.summary()));
    let pointer = match bm_core::profile::read_pointer(&layout.root) {
        Ok(p) => p,
        Err(_) => {
            log.push(format!(
                "[{addr}] no local profile loaded — `profile.sh fetch/unpack` first (workers verify it at startup)"
            ));
            return (false, log);
        }
    };
    log.push(format!(
        "[{addr}] profile: {} ({})",
        pointer.name,
        &pointer.hash[..12.min(pointer.hash.len())]
    ));
    let binary = match agent_binary_for(pre.os.as_str(), pre.arch.as_str(), layout) {
        Ok(b) => b,
        Err(e) => {
            log.push(format!("[{addr}] {e}"));
            return (false, log);
        }
    };
    log.push(format!("[{addr}] agent binary: {}", binary.display()));
    let tts = match tts_binary_for(pre.os.as_str(), pre.arch.as_str(), layout) {
        Ok(b) => b,
        Err(e) => {
            log.push(format!("[{addr}] {e}"));
            return (false, log);
        }
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
    );
    log.append(&mut flow);
    (after.configured(env!("CARGO_PKG_VERSION")), log)
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
    let mut inner = state::Inner::new(layout, settings);
    // No profile, no run: the live tree is ignored and may be absent or
    // drifted — refuse before touching the ledger, naming the fix.
    let pointer = bm_core::profile::verify(&inner.layout.root)?;
    println!(
        "profile {} ({})",
        pointer.name,
        &pointer.hash[..12.min(pointer.hash.len())]
    );
    inner.load_ledger();
    inner.check_profile()?;
    inner.reconcile(start, count);
    let shared = std::sync::Arc::new(tokio::sync::Mutex::new(inner));
    // Lease reaper: expired leases return to the pool, no strike.
    let reaper = shared.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            let expired = reaper.lock().await.reap();
            for id in expired {
                println!("[inductor] lease expired, requeued {id} (no strike)");
            }
        }
    });
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
/// Pick the TTS sidecar binary for the target platform.
///
/// Unlike [`agent_binary_for`], this one is a **release** build: bm-tts's hot
/// loop is a hand-written SIMD matvec, and a debug build would give all of that
/// back. Missing is a build error, not a guess — `make tts` produces the
/// linux/x86_64 one.
fn tts_binary_for(os: &str, arch: &str, layout: &Layout) -> anyhow::Result<std::path::PathBuf> {
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
    for cand in agent_candidates(os, arch, layout) {
        if cand.is_file() {
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
                    out.push("note: no profile loaded — `profile.sh fetch/unpack` first".into())
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
/// The AWS pool: the definition, and what the account holds.
///
/// Returns the lines to show, like [`workspace_cmd`] — the CLI prints them and
/// the dashboard could log the same operation. Nothing here starts, stops or
/// terminates anything: `up`/`down` are the commands that spend money, and they
/// are not written yet.
fn aws_cmd(root: &std::path::Path, cmd: AwsCmd) -> anyhow::Result<Vec<String>> {
    use bm_core::provision::{
        describe_image_args, instance_line, parse_instances, run_instances_args, terminate_args,
        AwsConfig,
    };
    let path = root.join(".bm").join("aws.json");
    let template = root.join(bm_core::provision::DEFAULT_FILE);
    let mut out: Vec<String> = Vec::new();
    match cmd {
        AwsCmd::Init { force } => {
            if path.exists() && !force {
                anyhow::bail!(
                    "{} already exists — edit it, or pass --force to replace it",
                    path.display()
                );
            }
            // Seed from the tracked template when it is there, so the file the
            // operator edits carries the `_note`s explaining each field rather
            // than a bare struct dump. The notes are ignored on read — serde
            // drops what the struct does not name — and the round trip proves
            // a hand-edited template that has drifted from the shape cannot
            // seed a broken config.
            if template.is_file() {
                let text = std::fs::read_to_string(&template)?;
                serde_json::from_str::<AwsConfig>(&text)
                    .map_err(|e| anyhow::anyhow!("{} does not parse: {e}", template.display()))?;
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&path, text)?;
                out.push(format!(
                    "wrote {} (from {})",
                    path.display(),
                    template.display()
                ));
            } else {
                AwsConfig::default().save(&path)?;
                out.push(format!("wrote {}", path.display()));
            }
            out.push(String::new());
            out.push("Fill in these, then `aws show` lists what is still missing:".into());
            out.push("  region, subnet_id, security_group_id, iam_instance_profile".into());
            out.push("  keypairs.<region>  — the EC2 keypair NAME, per region".into());
            out.push("  bucket             — leave empty to rsync the assets from here".into());
            out.push(String::new());
            out.push("One-off setup this tool deliberately does not do for you:".into());
            out.push("  aws ec2 create-key-pair --key-name <name> --query KeyMaterial --output text > .bm/aws/<region>.pem".into());
            out.push("  chmod 600 .bm/aws/<region>.pem".into());
            out.push("  aws s3 mb s3://<bucket> --region <region>   # if publishing assets".into());
            out.push("  aws iam create-instance-profile --instance-profile-name <name>   # + a role that can read the bucket".into());
            out.push(String::new());
            out.push("The identity you run this as needs EC2 and nothing else:".into());
            out.push(
                "  ec2:DescribeInstances, ec2:RunInstances, ec2:TerminateInstances, ec2:CreateTags"
                    .into(),
            );
            out.push("  ec2:DescribeSubnets, ec2:DescribeSecurityGroups, ec2:DescribeImages, ec2:DescribeKeyPairs".into());
            out.push("  iam:PassRole  (only on the instance profile above)".into());
            out.push(
                "Nothing account-wide, no billing, no S3 write unless you publish assets yourself."
                    .into(),
            );
            out.push(String::new());
            out.push("Creating, tagging and terminating boxes is money and destruction — those are `aws up` / `aws down`, next.".into());
            Ok(out)
        }
        AwsCmd::Show => {
            let cfg = AwsConfig::load_layered(root);
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
                out.push("ready: nothing missing".into());
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
            let cfg = AwsConfig::load_layered(root);
            if cfg.region.trim().is_empty() {
                anyhow::bail!("no region set — `aws init`, then fill it in");
            }
            out.push(format!(
                "{} · tag {} · spot={}",
                cfg.region, cfg.tag_key, cfg.spot
            ));
            let json = aws_cli_instances(&cfg.region, &cfg.tag_key)?;
            let Some(instances) = parse_instances(&json, &cfg.tag_key) else {
                anyhow::bail!(
                    "the aws CLI answered something this does not understand — reporting an empty account here would be the one wrong answer that costs money"
                );
            };
            if instances.is_empty() {
                out.push("no boxes running (nothing carries this tag)".into());
            }
            for i in &instances {
                out.push(instance_line(i));
            }
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
            if !missing.is_empty() {
                let mut msg = String::from("the pool is not ready to launch:");
                for m in missing {
                    msg.push_str(&format!("\n  - {m}"));
                }
                anyhow::bail!("{msg}");
            }
            // The marker tag's value *is* the profile hash, so without one the
            // box would be untraceable in `ls` and unprovisionable anyway —
            // `provision` refuses without a loaded profile. Better to say so
            // here than to rent a box that cannot be used.
            if hash.is_empty() {
                anyhow::bail!(
                    "no profile loaded — the marker tag records which profile a box was built for, and provisioning refuses without one; `profile.sh fetch/unpack <name>` first"
                );
            }
            // The cap is checked against the total, not against this call: a
            // cap that only counts what one invocation asked for is not a cap.
            let json = aws_cli_instances(&cfg.region, &cfg.tag_key)?;
            let live = parse_instances(&json, &cfg.tag_key)
                .ok_or_else(|| anyhow::anyhow!("the aws CLI answered something unexpected"))?
                .iter()
                .filter(|i| matches!(i.state.as_str(), "pending" | "running" | "stopping"))
                .count() as u32;
            if live + count > cfg.max_workers {
                anyhow::bail!(
                    "{live} box(es) already live and {count} asked for, over max_workers={} — raise it in `.bm/aws.json` or launch fewer",
                    cfg.max_workers
                );
            }
            // The mapping must name the image's own root device (`/dev/sda1` on
            // Ubuntu, `/dev/xvda` on Amazon Linux), so it is resolved rather
            // than assumed — otherwise `disk_gb` is silently ignored.
            let image = cfg.image().unwrap_or_default().to_string();
            let root_device = aws_cli_text(&describe_image_args(&cfg, &image))?;
            let argv = run_instances_args(&cfg, count, root_device.trim(), &hash);
            out.push(format!(
                "{live} live, cap {}, launching {count} tagged {}",
                cfg.max_workers, cfg.tag_key
            ));
            out.push(format!("aws {}", argv.join(" ")));
            let raw = aws_cli_raw(&argv)?;
            let launched = parse_instances(&raw, &cfg.tag_key).unwrap_or_default();
            if launched.is_empty() {
                out.push("the launch answered without any instances — check the account".into());
            }
            for i in &launched {
                out.push(format!("launched {}", instance_line(i)));
            }
            out.push(String::new());
            out.push(
                "Next: `provision --addr <ip>` each one, then `:B` to start their workers.".into(),
            );
            Ok(out)
        }
        AwsCmd::Down { dry_run } => {
            let cfg = AwsConfig::load_layered(root);
            if cfg.region.trim().is_empty() {
                anyhow::bail!("no region set — `aws init`, then fill it in");
            }
            let json = aws_cli_instances(&cfg.region, &cfg.tag_key)?;
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
            let raw = aws_cli_raw(&terminate_args(&cfg.region, &ids))?;
            for i in parse_instances(&raw, &cfg.tag_key).unwrap_or_default() {
                out.push(format!("terminating {} ({})", i.id, i.state));
            }
            Ok(out)
        }
    }
}

/// `aws ec2 describe-instances`, filtered to the boxes we tagged.
///
/// The filter is `tag-key`, not a value: it matches every box we started
/// whatever profile it was built for, and it cannot match a stranger's
/// instances.
fn aws_cli_instances(region: &str, tag_key: &str) -> anyhow::Result<String> {
    aws_cli_raw(&[
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
    ])
}

/// Run one `aws` subcommand and hand back stdout, or an error naming the fix.
///
/// Shared by `ls`/`up`/`down` so the three cannot disagree about what a missing
/// CLI, missing credentials, or a policy that forbids the call means. Those are
/// all normal first-run states, so each gets a sentence naming the fix instead
/// of a raw exit status.
fn aws_cli_raw(args: &[String]) -> anyhow::Result<String> {
    let out = match std::process::Command::new("aws").args(args).output() {
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
            " — no credentials in the standard chain; `aws configure sso`, or set AWS_PROFILE"
        } else if err.contains("UnauthorizedOperation") || err.contains("not authorized") {
            " — the credentials were found but the identity may not call this; `aws init` lists the actions the pool needs"
        } else if err.contains("InvalidClientTokenId") || err.contains("ExpiredToken") {
            " — the credentials are stale; refresh them (`aws sso login`)"
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
fn aws_cli_text(args: &[String]) -> anyhow::Result<String> {
    let text = aws_cli_raw(args)?.trim().to_string();
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
    let log = tokio::task::spawn_blocking({
        let (layout, addr, user) = (layout.clone(), addr.clone(), user.clone());
        move || provision_machine(&layout, &addr, &user, port, key, force)
    })
    .await?;
    let (ready, lines) = log;
    for line in &lines {
        println!("{line}");
    }
    if !ready {
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
        Cmd::Roster { .. } | Cmd::Digest { .. } | Cmd::Workspace { .. } | Cmd::Aws { .. }
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
    }
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
            let (ready, lines) = provision_machine(&layout, addr, "thang", 22, None, false);
            assert!(ready, "{addr} must always be ready");
            assert_eq!(lines.len(), 1, "{lines:?}");
            assert!(lines[0].contains("nothing to provision"), "{}", lines[0]);
        }
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
        // never another platform's binary.
        let err = agent_binary_for("linux", "aarch64", &layout).unwrap_err();
        assert!(err.to_string().contains("linux/aarch64"), "{err}");
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
}
