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
use super::parts::stage_part;
use super::parts::Part;
use super::parts::PartCtx;
use super::parts::Parts;
use super::prepare::prepare_chapter;
use super::prompts::build_staging_prompt;
use super::quotes::effective_text;
use super::quotes::needs_proofread;
use super::quotes::quote_findings;
use super::quotes::repair_quotes;
use super::quotes::repaired_record;
use super::quotes::repaired_txt;
use super::quotes::structural;
use super::sound::build_sound_prompt;
use super::sound::parse_sound_script;
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
    let mut prev: Option<String> = None;
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
                false,
            ),
            Round::Repair | Round::Sound => {
                unreachable!("these passes have their own runners, not run_g")
            }
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
            Round::Repair | Round::Sound => {
                unreachable!("these passes have their own runners, not run_g")
            }
        };
        match judged {
            Ok(value) => {
                // The code has checked the shape; the judge is the one check it
                // cannot make, and it is asked on a chapter that otherwise passed.
                if round == Round::Staging {
                    let judged = judge::check(
                        &value,
                        &ctx.settings.engine,
                        &ctx.settings.digest.judge,
                    )
                    .await;
                    if let Some(why) = &judged.skipped {
                        ctx.report(to, format!("{}{why} (no verdict)", part_prefix(ctx.part)));
                    }
                    for doubt in &judged.doubts {
                        ctx.report(to, format!("{}{doubt}", part_prefix(ctx.part)));
                    }
                    if let Some(c) = judged.complaint {
                        // **One repair, then the chapter goes through.** A model
                        // that scores 0.01 on tags the code already accepted
                        // cannot be allowed to hold a chapter hostage, and the
                        // model has nothing left to change the second time.
                        if attempt == 0 {
                            ctx.report(
                                to,
                                format!(
                                    "{}{} judge said {c}, asking for one repair",
                                    part_prefix(ctx.part),
                                    round.as_str(),
                                ),
                            );
                            complaint = Some(c);
                            continue;
                        }
                        ctx.report(
                            to,
                            format!(
                                "{}{c}; the judge is not holding the chapter for it",
                                part_prefix(ctx.part)
                            ),
                        );
                    }
                }
                return Ok(value);
            }
            Err(c) => match route(c.blame, round, attempt) {
                // Someone else's fault: the answer is dropped without a second
                Route::Back => {
                    return Err(Fail::Back(
                        c.blame,
                        format!(
                            "{}: {} (the {} step owns that)",
                            round.as_str(),
                            c.why,
                            c.blame.as_str()
                        ),
                    ))
                }
                Route::Die => {
                    // The repair changed nothing: the same complaint twice means
                    // the fault sits one step out, so blame walks outward
                    // instead of dying on a wall it cannot fix.
                    if let Some(blame) = escalate(round, prev.as_deref(), &c) {
                        return Err(Fail::Back(
                            blame,
                            format!(
                                "{} failed twice with the same complaint ({}); handing to {}",
                                round.as_str(),
                                c.why,
                                blame.as_str()
                            ),
                        ));
                    }
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
                    prev = Some(c.why.clone());
                    complaint = Some(c.why);
                }
            },
        }
    }
    // The loop returns on attempt 0 (accepted, or handed back) or attempt 1
    unreachable!("run_g leaves on its first or second attempt")
}

/// The chapter's sound pass: one G over every staged line, then one repair if a
/// gate refuses the answer. Sound design is chapter-wide, so this runs once for
/// the whole chapter rather than per part.
#[allow(clippy::too_many_arguments)]
async fn run_sound(
    layout: &Layout,
    n: u32,
    analyzer: &str,
    settings: &Settings,
    context: &Value,
    staged: &Value,
    chapter_text: &str,
    vocab: &Vocabulary,
    calls: &mut GCalls,
    progress: &mut (dyn FnMut(f32, String) + Send),
) -> Result<Vec<Value>> {
    let mut segments = staged
        .get("segments")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // The sound pass owns these now: drop whatever a staging answer left on a
    // line, so a stray field cannot survive the pass that replaces it.
    for line in segments.iter_mut() {
        if let Some(object) = line.as_object_mut() {
            for key in ["scene", "music", "sound_after", "stop_after"] {
                object.remove(key);
            }
        }
    }
    let mut complaint: Option<String> = None;
    for attempt in 0..2 {
        let prompt = build_sound_prompt(layout, context, &segments)?;
        // A G like any other: it spends from the same chapter budget, so a
        // chapter cannot buy its way past a gate with extra calls.
        calls.spend(Round::Sound.as_str())?;
        let raw = if attempt == 0 {
            generate_retrying(&prompt, analyzer, settings, progress, 0.88, 0.96).await?
        } else {
            repair_once(
                &prompt,
                &anyhow::anyhow!(complaint.clone().unwrap_or_default()),
                analyzer,
                settings,
            )
            .await?
        };
        let dump = if attempt == 0 {
            "digest-sound"
        } else {
            "digest-sound-retry"
        };
        dump_raw(layout, dump, &raw);
        match parse_sound_script(&raw, &segments, chapter_text, vocab) {
            Ok(lines) => return Ok(lines),
            Err(why) if attempt == 0 => {
                progress(
                    0.96,
                    format!("digest ch{n} sound gate said {why}, asking for one repair"),
                );
                complaint = Some(why);
            }
            Err(why) => {
                let dump = layout.data().join(".last-analyze-raw.json");
                let _ = atomic_write(&dump, &raw);
                anyhow::bail!(
                    "digest sound invalid ({why}); raw saved to {}",
                    dump.display()
                );
            }
        }
    }
    unreachable!("run_sound leaves on its first or second attempt")
}

/// The chapter's proofread: the text every later pass reads. Run once per
/// chapter version — and re-run once when a later gate blames the text, which
/// is what `analyze_chapter` does on a `Back(Repair)`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn proofread_text(
    layout: &Layout,
    n: u32,
    original: &str,
    analyzer: &str,
    settings: &Settings,
    calls: &mut GCalls,
    progress: &mut (dyn FnMut(f32, String) + Send),
) -> Result<String> {
    // The proofread is the chapter's first prompt and it runs on every chapter,
    // faulted or not: a satisfied scan is not a clean text, and every step after
    // this one reads what this pass returns. A chapter that already carries one
    // has had the pass — the operator's own paste and this call are the same
    // pass, so it is not paid for twice.
    let text = effective_text(layout, n, original);
    if !needs_proofread(layout, n, original) {
        return Ok(text);
    }
    let findings = quote_findings(&text);
    let faulted = findings.iter().any(structural);
    match repair_quotes(
        layout,
        n,
        &text,
        &findings,
        analyzer,
        settings,
        calls,
        progress,
    )
    .await
    {
        Ok(fixed) => Ok(fixed),
        // Nothing the scan can fault, so nothing to stop the chapter for: a
        // model that answers in prose instead of JSON had nothing to answer.
        Err(e) if !faulted => {
            eprintln!("ch{n} proofread returned nothing usable ({e:#}); the text stands");
            progress(0.02, format!("ch{n} proofread: nothing to change"));
            Ok(text)
        }
        Err(e) => Err(e.context(format!(
            "ch{n} the proofread could not put the quotation structure right ({} \
             finding(s)); the chapter is not digested",
            findings.len()
        ))),
    }
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
    let mut text = proofread_text(layout, n, &original, analyzer, settings, &mut calls, progress).await?;
    // A later gate can blame the text: one fresh proofread, then the chapter
    // is staged again from it. Past that the proofread itself is disputed and
    // the chapter stops instead of ping-ponging paid calls.
    let mut reproofs = 0;
    let (parts, vocab, windows, prepared, total) = loop {
        let prepared = prepare_chapter(&text);
        let vocab = vocabulary(layout, &settings.engine)?;
        let windows = plan_windows(&prepared, &settings.digest);
        let total = windows.len();
        // The parts are known, so the rest of the budget can be sized against
        // them — again after a reproof, which spends a whole second run.
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
    // Set when a part blames the proofread: the for loop stops and the whole
    // chapter is staged again from the fresh text below.
    let mut redo = false;
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
        let (context, script) = match stage_part(
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
        .await
        {
            Ok(done) => done,
            Err(Fail::Dead(e)) => return Err(e),
            Err(Fail::Back(Round::Repair, why)) => {
                if reproofs > 0 {
                    return Err(anyhow::anyhow!(
                        "digest ch{n} still misaligned after a fresh proofread ({why}); \
                         the proofread itself is disputed, shelve it"
                    ));
                }
                reproofs += 1;
                // The sidecar is the blamed evidence: drop it so the next
                // proofread is a new one, and forget staged parts, which were
                // staged against the old text.
                parts.clear();
                let _ = std::fs::remove_file(repaired_txt(layout, n));
                let _ = std::fs::remove_file(repaired_record(layout, n));
                progress(
                    0.05,
                    format!("ch{n} staging blames the proofread ({why}); one fresh proofread"),
                );
                calls.left += 3;
                text = proofread_text(
                    layout, n, &original, analyzer, settings, &mut calls, progress,
                )
                .await?;
                redo = true;
                break;
            }
            Err(Fail::Back(blame, why)) => {
                return Err(anyhow::anyhow!(
                    "digest {} owns that ({why}), out of the parts loop's reach",
                    blame.as_str()
                ));
            }
        };
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
    // A part that blamed the proofread restarts the chapter from the fresh
    // text; otherwise the staged parts are the chapter's.
    if redo {
        continue;
    }
    break (parts, vocab, windows, prepared, total);
    };

    // ---- the sound pass, once the whole chapter is staged ------------------
    // Sound is chapter-wide — a bed that opens in one part closes in another, a
    // mood that turns mid-chapter crossfades once — so it is decided here, in
    // one call over every staged line, rather than part by part.
    let (context, conflicts) = merge_contexts(&parts.done);
    let mut script = merge_scripts(parts.done.iter().map(|p| &p.script));
    progress(0.88, format!("digest ch{n} via {analyzer}: sound"));
    let lines = run_sound(
        layout, n, analyzer, settings, &context, &script, &text, &vocab, &mut calls, progress,
    )
    .await?;
    script["segments"] = json!(lines);

    let mut outcome = assemble_outcome(bible, &context, &script, &text)?;
    // A disagreement between parts is the operator's to settle, so it is said
    for w in conflicts {
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
    /// The content pass: the chapter text itself — the marks, the words, the
    /// format — put right before anything is read out of it. Every chapter runs
    /// it first, and no later round may edit the text again.
    Repair,
    /// Who speaks which event. Generated and validated first, and read as data
    Attribution,
    /// The segments, their text and their acting mood.
    Staging,
    /// Where the chapter is set, what music plays under it, and which spot
    /// sounds fire — decided once for the whole chapter, over every staged line.
    Sound,
}

impl Round {
    pub fn as_str(self) -> &'static str {
        match self {
            Round::Repair => "repair",
            Round::Attribution => "attribution",
            Round::Staging => "staging",
            Round::Sound => "sound",
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
    /// Another step owns the fault: which one, and why. The runner either
    /// re-runs that step or, when the step is out of its reach, dies naming it.
    Back(Round, String),
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

/// The step outward from `round`: where blame walks when a repair changes
/// nothing. Staging re-resolves the cast, attribution re-reads the text.
/// Repair and sound have no outward step from here.
pub(crate) fn outer(round: Round) -> Option<Round> {
    match round {
        Round::Staging => Some(Round::Attribution),
        Round::Attribution => Some(Round::Repair),
        Round::Repair | Round::Sound => None,
    }
}

/// Whether a second identical failure stops retrying this step. A repair that
/// leaves the complaint word-for-word is no progress, so the fault must sit
/// one step out — unless there is no step out, and the chapter dies.
pub(crate) fn escalate(round: Round, prev: Option<&str>, complaint: &Complaint) -> Option<Round> {
    if complaint.blame == round && prev == Some(complaint.why.as_str()) {
        return outer(round);
    }
    None
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

    /// Add a part's worth of calls once the windows are known, plus the
    /// chapter-level sound pass's own ask. Called once.
    pub(crate) fn allow_parts(&mut self, parts: usize) {
        self.left += 4 * parts + 2;
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
                let wait = retry_wait(parse_retry_delay(&msg), 30.0 * 2f64.powi(attempt));
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
