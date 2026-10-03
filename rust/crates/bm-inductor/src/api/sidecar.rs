use super::*;

/// Build the client used for every TTS-sidecar call.
///
/// `no_proxy` is not optional: the sidecar is a LAN service on loopback, and a
/// configured `HTTP_PROXY` would otherwise intercept it, which silently
/// downgrades the roster to the offline fallback and makes previews 502.
pub(crate) fn sidecar_client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(timeout)
        .no_proxy()
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Health plus capability, mirroring the agent's sidecar gate: the server
/// must serve the policy endpoint the agent was built against, or a stale
/// server from a previous deploy answers health but lacks `/preview`.
pub(crate) async fn sidecar_serving(base: &str) -> bool {
    let Ok(http) = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .no_proxy()
        .build()
    else {
        return false;
    };
    let health = http
        .get(format!("{base}/health"))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false);
    if !health {
        return false;
    }
    let Ok(resp) = http.get(format!("{base}/policy")).send().await else {
        return false;
    };
    let text = resp.text().await.unwrap_or_default();
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|p| p.get("allowed_voices").cloned())
        .and_then(|v| v.as_array().cloned())
        .is_some()
}

/// Start the local sidecar for audition duty unless one already answers.
///
/// Preview/audition is the one path that needs TTS with no render task
/// running, and since the sidecar's lifecycle went per-task, idle means
/// down. So the first audition of a quiet cluster boots the server (model
/// load takes minutes) and leaves it up: stopping it after every sample
/// would make every audition pay the load again. The binary and argv are
/// the agent's own (`Layout::sidecar_command`), so the two can never name
/// different servers.
pub(crate) async fn ensure_sidecar(layout: &bm_core::Layout) -> anyhow::Result<()> {
    if sidecar_serving(SIDECAR).await {
        return Ok(());
    }
    let port = SIDECAR
        .rsplit(':')
        .next()
        .and_then(|p| p.trim_end_matches('/').parse().ok())
        .unwrap_or(8818);
    let (bin, args) = layout.sidecar_command(port, bm_core::config::tts_threads());
    if !bin.is_file() {
        anyhow::bail!(
            "no TTS sidecar at {} — build it (`make build`) or provision this box",
            bin.display()
        );
    }
    // Detached by dropping the handle: this is audition duty, not a render
    // task, so no per-task owner exists to reap it. It lives until the box
    // reboots or `X` sweeps it, exactly like the provision-started one did.
    let _ = tokio::process::Command::new(&bin)
        .args(&args)
        .env("LD_LIBRARY_PATH", layout.tts_lib_dir())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_secs(5)).await;
        if sidecar_serving(SIDECAR).await {
            return Ok(());
        }
    }
    anyhow::bail!("TTS sidecar started but never answered /health")
}

/// Render one voice's speech so it can be auditioned before it is assigned.
///
/// Without `text` this is the voice *sample*: the sidecar's fixed audition line,
/// which is the only way two voices are comparable. With `text` it is a real
/// line from the book, which is what an operator actually wants to hear before
/// committing a swap.
///
/// Either way the bytes come back in `OpResult::audio_b64` and **nothing is
/// written here**. The inductor never plays anything, it is a server, and the
/// speaker is on the client's desk, so it is also the wrong machine to put a
/// file on: a path is useless to a client that does not share this filesystem,
/// and an audition that lands in `data/` accumulates one clip per voice
/// auditioned. The client owns the file, because the client owns the speaker.
pub(crate) async fn op_preview_voice(
    layout: &bm_core::Layout,
    voice: &str,
    text: Option<&str>,
) -> OpResult {
    let voice = voice.trim();
    if voice.is_empty() {
        return OpResult::fail("preview needs a voice name");
    }
    // Audition is the one path that needs TTS with no render task running:
    // boot the sidecar here rather than failing onto an idle box.
    if let Err(e) = ensure_sidecar(layout).await {
        return OpResult::fail(format!("preview {voice}: {e:#}"));
    }
    // Two routes into the sidecar, and the difference is the point. No text
    // means `/preview`, which speaks the sidecar's fixed audition line, the
    // only way two voice samples are comparable. Text means `/infer`, which is
    // how an operator hears a *real* line from the book instead of a sample.
    let line = text.map(str::trim).filter(|t| !t.is_empty());
    let (path, body) = match line {
        Some(t) => ("/infer", serde_json::json!({"voice": voice, "text": t})),
        None => ("/preview", serde_json::json!({"voice": voice})),
    };
    let http = match reqwest::Client::builder()
        .timeout(Duration::from_secs(180))
        .no_proxy()
        .build()
    {
        Ok(c) => c,
        Err(e) => return OpResult::fail(format!("preview {voice}: {e:#}")),
    };
    let resp = match http
        .post(format!("{SIDECAR}{path}"))
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return OpResult::fail(format!(
                "preview {voice}: TTS sidecar unreachable at {SIDECAR} ({e})"
            ))
        }
    };
    if !resp.status().is_success() {
        let code = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return OpResult::fail(format!(
            "preview {voice}: sidecar {code} — {}",
            bm_core::util::head_chars(body.trim(), 200)
        ));
    }
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => return OpResult::fail(format!("preview {voice}: read failed ({e})")),
    };
    let what = match line {
        Some(_) => "line",
        None => "sample",
    };
    audio_result(voice, what, &bytes)
}

/// Turn a rendered wav into the op's answer.
///
/// Split out from the HTTP call so the contract is testable without a sidecar:
/// bytes in, base64 out, and **nothing written**. The inductor is the wrong
/// machine to put a sample on, a path is useless to a client that does not
/// share this filesystem, and a clip that landed in `data/` would accumulate
/// one file per voice auditioned, which is exactly what the operator asked it
/// not to do.
pub(crate) fn audio_result(voice: &str, what: &str, bytes: &[u8]) -> OpResult {
    if bytes.is_empty() {
        // A 200 with an empty body is not audio. Passing it on would make the
        // client report a playback failure for a render that produced nothing.
        return OpResult::fail(format!("preview {voice}: the sidecar returned no audio"));
    }
    OpResult::ok(format!(
        "preview {voice} ({what}): {} KB",
        bytes.len() / 1024
    ))
    .with_audio_b64(base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        bytes,
    ))
}

/// Serve one already-rendered segment for a voice: no synthesis, just bytes
/// from this inductor's segment cache.
///
/// Only what is on local disk counts. Segments rendered on another box stay
/// there (merge affinity), and fetching them over ssh would turn a keypress
/// into a network operation with its own failure modes, the miss says so
/// instead, and names what would fix it. Discovery lives in `bm_core` so a
/// disconnected TUI can run the same lookup against its own checkout.
pub(crate) fn op_segment(
    layout: &bm_core::Layout,
    engine: &str,
    character: &str,
    voice: &str,
    text: Option<&str>,
) -> OpResult {
    let voice = voice.trim();
    if voice.is_empty() {
        return OpResult::fail("segment needs a voice name");
    }
    let cands = bm_core::assemble::rendered_segments(layout, engine, voice);
    if cands.is_empty() {
        return OpResult::fail(bm_core::assemble::segment_miss(
            layout, character, voice, false,
        ));
    }
    // An exact line plays that sentence or misses honestly, never a nearby
    // one. Without it, T triages on a random segment.
    let exact = text.map(str::trim).filter(|t| !t.is_empty());
    if let Some(want) = exact {
        match bm_core::assemble::pick_exact(&cands, character, want) {
            Some(pick) => return serve_segment(pick),
            None => {
                // The held line never rendered in this voice, the normal
                // state for a fresh swap, which renders chapter by chapter.
                // Fall back to one of hers that did, still zero synthesis:
                // the served sentence is held, so T compares on it rather
                // than another random pick.
                match bm_core::assemble::pick_rendered(&cands, character) {
                    Some(pick) => return serve_segment(pick),
                    None => {
                        return OpResult::fail(bm_core::assemble::segment_miss(
                            layout, character, voice, true,
                        ))
                    }
                }
            }
        }
    }
    let pick =
        bm_core::assemble::pick_rendered(&cands, character).expect("a non-empty pool always picks");
    serve_segment(pick)
}

/// Turn a picked segment into the op's answer: bytes, plus whose sentence it
/// is so the client can show and hold it.
fn serve_segment(pick: &bm_core::assemble::RenderedSegment) -> OpResult {
    let bytes = match pick.read_bytes() {
        Ok(b) => b,
        Err(e) => return OpResult::fail(format!("segment unreadable: {e}")),
    };
    let mut res = OpResult::ok(format!(
        "segment: “{}” ch{} ({} KB, rendered — nothing synthesized)",
        pick.speaker,
        pick.chapter,
        bytes.len() / 1024
    ))
    .with_audio_b64(base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        &bytes,
    ));
    if !pick.text.trim().is_empty() {
        res = res.with_line(pick.speaker.clone(), pick.text.clone());
    }
    res
}
