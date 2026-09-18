//! `bm-tts` — the VieNeu-TTS v3 Turbo sidecar, without Python.
//!
//!     bm-tts --models <dir> --codec <dir> --dict <sea_g2p.bin> --voices <json>
//!            [--port 8818] [--bind 0.0.0.0] [--threads N]
//!
//! Same six endpoints on the same port as `python/tts_server.py`, so nothing
//! downstream changes: `bm-agent` is a thin HTTP client that never loads a
//! model, and it keeps talking to `http://127.0.0.1:8818`.
//!
//! Everything is loaded before the listener opens, for the same reason the
//! reference calls `vn.engine()` before `serve_forever()`: a health check that
//! answers before the model is ready makes a cold start look like a fast one.

use anyhow::{Context, Result};
use bm_tts::server::{router, Server};
use bm_tts::synth::Synth;
use bm_tts::text::FrontEnd;
use bm_tts::voice::Roster;
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "bm-tts",
    version,
    about = "VieNeu-TTS v3 Turbo sidecar (ONNX, no Python)"
)]
struct Args {
    /// The backbone: config.json, tokenizer.json, vieneu_v3_heads.npz, the graphs.
    #[arg(long)]
    models: PathBuf,
    /// The MOSS audio codec's decode graph.
    #[arg(long)]
    codec: PathBuf,
    /// The sea-g2p phoneme dictionary (`sea_g2p.bin`).
    #[arg(long)]
    dict: PathBuf,
    /// The preset voice store (`voices_v3_turbo.json`).
    #[arg(long)]
    voices: PathBuf,
    #[arg(long, default_value_t = 8818)]
    port: u16,
    #[arg(long, default_value = "0.0.0.0")]
    bind: String,
    /// ONNX intra-op threads. 0 = half the cores, capped at 8, like the reference.
    #[arg(long, default_value_t = 0)]
    threads: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    for (what, p) in [
        ("models", &args.models),
        ("codec", &args.codec),
        ("dict", &args.dict),
        ("voices", &args.voices),
    ] {
        if !p.exists() {
            anyhow::bail!("--{what} does not exist: {}", p.display());
        }
    }

    let started = std::time::Instant::now();
    let front = FrontEnd::new(args.dict.to_str().context("--dict must be valid UTF-8")?)?;
    let roster = Roster::load(&args.voices)?;
    let synth = Synth::load(&args.models, &args.codec, args.threads)?;
    let voices = roster.voices.len();
    let default = roster.default_voice.clone().unwrap_or_else(|| "?".into());
    let server = Server::new(front, roster, synth);
    eprintln!(
        "loaded {voices} preset voices (default {default:?}) in {:.1?}",
        started.elapsed()
    );

    let addr = format!("{}:{}", args.bind, args.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    eprintln!("TTS worker on {addr} (no auth — LAN/SSH-tunnel only)");
    axum::serve(listener, router(server))
        .await
        .context("serving")?;
    Ok(())
}
