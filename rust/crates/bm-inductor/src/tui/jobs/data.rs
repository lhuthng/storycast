use super::reqs::send;
use super::reqs::DoneKind;
use super::reqs::Ev;
use super::*;

pub(crate) async fn job_op(
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    api: String,
    http: reqwest::Client,
    req: OpRequest,
    layout: bm_core::Layout,
) {
    // Ops can wait on the analyzer for minutes; the shared 15s
    // client would time them out. Polling keeps the short one.
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()
        .unwrap_or(http);
    let name = req.op.as_str().to_string();
    let voice = req.voice.clone();
    let op = req.op;
    let key = op_key(&req);
    let character = req.character.clone();
    let mut audio_b64: Option<String> = None;
    let mut line_speaker: Option<String> = None;
    let mut line_text: Option<String> = None;
    let ok = match http.post(format!("{api}/api/op")).json(&req).send().await {
        Ok(r) => match r.json::<bm_proto::OpResult>().await {
            Ok(res) => {
                let level = if res.ok { Level::Ok } else { Level::Error };
                send(&tx, level, format!("{name}: {}", res.message));
                audio_b64 = res.audio_b64;
                line_speaker = res.line_speaker;
                line_text = res.line_text;
                res.ok
            }
            Err(e) => {
                send(&tx, Level::Error, format!("{name}: bad result: {e}"));
                false
            }
        },
        Err(e) => {
            // Swap-voice and remix survive a dead inductor: same mutation
            // against the files, guarded by inductor-down + no-local-workers.
            // Every other op genuinely needs the scheduler.
            if op == Op::SwapVoice {
                match crate::api::offline_swap(
                    &api,
                    &layout,
                    &character.clone().unwrap_or_default(),
                    &voice.clone().unwrap_or_default(),
                )
                .await
                {
                    Ok(msg) => {
                        send(&tx, Level::Ok, format!("{name}: {msg}"));
                        true
                    }
                    Err(msg) => {
                        send(
                            &tx,
                            Level::Error,
                            format!("{name}: {msg} (inductor also unreachable: {e})"),
                        );
                        false
                    }
                }
            } else if op == Op::Remix {
                match crate::api::offline_remix(
                    &api,
                    &layout,
                    req.speed,
                    req.effect_volume,
                    req.music_volume,
                    req.inject_volume,
                )
                .await
                {
                    Ok(msg) => {
                        send(&tx, Level::Ok, format!("{name}: {msg}"));
                        true
                    }
                    Err(msg) => {
                        send(
                            &tx,
                            Level::Error,
                            format!("{name}: {msg} (inductor also unreachable: {e})"),
                        );
                        false
                    }
                }
            } else if op == Op::SoundChanged {
                match crate::api::offline_sound_changed(&api, &layout).await {
                    Ok(msg) => {
                        send(&tx, Level::Ok, format!("{name}: {msg}"));
                        true
                    }
                    Err(msg) => {
                        send(
                            &tx,
                            Level::Error,
                            format!("{name}: {msg} (inductor also unreachable: {e})"),
                        );
                        false
                    }
                }
            } else {
                send(&tx, Level::Error, format!("{name} failed: {e}"));
                false
            }
        }
    };
    let _ = tx.send(Ev::Done(DoneKind::Op {
        op,
        key,
        ok,
        voice,
        audio_b64,
        line_speaker,
        line_text,
    }));
}

pub(crate) async fn job_load_roster(
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    api: String,
    http: reqwest::Client,
    layout: bm_core::Layout,
) {
    // Instant first: every piece the picker needs is on this disk, so show
    // it now instead of after an inductor hop plus a sidecar round trip.
    // (That chain cost 35s worst case while the sidecar booted: 15s TUI
    // timeout, then 20s of server-side sidecar timeouts, then the offline
    // build anyway.)
    let disk = layout.clone();
    match tokio::task::spawn_blocking(move || crate::api::local_roster(&disk)).await {
        Ok(roster) => {
            let _ = tx.send(Ev::Roster(Ok(roster)));
        }
        Err(e) => {
            let _ = tx.send(Ev::Roster(Err(format!("local roster failed: {e}"))));
        }
    }
    // ...then upgrade to live when the inductor answers with a sidecar
    // behind it. Anything else keeps the local roster already shown.
    if let Ok(r) = http.get(format!("{api}/api/roster")).send().await {
        if let Ok(roster) = r.json::<Roster>().await {
            if roster.source.starts_with("live") {
                let _ = tx.send(Ev::Roster(Ok(roster)));
            }
        }
    }
    let _ = tx.send(Ev::Done(DoneKind::Other));
}

/// Index every script's lines by speaker, off the UI thread.
///
/// `spawn_blocking` because this is a hundred file opens: cheap warm, but it is
/// I/O, and the UI task is the one thing the TUI is not allowed to stall.
pub(crate) async fn job_load_lines(
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    layout: bm_core::Layout,
) {
    let res = tokio::task::spawn_blocking(move || crate::tui::audition::index_lines(&layout))
        .await
        .unwrap_or_else(|e| Err(format!("line index task failed: {e}")));
    let _ = tx.send(Ev::Lines(res));
    // `dispatch` counts every job and only `Done` decrements, so a job that
    // reports its payload without one leaves the footer claiming a job is
    // running for the rest of the session, and nothing else ever clears it.
    // Every arm of `run_job` owes exactly one of these.
    let _ = tx.send(Ev::Done(DoneKind::Other));
}

/// Read the sound-design pools and what each entry is used for, off the UI
/// thread. Same `spawn_blocking` reasoning as `job_load_lines`: a hundred file
/// opens, and the UI task is the one thing the TUI may not stall.
pub(crate) async fn job_load_sounds(
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    layout: bm_core::Layout,
) {
    let res = tokio::task::spawn_blocking(move || crate::tui::sound::load(&layout))
        .await
        .unwrap_or_else(|e| Err(format!("sound design task failed: {e}")));
    let _ = tx.send(Ev::Sounds(res.map(Box::new)));
    // Every arm of `run_job` owes exactly one of these; see `job_load_lines`.
    let _ = tx.send(Ev::Done(DoneKind::Other));
}

/// Serve one already-rendered segment without an inductor: the same lookup
/// `Op::Segment` runs server-side, against this checkout's files. Reports
/// through `DoneKind::Op` with the same shape, so the Done handler, line
/// holding, playback, marker release, cannot tell the two paths apart.
pub(crate) async fn job_segment(
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    layout: bm_core::Layout,
    character: String,
    voice: String,
    text: String,
) {
    let key = op_key(&OpRequest {
        op: Op::Segment,
        ..Default::default()
    });
    let voice_job = voice.clone();
    let out = tokio::task::spawn_blocking(move || {
        let engine = bm_core::config::Settings::load(&layout.settings()).engine;
        let cands = bm_core::assemble::rendered_segments(&layout, &engine, &voice_job);
        if cands.is_empty() {
            let mut msg = bm_core::assemble::segment_miss(&layout, &character, &voice_job, false);
            msg.push_str("; connect (:B) to synthesize instead");
            return Err(msg);
        }
        // An exact line plays that sentence or misses honestly, like the op
        // with the same fallback to one of hers that did render, so a
        // fresh swap (rendered chapter by chapter) still auditions.
        let want = text.trim();
        if !want.is_empty() {
            match bm_core::assemble::pick_exact(&cands, &character, want) {
                Some(pick) => return serve_local_segment(pick),
                None => match bm_core::assemble::pick_rendered(&cands, &character) {
                    Some(pick) => return serve_local_segment(pick),
                    None => {
                        return Err(format!(
                            "{} (needs :B to render it)",
                            bm_core::assemble::segment_miss(&layout, &character, &voice_job, true)
                        ))
                    }
                },
            }
        }
        let pick = bm_core::assemble::pick_rendered(&cands, &character)
            .expect("a non-empty pool always picks");
        serve_local_segment(pick)
    })
    .await;
    match out {
        Ok(Ok((speaker, text, b64, len))) => {
            send(
                &tx,
                Level::Ok,
                format!(
                    "segment: “{speaker}” ({} KB, local — nothing synthesized)",
                    len / 1024
                ),
            );
            let (line_speaker, line_text) = if text.trim().is_empty() {
                (None, None)
            } else {
                (Some(speaker), Some(text))
            };
            let _ = tx.send(Ev::Done(DoneKind::Op {
                op: Op::Segment,
                key,
                ok: true,
                voice: Some(voice),
                audio_b64: Some(b64),
                line_speaker,
                line_text,
            }));
        }
        Ok(Err(msg)) => fail_segment(&tx, &key, &voice, msg),
        Err(e) => fail_segment(&tx, &key, &voice, format!("segment task crashed: {e}")),
    }
}

/// A picked local segment into the job's answer shape: speaker, text, base64
/// audio and its size for the status line.
fn serve_local_segment(
    pick: &bm_core::assemble::RenderedSegment,
) -> Result<(String, String, String, usize), String> {
    let bytes = pick.read_bytes()?;
    Ok((
        pick.speaker.clone(),
        pick.text.clone(),
        B64.encode(&bytes),
        bytes.len(),
    ))
}

/// Synthesize one line with this checkout's venv: the disconnected form of
/// `Op::PreviewVoice`. Reports through the same `DoneKind::Op`, so playback,
/// markers and the previewed checklist cannot tell it from a render.
pub(crate) async fn job_preview_local(
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    layout: bm_core::Layout,
    voice: String,
    text: String,
) {
    let key = op_key(&OpRequest {
        op: Op::PreviewVoice,
        ..Default::default()
    });
    let voice_done = voice.clone();
    let done = |ok: bool, audio_b64: Option<String>| {
        Ev::Done(DoneKind::Op {
            op: Op::PreviewVoice,
            key: key.clone(),
            ok,
            voice: Some(voice_done.clone()),
            audio_b64,
            line_speaker: None,
            line_text: None,
        })
    };
    let out = tokio::task::spawn_blocking(move || {
        let wav = std::env::temp_dir().join(format!(
            "bm-preview-{}-{}.wav",
            std::process::id(),
            bm_proto::now_secs()
        ));
        (|| {
            bm_core::pool::synth_preview(&layout.root, &voice, &text, &wav)
                .map_err(|e| format!("{e:#}"))?;
            let bytes = std::fs::read(&wav).map_err(|e| format!("reading preview wav: {e}"))?;
            let _ = std::fs::remove_file(&wav);
            Ok::<Vec<u8>, String>(bytes)
        })()
    })
    .await;
    match out {
        Ok(Ok(bytes)) if !bytes.is_empty() => {
            send(
                &tx,
                Level::Ok,
                format!("preview {voice_done} (local): {} KB", bytes.len() / 1024),
            );
            let _ = tx.send(done(true, Some(B64.encode(&bytes))));
        }
        Ok(Ok(_)) => {
            send(
                &tx,
                Level::Error,
                format!("preview {voice_done} (local): no audio rendered"),
            );
            let _ = tx.send(done(false, None));
        }
        Ok(Err(e)) => {
            send(
                &tx,
                Level::Error,
                format!("preview {voice_done} (local): {e}"),
            );
            let _ = tx.send(done(false, None));
        }
        Err(e) => {
            send(
                &tx,
                Level::Error,
                format!("preview {voice_done} (local) task failed: {e}"),
            );
            let _ = tx.send(done(false, None));
        }
    }
}

fn fail_segment(tx: &tokio::sync::mpsc::UnboundedSender<Ev>, key: &str, voice: &str, msg: String) {
    send(tx, Level::Error, format!("segment failed: {msg}"));
    let _ = tx.send(Ev::Done(DoneKind::Op {
        op: Op::Segment,
        key: key.to_string(),
        ok: false,
        voice: Some(voice.to_string()),
        audio_b64: None,
        line_speaker: None,
        line_text: None,
    }));
}
pub(crate) async fn job_llm_models(
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    provider: String,
    kind: String,
    base_url: String,
    key: String,
) {
    let result = bm_core::digest::fetch_models(&provider, &kind, &base_url, &key)
        .await
        .map_err(|e| format!("{provider}: {e:#}"));
    let _ = tx.send(Ev::LlmModels { provider, result });
}

#[cfg(test)]
mod tests {
    use super::super::provision::provision_stop_reason;

    #[test]
    fn a_provision_stop_names_the_cause_the_run_carried() {
        // The pre-flight failure that started all this: the log line is right
        // there, and it contains none of the words the old scan looked for, so
        // the pane said "provision INCOMPLETE" and told the operator to retry
        // the same click. The run now carries the reason, and the pane shows
        // it whatever the log happens to say.
        let why = "no TTS sidecar binary for linux/x86_64 at /repo/rust/target/x86_64-unknown-linux-gnu/release/bm-tts (linux/x86_64: `make tts`)";
        let lines = vec![
            "[10.0.0.1] profile: xianxia (6b8d5fc00761)".to_string(),
            format!("[10.0.0.1] {why}"),
        ];
        assert_eq!(provision_stop_reason(Some(why), &lines), why);
        // The scan alone still cannot see it, the honest reason the field
        // exists, pinned so nobody deletes the field and calls it a cleanup.
        assert_eq!(provision_stop_reason(None, &lines), "provision INCOMPLETE");
    }

    #[test]
    fn a_carried_reason_is_used_verbatim_and_keeps_its_own_words() {
        let why = "no local profile loaded — load one first (`:profile` in the dashboard)";
        assert_eq!(
            provision_stop_reason(Some(why), &["[10.0.0.1] unrelated".to_string()]),
            why
        );
    }

    #[test]
    fn the_log_scan_still_finds_the_inner_cause_and_drops_the_address() {
        let lines = vec![
            "[10.0.0.1] agent install failed: rsync push failed".to_string(),
            "[10.0.0.1] rsync: command not found".to_string(),
        ];
        assert_eq!(
            provision_stop_reason(None, &lines),
            "rsync: command not found",
            "the root cause, not the wrapper, and without the address the pane already shows"
        );
    }

    #[test]
    fn a_failure_with_nothing_to_say_says_only_that_it_is_incomplete() {
        // The last resort, and the string this whole change exists to avoid.
        let lines = vec!["[10.0.0.1] starting worker".to_string()];
        assert_eq!(provision_stop_reason(None, &lines), "provision INCOMPLETE");
        // An empty carried reason is not a reason.
        assert_eq!(
            provision_stop_reason(Some("   "), &lines),
            "provision INCOMPLETE"
        );
    }
}
