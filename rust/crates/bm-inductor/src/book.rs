use super::*;

/// The link check, on a blocking thread.
pub(crate) async fn cmd_check(settings: Settings, url: String, timeout: u64) -> anyhow::Result<()> {
    tokio::task::spawn_blocking(move || cmd_check_blocking(&settings, &url, timeout))
        .await
        .map_err(|e| anyhow::anyhow!("the link check panicked: {e}"))?
}

fn cmd_check_blocking(settings: &Settings, url: &str, timeout: u64) -> anyhow::Result<()> {
    let opts = bm_core::crawl::probe::Options {
        // The workspace's own agent and headers, so a check goes out exactly as
        user_agent: settings.crawl.user_agent.clone(),
        headers: settings.crawl.headers.clone(),
        timeout_secs: timeout,
    };
    let check = bm_core::crawl::probe::probe(url, &opts)?;

    println!("{}", check.url);
    if check.final_url != check.url {
        println!("  -> {}", check.final_url);
    }
    println!(
        "  HTTP {}  ·  {} bytes  ·  {} bytes of prose",
        check.status, check.bytes, check.text_bytes
    );
    if !check.guess.is_empty() {
        println!("  title: {}", check.guess);
    }
    let mark = if check.verdict.crawlable() {
        "ok"
    } else if check.retryable() {
        "blocked (retryable)"
    } else {
        "blocked"
    };
    println!("  {mark}: {}", check.detail);

    // A recognised site gets its crawler named, whether the check passed or not:
    if let Some(site) = bm_core::crawl::for_url(url) {
        print!("{}", bm_core::crawl::known::note(site));
    }

    // A non-`Ok` verdict is a **failed check**, so a script can gate on it, but
    if !check.verdict.crawlable() {
        if check.verdict == bm_core::crawl::probe::Verdict::Cloudflare {
            // Say what is actually true, because the plausible-sounding wrong
            println!(
                "\n  Cloudflare is refusing this client, and nothing in this repo\n  \
                 bypasses that: no TLS-fingerprint spoofing, no browser engine,\n  \
                 no challenge solver. The crawler speaks HTTP/1.1 with rustls and\n  \
                 a header-shaped request, and some sites refuse that on the\n  \
                 fingerprint alone.\n\n  \
                 Two things are worth trying, in this order:\n    \
                 1. a real browser user agent — the default \"Mozilla/5.0\" is\n       \
                 thin, and \"crawl\".\"user_agent\" is a one-line change;\n    \
                 2. a session cookie: open the page in a browser, solve the\n       \
                 challenge, copy the cf_clearance cookie into\n       \
                 \"crawl\".\"headers\", and run this command again to confirm it."
            );
        }
        std::process::exit(1);
    }
    Ok(())
}

/// `digest`, the analyzer's answer for one chapter, and nothing else.
pub(crate) async fn cmd_crawl(
    layout: &Layout,
    settings: &Settings,
    start: u32,
    count: u32,
    force: bool,
) -> anyhow::Result<()> {
    use anyhow::Context as _;
    // Everything — the index build included — runs on a blocking thread: the
    let layout = layout.clone();
    let settings = settings.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let index = bm_core::crawl::chapter_index(&layout, &settings, start, count, force)?;
        let spec = bm_core::crawl::provider::spec_from_settings(&layout, &settings);
        let how = if spec.engine.is_empty() {
            "built-in fetcher".to_string()
        } else {
            format!("{} {}", spec.engine, spec.script)
        };
        println!(
            "crawl: {how}, chapters {start}..{} (index: data/crawl-index.json)",
            start + count
        );
        let mut fetched = 0usize;
        let mut absent = 0usize;
        let mut blocked = 0usize;
        for n in start..start.saturating_add(count) {
            if index.is_absent(n) {
                println!("ch{n}: not in the index — skipping");
                absent += 1;
                continue;
            }
            let url = index.url(n).map(str::to_string);
            let crawled = bm_core::crawl::Provider::new(&spec).crawl(n, url.as_deref(), 1)?;
            for line in &crawled.log {
                println!("ch{n}: {line}");
            }
            match crawled.outcome {
                bm_core::crawl::CrawlOutcome::Text { text, .. } => {
                    bm_core::atomic_write(&layout.chapter_txt(n), &text)?;
                    println!("ch{n}: crawled ({} bytes)", text.len());
                    fetched += 1;
                }
                bm_core::crawl::CrawlOutcome::Absent { reason } => {
                    println!("ch{n}: absent — {reason}");
                    absent += 1;
                }
                bm_core::crawl::CrawlOutcome::Blocked(b) => {
                    println!("ch{n}: blocked [{:?}] — {}", b.class, b.detail);
                    blocked += 1;
                }
            }
        }
        println!(
            "crawl: {fetched} fetched, {absent} absent, {blocked} blocked — `serve` will plan what landed"
        );
        Ok(())
    })
    .await
    .context("the crawl thread panicked")?
}

pub(crate) async fn cmd_digest(
    layout: &Layout,
    settings: &Settings,
    chapter: u32,
    analyzer: Option<&str>,
    write: bool,
    json: bool,
) -> anyhow::Result<()> {
    let txt = layout.chapter_txt(chapter);
    if !txt.is_file() {
        anyhow::bail!(
            "no chapter text at {} — crawl it first (`serve`), or pass a chapter that exists",
            txt.display()
        );
    }
    // The flag wins; otherwise the active provider in `.bm/llm.json` (TUI:
    let llm = bm_core::config::LlmConfig::load_or_seed(&layout.root, settings);
    if let Some(flag) = analyzer {
        if llm.backend_for(flag).is_none() {
            anyhow::bail!("unknown analyzer {flag:?} — pick one with `tui` (L)");
        }
    }
    let (active, asettings) = llm.offer_analyzer(settings);
    let analyzer = analyzer.unwrap_or(active.as_str()).to_string();
    if analyzer.is_empty() {
        anyhow::bail!("no LLM provider is active — add a key with `tui` (L), then retry");
    }
    // The overlay carries the active model, endpoint AND backend slot, so
    let settings = settings.with_analyzer_settings(&asettings);
    let bible = bm_core::digest::load_bible(&layout.bible());
    let mut progress = |_f: f32, s: String| eprintln!("{s}");
    let out = bm_core::digest::analyze_chapter(
        layout,
        chapter,
        &bible,
        &settings,
        &analyzer,
        &mut progress,
    )
    .await?;

    for line in &out.log {
        eprintln!("{line}");
    }
    for w in &out.warnings {
        eprintln!("WARN: {w}");
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&out.script)?);
    } else {
        let sounds = out
            .script
            .get("segments")
            .and_then(|s| s.as_array())
            .map(|segs| {
                segs.iter()
                    .filter(|i| bm_core::util::is_sound_item(i))
                    .count()
            })
            .unwrap_or(0);
        let lines = out.segments.saturating_sub(sounds);
        println!(
            "ch{chapter} via {analyzer}: {lines} lines, {sounds} sound items, {} warnings",
            out.warnings.len()
        );
    }
    if write {
        let path = layout.script(chapter);
        bm_core::atomic_write(&path, &serde_json::to_string_pretty(&out.script)?)?;
        eprintln!(
            "wrote {} — caches NOT invalidated and nothing requeued, so segments on disk \
             may no longer match this script; `serve` reconciles that when you next run it",
            path.display()
        );
    }
    Ok(())
}

/// The backup runner's options, bundled so one call carries the whole request.
pub(crate) struct BackupOpts {
    pub(crate) start: Option<u32>,
    pub(crate) through: Option<u32>,
    pub(crate) analyzer: Option<String>,
    pub(crate) model: Option<String>,
    /// The **model service's** base URL, not the inductor's. `None` means the
    pub(crate) model_api: Option<String>,
    /// Where the accepted chapters are reported. Defaults to this machine.
    pub(crate) inductor: Option<String>,
    /// Re-asks per refused round. See `--retries`.
    pub(crate) retries: u32,
    pub(crate) dry_run: bool,
}

/// Resolve the analyzer, model and endpoint a headless run will use.
fn resolve_analyzer(
    layout: &Layout,
    mut settings: Settings,
    analyzer: Option<String>,
    model: Option<String>,
    model_api: Option<&str>,
) -> anyhow::Result<(String, Settings)> {
    let llm = bm_core::config::LlmConfig::load_or_seed(&layout.root, &settings);
    let api_hint = model_api.unwrap_or_default();
    let (active, _) = llm.offer_analyzer(&settings);
    let analyzer = match analyzer {
        Some(a) => a.to_string(),
        None if !active.is_empty() => active,
        None if api_hint.contains("openrouter") => "openrouter".to_string(),
        None if api_hint.contains("googleapis") => "gemini".to_string(),
        None => settings.analyzer.clone(),
    };
    if llm.backend_for(&analyzer).is_none() {
        anyhow::bail!("unknown analyzer {analyzer:?} — pick one with `tui` (L)");
    }
    if analyzer.is_empty() {
        anyhow::bail!("no LLM provider is active — add a key with `tui` (L), then retry");
    }
    let backend = llm
        .backend_for(&analyzer)
        .expect("validated above: the analyzer names a provider or legacy slot");
    settings = settings.with_analyzer_settings(&llm.offer_analyzer(&settings).1);
    settings.analyzer_backend = backend.clone();
    // The model and the endpoint land on whichever fields the chosen service
    if let Some(model) = model.filter(|m| !m.trim().is_empty()) {
        match backend.as_str() {
            "openai" => settings.openrouter_model = model,
            "gemini" => settings.analyze_models = vec![model],
            _ => settings.local_model = model,
        }
    }
    // `--api` lands on the endpoint field the chosen slot actually reads — both
    if let Some(url) = model_api {
        match backend.as_str() {
            "openai" => settings.openrouter_url = url.to_string(),
            "gemini" => settings.gemini_url = url.to_string(),
            _ => {}
        }
    }
    Ok((analyzer, settings))
}

/// `backup`, be the digestor while the cluster's analyzer has no quota.
pub(crate) async fn cmd_backup(
    layout: &Layout,
    settings: Settings,
    opts: BackupOpts,
) -> anyhow::Result<()> {
    let BackupOpts {
        start,
        through,
        analyzer,
        model,
        model_api,
        inductor,
        retries,
        dry_run,
    } = opts;
    // **The two addresses are different things, and the difference is the whole
    let api = inductor.unwrap_or_else(|| format!("http://127.0.0.1:{}", settings.control_port));
    let model_api = model_api.map(|a| a.trim_end_matches('/').to_string());
    let (analyzer, settings) =
        resolve_analyzer(layout, settings, analyzer, model, model_api.as_deref())?;
    let backend = settings.analyzer_backend.clone();
    // Where a digest may begin is not a free choice: the deltas have to land in
    let start = match start {
        Some(n) => n,
        None => {
            let mut n = 1u32;
            while layout.digested(n) {
                n += 1;
            }
            n
        }
    };
    let last = match through {
        Some(t) => t,
        // To the end of the **book the ledger knows about**, the highest
        // invent a book nobody asked for. A bare `backup` is "carry on with the
        // book", not "do one chapter and stop".
        None => {
            let from_ledger = bm_core::read_json::<serde_json::Value>(&layout.ledger())
                .ok()
                .and_then(|doc| {
                    doc.get("tasks")
                        .and_then(|t| t.as_array())
                        .and_then(|tasks| {
                            tasks
                                .iter()
                                .filter(|t| {
                                    t.get("stage").and_then(|s| s.as_str()) == Some("digest")
                                })
                                .filter_map(|t| t.get("chapter").and_then(|c| c.as_u64()))
                                .max()
                        })
                })
                .map(|max| max as u32);
            match from_ledger {
                Some(max) => max,
                None => {
                    let mut n = start;
                    while layout.chapter_txt(n + 1).is_file() {
                        n += 1;
                    }
                    n
                }
            }
        }
    };
    if last < start {
        anyhow::bail!("--through {last} is before ch{start}");
    }
    if !layout.chapter_txt(start).is_file() {
        anyhow::bail!(
            "ch{start} has no chapter text at {} — crawl it first",
            layout.chapter_txt(start).display()
        );
    }
    // Every arm reads the settings the run will actually use, `--api`
    let endpoint = match backend.as_str() {
        "openai" => settings.openrouter_url.clone(),
        "ollama" => settings.ollama_url.clone(),
        _ => settings.gemini_url.clone(),
    };
    eprintln!(
        "backup digest: ch{start}..ch{last} via {analyzer} at {endpoint} → reporting to {api}"
    );
    let http = reqwest::Client::new();
    // The backend is a precondition, the same way it is for `make tui`: named
    manual::require_inductor(&api, &http)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    let digested = |n: u32| layout.digested(n);

    let mut done = 0u32;
    for n in start..=last {
        let mut next = manual::open(layout, &settings.engine, n, &digested)
            .map_err(|e| anyhow::anyhow!("ch{n}: {e}"))?;
        let mut cast: Option<serde_json::Value> = None;

        loop {
            let (round, prompt) = match &next {
                manual::Next::Prompt { round, text, .. } => (*round, text.clone()),
                manual::Next::Done(outcome) => {
                    if dry_run {
                        println!(
                            "ch{n}: {} segments (dry run, not reported)",
                            outcome.segments
                        );
                    } else {
                        let line = manual::report_or_stop(
                            &api,
                            &http,
                            n,
                            &outcome.script,
                            &outcome.delta,
                            format!("digest ch{n} by backup"),
                        )
                        .await
                        .map_err(|e| anyhow::anyhow!("ch{n}: {e}"))?;
                        println!("ch{n}: {line}");
                    }
                    done += 1;
                    break;
                }
            };
            // Round 1's validated cast rides on round 2's prompt; it is what
            if let Some(c) = next.cast() {
                cast = Some(c.clone());
            }

            // The part is in the line, not just the round: a chapter staged in
            let (part, part_slug) = match next.part() {
                Some(part) if part.total > 1 => (
                    format!(" (part {}/{})", part.index, part.total),
                    format!("-part{}", part.index),
                ),
                _ => (String::new(), String::new()),
            };
            eprintln!(
                "ch{n}: {}{part} prompt ready ({} bytes) via {analyzer}",
                round.as_str(),
                prompt.len()
            );
            // A refused round is re-asked with the validator's own words, and
            let mut accepted = None;
            let mut complaint = String::new();
            for attempt in 0..=retries {
                let asked = if attempt == 0 {
                    prompt.clone()
                } else {
                    eprintln!(
                        "ch{n}: {}{part} answer refused ({complaint}) — repair {attempt}/{retries}",
                        round.as_str()
                    );
                    manual::repair_prompt(&prompt, &complaint)
                };
                let answer = manual::ask(&asked, &analyzer, &settings)
                    .await
                    .map_err(|e| anyhow::anyhow!("ch{n} round {}: {e}", round.as_str()))?;
                // Kept, not just parsed. `manual::ask` is outside the worker's
                let tag = if attempt == 0 {
                    format!("backup-{}-{}", round.as_str(), part_slug)
                } else {
                    format!(
                        "backup-{}{}-{}-repair{}",
                        round.as_str(),
                        part_slug,
                        n,
                        attempt
                    )
                };
                bm_core::digest::dump_raw(layout, &tag, &answer);
                // The prompt beside the answer. An answer on disk answers "what
                // did it say"; a wrong answer can only be argued with once the
                if std::env::var("BM_DIGEST_RAW").is_ok() {
                    let path = layout.data().join(format!(".last-{tag}-prompt.txt"));
                    let _ = bm_core::atomic_write(&path, &asked);
                    eprintln!("prompt -> {}", path.display());
                }
                match manual::advance(layout, &settings.engine, n, round, &answer, cast.as_ref()) {
                    Ok(step) => {
                        accepted = Some(step);
                        break;
                    }
                    Err(e) => {
                        complaint = e;
                        if attempt == retries {
                            anyhow::bail!(
                                "ch{n} round {} refused {attempts} time(s); last: {complaint}",
                                round.as_str(),
                                attempts = attempt + 1
                            );
                        }
                    }
                }
            }
            next = accepted.expect("the loop either lands or bails");
        }
    }
    if dry_run {
        println!("{done} chapter(s) digested, nothing reported (--dry-run)");
    } else {
        println!("{done} chapter(s) reported to {api}");
    }
    Ok(())
}

/// What `excerpts` was asked to do, gathered so the front end's flags do not
pub(crate) struct ExcerptOpts {
    pub(crate) start: u32,
    pub(crate) through: Option<u32>,
    pub(crate) analyzer: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) model_api: Option<String>,
    pub(crate) retries: u32,
    pub(crate) force: bool,
    pub(crate) dry_run: bool,
}

/// Backfill the chapter excerpts the digest's cast pass would have written.
pub(crate) async fn cmd_excerpts(
    layout: &Layout,
    settings: Settings,
    opts: ExcerptOpts,
) -> anyhow::Result<()> {
    let ExcerptOpts {
        start,
        through,
        analyzer,
        model,
        model_api,
        retries,
        force,
        dry_run,
    } = opts;
    let model_api = model_api.map(|a| a.trim_end_matches('/').to_string());
    let (analyzer, settings) =
        resolve_analyzer(layout, settings, analyzer, model, model_api.as_deref())?;
    let endpoint = match settings.analyzer_backend.as_str() {
        "openai" => settings.openrouter_url.clone(),
        "ollama" => settings.ollama_url.clone(),
        _ => settings.gemini_url.clone(),
    };
    // To the end of the book on disk, the same way `crawl` counts a range: the
    let last = match through {
        Some(t) => t,
        None => {
            let mut n = start;
            while layout.chapter_txt(n + 1).is_file() {
                n += 1;
            }
            n
        }
    };
    if last < start {
        anyhow::bail!("--through {last} is before ch{start}");
    }
    eprintln!(
        "excerpt backfill: ch{start}..ch{last} via {analyzer} at {endpoint}{}",
        if dry_run { " (dry run)" } else { "" }
    );

    let mut written = 0u32;
    let mut skipped = 0u32;
    let mut failed = 0u32;
    for n in start..=last {
        let chapter = layout.chapter_txt(n);
        if !chapter.is_file() {
            eprintln!("ch{n}: no chapter text at {} — skipped", chapter.display());
            skipped += 1;
            continue;
        }
        let current = bm_core::read_json::<serde_json::Value>(&layout.script(n))
            .ok()
            .and_then(|s| {
                s.get("excerpt")
                    .and_then(|e| e.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_default();
        if !force && !current.trim().is_empty() {
            eprintln!(
                "ch{n}: excerpt already present — skipped ({})",
                current.chars().count()
            );
            skipped += 1;
            continue;
        }
        let text = std::fs::read_to_string(&chapter)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", chapter.display()))?;
        let prompt = bm_core::digest::build_excerpt_prompt(layout, n, &text)
            .map_err(|e| anyhow::anyhow!("ch{n}: {e:#}"))?;

        let mut excerpt: Option<String> = None;
        let mut complaint = "the answer contained no excerpt".to_string();
        for attempt in 0..=retries {
            let asked = if attempt == 0 {
                prompt.clone()
            } else {
                manual::repair_prompt(&prompt, &complaint)
            };
            let answer = manual::ask(&asked, &analyzer, &settings)
                .await
                .map_err(|e| anyhow::anyhow!("ch{n}: {e}"))?;
            bm_core::digest::dump_raw(layout, &format!("excerpt-{n}"), &answer);
            match bm_core::digest::parse_excerpt(&answer) {
                Some(found) => {
                    excerpt = Some(found);
                    break;
                }
                None => complaint = "the answer contained no excerpt".to_string(),
            }
        }
        let Some(excerpt) = excerpt else {
            eprintln!("ch{n}: no excerpt after {} ask(s) — skipped", retries + 1);
            failed += 1;
            continue;
        };
        if dry_run {
            println!(
                "ch{n}: {} chars (dry run, not written)\n  {excerpt}",
                excerpt.chars().count()
            );
        } else {
            bm_core::digest::write_excerpt(layout, n, &excerpt)
                .map_err(|e| anyhow::anyhow!("ch{n}: {e:#}"))?;
            println!("ch{n}: {} chars written", excerpt.chars().count());
        }
        written += 1;
    }
    println!("excerpt backfill done: {written} filled, {skipped} already present, {failed} failed");
    Ok(())
}
