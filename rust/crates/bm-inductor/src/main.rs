//! Inductor: control API + scheduler. Workers report facts; this decides.

mod api;
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
        #[arg(long, default_value = "21")]
        start: u32,
        #[arg(long, default_value = "80")]
        count: u32,
    },
    /// Onboard one machine by address: probe, push what's missing, verify.
    Provision {
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
    },
}

/// Blocking provision run shared by the CLI and the TUI background task.
/// Returns the log lines for display.
pub fn provision_machine(
    layout: &Layout,
    addr: &str,
    user: &str,
    port: u16,
    key: Option<String>,
    force: bool,
) -> Vec<String> {
    use bm_core::provision::{provision, Ssh};
    let mut log = Vec::new();
    let probe_ssh = Ssh {
        target: format!("{user}@{addr}"),
        port,
        key: key.clone(),
        local: matches!(addr, "127.0.0.1" | "localhost" | "::1"),
    };
    let pre = probe_ssh.probe();
    log.push(format!("[{addr}] {}", pre.summary()));
    let binary = match agent_binary_for(pre.arch.as_str(), layout) {
        Ok(b) => b,
        Err(e) => {
            log.push(format!("[{addr}] {e}"));
            return log;
        }
    };
    log.push(format!("[{addr}] agent binary: {}", binary.display()));
    let mut m = Machine::new(addr, user, port, key, "worker");
    m.tts_url = Some("http://127.0.0.1:8818".into());
    let (_after, mut flow) =
        provision(&m, &layout.root, &binary, env!("CARGO_PKG_VERSION"), force);
    log.append(&mut flow);
    log
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
    for line in &log {
        println!("{line}");
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
            let path = layout.bm_state().join("ledger.json");
            let mut doc: serde_json::Value = std::fs::read_to_string(&path)
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or(serde_json::json!({"tasks": [], "machines": []}));
            if let Some(ms) = doc.get_mut("machines").and_then(|v| v.as_array_mut()) {
                ms.retain(|x| x.get("addr").and_then(|a| a.as_str()) != Some(addr.as_str()));
                ms.push(serde_json::to_value(&m)?);
            }
            std::fs::create_dir_all(layout.bm_state())?;
            bm_core::atomic_write(&path, &serde_json::to_string_pretty(&doc)?)?;
            println!("[{}] recorded in ledger file (no live inductor found)", addr);
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
    check_bins()?;
    match cli.cmd {
        Cmd::Serve { port, bind, start, count } => {
            cmd_serve(layout, settings, port, &bind, start, count).await
        }
        Cmd::Provision { addr, user, port, key, api_port, force } => {
            cmd_provision(layout, addr, user, port, key, api_port, force).await
        }
        Cmd::Tui { api } => tui::run(&api, layout).await,
    }
}
