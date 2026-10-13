use super::stages::pending_units;
use super::stages::render_offered_units;
use super::*;

/// Run one offered task on this box, start to finish.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_offer(
    layout: &Layout,
    settings: &Settings,
    offer: &TaskOffer,
    shared: &Shared,
    sidecar: &mut Sidecar,
    fetch: Option<(&reqwest::Client, &str)>,
    // Whether a render on this box may ensure a sidecar at all. `false` is
    keep_sidecar: bool,
    // The ONNX thread count the inductor pushed for this box's sidecar, or
    tts_threads: Option<u32>,
) -> Result<TaskResult> {
    use bm_proto::Stage::*;
    let n = offer.chapter;
    // **The offer's binding decides where this task's files live.** See
    let bound = layout.rebind(&offer.adapter, &offer.engine);
    let layout = &bound;
    warn_on_pack_mismatch(layout, offer);
    // Credentials first: everything below, including the TTS sidecar this
    let installed = install_credentials(&offer.credentials);
    if !installed.is_empty() {
        println!("[ok] credentials from inductor: {}", installed.join(", "));
    }
    // The offer is authoritative: materialize its artifacts first so any
    if let Some(text) = &offer.text {
        bm_core::atomic_write(&layout.chapter_txt(n), text)?;
    }
    if let Some(script) = &offer.script {
        bm_core::atomic_write(&layout.script(n), &serde_json::to_string_pretty(script)?)?;
    }
    // The cast decides the filenames this stage will look for, so the offer's
    if let Some(cast) = &offer.cast {
        bm_core::atomic_write(
            &layout.cast(&offer.engine),
            &serde_json::to_string_pretty(cast)?,
        )?;
    }
    if let Some(bible) = &offer.bible {
        if offer.stage == bm_proto::Stage::Merge {
            bm_core::atomic_write(&layout.bible(), &serde_json::to_string_pretty(bible)?)?;
        }
    }
    match offer.stage {
        Crawl => {
            // The offer's spec is authoritative: it carries the script the
            let spec = offer
                .crawl
                .clone()
                .unwrap_or_else(|| bm_core::crawl::spec_from_settings(layout, settings));
            let (units, text, report) = run_crawl(
                layout,
                n,
                spec,
                offer.url.clone(),
                offer.attempt.max(1),
                shared,
            )
            .await?;
            // An absent chapter is a *success* whose artifact does not exist;
            let ok = text.is_some() || report.verdict == bm_proto::CrawlVerdict::Absent;
            let detail = match (&report.verdict, &text) {
                (bm_proto::CrawlVerdict::Text, Some(t)) => {
                    format!("crawled ch{n} ({} bytes)", t.len())
                }
                (bm_proto::CrawlVerdict::Absent, _) => {
                    format!("ch{n} is not on the site: {}", report.detail)
                }
                _ => format!("crawl ch{n} blocked [{}]: {}", report.class, report.detail),
            };
            Ok(TaskResult {
                ok,
                detail,
                delta: None,
                units,
                script: None,
                text,
                crawl: Some(report),
                mp3_b64: None,
                cues_b64: None,
                unit_files: Vec::new(),
            })
        }
        Digest => {
            let bible = offer.bible.clone().unwrap_or(json!({"characters": []}));
            // The inductor's pick wins; an empty offer (no active provider)
            let analyzer = if offer.analyzer.is_empty() {
                settings.analyzer.clone()
            } else {
                offer.analyzer.clone()
            };
            // The backend name travels in `offer.analyzer`; what it runs
            let mut digest_settings = settings.with_analyzer_settings(&offer.analyzer_settings);
            // The id rides the offer; the overlay carries model, endpoint and
            digest_settings.analyzer = analyzer.clone();
            let (delta, script) = run_digest(
                layout,
                n,
                &bible,
                &digest_settings,
                &analyzer,
                shared,
                false,
            )
            .await?;
            Ok(TaskResult {
                ok: true,
                detail: format!("digest ch{n} via {analyzer}"),
                delta: Some(delta),
                units: 1,
                script: Some(script),
                text: None,
                crawl: None,
                mp3_b64: None,
                cues_b64: None,
                unit_files: Vec::new(),
            })
        }
        Render => {
            // The sidecar policy first, as a **per-call snapshot** the caller
            if !keep_sidecar {
                return Err(anyhow::Error::new(PolicyRefusal(
                    "render skipped: this box's policy turns render off, so the TTS sidecar is not kept",
                )));
            }
            // The memory guard, and this is the only place it can safely run:
            sidecar.recycle_if_over_budget().await;
            sidecar.ensure(layout, tts_threads).await?;
            let (units, unit_files) = match render_action(offer.render_units.as_deref()) {
                // Old inductor: plan from the local script, keep files
                RenderAction::Legacy => (
                    run_render(layout, n, &offer.engine, &sidecar.tts(), shared).await?,
                    Vec::new(),
                ),
                // The offer names no units at all: nothing to speak.
                RenderAction::Noop => (0, Vec::new()),
                // The takes this offer carries, minus what this box already has.
                RenderAction::Units => {
                    // Proven non-empty by the match above.
                    let list = offer.render_units.as_deref().unwrap_or(&[]);
                    let todo =
                        pending_units(list, &offer.render_force, &layout.seg_dir(&offer.engine, n));
                    if todo.is_empty() {
                        (0, Vec::new())
                    } else {
                        render_offered_units(
                            layout,
                            n,
                            &offer.engine,
                            &todo,
                            &sidecar.tts(),
                            shared,
                        )
                        .await?
                    }
                }
            };
            // Count the takes against the process that spoke them, so the guard
            sidecar.note_renders(units);
            // **No per-task stop.** The sidecar is warmed on purpose: keeping
            Ok(TaskResult {
                ok: true,
                detail: format!("render ch{n} ({units} calls)"),
                delta: None,
                units,
                script: None,
                text: None,
                crawl: None,
                mp3_b64: None,
                cues_b64: None,
                unit_files,
            })
        }
        Merge => {
            // Merge needs no TTS, and its ffmpeg pass is the other large
            sidecar.reap_all().await;
            let path = run_merge(
                layout,
                n,
                &offer.engine,
                MergeJob {
                    gap_ms: offer.gap_ms,
                    speed: offer.speed,
                    on: bm_core::ambience::LayerSwitch::new(
                        offer.ambience,
                        offer.music,
                        offer.effect_volume,
                        offer.music_volume,
                        offer.inject_volume,
                    ),
                    takes: offer.merge_takes.clone(),
                },
                shared,
                fetch,
            )
            .await?;
            // **The product always comes home in the report.** This used to
            let mp3 = Some(base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                &std::fs::read(&path).with_context(|| format!("reading merged {path}"))?,
            ));
            // The sidecar is half the product: captions ride the same report,
            let cues = std::fs::read(bm_core::assemble::cues_path(std::path::Path::new(&path)))
                .ok()
                .map(|raw| base64::Engine::encode(&base64::engine::general_purpose::STANDARD, raw));
            Ok(TaskResult {
                ok: true,
                detail: format!("merge ch{n} -> {path}"),
                delta: None,
                units: 1,
                script: None,
                text: None,
                crawl: None,
                mp3_b64: mp3,
                cues_b64: cues,
                unit_files: Vec::new(),
            })
        }
    }
}

#[derive(Debug)]
pub(crate) struct TaskResult {
    pub(crate) ok: bool,
    pub(crate) detail: String,
    pub(crate) delta: Option<Value>,
    pub(crate) units: u64,
    pub(crate) script: Option<Value>,
    pub(crate) text: Option<String>,
    /// Crawl only: what happened, when "ok" alone cannot say it, an absent
    pub(crate) crawl: Option<bm_proto::CrawlReport>,
    pub(crate) mp3_b64: Option<String>,
    /// Merge only: the cue sidecar beside the mix, base64.
    pub(crate) cues_b64: Option<String>,
    pub(crate) unit_files: Vec<bm_proto::UnitFile>,
}

/// Say what the sidecar guard will do, once, when a worker starts.
pub(crate) fn announce_budget() {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    println!("{}", Budget::from_env().describe(sys.total_memory()));
    // The ONNX thread count is the other per-box number that decides what a
    match bm_core::config::tts_threads() {
        0 => println!(
            "TTS sidecar threads: the sidecar's own default (half this box's cores, capped at 8) — set BM_TTS_THREADS to use more"
        ),
        n => println!("TTS sidecar threads: {n} (BM_TTS_THREADS)"),
    }
}
