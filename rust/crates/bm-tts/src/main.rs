//! `bm-tts` — the VieNeu-TTS v3 Turbo sidecar, without Python.

use anyhow::{Context, Result};
use bm_tts::server::{router, Backend, Server};
use bm_tts::synth::Synth;
use bm_tts::text::FrontEnd;
use bm_tts::voice::Roster;
use clap::Parser;
use std::path::PathBuf;
use std::sync::Mutex;

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
    /// The MOSS audio codec's decode graph. VieNeu only: a bundle engine
    #[arg(long)]
    codec: Option<PathBuf>,
    /// The sea-g2p phoneme dictionary (`sea_g2p.bin`). VieNeu only: an engine
    #[arg(long)]
    dict: Option<PathBuf>,
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

    for (what, p) in [("models", &args.models), ("voices", &args.voices)] {
        if !p.exists() {
            anyhow::bail!("--{what} does not exist: {}", p.display());
        }
    }

    let addr = format!("{}:{}", args.bind, args.port);
    // **Bind first.** The port is the single-instance lock, and taking it before
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
    let started = std::time::Instant::now();
    let (models, codec, dict, voices_path) = (
        args.models.clone(),
        args.codec.clone(),
        args.dict.clone(),
        args.voices.clone(),
    );
    let _ = &codec;
    let threads = args.threads;
    let is_pocket = models.join("pocket.safetensors").is_file();
    let (backend, voices, default) = tokio::task::spawn_blocking(move || -> Result<_, _> {
        // A Pocket tree handed to a VieNeu-only build is refused rather than
        #[cfg(not(feature = "pocket"))]
        if is_pocket {
            anyhow::bail!(
                "{} is a Pocket TTS tree, but this bm-tts was built without it — \
                 rebuild with `--features pocket`",
                models.display()
            );
        }

        // The `return` (rather than if/else) is load-bearing: without the
        #[cfg(feature = "pocket")]
        if is_pocket {
            let engine = bm_tts::pocket::Pocket::load(&models, &voices_path, threads)?;
            let voices = engine.voices.len();
            let default = engine.default_voice.clone().unwrap_or_else(|| "?".into());
            return Ok((
                Backend::Pocket {
                    engine: Box::new(Mutex::new(engine)),
                },
                voices,
                default,
            ));
        }

        {
            let (codec, dict) = match (codec, dict) {
                (Some(c), Some(d)) => (c, d),
                (None, _) => anyhow::bail!("--codec is required for the vieneu engine"),
                (_, None) => anyhow::bail!("--dict is required for the vieneu engine"),
            };
            // The codec is a *directory* holding the decode graph, not a file:
            if !codec.is_dir() {
                anyhow::bail!(
                    "--codec does not exist or is not a directory: {}",
                    codec.display()
                );
            }
            if !dict.is_file() {
                anyhow::bail!("--dict does not exist: {}", dict.display());
            }
            let front = FrontEnd::new(dict.to_str().context("--dict must be valid UTF-8")?)?;
            let roster = Roster::load(&voices_path)?;
            let synth = Synth::load(&models, &codec, threads)?;
            let voices = roster.voices.len();
            let default = roster.default_voice.clone().unwrap_or_else(|| "?".into());
            Ok((
                Backend::Vieneu {
                    front,
                    roster,
                    synth: Mutex::new(synth),
                },
                voices,
                default,
            ))
        }
    })
    .await
    .context("the model-load task panicked")??;

    server.fill(backend);
    eprintln!(
        "loaded {voices} preset voices (default {default:?}) in {:.1?} — now serving on {addr}",
        started.elapsed()
    );

    // Two ways to end: the serve task failing, or a `POST /shutdown`. Returning
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
