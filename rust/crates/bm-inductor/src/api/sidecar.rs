use super::*;

/// Build the client used for every TTS-sidecar call.
pub(crate) fn sidecar_client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(timeout)
        .no_proxy()
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Health plus capability, mirroring the agent's sidecar gate: the server
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
    if let Err(e) = ensure_sidecar(layout).await {
        return OpResult::fail(format!("preview {voice}: {e:#}"));
    }
    // Two routes into the sidecar, and the difference is the point. No text
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
        if let Some(hint) = stale_roster_hint(layout, voice, &body) {
            return OpResult::fail(hint);
        }
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

/// The audition sidecar reads its roster once at startup, so a voice enrolled
/// after it started is on disk but unknown in memory until it restarts. Name
/// that instead of passing on the sidecar's `:prov` advice, which pushes to
/// workers and can never fix the box serving this preview.
pub(crate) fn stale_roster_hint(
    layout: &bm_core::Layout,
    voice: &str,
    body: &str,
) -> Option<String> {
    if !body.contains("unknown voice") {
        return None;
    }
    // Same key as the sidecar's own lookup (`bm_tts::voice::norm`): folded
    // with separators dropped, so `phong-le-2` still finds `Phong Le 2`.
    let norm = |s: &str| {
        bm_core::util::fold(s)
            .chars()
            .filter(|c| !matches!(c, '-' | '_' | ' '))
            .collect::<String>()
    };
    let want = norm(voice);
    let on_disk = bm_core::pool::installed_voices(layout).is_some_and(|names| {
        names.iter().any(|n| n == voice || norm(n) == want)
    });
    on_disk.then(|| {
        format!(
            "preview {voice}: the local sidecar is serving a stale roster (it started before {voice} was enrolled) — restart it (`pkill -x bm-tts`, it relaunches on the next preview) and `:prov` the workers so renders speak it too"
        )
    })
}

/// Turn a rendered wav into the op's answer.
pub(crate) fn audio_result(voice: &str, what: &str, bytes: &[u8]) -> OpResult {
    if bytes.is_empty() {
        // A 200 with an empty body is not audio. Passing it on would make the
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
    let exact = text.map(str::trim).filter(|t| !t.is_empty());
    if let Some(want) = exact {
        match bm_core::assemble::pick_exact(&cands, character, want) {
            Some(pick) => return serve_segment(pick),
            None => {
                // The held line never rendered in this voice, the normal
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
