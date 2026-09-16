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
#[command(name = "bm-inductor", about = "Cluster orchestrator for the novel pipeline")]
struct Cli {
    /// Repo root (discovered via prompts/analyze.txt when omitted).
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
    /// Link a machine by name: remembers how to reach it so `provision --box`
    /// needs no flags. Writes `.bm/machines.json`, which is ignored.
    Link {        /// Short handle, e.g. `box-1`.
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
    let binary = match agent_binary_for(pre.arch.as_str(), layout) {
        Ok(b) => b,
        Err(e) => {
            log.push(format!("[{addr}] {e}"));
            return (false, log);
        }
    };
    log.push(format!("[{addr}] agent binary: {}", binary.display()));
    let mut m = Machine::new(addr, user, port, key, "worker");
    m.tts_url = Some("http://127.0.0.1:8818".into());
    let (after, mut flow) =
        provision(&m, &layout.root, &binary, env!("CARGO_PKG_VERSION"), force, Some(pre));
    log.append(&mut flow);
    (after.configured(env!("CARGO_PKG_VERSION")), log)
}

fn check_bins() -> anyhow::Result<()> {
    for bin in ["ssh", "rsync", "ffmpeg"] {
        let found = std::env::var_os("PATH").map(|paths| {
            std::env::split_paths(&paths).any(|d| d.join(bin).is_file())
        }).unwrap_or(false);
        if !found {
            anyhow::bail!("{bin} not found on PATH: provisioning needs ssh/rsync, merging needs ffmpeg");
        }
    }
    Ok(())
}

async fn cmd_serve(layout: Layout, settings: Settings, port: u16, bind: &str, start: u32, count: u32) -> anyhow::Result<()> {
    let mut inner = state::Inner::new(layout, settings);
    inner.load_ledger();
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
    axum::serve(tokio::net::TcpListener::bind(&addr).await?, app.into_make_service()).await?;
    Ok(())
}

/// Pick the agent binary matching the target arch. Cross builds live next to
/// the native one; a missing cross binary is a build error, not a guess.
fn agent_binary_for(arch: &str, layout: &Layout) -> anyhow::Result<std::path::PathBuf> {
    let dir = layout.root.join("rust/target");
    let cand = if arch == "x86_64" {
        dir.join("x86_64-unknown-linux-gnu/debug/bm-agent")
    } else {
        dir.join("debug/bm-agent")
    };
    if cand.is_file() {
        return Ok(cand);
    }
    anyhow::bail!(
        "no agent binary for arch {arch} at {} (build it first)",
        cand.display()
    )
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
            println!("[{}] recorded in ledger file (no live inductor found)", addr);
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
    let layout = match cli.root {
        Some(r) => Layout::new(r),
        None => Layout::discover()?,
    };
    bm_core::config::load_dotenv(&layout.root.join(".env"));
    let settings = Settings::load(&layout.settings());
    // `roster` is local JSON work: requiring ssh/rsync/ffmpeg to rewrite a cast
    // file would make it unusable on exactly the machine that needs it.
    if !matches!(&cli.cmd, Cmd::Roster { .. }) {
        check_bins()?;
    }
    match cli.cmd {
        Cmd::Serve { port, bind, start, count } => {
            cmd_serve(layout, settings, port, &bind, start, count).await
        }
        Cmd::Provision { r#box, addr, user, port, key, api_port, force } => {
            // A linked box fills every flag it stored; explicit flags win for
            // the rest. Neither is an error until both are missing an address.
            let linked = r#box
                .as_deref()
                .map(|name| {
                    bm_core::provision::load_boxes(&layout.machines())
                        .into_iter()
                        .find(|b| b.name == name)
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "no linked box {name:?} (see `link --help`)"
                            )
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
        Cmd::Segments { from, collect, prune, dry_run } => {
            // Separate paths: the report hashes every local byte (~100s on a
            // full store in debug builds), while prune only lists names.
            // Neither piggybacks on the other.
            if prune {
                segments::cmd_prune(&layout, &settings, !dry_run)
            } else {
                segments::cmd_segments(&layout, &settings, &from, collect, dry_run)
            }
        }
        Cmd::Link { name, addr, user, port, key } => {            let bxo = bm_core::provision::LinkedBox {
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
            if once {
                tui::snapshot(&api).await
            } else {
                tui::run(&api, layout).await
            }
        }
        Cmd::Roster { cmd } => match cmd {
            RosterCmd::MigrateCast { dry_run } => cmd_roster_migrate_cast(&layout, dry_run),
            RosterCmd::AddSample { path, tags, name } => cmd_roster_add_sample(&layout, &path, tags, name),
        },
    }
}
