use super::sidecar::op_preview_voice;
use super::sidecar::op_segment;
use super::sidecar::sidecar_client;
use super::*;

pub(crate) async fn op(State(st): State<Shared>, Json(req): Json<OpRequest>) -> Json<OpResult> {
    match req.op {
        bm_proto::Op::Dispatch => {
            let go = req.go.unwrap_or(true);
            let line = { st.lock().await.set_dispatch(go) };
            let line = match enqueue_remainder(&st, go).await {
                Some(more) => format!("{line}; {more}"),
                None => line,
            };
            Json(OpResult::ok(line))
        }
        bm_proto::Op::Translate => {
            let (start, count) = (req.start.unwrap_or(1), req.count.unwrap_or(1));
            // The chapter index first, and **outside the lock**: building it can
            let (index, index_note) = {
                let inner = st.lock().await;
                if inner.settings.crawl.is_manual() {
                    (None, String::new())
                } else {
                    let (layout, settings) = (inner.layout.clone(), inner.settings.clone());
                    drop(inner);
                    match tokio::task::spawn_blocking(move || {
                        bm_core::crawl::chapter_index(&layout, &settings, start, count, false)
                    })
                    .await
                    {
                        Ok(Ok(idx)) => (Some(idx), String::new()),
                        // A broken crawler is worth knowing about *now*: the
                        Ok(Err(e)) => (None, format!("; no chapter index ({e:#})")),
                        Err(_) => (
                            None,
                            "; no chapter index (the index thread panicked)".into(),
                        ),
                    }
                }
            };
            let mut inner = st.lock().await;
            // Reconcile first: enqueue alone only tops up crawl+digest, so a
            inner.reconcile(start, count);
            // The operator just named the range, so from here this process works
            inner.set_authored_range(start, count);
            let absent = index.as_ref().map(|i| inner.apply_index(i)).unwrap_or(0);
            let (crawls, digests) = inner.enqueue_translate(start, count);
            let absent_note = if absent > 0 {
                format!("; {absent} chapter(s) are not on the site")
            } else {
                String::new()
            };
            Json(OpResult::ok(format!(
                "translate ch{start}..: {crawls} crawls + {digests} digests queued{absent_note}{index_note}"
            )))
        }
        bm_proto::Op::Import => {
            // Reading a file and rewriting a chapter is local, blocking disk
            let layout = { st.lock().await.layout.clone() };
            let (chapter, paths) = (req.chapter, req.paths.clone());
            let done = match tokio::task::spawn_blocking(move || {
                bm_core::crawl::import::import_all(&layout, chapter, &paths)
            })
            .await
            {
                Ok(Ok((done, line))) => Ok((done, line)),
                Ok(Err(e)) => Err(format!("import refused: {e:#}")),
                Err(e) => Err(format!("import failed: {e}")),
            };
            let (done, line) = match done {
                Ok(v) => v,
                Err(msg) => return Json(OpResult::fail(msg)),
            };
            let mut inner = st.lock().await;
            for got in &done {
                inner.mark_imported(got.n, got.bytes);
            }
            Json(OpResult::ok(format!("imported {line}")))
        }
        bm_proto::Op::CrawlSetup => {
            let (layout, settings) = {
                let mut inner = st.lock().await;
                if let Some(t) = req.url_template.clone() {
                    inner.settings.url_template = t.clone();
                    let _ = inner.settings.save(&inner.layout.settings());
                }
                (inner.layout.clone(), inner.settings.clone())
            };
            Json(op_crawl_setup(&layout, &settings, req.start.unwrap_or(1)).await)
        }
        bm_proto::Op::Voices => {
            let (layout, engine) = {
                let inner = st.lock().await;
                (inner.layout.clone(), inner.settings.engine.clone())
            };
            Json(op_voices(&layout, &engine).await)
        }
        bm_proto::Op::SwapVoice => {
            let (character, voice) = (
                req.character.clone().unwrap_or_default(),
                req.voice.clone().unwrap_or_default(),
            );
            if character.is_empty() || voice.is_empty() {
                return Json(OpResult::fail("swap needs character + voice"));
            }
            let mut inner = st.lock().await;
            // **Queued, not refused.** The op is unchanged for the caller and
            match inner.exclusive_request(bm_proto::ExclusiveOp::SwapVoice {
                character,
                voice,
                chapters: Vec::new(),
            }) {
                Ok(msg) => Json(OpResult::ok(msg)),
                Err(e) => Json(OpResult::fail(format!("swap failed: {e:#}"))),
            }
        }
        bm_proto::Op::PreviewVoice => {
            // No lock taken: rendering a sample reads no scheduler state, and
            let layout = st.lock().await.layout.clone();
            let voice = req.voice.clone().unwrap_or_default();
            Json(op_preview_voice(&layout, &voice, req.text.as_deref()).await)
        }
        bm_proto::Op::Segment => {
            // Files only, no lock beyond cloning two small values: the whole
            let (layout, engine) = {
                let inner = st.lock().await;
                (inner.layout.clone(), inner.settings.engine.clone())
            };
            let character = req.character.clone().unwrap_or_default();
            let voice = req.voice.clone().unwrap_or_default();
            Json(op_segment(
                &layout,
                &engine,
                &character,
                &voice,
                req.text.as_deref(),
            ))
        }
        bm_proto::Op::Eta => {
            let inner = st.lock().await;
            let (start, count) = (req.start.unwrap_or(1), req.count.unwrap_or(1));
            Json(OpResult::ok(inner.op_eta(start, count)))
        }
        bm_proto::Op::Requeue => {
            let mut inner = st.lock().await;
            Json(OpResult::ok(inner.op_requeue_orphans()))
        }
        bm_proto::Op::Retry => {
            let mut inner = st.lock().await;
            // Three scopes, narrowing in this order. A stage + chapter is one
            match (req.stage, req.chapter) {
                (Some(stage), Some(chapter)) => Json(OpResult::ok(inner.op_retry_task(
                    stage,
                    chapter,
                    req.force.unwrap_or(false),
                ))),
                (None, Some(chapter)) => Json(OpResult::ok(inner.op_retry_chapter(chapter))),
                (Some(_), None) => Json(OpResult::fail(
                    "retry needs a chapter when a stage is named",
                )),
                _ => Json(OpResult::ok(inner.op_retry_shelved())),
            }
        }
        bm_proto::Op::RetryTask => {
            let (stage, chapter, force) = (req.stage, req.chapter, req.force.unwrap_or(false));
            match (stage, chapter) {
                (Some(stage), Some(chapter)) => {
                    let mut inner = st.lock().await;
                    Json(OpResult::ok(inner.op_retry_task(stage, chapter, force)))
                }
                _ => Json(OpResult::fail("retry-task requires stage and chapter")),
            }
        }
        bm_proto::Op::Release => {
            // Two scopes, exactly one per request, and the refusal keeps them
            let force = req.force.unwrap_or(false);
            match (req.worker.clone(), req.stage, req.chapter) {
                (Some(worker), ..) => {
                    let mut inner = st.lock().await;
                    Json(OpResult::ok(inner.op_release_worker(&worker, force)))
                }
                (None, Some(stage), Some(chapter)) => {
                    let mut inner = st.lock().await;
                    Json(OpResult::ok(inner.op_release_task(stage, chapter, force)))
                }
                _ => Json(OpResult::fail(
                    "release needs a worker, or a stage and a chapter",
                )),
            }
        }
        bm_proto::Op::Reconcile => {
            // Plan under the lock, think outside it: the LLM call takes
            let (layout, settings) = {
                let inner = st.lock().await;
                (inner.layout.clone(), inner.settings.clone())
            };
            Json(op_reconcile(&st, &layout, &settings).await)
        }
        bm_proto::Op::Retag => {
            let dry_run = req.dry_run.unwrap_or(false);
            let mut inner = st.lock().await;
            // A dry run reads scripts and writes nothing, so it must never park
            let outcome = match dry_run {
                true => inner.op_retag(true),
                false => inner.exclusive_request(bm_proto::ExclusiveOp::Retag {
                    chapters: Vec::new(),
                }),
            };
            match outcome {
                Ok(msg) => Json(OpResult::ok(msg)),
                Err(e) => Json(OpResult::fail(format!("retag failed: {e:#}"))),
            }
        }
        bm_proto::Op::Recast => {
            let (chapter, fixes, remove) = (req.chapter, req.fixes.clone(), req.remove.clone());
            match chapter {
                Some(chapter) => {
                    let mut inner = st.lock().await;
                    let outcome = inner.exclusive_request(bm_proto::ExclusiveOp::Recast {
                        chapter,
                        fixes,
                        remove,
                    });
                    match outcome {
                        Ok(msg) => Json(OpResult::ok(msg)),
                        Err(e) => Json(OpResult::fail(format!("recast failed: {e:#}"))),
                    }
                }
                _ => Json(OpResult::fail("recast requires a chapter")),
            }
        }
        bm_proto::Op::FixSpeaker => {
            // All three names are required, and the third is the check rather
            let (chapter, segment, expect, speaker) = (
                req.chapter,
                req.segment,
                req.expect.clone(),
                req.speaker.clone(),
            );
            match (chapter, segment, expect, speaker) {
                (Some(chapter), Some(segment), Some(expect), Some(speaker))
                    if chapter > 0 && segment > 0 =>
                {
                    let mut inner = st.lock().await;
                    let outcome = inner.exclusive_request(bm_proto::ExclusiveOp::FixSpeaker {
                        chapter,
                        segment,
                        expect,
                        speaker,
                    });
                    match outcome {
                        Ok(msg) => Json(OpResult::ok(msg)),
                        Err(e) => Json(OpResult::fail(format!("fix-speaker failed: {e:#}"))),
                    }
                }
                _ => Json(OpResult::fail(
                    "fix-speaker requires chapter, segment, expect and speaker",
                )),
            }
        }
        bm_proto::Op::Merge => {
            // One survivor, one or more absorbed: the manual form of a
            let (survivor, absorbed) = (req.survivor.clone(), req.absorbed.clone());
            match survivor {
                Some(survivor) if !survivor.trim().is_empty() && !absorbed.is_empty() => {
                    let mut inner = st.lock().await;
                    let outcome = inner.exclusive_request(bm_proto::ExclusiveOp::Merge {
                        survivor,
                        absorbed,
                        chapters: Vec::new(),
                    });
                    match outcome {
                        Ok(msg) => Json(OpResult::ok(msg)),
                        Err(e) => Json(OpResult::fail(format!("merge failed: {e:#}"))),
                    }
                }
                _ => Json(OpResult::fail(
                    "merge requires a survivor and at least one absorbed name",
                )),
            }
        }
        bm_proto::Op::Remix => {
            let mut inner = st.lock().await;
            // The `None` semantics are the direct op's, unchanged. Speed, fx
            for (what, v) in [
                ("speed", req.speed),
                ("fx volume", req.effect_volume),
                ("music volume", req.music_volume),
            ] {
                if v.is_none() {
                    return Json(OpResult::fail(format!("remix needs {what}")));
                }
            }
            let inject = req.inject_volume.unwrap_or(inner.settings.inject_volume);
            let outcome = inner.exclusive_request(bm_proto::ExclusiveOp::Remix {
                speed: req.speed.unwrap_or(1.0),
                effect_volume: req.effect_volume.unwrap_or(1.0),
                music_volume: req.music_volume.unwrap_or(1.0),
                inject_volume: inject,
            });
            match outcome {
                Ok(msg) => Json(OpResult::ok(msg)),
                Err(e) => Json(OpResult::fail(format!("remix failed: {e:#}"))),
            }
        }
        bm_proto::Op::SoundChanged => {
            // No `ensure_idle`: nothing here writes a voice or a cache. It
            let mut inner = st.lock().await;
            Json(OpResult::ok(inner.op_sound_changed()))
        }
        bm_proto::Op::Rerender => {
            let mut inner = st.lock().await;
            match inner.exclusive_request(bm_proto::ExclusiveOp::Rerender) {
                Ok(msg) => Json(OpResult::ok(msg)),
                Err(e) => Json(OpResult::fail(format!("rerender failed: {e:#}"))),
            }
        }
        bm_proto::Op::Remerge => {
            let mut inner = st.lock().await;
            match inner.exclusive_request(bm_proto::ExclusiveOp::Remerge) {
                Ok(msg) => Json(OpResult::ok(msg)),
                Err(e) => Json(OpResult::fail(format!("remerge failed: {e:#}"))),
            }
        }
        bm_proto::Op::ShutdownWorkers => {
            let mut inner = st.lock().await;
            Json(OpResult::ok(inner.op_shutdown_workers()))
        }
        bm_proto::Op::ShutdownWhenIdle => {
            let mut inner = st.lock().await;
            Json(OpResult::ok(inner.op_shutdown_when_idle()))
        }
        bm_proto::Op::Exclusive => {
            // The write the request already carries its arguments for: swap
            let Some(mut op) = req.exclusive else {
                return Json(OpResult::fail("exclusive requires a write to queue"));
            };
            match &mut op {
                bm_proto::ExclusiveOp::SwapVoice {
                    character, voice, ..
                } => {
                    *character = req.character.clone().unwrap_or_default();
                    *voice = req.voice.clone().unwrap_or_default();
                }
                bm_proto::ExclusiveOp::Recast {
                    chapter,
                    fixes,
                    remove,
                } => {
                    *chapter = req.chapter.unwrap_or(0);
                    *fixes = req.fixes.clone();
                    *remove = req.remove.clone();
                }
                bm_proto::ExclusiveOp::FixSpeaker {
                    chapter,
                    segment,
                    expect,
                    speaker,
                } => {
                    *chapter = req.chapter.unwrap_or(0);
                    *segment = req.segment.unwrap_or(0);
                    *expect = req.expect.clone().unwrap_or_default();
                    *speaker = req.speaker.clone().unwrap_or_default();
                }
                bm_proto::ExclusiveOp::Merge {
                    survivor, absorbed, ..
                } => {
                    *survivor = req.survivor.clone().unwrap_or_default();
                    *absorbed = req.absorbed.clone();
                }
                _ => {}
            }
            let mut inner = st.lock().await;
            match inner.exclusive_request(op) {
                Ok(msg) => Json(OpResult::ok(msg)),
                Err(e) => Json(OpResult::fail(format!("queued write refused: {e:#}"))),
            }
        }
        bm_proto::Op::ExclusiveCancel => {
            let route = req.exclusive.as_ref().map(|e| e.route().to_string());
            let mut inner = st.lock().await;
            let n = inner.exclusive_cancel(route.as_deref());
            if n == 0 {
                Json(OpResult::fail("nothing queued to drop".to_string()))
            } else {
                Json(OpResult::ok(format!(
                    "dropped {n} queued write(s) — the stages it held open take work again"
                )))
            }
        }
    }
}

/// Fold duplicates: deterministic canon-key folds over the bible AND the cast
async fn op_reconcile(
    st: &Shared,
    layout: &bm_core::Layout,
    settings: &bm_core::config::Settings,
) -> OpResult {
    let bible: serde_json::Value =
        bm_core::read_json(&layout.bible()).unwrap_or(serde_json::json!({"characters": []}));
    // Ambiguous aliases first: bare generics ("nữ tử", "tiền bối", "vị kia")
    let scrubbed: Vec<String> = {
        let mut inner = st.lock().await;
        let path = inner.layout.bible();
        let mut current: serde_json::Value =
            bm_core::read_json(&path).unwrap_or(serde_json::json!({"characters": []}));
        let log = bm_core::digest::scrub_ambiguous_aliases(&mut current);
        if !log.is_empty() && bm_core::digest::save_bible(&current, &path).is_ok() {
            inner.push_event("ok", format!("reconcile scrub: {}", log.join("; ")));
            inner.save();
        }
        log
    };
    let scrub_note = if scrubbed.is_empty() {
        String::new()
    } else {
        format!("scrubbed {} ambiguous aliases; ", scrubbed.len())
    };
    let plan = bm_core::digest::reconcile_plan(&bible);
    let mut merges = plan.folds;
    {
        let cast = bm_core::cast::read_cast(&settings.engine, &layout.cast(&settings.engine));
        let keys: Vec<String> = cast.keys().cloned().collect();
        // ponytail: linear scans, merge lists are tiny
        let mut seen: std::collections::HashSet<String> =
            merges.iter().flat_map(|(_, a)| a.iter().cloned()).collect();
        for (canonical, absorbs) in bm_core::digest::cast_only_folds(&bible, &keys) {
            let fresh: Vec<String> = absorbs
                .into_iter()
                .filter(|a| seen.insert(a.clone()))
                .collect();
            if fresh.is_empty() {
                continue;
            }
            match merges.iter_mut().find(|(c, _)| c == &canonical) {
                Some((_, a)) => a.extend(fresh),
                None => merges.push((canonical, fresh)),
            }
        }
    }
    if merges.is_empty() && plan.candidates.is_empty() {
        let n = bible
            .get("characters")
            .and_then(|c| c.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        return OpResult::ok(format!(
            "{scrub_note}reconcile: bible already clean ({n} characters)"
        ));
    }
    if !merges.is_empty() {
        let mut inner = st.lock().await;
        // **Queued, not refused**, like every other surgery: the fold waits for
        return match inner.exclusive_request(bm_proto::ExclusiveOp::Reconcile {
            merges,
            chapters: Vec::new(),
        }) {
            Ok(msg) => OpResult::ok(format!(
                "{scrub_note}{msg}{}",
                if plan.candidates.is_empty() {
                    String::new()
                } else {
                    format!(
                        "; {} ambiguous pairs remain — press m again",
                        plan.candidates.len()
                    )
                }
            )),
            Err(e) => OpResult::fail(format!("reconcile refused: {e:#}")),
        };
    }
    // No certain folds. The ambiguous pairs are listed for a human to judge
    let pairs: Vec<String> = plan
        .candidates
        .iter()
        .map(|(a, b)| format!("{a} / {b}"))
        .collect();
    OpResult::ok(format!(
        "reconcile: nothing certain to fold; ambiguous pairs (no auto-merge): {}",
        pairs.join("; ")
    ))
}

/// Persist the URL template and prove the crawler works, through the **same
async fn op_crawl_setup(
    layout: &bm_core::Layout,
    settings: &bm_core::config::Settings,
    sample: u32,
) -> OpResult {
    let (layout, settings) = (layout.clone(), settings.clone());
    let sample = sample.max(1);
    let probed = tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
        let index = bm_core::crawl::chapter_index(&layout, &settings, sample, 1, false)?;
        let spec = bm_core::crawl::spec_from_settings(&layout, &settings);
        let url = index.url(sample).map(str::to_string);
        let provider = bm_core::crawl::Provider::new(&spec);
        let how = if provider.is_scripted() {
            format!("{} {}", spec.engine, spec.script)
        } else {
            "built-in fetcher".to_string()
        };
        let crawled = provider.crawl(sample, url.as_deref(), 1)?;
        Ok(match &crawled.outcome {
            bm_core::crawl::CrawlOutcome::Text { text, .. } => {
                // The text **is** the verdict: it arrived, and it cleared the
                let first = text.lines().next().unwrap_or("");
                format!(
                    "probe ch{sample} via {how}: {} bytes, headline {:?} — read it: if that is \
                     not the chapter, the selector matched the wrong thing",
                    text.len(),
                    bm_core::util::head_chars(first, 60)
                )
            }
            bm_core::crawl::CrawlOutcome::Absent { reason } => {
                format!("probe ch{sample} via {how}: the site has no such chapter ({reason})")
            }
            bm_core::crawl::CrawlOutcome::Blocked(b) => format!(
                "probe ch{sample} via {how} BLOCKED [{}]: {}",
                b.class.as_str(),
                b.detail
            ),
        })
    })
    .await;
    match probed {
        Ok(Ok(message)) => OpResult::ok(message),
        Ok(Err(e)) => OpResult::fail(format!("probe crawl failed: {e:#}")),
        Err(e) => OpResult::fail(format!("probe crawl failed: {e}")),
    }
}
/// Read the sidecar roster and refill the cast's gaps. Falls back to the
async fn op_voices(layout: &bm_core::Layout, engine: &str) -> OpResult {
    // Strict, unlike the picker: this op *prunes* the cast, so it refuses
    let policy = bm_core::voices::effective_policy(engine);
    // Live roster when a sidecar answers, offline fallback otherwise.
    let http = sidecar_client(Duration::from_secs(10));
    let mut enrolled: Vec<String> = Vec::new();
    let mut live = false;
    if let Ok(r) = http.get(format!("{SIDECAR}/voices")).send().await {
        if let Ok(v) = r.json::<Vec<Vec<String>>>().await {
            enrolled = v
                .into_iter()
                .filter_map(|p| match p.as_slice() {
                    [label, id] if label == id => Some(id.clone()),
                    _ => None,
                })
                .collect();
            live = true;
        }
    }
    let cast_path = layout.cast(engine);
    let cast = bm_core::cast::read_cast(engine, &cast_path);
    let filled_from = cast.len();
    // Refill gaps across every script. load_cast never overwrites an existing
    let scripts = layout.scripts();
    for sp in &scripts {
        let installed = bm_core::pool::installed_voices(layout);
        if let Err(e) = bm_core::cast::load_cast(
            sp,
            &cast_path,
            &layout.bible(),
            &policy,
            installed.as_ref(),
            true,
        ) {
            return OpResult::fail(format!("cast refill failed on {}: {e:#}", sp.display()));
        }
    }
    let cast = bm_core::cast::read_cast(engine, &cast_path);
    let gaps = cast.len().saturating_sub(filled_from);
    OpResult::ok(format!(
        "voices ({}, {} enrolled clones): filled {gaps} gaps, {} speakers mapped",
        if live {
            "live roster"
        } else {
            "offline roster"
        },
        enrolled.len(),
        cast.len()
    ))
}
