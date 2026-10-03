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
/// and let the gate judge the answer.
///
/// Two rungs, because a failure means one of only two things. The gate blamed
/// this step, so it gets one more ask here carrying the complaint — a model
/// told exactly what it got wrong fixes it far more often than a fresh ask
/// guesses, which is why this is a retry in place and not a jump back to
/// itself. The gate blamed an **earlier** step, so this answer is abandoned at
/// once: it was never going to be right, and spending a call to be sure twice
/// is the expensive way to learn it.
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
        // `reask_staging` gives: a part's prompt is a few hundred KB, and two
        // reads cost less than holding every part's prompt for a chapter.
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
                // ask, and the step that owns it runs again.
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
    // (accepted or dead), so this is arithmetic rather than a path.
    unreachable!("run_g leaves on its first or second attempt")
}

/// Re-ask one part's staging round with a gate's complaint appended.
///
/// The prompt is re-rendered rather than carried: a part's prompt is a few
/// hundred KB of string, and two `read_to_string`s cost less than holding every
/// part's prompt for the length of a chapter. It is rendered from **that part's
/// own slice and cast**, so a repair can only ever answer for events it was
/// shown, and its answer goes through the same validators every other answer
/// does.
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
    // whose sound design never converges cannot buy repairs past the cap.
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
/// window when the chapter is too long for one answer to carry.
///
/// Attribution is generated and validated first. The staging pass receives that
/// map as data and never emits speakers, so a small model cannot regress a
/// mechanically separated dialogue event back to Narrator while it is choosing
/// scenes and sounds.
///
/// That is the whole contract, and splitting the chapter does not weaken it: a
/// window is a contiguous run of the same prepared events, the two rounds run on
/// it unchanged, and the parts are merged back into one script and one bible
/// delta. See [`window`] for where the cuts fall and [`Parts`] for what survives
/// a restart.
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
    // means the question is already answered, and this chapter costs nothing
    // extra; otherwise an unbalanced chapter spends one proofread call here and
    // then runs the same digest it always does.
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
    // two calls each is the floor, and the surplus is what the gates are for.
    calls.allow_parts(total);
    let mut parts = Parts::open(layout, n, &text, bible, &windows, settings);
    if total > 1 {
        // Before any call, because the number of calls is the operator's
        // business: a 40 KB chapter is sixteen of them, not two, and a digest
        // that looks stuck is only diagnosable once the plan said so.
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
    // rate whether this chapter is one part or sixteen. Within a part the two
    // rounds split the band: attribution first, and staging — the longer and
    // more expensive of the two — the rest of it.
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
        // resumes from is work that would have been accepted — not work in
        // progress, which is why a resume never has to re-validate a part.
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
    //
    // Rule 1 (a looping bed opened and never stopped) is a fact about the
    // **whole chapter**, so it is checked on the merged segments: a bed opened
    // at the end of one part and closed at the start of the next is closed, and
    // a per-part check would refuse exactly the long scene this feature exists
    // for. Its repair goes to the part that placed the surviving `sound`.
    //
    // Rule 2 (the text stages sounds and the script places none) is a fact about
    // one **part**, so it is checked part by part against that part's own prose —
    // a cue can only fail the text that contains it. For a chapter that did not
    // split, this is `sound_design_gap`'s two rules in their original order
    // against the whole chapter, which is what keeps the single-call digest's
    // behaviour intact.
    //
    // The cue scan is a heuristic about prose, not a contradiction: an idiom
    // trips it with nothing staged, and a gate that can never be satisfied is a
    // deadlock — every repair burns an LLM call and the chapter refuses 100% of
    // correct answers. So the block decays: 90%, then -25% per consecutive
    // failure, and below a coin flip the script is accepted with a loud warning
    // instead of refused. Deterministic (no dice): the same chapter always takes
    // the same path, and the loop always terminates within four evaluations.
    // An unclosed bed is still corruption rather than judgment — but the decay
    // covers its repair too, because a model that cannot close a bed after four
    // asks is not going to on the fifth, and a refused chapter helps nobody.
    let what = if total == 1 { "chapter" } else { "part" };
    // Each part's own prose, for rule 2 — or the chapter's own text when it did
    // not split, which is the text the single-call digest has always scanned.
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
    // out loud rather than resolved quietly: `mentions` is a map, a map holds one
    // value per key, and two parts naming different owners for one surface form
    // is a decision the alias table owes somebody.
    for w in conflicts {
        outcome.log.push(format!("   WARN: {w}"));
        outcome.warnings.push(w);
    }
    // `warnings` is dropped by the agent today; `log` is what the worker
    // prints, so the release lands in both — one for future readers, one
    // for the operator watching now.
    if let Some(w) = soft_released {
        outcome.log.push(format!("   WARN: {w}"));
        outcome.warnings.push(w);
    }
    if total > 1 {
        // The plan, in the log the worker prints: the only place the cost of a
        // long chapter is visible. Five parts and sixteen calls are the same
        // script as one call, and only these lines say which one happened.
        for (i, line) in part_lines(&windows, &prepared, settings)
            .into_iter()
            .enumerate()
        {
            outcome.log.insert(1 + i, line);
        }
    }
    // From here the chapter is a finished script and `digest_chapter` writes it,
    // so the checkpoint has done its job. Cleared rather than left: a re-digest,
    // or the operator taking the chapter over by hand, has to start clean
    // instead of resuming into parts of a script that already exists.
    parts.clear();
    progress(1.0, format!("digest ch{n} done"));
    Ok(outcome)
}

/// Everything after the two answers have parsed: merge the rounds, check the
/// grammar fixes against the chapter, build the script and the bible delta, and
/// describe what came out.
///
/// **Shared by the worker's automatic path and the operator's manual one, and
/// that is the point.** The manual route exists to be *the same digest* with a
/// person standing in for the model, not a second, looser one. One function,
/// rather than two that agree today. The `sound_design_gap` check stays in
/// the callers, because they answer it differently: the worker asks the model
/// again, the operator is told and gets to paste a better answer.
pub(crate) fn assemble_outcome(
    bible: &Value,
    context: &Value,
    script: &Value,
    text: &str,
) -> Result<DigestOutcome> {
    let data = merge_rounds(context, script);

    let mut log = Vec::new();
    // First line, before anything the model said. The split is decided from the
    // text alone, so this is the earliest a bad crawl is visible, and the only
    // place a *silent* one is, since a chapter with no dialogue has nothing for
    // any validator to object to. It is here rather than in the automatic path
    // because the manual path is where a person is standing there able to act
    // on it, and both paths need the same answer.
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
        // headline the crawl left on line 1. `Layout::chapter_title` prefers
        // this, so it is the mp3's filename *and* the spoken headline, one
        // value, two consumers, no chance of them disagreeing.
        "title": data.get("title").cloned().unwrap_or(json!("")),
        "atmosphere": data.get("atmosphere").cloned().unwrap_or(json!("")),
        // The chapter's end-state summary, the next chapter's attribution
        // prompt reads back as ---PREVIOUSLY---. Lives in the script root
        // beside title and atmosphere: no new file, and a re-digest of this
        // chapter rewrites it where the next one reads it.
        "excerpt": data.get("excerpt").cloned().unwrap_or(json!("")),
        "roster": data.get("roster").cloned().unwrap_or(json!([])),
        "mentions": data.get("mentions").cloned().unwrap_or(json!({})),
        "speakers": data.get("speakers").cloned().unwrap_or(json!({})),
        // Spot effects ride *inside* this array, as their own items between
        // the halves of the lines they belong to. There is deliberately no
        // sibling array: a directive parked outside the speech would have to
        // name its position, and every scheme for naming one (a phrase to cut
        // at, an ordinal) is a second copy of a fact the script already knows.
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
/// returned delta is merged by the inductor, never here.
///
/// [`analyze_chapter`] plus the one write it deliberately does not do. Keeping
/// the write here and only here is what lets the same analysis be run without
/// committing it.
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
/// the shape of its answer.
///
/// **Every step is a G: one LLM call plus the gate that judges it.** `Round` is
/// both the identity the operator's manual session drives and the step a
/// failure gets blamed on, because those are the same set of steps — there is
/// no fourth kind of thing that can fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Round {
    /// Who speaks which event. Generated and validated first, and read as data
    /// by every step after it.
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
///
/// The tag is the whole point. A gate failure is not always the running step's
/// fault, and when it is not, asking again cannot help: staging reads the cast
/// as data and never emits a speaker, so a script answer that trips over a
/// missing attribution is not a staging mistake — no number of staging retries
/// will ever produce the row that only the attribution step can write. Retrying
/// in place there burns an expensive call and then refuses the chapter anyway,
/// which is a real failure this codebase has already lived through.
///
/// So a gate says who is wrong, and the driver either retries the step that
/// failed (its own fault, and one more ask is the ordinary remedy) or hands
/// back to the step named here.
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
///
/// This is the whole difference between a ladder and a retry loop. A failure
/// the running step owns is worth exactly one more ask *in place* — a model
/// told what it got wrong fixes it far more often than a fresh ask guesses. A
/// failure an earlier step owns is worth **none**, at any attempt: the answer
/// was never going to be right, because the step that must change did not run.
/// Asking again spends a call to be sure twice, which is the expensive way to
/// learn what the blame already said.
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
///
/// Routing between steps is worth doing — but a chapter whose gates keep
/// handing work backwards must end, and ending it with a clear reason beats
/// ending it whenever the provider decides to bill the operator. The allowance
/// is generous on purpose: two calls per part is the floor, and the rest is
/// headroom for the retries that are the point of having gates.
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
    /// the step's own name, which for the phrase pass is a pass rather than a
    /// [`Round`] — it is not one of the two steps a script is made of.
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
///
/// Split out because the digest makes two calls now and the retry policy must
/// not differ between them, a round that gave up sooner than the other would
/// fail chapters for a reason that has nothing to do with the round.
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
                // thing whenever the gemini chain falls back. Say which one
                // answered, so the operator's screen stops naming a backend that
                // had already given up, this is the label that read "via gemini"
                // while opencode was the thing hanging.
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
///
/// One attempt, per round: a second failure is a chapter to look at, not a
/// prompt to keep re-sending. The caller decides whether the round is fatal.
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
        // has no progress sink, and the fallback has already said so in the log.
        Ok((t, _backend)) => Ok(t),
        Err(GenError::RateLimited(m)) => anyhow::bail!("repair attempt rate-limited: {m}"),
        Err(GenError::Fatal(e)) => Err(e),
    }
}
