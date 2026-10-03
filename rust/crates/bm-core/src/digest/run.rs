use super::attribution::build_attribution_prompt;
use super::attribution::Continuity;
use super::excerpt::previous_excerpts;
use super::manual::vocabulary;
use super::manual::Vocabulary;
use super::parse::merge_rounds;
use super::parse::parse_attribution;
use super::parse::parse_staged_script;
use super::parts::merge_contexts;
use super::parts::merge_scripts;
use super::parts::part_lines;
use super::parts::part_of;
use super::parts::part_prefix;
use super::parts::part_suffix;
use super::parts::plan_detail;
use super::parts::plan_line;
use super::parts::round_label;
use super::parts::sound_gap;
use super::parts::stage_part;
use super::parts::window_text;
use super::parts::Part;
use super::parts::PartCtx;
use super::parts::Parts;
use super::prepare::prepare_chapter;
use super::prompts::build_staging_prompt;
use super::prompts::PreparedChapter;
use super::quotes::effective_text;
use super::quotes::quote_findings;
use super::quotes::repair_quotes;
use super::sound_fields::gap_block_p;
use super::*;
/// **One G**: build the prompt, spend a call from the chapter's budget, ask,
pub(crate) async fn run_g(
    round: Round,
    ctx: &mut PartCtx<'_>,
    context: Option<&Value>,
    calls: &mut GCalls,
) -> Result<Value, Fail> {
    let suffix = part_suffix(ctx.part);
    let mut complaint: Option<String> = None;
    for attempt in 0..2 {
        let (from, to) = ctx.band(round);
        // Re-rendered per attempt rather than carried, for the reason
        let prompt = match round {
            Round::Attribution => {
                let previously = previous_excerpts(ctx.layout, ctx.n);
                build_attribution_prompt(
                    ctx.layout,
                    ctx.bible,
                    ctx.slice,
                    ctx.continuity,
                    previously.as_deref(),
                )
            }
            Round::Staging => build_staging_prompt(
                ctx.layout,
                &ctx.settings.engine,
                ctx.bible,
                context.expect("staging is only ever run against a cast"),
                ctx.slice,
                ctx.continuity,
            ),
        };
        let prompt = match prompt {
            Ok(prompt) => prompt,
            Err(e) => return Err(Fail::Dead(e)),
        };
        if let Err(e) = calls.spend(round.as_str()) {
            return Err(Fail::Dead(e));
        }
        ctx.report(
            from,
            round_label(ctx.n, ctx.analyzer, round.as_str(), ctx.part),
        );
        let raw = if attempt == 0 {
            generate_retrying(
                &prompt,
                ctx.analyzer,
                ctx.settings,
                &mut *ctx.progress,
                from,
                to,
            )
            .await
        } else {
            // The one repair, in place, carrying what the gate said.
            repair_once(
                &prompt,
                &anyhow::anyhow!(complaint.clone().unwrap_or_default()),
                ctx.analyzer,
                ctx.settings,
            )
            .await
        };
        let raw = match raw {
            Ok(raw) => raw,
            Err(e) => return Err(Fail::Dead(e)),
        };
        let dump = if attempt == 0 {
            format!("digest-{round}{suffix}")
        } else {
            format!("digest-{round}{suffix}-retry")
        };
        dump_raw(ctx.layout, &dump, &raw);
        let judged = match round {
            Round::Attribution => {
                parse_attribution(&raw, ctx.bible, ctx.slice, ctx.continuity.is_some())
            }
            Round::Staging => parse_staged_script(
                &raw,
                ctx.bible,
                context.expect("staging is only ever run against a cast"),
                ctx.slice,
                ctx.vocab,
            ),
        };
        match judged {
            Ok(value) => return Ok(value),
            Err(c) => match route(c.blame, round, attempt) {
                // Someone else's fault: the answer is dropped without a second
                Route::Back => {
                    return Err(Fail::Back(format!(
                        "{}: {} (the {} step owns that)",
                        round.as_str(),
                        c.why,
                        c.blame.as_str()
                    )))
                }
                Route::Die => {
                    let dump = ctx.layout.data().join(".last-analyze-raw.json");
                    let _ = atomic_write(&dump, &raw);
                    return Err(Fail::Dead(anyhow::anyhow!(
                        "digest {} invalid ({}); raw saved to {}",
                        round.as_str(),
                        c.why,
                        dump.display()
                    )));
                }
                Route::Retry => {
                    ctx.report(
                        to,
                        format!(
                            "{}{} gate said {why}, asking for one repair",
                            part_prefix(ctx.part),
                            round.as_str(),
                            why = c.why,
                        ),
                    );
                    complaint = Some(c.why);
                }
            },
        }
    }
    // The loop returns on attempt 0 (accepted, or handed back) or attempt 1
    unreachable!("run_g leaves on its first or second attempt")
}

/// Re-ask one part's staging round with a gate's complaint appended.
#[allow(clippy::too_many_arguments)]
async fn reask_staging(
    layout: &Layout,
    analyzer: &str,
    settings: &Settings,
    bible: &Value,
    vocab: &Vocabulary,
    slice: &PreparedChapter,
    context: &Value,
    index: usize,
    total: usize,
    plot: &[String],
    part: Option<(usize, usize)>,
    complaint: &str,
    calls: &mut GCalls,
) -> Result<Value> {
    let continuity = (total > 1).then_some(Continuity { index, total, plot });
    let prompt = build_staging_prompt(
        layout,
        &settings.engine,
        bible,
        context,
        slice,
        continuity.as_ref(),
    )?;
    // A G like any other: it spends from the same chapter budget, so a chapter
    calls.spend(Round::Staging.as_str())?;
    let again = repair_once(
        &prompt,
        &anyhow::anyhow!(complaint.to_string()),
        analyzer,
        settings,
    )
    .await?;
    dump_raw(
        layout,
        &format!("digest-staging-retry{}", part_suffix(part)),
        &again,
    );
    parse_staged_script(&again, bible, context, slice, vocab).map_err(|e| {
        let dump = layout.data().join(".last-analyze-raw.json");
        let _ = atomic_write(&dump, &again);
        anyhow::anyhow!(
            "digest {complaint} not fixed by one repair ({e}); raw saved to {}",
            dump.display()
        )
    })
}

/// Ask the analyzer for one chapter through two constrained passes, once per
pub async fn analyze_chapter(
    layout: &Layout,
    n: u32,
    bible: &Value,
    settings: &Settings,
    analyzer: &str,
    progress: &mut (dyn FnMut(f32, String) + Send),
) -> Result<DigestOutcome> {
    let chapter_path = layout.chapter_txt(n);
    let original = std::fs::read_to_string(&chapter_path)
        .with_context(|| format!("reading {}", chapter_path.display()))?;
    // One budget for every G this chapter runs, including the phrase pass below.
    let mut calls = GCalls::new(n);
    // The gate, before anything expensive. A sidecar from an earlier repair
    let text = effective_text(layout, n, &original);
    let text = match quote_findings(&text).first() {
        None => text,
        Some(_) => match repair_quotes(
            layout,
            n,
            &text,
            &quote_findings(&text),
            analyzer,
            settings,
            &mut calls,
            progress,
        )
        .await?
        {
            Some(fixed) => fixed,
            None => original.clone(),
        },
    };
    let prepared = prepare_chapter(&text);
    let vocab = vocabulary(layout)?;
    let windows = plan_windows(&prepared, &settings.digest);
    let total = windows.len();
    // The parts are known, so the rest of the budget can be sized against them:
    calls.allow_parts(total);
    let mut parts = Parts::open(layout, n, &text, bible, &windows, settings);
    if total > 1 {
        // Before any call, because the number of calls is the operator's
        progress(0.05, plan_line(n, analyzer, &windows, &prepared, settings));
        progress(0.06, plan_detail(&windows, &prepared));
        if parts.len() > 0 {
            progress(
                0.07,
                format!(
                    "resuming at part {} of {total} from the checkpoint",
                    parts.len() + 1
                ),
            );
        }
    }

    // 0.08..0.82, spent evenly across the parts, so the bar moves at the same
    let per = 0.74 / total as f32;
    for (i, window) in windows.iter().enumerate().skip(parts.len()) {
        let at = part_of(i, total);
        let from = 0.08 + i as f32 * per;
        let mid = from + per * 0.45;
        let to = from + per;
        let summaries = parts.summaries();
        let continuity = (total > 1).then_some(Continuity {
            index: i,
            total,
            plot: &summaries,
        });
        let slice = window.prepared(&prepared);
        let (context, script) = stage_part(
            layout,
            n,
            analyzer,
            settings,
            bible,
            &vocab,
            &slice,
            continuity.as_ref(),
            at,
            &mut calls,
            progress,
            from,
            mid,
            to,
        )
        .await?;
        // Stored only once both rounds parsed and validated, so what a restart
        let summary = context
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        parts.push(Part {
            from: window.from,
            to: window.to,
            summary,
            context,
            script,
        })?;
    }

    // ---- the sound-design gates, once every part is staged -----------------
    let what = if total == 1 { "chapter" } else { "part" };
    // Each part's own prose, for rule 2 — or the chapter's own text when it did
    let texts: Vec<String> = if total == 1 {
        vec![text.clone()]
    } else {
        windows.iter().map(|w| window_text(&prepared, w)).collect()
    };
    let mut soft_released: Option<String> = None;
    let mut attempt = 0u32;
    loop {
        let scripts: Vec<&Value> = parts.done.iter().map(|p| &p.script).collect();
        let scopes: Vec<&str> = texts.iter().map(String::as_str).collect();
        let Some((gap, owner)) = sound_gap(&scripts, &scopes, &vocab.injects, what) else {
            break;
        };
        let at = part_of(owner, total);
        progress(
            0.88,
            format!(
                "{}sound design incomplete, asking for repairs: {gap}",
                part_prefix(at)
            ),
        );
        // 0.90, 0.68, 0.51, then 0.38: below a coin flip, accept.
        if gap_block_p(attempt) < 0.5 {
            let msg = format!(
                "ch{n} {}sound-design gate soft-released ({gap})",
                part_prefix(at)
            );
            progress(0.88, format!("WARN: {msg}"));
            eprintln!("WARN: {msg}");
            soft_released = Some(msg);
            break;
        }
        let summaries = parts.summaries();
        let slice = windows[owner].prepared(&prepared);
        let script = reask_staging(
            layout,
            analyzer,
            settings,
            bible,
            &vocab,
            &slice,
            &parts.done[owner].context,
            owner,
            total,
            &summaries,
            at,
            &gap,
            &mut calls,
        )
        .await?;
        parts.done[owner].script = script;
        attempt += 1;
    }

    let (context, conflicts) = merge_contexts(&parts.done);
    let script = merge_scripts(parts.done.iter().map(|p| &p.script));
    let mut outcome = assemble_outcome(bible, &context, &script, &text)?;
    // A disagreement between parts is the operator's to settle, so it is said
    for w in conflicts {
        outcome.log.push(format!("   WARN: {w}"));
        outcome.warnings.push(w);
    }
    // `warnings` is dropped by the agent today; `log` is what the worker
    if let Some(w) = soft_released {
        outcome.log.push(format!("   WARN: {w}"));
        outcome.warnings.push(w);
    }
    if total > 1 {
        // The plan, in the log the worker prints: the only place the cost of a
        for (i, line) in part_lines(&windows, &prepared, settings)
            .into_iter()
            .enumerate()
        {
            outcome.log.insert(1 + i, line);
        }
    }
    // From here the chapter is a finished script and `digest_chapter` writes it,
    parts.clear();
    progress(1.0, format!("digest ch{n} done"));
    Ok(outcome)
}

/// Everything after the two answers have parsed: merge the rounds, check the
pub(crate) fn assemble_outcome(
    bible: &Value,
    context: &Value,
    script: &Value,
    text: &str,
) -> Result<DigestOutcome> {
    let data = merge_rounds(context, script);

    let mut log = Vec::new();
    // First line, before anything the model said. The split is decided from the
    log.push(prepare_chapter(text).split_summary());
    let warnings = warn_vietnamese(&data, bible);

    // Grammar fixes must reference text that is actually in the chapter.
    let fixes = data
        .get("fixes")
        .and_then(|f| f.as_array())
        .cloned()
        .unwrap_or_default();
    for fx in &fixes {
        let before = fx.get("before").and_then(|b| b.as_str()).unwrap_or("");
        let after = fx.get("after").and_then(|a| a.as_str()).unwrap_or("");
        if before.is_empty() || after.is_empty() {
            anyhow::bail!("fix needs before+after: {fx}");
        }
        if !text.contains(before) {
            log.push(format!(
                "   WARN: fix source not found in chapter: {:?}",
                head_chars(before, 60)
            ));
        }
    }
    if !fixes.is_empty() {
        log.push(format!("   grammar fixes: {}", fixes.len()));
    }

    let segments = data
        .get("segments")
        .and_then(|s| s.as_array())
        .cloned()
        .unwrap_or_default();

    let script = json!({
        // The chapter's own name, rewritten out of the machine-translated
        "title": data.get("title").cloned().unwrap_or(json!("")),
        "atmosphere": data.get("atmosphere").cloned().unwrap_or(json!("")),
        // The chapter's end-state summary, the next chapter's attribution
        "excerpt": data.get("excerpt").cloned().unwrap_or(json!("")),
        "roster": data.get("roster").cloned().unwrap_or(json!([])),
        "mentions": data.get("mentions").cloned().unwrap_or(json!({})),
        "speakers": data.get("speakers").cloned().unwrap_or(json!({})),
        // Spot effects ride *inside* this array, as their own items between
        "segments": segments,
        "fixes": fixes,
    });

    let delta = json!({
        "new_characters": data.get("new_characters").cloned().unwrap_or(json!([])),
        "new_aliases": data.get("new_aliases").cloned().unwrap_or(json!({})),
        "roster": data.get("roster").cloned().unwrap_or(json!([])),
        "speakers": data.get("speakers").cloned().unwrap_or(json!({})),
        "segments": script.get("segments").cloned().unwrap_or(json!([])),
    });

    log.push(format!(
        "segments={} sounds={} roster={}",
        script
            .get("segments")
            .and_then(|s| s.as_array())
            .map(|s| s.len())
            .unwrap_or(0),
        script
            .get("segments")
            .and_then(|s| s.as_array())
            .map(|s| s.iter().filter(|i| crate::util::is_sound_item(i)).count())
            .unwrap_or(0),
        squeeze_ws(
            &script
                .get("roster")
                .map(|r| r.to_string())
                .unwrap_or_else(|| "[]".into())
        ),
    ));

    Ok(DigestOutcome {
        segments: script
            .get("segments")
            .and_then(|s| s.as_array())
            .map(|s| s.len())
            .unwrap_or(0),
        script,
        delta,
        log,
        warnings,
    })
}

/// Digest one chapter and persist it. `bible` is the inductor's snapshot; the
pub async fn digest_chapter(
    layout: &Layout,
    n: u32,
    bible: &Value,
    settings: &Settings,
    analyzer: &str,
    progress: &mut (dyn FnMut(f32, String) + Send),
) -> Result<DigestOutcome> {
    let mut out = analyze_chapter(layout, n, bible, settings, analyzer, progress).await?;
    write_script(layout, n, &out.script)?;
    out.log.push(format!("-> {}", layout.script(n).display()));
    Ok(out)
}

/// The digest's steps, named for what each one is asked to do rather than for
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Round {
    /// Who speaks which event. Generated and validated first, and read as data
    Attribution,
    /// Which scenes, sounds and beds the events become.
    Staging,
}

impl Round {
    pub fn as_str(self) -> &'static str {
        match self {
            Round::Attribution => "attribution",
            Round::Staging => "staging",
        }
    }
}

impl std::fmt::Display for Round {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A gate failure, tagged with the step that **owns** the fault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Complaint {
    /// The step that can fix this.
    pub blame: Round,
    /// What is wrong, in the words the model needs to fix it.
    pub why: String,
}

impl Complaint {
    pub fn new(blame: Round, why: impl std::fmt::Display) -> Self {
        Self {
            blame,
            why: why.to_string(),
        }
    }
}

impl std::fmt::Display for Complaint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.why)
    }
}

impl std::error::Error for Complaint {}

/// What a failed G does to the chapter.
pub(crate) enum Fail {
    /// No call from here can fix it. The chapter is over, with a reason.
    Dead(anyhow::Error),
    /// Another step owns the fault. Re-run that step, then this one again.
    Back(String),
}

/// What one gate failure means for the chapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Route {
    /// The running step's own fault, and it has an ask left.
    Retry,
    /// An earlier step's fault. The answer is dropped without a second call.
    Back,
    /// Its own fault, and one ask did not fix it.
    Die,
}

/// **The severity rule, in one place and four lines.**
pub(crate) fn route(blame: Round, running: Round, attempt: u32) -> Route {
    if blame != running {
        return Route::Back;
    }
    if attempt == 0 {
        Route::Retry
    } else {
        Route::Die
    }
}

/// The chapter's LLM-call budget, shared by every G it runs.
pub(crate) struct GCalls {
    pub(crate) chapter: u32,
    pub(crate) left: usize,
    pub(crate) spent: usize,
}

impl GCalls {
    /// A chapter that has not split yet gets the phrase pass's own three asks.
    pub(crate) fn new(chapter: u32) -> Self {
        Self {
            chapter,
            left: 3,
            spent: 0,
        }
    }

    /// Add a part's worth of calls once the windows are known. Called once.
    pub(crate) fn allow_parts(&mut self, parts: usize) {
        self.left += 4 * parts;
    }

    /// Spend one call, or refuse the chapter rather than make it. The label is
    pub(crate) fn spend(&mut self, label: &str) -> Result<()> {
        if self.left == 0 {
            anyhow::bail!(
                "ch{} spent its budget of {} LLM calls without a passing gate; \
                 the last failure decides the chapter, and another call is not \
                 going to change it",
                self.chapter,
                self.spent
            );
        }
        self.left -= 1;
        self.spent += 1;
        eprintln!("ch{} {label} call {}", self.chapter, self.spent);
        Ok(())
    }
}
/// One generation, retried through rate limits.
async fn generate_retrying(
    prompt: &str,
    analyzer: &str,
    settings: &Settings,
    progress: &mut (dyn FnMut(f32, String) + Send),
    from: f32,
    to: f32,
) -> Result<String> {
    let mut last_rl = String::new();
    for attempt in 0..6 {
        match generate(prompt, analyzer, settings).await {
            Ok((t, backend)) => {
                // The configured backend and the one that ran are not the same
                if backend.as_str() != analyzer {
                    progress(
                        to,
                        format!(
                            "{analyzer} gave up — this round was answered by {}",
                            backend.as_str()
                        ),
                    );
                }
                return Ok(t);
            }
            Err(GenError::RateLimited(msg)) => {
                let wait = parse_retry_delay(&msg)
                    .unwrap_or_else(|| (30.0 * 2f64.powi(attempt)).min(300.0));
                progress(
                    (from + 0.05 * attempt as f32).min(to),
                    format!("rate-limited, sleeping {wait:.0}s"),
                );
                tokio::time::sleep(Duration::from_secs_f64(wait)).await;
                last_rl = msg;
            }
            Err(GenError::Fatal(e)) => return Err(e),
        }
    }
    anyhow::bail!("analyzer {analyzer} still rate-limited after retries: {last_rl}")
}

/// Ask the same round again with the validator's complaint appended.
async fn repair_once(
    prompt: &str,
    complaint: &anyhow::Error,
    analyzer: &str,
    settings: &Settings,
) -> Result<String> {
    let repair = format!(
        "{prompt}\n\nYour last output was invalid: {complaint}. Return ONLY the corrected JSON object."
    );
    match generate(&repair, analyzer, settings).await {
        // The backend that answered a repair is not re-labelled here: this path
        Ok((t, _backend)) => Ok(t),
        Err(GenError::RateLimited(m)) => anyhow::bail!("repair attempt rate-limited: {m}"),
        Err(GenError::Fatal(e)) => Err(e),
    }
}
