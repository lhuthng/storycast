use super::*;

/// The link check, on a blocking thread.
///
/// **The crawl HTTP client cannot be built or dropped inside a tokio task**, and
/// this is the same constraint the provider works under: `reqwest::blocking`
/// owns a runtime of its own. `spawn_blocking` keeps it off the async worker and
/// keeps that runtime out of the way, without it the command panics on drop
/// before it prints anything.
pub(crate) async fn cmd_check(settings: Settings, url: String, timeout: u64) -> anyhow::Result<()> {
    tokio::task::spawn_blocking(move || cmd_check_blocking(&settings, &url, timeout))
        .await
        .map_err(|e| anyhow::anyhow!("the link check panicked: {e}"))?
}

fn cmd_check_blocking(settings: &Settings, url: &str, timeout: u64) -> anyhow::Result<()> {
    let opts = bm_core::crawl::probe::Options {
        // The workspace's own agent and headers, so a check goes out exactly as
        // a crawl would, including a session cookie that is part of the setup
        // being validated.
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
    // the person who is about to paste this URL into `settings.json` is the one
    // who needs to know which script to put there, and a check that says "ok"
    // without it has left the actual work undone.
    if let Some(site) = bm_core::crawl::for_url(url) {
        print!("{}", bm_core::crawl::known::note(site));
    }

    // A non-`Ok` verdict is a **failed check**, so a script can gate on it, but
    // the reason is always printed first, because "exit 1" on its own helps
    // nobody choose between a cookie, a browser user agent, and a different site.
    if !check.verdict.crawlable() {
        if check.verdict == bm_core::crawl::probe::Verdict::Cloudflare {
            // Say what is actually true, because the plausible-sounding wrong
            // answer here costs an afternoon of someone's time.
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
///
/// Deliberately not routed through the control API: a request to a running
/// inductor is a request to the *pipeline*, and the whole point of this command
/// is to ask the question without waking it. It reads the chapter text and the
/// bible, calls [`bm_core::digest::analyze_chapter`], the same function the
/// digest worker calls, and prints the result.
/// `crawl`: the worker's own fetch, run here, for a book that needs no
/// cluster to be read — an EPUB is a lookup, not a website.
///
/// The chapter index is built exactly the way a run builds it (same
/// fingerprint, same `data/crawl-index.json`, so a later `serve` reuses what
/// this wrote), and each chapter lands at `data/chapters/chNN.txt`, the file
/// reconcile plans from. A block or an absent chapter is reported and
/// skipped, not fatal: a book whose tail is missing should still get the
/// chapters it has.
pub(crate) async fn cmd_crawl(
    layout: &Layout,
    settings: &Settings,
    start: u32,
    count: u32,
    force: bool,
) -> anyhow::Result<()> {
    use anyhow::Context as _;
    // Everything — the index build included — runs on a blocking thread: the
    // crawl host builds a blocking HTTP client even when an EPUB never
    // fetches, and creating or dropping that client inside the tokio runtime
    // is the panic this command must not have.
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
    // `L`) decides, not the workspace settings mirror. A named flag must be
    // a provider the file knows or a legacy backend name — anything else
    // would route OpenAI-compatible by construction and fail confusingly.
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
    // `generate` routes by slot while progress lines name the provider id.
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
    /// flag was not given, which leaves the provider's own entry alone.
    pub(crate) model_api: Option<String>,
    /// Where the accepted chapters are reported. Defaults to this machine.
    pub(crate) inductor: Option<String>,
    /// Re-asks per refused round. See `--retries`.
    pub(crate) retries: u32,
    pub(crate) dry_run: bool,
}

/// Resolve the analyzer, model and endpoint a headless run will use.
///
/// Shared by `backup` and `excerpts`, so the two headless callers cannot
/// disagree about which provider answered or which endpoint it reached. The
/// flag wins; otherwise the active provider in `.bm/llm.json`; the `--api`
/// address only decides when neither says (a gateway at a name of its own).
/// The overlay then carries the active model, endpoint AND backend slot, so
/// `generate` routes by slot while progress lines name the provider id, and the
/// flag overrides land after it — `--model`/`--api` win.
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
    // reads, so one `--model` and one `--api` cover every backend.
    if let Some(model) = model.filter(|m| !m.trim().is_empty()) {
        match backend.as_str() {
            "openai" => settings.openrouter_model = model,
            "gemini" => settings.analyze_models = vec![model],
            _ => settings.local_model = model,
        }
    }
    // `--api` lands on the endpoint field the chosen slot actually reads — both
    // wires append their own path to it — so one flag covers every backend.
    // Leaving the Gemini slot out meant `--analyzer gemini --api
    // https://gateway.example` quietly called Google instead.
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
///
/// Chapters are digested **in order**, and the run stops at the first failure:
/// each chapter's bible delta lands on top of its predecessor's, so skipping
/// ahead would merge deltas out of order. Nothing is written here, the
/// inductor is the single writer of the bible and the script, and it does that
/// when the report lands.
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
    // point of this command's arguments.** `model_api` is where the two digest
    // calls go, a model service. The report goes to an *inductor*, which is
    // this machine's own control API: it is where the ledger and the bible
    // live, exactly as it is for `make tui`.
    let api = inductor.unwrap_or_else(|| format!("http://127.0.0.1:{}", settings.control_port));
    let model_api = model_api.map(|a| a.trim_end_matches('/').to_string());
    let (analyzer, settings) =
        resolve_analyzer(layout, settings, analyzer, model, model_api.as_deref())?;
    let backend = settings.analyzer_backend.clone();
    // Where a digest may begin is not a free choice: the deltas have to land in
    // chapter order, so the only legal start is the chapter after the last one
    // with a script on disk. Asking for anything else would merge this book's
    // bible out of order, which is why the guess is the default rather than a
    // number the operator has to remember.
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
        // chapter that has a digest row. Not "every chapter file on disk":
        // `data/chapters/` can hold text for a range this run never enqueued
        // (200 files against a 100-chapter ledger), and digesting those would
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
    // included: a constant here printed Google's host while the requests went
    // to the operator's gateway, which is a bug in the one line a human reads
    // to work out where a backup digest is going.
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
    // in a second, before the first chapter, so a missing inductor costs a
    // message instead of a whole range.
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
            // round 2 was rendered against and what its answer is checked
            // against, so it has to be carried forward.
            if let Some(c) = next.cast() {
                cast = Some(c.clone());
            }

            // The part is in the line, not just the round: a chapter staged in
            // four parts asks four round 1s, and the log has to say which one is
            // waiting — otherwise a backup run through a long chapter reads like
            // the same prompt four times.
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
            // the complaint is the instruction: each retry is a different ask
            // because the previous one is named in it. Bounded, because a model
            // that cannot satisfy the gate is a chapter to look at, but not
            // after a single refusal, which is what cost ch79.
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
                // `call`, so the automatic path's own `dump_raw` never sees this
                // answer — and a `--dry-run` has nothing else to inspect: it
                // reports a segment count and exits. Without this, asking what
                // the model actually said about a chapter means paying for the
                // round again. Keyed by the repair attempt too, because the
                // answer that was accepted is not the answer that was refused,
                // and it is the refused one that explains the refusal.
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
                // exact bytes it was given are next to it, and rebuilding that
                // prompt means rebuilding the bible and the view by hand.
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
/// have to be threaded one by one.
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
///
/// The field is the one cross-chapter memory the analyzer gets: chapter `n+1`
/// is handed chapter `n`'s excerpt under `---PREVIOUSLY---`, so a book whose
/// early chapters were digested before the field existed resolves every late
/// chapter against silence. Re-digesting to recover it would re-decide every
/// speaker and invalidate segments, so this asks only the excerpt question —
/// the digest's own rule, bible and window — and writes the answer as one field
/// of the existing script.
///
/// **Chapters run in ascending order and each write is visible to the next
/// prompt**, so the run repairs the chain, not just the field: chapter `n`'s
/// fresh excerpt is what chapter `n+1` is asked against. A chapter that fails
/// is named and skipped rather than stopping the sweep — an excerpt is a soft
/// field, and a missing one costs one chapter's memory, never the run.
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
    // excerpts are keyed to chapter files, and a chapter text is what the prompt
    // reads, so a chapter with no text is nothing to ask about.
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
