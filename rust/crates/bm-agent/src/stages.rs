use super::*;

/// Crawl one chapter, through whichever provider the offer named.
///
/// The stage itself is now thin on purpose: *how* a chapter is obtained is a
/// script's business (or the built-in fetcher's), and the only things this
/// function owns are the ones that are the same for every provider, the
/// progress line, the artifact write, and the verdict that travels back in the
/// completion.
///
/// **On a blocking thread.** The provider uses a blocking HTTP client and runs
/// an interpreter, so it cannot live inside the async task: `spawn_blocking`
/// keeps the heartbeat alive and keeps a nested runtime out of a tokio worker.
pub(crate) async fn run_crawl(
    layout: &Layout,
    n: u32,
    spec: bm_proto::CrawlSpec,
    url: Option<String>,
    attempt: u32,
    shared: &Shared,
) -> Result<(u64, Option<String>, bm_proto::CrawlReport)> {
    let how = if spec.engine.is_empty() {
        "built-in".to_string()
    } else {
        format!("{} {}", spec.engine, spec.script)
    };
    set_progress(shared, 0.05, format!("crawl ch{n} ({how})"));
    let crawled = tokio::task::spawn_blocking(move || {
        bm_core::crawl::Provider::new(&spec).crawl(n, url.as_deref(), attempt)
    })
    .await
    .context("the crawl thread panicked")??;

    let report = bm_core::crawl::report_of(&crawled);
    for line in &crawled.log {
        println!("crawl ch{n}: {line}");
    }
    let text = match &crawled.outcome {
        bm_core::crawl::CrawlOutcome::Text { text, .. } => {
            bm_core::atomic_write(&layout.chapter_txt(n), text)?;
            set_progress(
                shared,
                1.0,
                format!("{how} crawled ch{n} ({} bytes)", text.len()),
            );
            Some(text.clone())
        }
        // Nothing to write: the site has no such chapter. A terminal
        // non-failure, so no strike and no artifact.
        bm_core::crawl::CrawlOutcome::Absent { reason } => {
            set_progress(shared, 1.0, format!("ch{n} is not on the site: {reason}"));
            None
        }
        bm_core::crawl::CrawlOutcome::Blocked(b) => {
            set_progress(shared, 1.0, format!("crawl ch{n} blocked: {}", b.detail));
            None
        }
    };
    Ok((1, text, report))
}

pub(crate) async fn run_digest(
    layout: &Layout,
    n: u32,
    bible: &Value,
    settings: &Settings,
    analyzer: &str,
    shared: &Shared,
    merge_local: bool,
) -> Result<(Value, Value)> {
    let layout = layout.clone();
    let layout2 = layout.clone();
    let bible = bible.clone();
    let settings = settings.clone();
    let analyzer = analyzer.to_string();
    let shared2 = shared.clone();
    let (outcome, warnings) = tokio::task::spawn_blocking(move || -> Result<_> {
        let rt = tokio::runtime::Handle::current();
        let mut last = (0.0, String::new());
        let mut cb = |f: f32, s: String| {
            last = (f, s.clone());
            set_progress(&shared2, f, s);
        };
        // Analyze only, never persist: the inductor is the single writer of
        // the script and the bible. A worker-mode write would land on the
        // shared disk underneath the winner, a lost digest race's orphaned
        // thread finishing late would overwrite the applied script with its
        // own uncanonicalized version. The script travels home in the report.
        let outcome = rt.block_on(bm_core::digest::analyze_chapter(
            &layout2, n, &bible, &settings, &analyzer, &mut cb,
        ))?;
        Ok((outcome, last))
    })
    .await??;
    let _ = warnings;
    for line in &outcome.log {
        println!("{line}");
    }
    if merge_local {
        // Standalone mode keeps legacy behaviour: persist the script and
        // merge here. Worker mode returns the delta and the inductor merges
        // as the single writer.
        bm_core::digest::write_script(&layout, n, &outcome.script)?;
        let mut local: Value =
            serde_json::from_str(&std::fs::read_to_string(layout.bible())?).unwrap_or(json!({}));
        bm_core::digest::merge_bible(&mut local, &outcome.delta, &format!("{n:02}"));
        bm_core::digest::save_bible(&local, &layout.bible())?;
    }
    set_progress(
        shared,
        1.0,
        format!("digest ch{n} done ({} segments)", outcome.segments),
    );
    Ok((outcome.delta, outcome.script))
}

/// What a render offer asks for. Pure, so the zero-units arm, "do nothing
/// and report success", the easiest arm to write as a fall-through, is
/// pinned by a test instead of by inspection.
///
/// The offered list is the chapter's **whole** unit set, not the difference
/// against the inductor's store: the inductor cannot see this box's disk. What
/// this box still has to speak is therefore decided against the disk, in
/// `pending_units`, and not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RenderAction {
    /// Old inductor (no `render_units`): plan from the local script.
    Legacy,
    /// The offer names no units at all, an empty chapter. Report `ok` with
    /// `units: 0` at once.
    Noop,
    /// Consider these units; speak the ones this box does not already hold.
    Units,
}

pub(crate) fn render_action(render_units: Option<&[bm_proto::RenderUnitSpec]>) -> RenderAction {
    match render_units {
        None => RenderAction::Legacy,
        Some([]) => RenderAction::Noop,
        Some(_) => RenderAction::Units,
    }
}

/// The offered units whose file this box does not already hold, plus every
/// unit the inductor flagged as forced.
///
/// Forced names are the ones the inductor's own store lacks, the surgical
/// set a swap or retag just deleted. They render even when this disk holds a
/// same-named file, or a warm box keeps serving stale bytes under the new
/// text. Everything else skips on presence, as before.
pub(crate) fn pending_units<'a>(
    offered: &'a [bm_proto::RenderUnitSpec],
    force: &[String],
    seg_dir: &std::path::Path,
) -> Vec<&'a bm_proto::RenderUnitSpec> {
    offered
        .iter()
        .filter(|u| {
            force.iter().any(|f| f == &u.name)
                || !seg_dir
                    .join(&u.name)
                    .metadata()
                    .map(|m| m.len() > 1000)
                    .unwrap_or(false)
        })
        .collect()
}

pub(crate) async fn render_offered_units(
    layout: &Layout,
    n: u32,
    engine: &str,
    units: &[&bm_proto::RenderUnitSpec],
    tts: &Tts,
    shared: &Shared,
) -> Result<(u64, Vec<bm_proto::UnitFile>)> {
    let total = units.len();
    let seg_dir = layout.seg_dir(engine, n);
    std::fs::create_dir_all(&seg_dir)?;
    let mut files = Vec::with_capacity(total);
    for (i, u) in units.iter().enumerate() {
        set_progress(
            shared,
            i as f32 / total.max(1) as f32,
            format!("render ch{n} {} ({}/{})", u.tag, i + 1, total),
        );
        let wav = tts
            .infer(&u.text, &u.voice, u.temperature, u.silence_p, engine)
            .await?;
        // The storage tier rides the offer: an `.mp3` name with a bitrate is
        // stored encoded, everything else is the sidecar's wav under whatever
        // name it was given. The encode is this box's job because the
        // sidecar speaks wav, and the name is checked rather than trusted —
        // a `.mp3` name holding wav bytes would be a lie every reader after
        // the store pays for.
        let bytes: std::borrow::Cow<[u8]> = if u.name.ends_with(".mp3") && u.mp3_kbps > 0 {
            let kbps = u.mp3_kbps;
            let src = std::env::temp_dir()
                .join(format!("bm-encode-{}-{}", std::process::id(), u.take_key))
                .with_extension("wav");
            let dst = src.with_extension("mp3");
            std::fs::write(&src, &wav)?;
            let (src_in, dst_in) = (src.clone(), dst.clone());
            tokio::task::spawn_blocking(move || {
                bm_core::assemble::encode_mp3(&src_in, &dst_in, kbps)
            })
            .await
            .map_err(|e| anyhow::anyhow!("encode task failed: {e}"))?
            .with_context(|| format!("encoding {} at {kbps}k", u.name))?;
            let _ = std::fs::remove_file(&src);
            let encoded = std::fs::read(&dst)?;
            let _ = std::fs::remove_file(&dst);
            std::borrow::Cow::Owned(encoded)
        } else {
            std::borrow::Cow::Borrowed(&wav[..])
        };
        std::fs::write(seg_dir.join(&u.name), &bytes)?;
        files.push(bm_proto::UnitFile {
            name: u.name.clone(),
            b64: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes),
        });
    }
    set_progress(
        shared,
        1.0,
        format!("render ch{n} done ({total} new calls)"),
    );
    Ok((total as u64, files))
}

/// Legacy path: plan from the local script (old inductor, or no units
/// offered). Keeps files locally and uploads nothing.
pub(crate) async fn run_render(
    layout: &Layout,
    n: u32,
    engine: &str,
    tts: &Tts,
    shared: &Shared,
) -> Result<u64> {
    let script_path = layout.script(n);
    let cast_path = layout.cast(engine);
    let seg_dir = layout.seg_dir(engine, n);
    let text = std::fs::read_to_string(&script_path)?;
    let data: Value = serde_json::from_str(&text)?;
    let segments = data
        .get("segments")
        .and_then(|s| s.as_array())
        .cloned()
        .unwrap_or_default();
    let policy = bm_core::cast::policy_for_bible(engine, layout);
    let installed = bm_core::pool::installed_voices(layout);
    let cast = bm_core::cast::load_cast(
        &script_path,
        &cast_path,
        &layout.bible(),
        &policy,
        installed.as_ref(),
        true,
    )?;
    let local = engine == "vieneu";
    let planned = bm_core::assemble::Planned::plan(&segments);
    let first = planned
        .speech
        .first()
        .map(|s| s.get("text").and_then(|t| t.as_str()).unwrap_or(""))
        .unwrap_or("");
    let title = bm_core::assemble::title_speech(layout, n, &cast, first);
    let units = bm_core::assemble::plan_render(&planned, &cast, &seg_dir, local, title.as_ref())?;
    // A take's file is content-addressed, and the chapter's plan is what names
    // it, so this path owns the plan the way the inductor does when it offers:
    // build it, reconcile it against the stored one — a first plan adopts an
    // existing legacy cache, a re-plan trusts the files it recorded — and write
    // it back. Rendering then covers exactly the takes the diff calls dirty, and
    // the mixer reads the names this wrote, so a hand-driven render and a
    // hand-driven merge agree without either recomputing a name.
    std::fs::create_dir_all(&seg_dir)?;
    let plan_path = layout.plan(n);
    let stored = bm_core::assemble::RenderPlan::load(&plan_path);
    // The same setting the inductor plans from, read here because this path
    // owns the plan on this box: the tier decides the extension the take
    // names carry.
    let quality =
        bm_core::assemble::TakeQuality::parse(&Settings::load(&layout.settings()).take_quality);
    let up = bm_core::assemble::reconcile(
        stored.as_ref(),
        bm_core::assemble::RenderPlan::build(n, engine, &units, quality),
        &seg_dir,
    );
    up.plan.save(&plan_path)?;
    // Unit and plan entry are the same position: `build` maps them in order.
    let todo: Vec<_> = up
        .dirty
        .iter()
        .filter_map(|i| {
            let unit = units.get(*i)?;
            let file = up.plan.takes.get(*i)?.file.clone();
            Some((unit, file))
        })
        .collect();
    let total = todo.len();
    // No accent gate: any voice the sidecar can synthesize is allowed. If the
    // engine itself rejects a voice, that failure surfaces from /infer.
    // The render audit log belongs with the rest of the state, not in
    // `output/`. `output/` holds deliverables and nothing else, a machine
    // -readable record of TTS calls sitting beside the mp3s is a stray
    // intermediate in the one directory an operator actually looks at.
    let manifest = layout.bm_state().join("render-manifest.jsonl");
    for (i, (u, dest)) in todo.iter().enumerate() {
        set_progress(
            shared,
            i as f32 / total.max(1) as f32,
            format!("render ch{n} {} ({}/{})", u.tag, i + 1, total),
        );
        let wav = tts
            .infer(&u.text, &u.voice, u.temperature, u.silence_p, engine)
            .await?;
        std::fs::write(seg_dir.join(dest), &wav)?;
        bm_core::assemble::manifest_append(
            &manifest,
            &json!({"chapter": n, "tag": u.tag, "voice": u.voice,
                    "temp": u.temperature, "engine": engine}),
        )?;
    }
    set_progress(
        shared,
        1.0,
        format!("render ch{n} done ({total} new calls)"),
    );
    Ok(total as u64)
}

/// The mix a merge runs with: the offer's own settings, or this box's on the
/// hand-driven path. Grouped because they travel together and because the
/// take list is the plan's, not something the mixer may recompute.
pub(crate) struct MergeJob {
    pub(crate) gap_ms: u32,
    pub(crate) speed: f64,
    pub(crate) on: bm_core::ambience::LayerSwitch,
    /// The chapter's take files in mix order. Empty means "no plan", this box
    /// then names them itself, exactly as before takes were content-addressed.
    pub(crate) takes: Vec<String>,
}

pub(crate) async fn run_merge(
    layout: &Layout,
    n: u32,
    engine: &str,
    job: MergeJob,
    shared: &Shared,
    fetch: Option<(&reqwest::Client, &str)>,
) -> Result<String> {
    let MergeJob {
        gap_ms,
        speed,
        on,
        takes,
    } = job;
    set_progress(shared, 0.1, format!("merge ch{n}"));
    // Pieces, not chapters: a merge runs on any box, so it pulls the takes
    // it lacks from the inductor, whose store holds every completed take
    // instead of requiring them on local disk. Offer-driven only: the
    // hand-driven path carries no take list and mixes what is here.
    if !takes.is_empty() {
        if let Some((http, inductor)) = fetch {
            let seg_dir = layout.seg_dir(engine, n);
            std::fs::create_dir_all(&seg_dir)?;
            for name in &takes {
                let dest = seg_dir.join(name);
                if dest.metadata().map(|m| m.len() > 1000).unwrap_or(false) {
                    continue;
                }
                let url = format!("{inductor}/api/segment?chapter={n}&engine={engine}&name={name}");
                let bytes = http
                    .get(&url)
                    .send()
                    .await
                    .with_context(|| format!("pulling segment {name}"))?
                    .error_for_status()
                    .with_context(|| format!("pulling segment {name}"))?
                    .bytes()
                    .await
                    .with_context(|| format!("pulling segment {name}"))?;
                if bytes.len() <= 1000 {
                    anyhow::bail!("segment {name} pulled {} bytes — not a take", bytes.len());
                }
                std::fs::write(&dest, &bytes).with_context(|| format!("storing segment {name}"))?;
            }
        }
    } // Everything the merge writes goes into one per-chapter scratch directory
      // under `.bm/`, never into `output/`. `assemble` hands back the mp3 (or a
      // wav when ffmpeg is missing) and `publish` renames it out to `output/`,
      // after which the whole scratch directory can go.
    let scratch = layout.scratch_ch(n);
    // The inductor's plan names the files; a worker that recomputed them would
    // look for legacy names and find nothing. An empty list is an old inductor:
    // then `assemble` plans the names itself, exactly as before.
    let takes: Option<Vec<String>> = (!takes.is_empty()).then_some(takes);
    let params = (
        layout.clone(),
        n,
        engine.to_string(),
        gap_ms,
        speed,
        on,
        scratch.clone(),
        takes,
    );
    let assembled = tokio::task::spawn_blocking(move || {
        let (layout, n, engine, gap_ms, speed, on, scratch, takes) = params;
        bm_core::assemble::assemble(
            &layout,
            &layout.script(n),
            &layout.cast(&engine),
            &layout.bible(),
            &layout.seg_dir(&engine, n),
            &scratch,
            n,
            gap_ms,
            on,
            speed,
            &engine,
            &layout.assets(),
            takes.as_deref(),
        )
    })
    .await??;
    let final_path = bm_core::assemble::publish(&assembled, layout, n)?;
    // The product is out of scratch now, so the directory goes. Deliberately
    // left behind when anything above failed: a failed merge's scratch is the
    // only evidence it leaves, and `bm-inductor gc` sweeps stale ones.
    let _ = std::fs::remove_dir_all(&scratch);
    set_progress(shared, 1.0, format!("merge ch{n} done"));
    Ok(final_path.display().to_string())
}

// ---------------------------------------------------------------------------
// worker loop against the inductor API
// ---------------------------------------------------------------------------
