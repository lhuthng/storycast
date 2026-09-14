//! Inductor: control API + scheduler. Workers report facts; this decides.

mod api;
mod state;

use bm_core::{config::Settings, Layout};
use clap::Parser;

#[derive(Parser)]
#[command(name = "bm-inductor", about = "Cluster orchestrator for the novel pipeline")]
struct Cli {
    /// Repo root (discovered via prompts/analyze.txt when omitted).
    #[arg(long)]
    root: Option<std::path::PathBuf>,
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
    for bin in ["ssh", "rsync", "ffmpeg"] {
        let found = std::env::var_os("PATH").map(|paths| {
            std::env::split_paths(&paths).any(|d| d.join(bin).is_file())
        }).unwrap_or(false);
        if !found {
            anyhow::bail!("{bin} not found on PATH: provisioning needs ssh/rsync, merging needs ffmpeg");
        }
    }
    let mut inner = state::Inner::new(layout, settings);
    inner.load_ledger();
    inner.reconcile(cli.start, cli.count);
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
    let addr = format!("{}:{}", cli.bind, cli.port);
    println!("inductor on http://{addr}");
    axum::serve(tokio::net::TcpListener::bind(&addr).await?, app.into_make_service()).await?;
    Ok(())
}
