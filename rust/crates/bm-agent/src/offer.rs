use super::stages::pending_units;
use super::stages::render_offered_units;
use super::*;

/// Run one offered task on this box, start to finish.
///
/// **No scheduling calls home.** A worker never asks for work, the offer is
/// authoritative and everything scheduling needs arrived inside it. The one
/// exception is data, not scheduling: a merge pulls the take files it lacks
/// from the inductor (see `run_merge`), exactly as a render pushes its units
/// there. `fetch` carries the client and base URL for that pull; tests pass
/// `None`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_offer(
    layout: &Layout,
    settings: &Settings,
    offer: &TaskOffer,
    shared: &Shared,
    sidecar: &mut Sidecar,
    fetch: Option<(&reqwest::Client, &str)>,
    // Whether a render on this box may ensure a sidecar at all. `false` is
    // the inductor's instruction (`POST /sidecar-policy`): the operator's
    // policy turned render off, and a render, the one stage that cannot
    // run without the ~2.85 GB model, is refused rather than served by
    // re-warming it. A snapshot taken per call, so no hidden mutable state
    // sits on `Sidecar` for an offer to read stale.
    keep_sidecar: bool,
    // The ONNX thread count the inductor pushed for this box's sidecar, or
    // `None` for the sidecar's own default. A per-call snapshot, like
    // `keep_sidecar`, so an edit made mid-task is applied at the next
    // boundary rather than under a running render.
    tts_threads: Option<u32>,
) -> Result<TaskResult> {
    use bm_proto::Stage::*;
    let n = offer.chapter;
    // **The offer's binding decides where this task's files live.** See
    // `Layout::rebind` for the divergence it closes; in one line, a worker root
    // is a flat mirror, so without this the box keys `cast-*` and
    // `segments-*` under `default` while the inductor that drives it keys them
    // under the adapter its ledger names. The offer is the authority because it
    // is the inductor's answer, taken at the moment it decided to hand this box
    // this task — a re-pointed inductor reaches a box on its next offer rather
    // than on its next provisioning.
    let bound = layout.rebind(&offer.adapter, &offer.engine);
    let layout = &bound;
    warn_on_pack_mismatch(layout, offer);
    // Credentials first: everything below, including the TTS sidecar this
    // call may spawn, reads them from the environment.
    let installed = install_credentials(&offer.credentials);
    if !installed.is_empty() {
        println!("[ok] credentials from inductor: {}", installed.join(", "));
    }
    // The offer is authoritative: materialize its artifacts first so any
    // machine can run any stage without shared storage.
    if let Some(text) = &offer.text {
        bm_core::atomic_write(&layout.chapter_txt(n), text)?;
    }
    if let Some(script) = &offer.script {
        bm_core::atomic_write(&layout.script(n), &serde_json::to_string_pretty(script)?)?;
    }
    // The cast decides the filenames this stage will look for, so the offer's
    // copy wins over anything on this box. Without it a merge on a provisioned
    // worker names every segment differently from the render and finds none.
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
            // inductor read, so a box that has not been re-provisioned still
            // runs the crawler the operator edited. An offer without one (an
            // older inductor) falls back to this box's own settings, which is
            // the pre-script behaviour.
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
            // a block is a failure whose message and class travel back so the
            // inductor can shelve it at once when retrying would prove nothing.
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
                unit_files: Vec::new(),
            })
        }
        Digest => {
            let bible = offer.bible.clone().unwrap_or(json!({"characters": []}));
            // The inductor's pick wins; an empty offer (no active provider)
            // falls back to this worker's own settings, which refuses with
            // "press L" when it names nothing either.
            let analyzer = if offer.analyzer.is_empty() {
                settings.analyzer.clone()
            } else {
                offer.analyzer.clone()
            };
            // The backend name travels in `offer.analyzer`; what it runs
            // travels in `offer.analyzer_settings`. Both are needed here: this
            // box may have no `.bm/settings.json` at all (provisioning copies
            // the sources bundle, never `.bm/`, that
            // is the inductor's state), in which case `Settings::load` silently
            // returns `Settings::default()` and the compiled-in model runs
            // instead of the operator's. That is the bug that made a box
            // configured for `gemini-3.5-flash-lite` call `gemini-3.5-flash`.
            let mut digest_settings = settings.with_analyzer_settings(&offer.analyzer_settings);
            // The id rides the offer; the overlay carries model, endpoint and
            // slot. Both land here so error lines name the provider (`who`)
            // while routing reads the slot.
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
                unit_files: Vec::new(),
            })
        }
        Render => {
            // The sidecar policy first, as a **per-call snapshot** the caller
            // took before this offer ran: an operator who turned render off
            // for this box must not get a model relaunched under them by a
            // stray or stale render offer. A typed error, not a bare string
            // the serve path answers it with a 403, which the dispatcher
            // reads as "strike-free refusal, release the rows", while every
            // other render failure answers 200 `ok: false`, a strike. A
            // policy decision must never cost a chapter its three strikes
            // (three flips would shelve it); see `push::task` and
            // `dispatch::run_one`.
            if !keep_sidecar {
                return Err(anyhow::Error::new(PolicyRefusal(
                    "render skipped: this box's policy turns render off, so the TTS sidecar is not kept",
                )));
            }
            // The memory guard, and this is the only place it can safely run:
            // between tasks. A model that has outgrown its budget is recycled
            // here, before the first `/infer` of this offer, never during one.
            // See `Sidecar::recycle_if_over_budget` for what it measures and
            // why it is not the idle reaper's job.
            sidecar.recycle_if_over_budget().await;
            sidecar.ensure(layout, tts_threads).await?;
            let (units, unit_files) = match render_action(offer.render_units.as_deref()) {
                // Old inductor: plan from the local script, keep files
                // locally, upload nothing, exactly as before the migration.
                RenderAction::Legacy => (
                    run_render(layout, n, &offer.engine, &sidecar.tts(), shared).await?,
                    Vec::new(),
                ),
                // The offer names no units at all: nothing to speak.
                RenderAction::Noop => (0, Vec::new()),
                // The takes this offer carries, minus what this box already has.
                // Skipping here rather than on the inductor is the point: the
                // inductor cannot see this disk, and a partial offer is what
                // used to leave a box holding a strict subset of a chapter.
                //
                // `render_batch` takes of one chapter arrive per offer; the
                // worker already loops over a list, so batching changed nothing
                // here, only how often this arm is entered.
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
            // has a second trigger that cannot be misread (see
            // `SIDECAR_MAX_RENDERS`). Counted after the fact and not before:
            // a batch that failed halfway still spent the model on the takes it
            // did speak, and `units` is exactly those.
            sidecar.note_renders(units);
            // **No per-task stop.** The sidecar is warmed on purpose: keeping
            // the ~2.85 GB model resident across render tasks is the whole
            // reason a per-take schedule is affordable, and stopping here would
            // reload it for every offer. It is reaped on idle by
            // `sidecar_reaper`, recycled by the budget guard above, and stopped
            // explicitly before a merge.
            Ok(TaskResult {
                ok: true,
                detail: format!("render ch{n} ({units} calls)"),
                delta: None,
                units,
                script: None,
                text: None,
                crawl: None,
                mp3_b64: None,
                unit_files,
            })
        }
        Merge => {
            // Merge needs no TTS, and its ffmpeg pass is the other large
            // working set on the box: drop the model first, including a
            // provision-started one this worker never spawned, so the two
            // never co-reside in 8 GiB.
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
            // depend on whether the box was the local node: a local merge
            // leaned on `publish()` having renamed the file straight into the
            // inductor's own `output/`, which is only true when the two share
            // a filesystem. Shipping it every time costs a few MB of base64
            // and removes the branch, and the inductor writes it to
            // `final_mp3` either way, idempotently for a box that already put
            // it there.
            let mp3 = Some(base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                &std::fs::read(&path).with_context(|| format!("reading merged {path}"))?,
            ));
            Ok(TaskResult {
                ok: true,
                detail: format!("merge ch{n} -> {path}"),
                delta: None,
                units: 1,
                script: None,
                text: None,
                crawl: None,
                mp3_b64: mp3,
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
    /// chapter and a login wall are both `ok: false`-shaped facts with entirely
    /// different consequences, and the inductor cannot tell them apart from a
    /// boolean and a sentence.
    pub(crate) crawl: Option<bm_proto::CrawlReport>,
    pub(crate) mp3_b64: Option<String>,
    pub(crate) unit_files: Vec<bm_proto::UnitFile>,
}

/// Say what the sidecar guard will do, once, when a worker starts.
///
/// The guard's numbers are a judgement until a box has measured them, and a
/// measurement needs a record of what was in force, otherwise a log full of
/// recycles says nothing about which budget produced them. One line, at startup,
/// in the same place the worker announces itself.
///
/// Resolved from the same [`Budget::from_env`] the guard uses, so the line
/// cannot describe a budget other than the one being applied.
pub(crate) fn announce_budget() {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    println!("{}", Budget::from_env().describe(sys.total_memory()));
    // The ONNX thread count is the other per-box number that decides what a
    // render costs, and the guard's line does not carry it. `0` is the
    // sidecar's own default, so say which one is in force — a box that set
    // `BM_TTS_THREADS` can then see that it took.
    match bm_core::config::tts_threads() {
        0 => println!(
            "TTS sidecar threads: the sidecar's own default (half this box's cores, capped at 8) — set BM_TTS_THREADS to use more"
        ),
        n => println!("TTS sidecar threads: {n} (BM_TTS_THREADS)"),
    }
}
