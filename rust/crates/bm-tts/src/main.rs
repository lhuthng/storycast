//! `bm-tts` — the VieNeu-TTS v3 Turbo sidecar, without Python.
//!
//!     bm-tts --models <dir> --codec <dir> --dict <sea_g2p.bin> --voices <json>
//!            [--port 8818] [--bind 0.0.0.0] [--threads N]
//!
//! The same endpoints on the same port as `python/tts_server.py` (plus
//! `/shutdown`, below), so nothing downstream changes: `bm-agent` is a thin HTTP
//! client that never loads a model, and it keeps talking to
//! `http://127.0.0.1:8818`.
//!
//! The listener binds **before** the model loads, and `/health` answers
//! **503 `{"status":"loading"}`** until it is ready. The reference instead
//! calls `vn.engine()` before `serve_forever()`, which makes "starting" and
//! "absent" look identical (a closed port) — and a caller that cannot tell them
//! apart spawns a duplicate. Two ~2.85 GB models on an 8 GiB box is the OOM
//! this repo kept hitting, so the port is taken first and readiness is honest:
//! a 200 from `/health` still means "ready", exactly as before.
//!
//! `POST /shutdown` asks the process to exit. It exists because a
//! provision-started sidecar is nobody's child (`nohup … &`), so the two places
//! that must *not* co-reside with it — a merge's ffmpeg pass, and a worker
//! shutting down — have no signal to send it otherwise.

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

    let addr = format!("{}:{}", args.bind, args.port);
    // **Bind first.** The port is the single-instance lock, and taking it before
    // ~2.85 GB of weights are loaded is what makes a second concurrent launch
    // cheap (it dies on the bind) rather than fatal (two models in RAM on an
    // 8 GiB box). `/health` answers 503 from here until the load finishes.
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    let server = Server::new();
    let serving = tokio::spawn({
        let router = router(server.clone());
        async move { axum::serve(listener, router).await }
    });
    eprintln!("TTS worker on {addr} bound — loading models (health answers 503 until ready)");

    // The load is blocking CPU/IO: keep it off the async threads so the bound
    // listener can answer 503 while it runs.
    let started = std::time::Instant::now();
    let (models, codec, dict, voices_path) = (
        args.models.clone(),
        args.codec.clone(),
        args.dict.clone(),
        args.voices.clone(),
    );
    let threads = args.threads;
    let (front, roster, synth) =
        tokio::task::spawn_blocking(move || -> Result<(FrontEnd, Roster, Synth)> {
            let front = FrontEnd::new(dict.to_str().context("--dict must be valid UTF-8")?)?;
            let roster = Roster::load(&voices_path)?;
            let synth = Synth::load(&models, &codec, threads)?;
            Ok((front, roster, synth))
        })
        .await
        .context("the model-load task panicked")??;

    let voices = roster.voices.len();
    let default = roster.default_voice.clone().unwrap_or_else(|| "?".into());
    server.fill(front, roster, synth);
    eprintln!(
        "loaded {voices} preset voices (default {default:?}) in {:.1?} — now serving on {addr}",
        started.elapsed()
    );

    // Two ways to end: the serve task failing, or a `POST /shutdown`. Returning
    // from `main` drops the model, which is the whole point of the endpoint —
    // the caller is a worker about to run ffmpeg on an 8 GiB box, or one that is
    // exiting and would otherwise leave 2.85 GB behind for a box nobody drives.
    tokio::select! {
        r = serving => {
            r.context("the serve task panicked")?.context("serving")?;
        }
        _ = server.await_shutdown() => {
            eprintln!("shutdown requested — exiting (the model returns to the OS)");
        }
    }
    Ok(())
}
