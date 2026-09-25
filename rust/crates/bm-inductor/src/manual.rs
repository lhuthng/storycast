//! The manual digest flow, shared by the TUI's `:digest` manager and the
//! headless backup runner.
//!
//! The gesture is two rounds and no more: round 1 asks *who speaks*, round 2
//! asks *how it sounds*. Between them there is never more than one thing to do
//! next, and this module is the one place that knows what it is.
//!
//! **It is the worker's digest with a person standing in for the analyzer.**
//! The prompts come from `bm_core::digest::manual_prompt` — the same
//! source-gated attribution and staging builders the automatic path renders —
//! and the answers are checked by the same validators. Nothing here invents a
//! second contract, so a chapter finished by hand cannot land in the library
//! that the automatic path would have refused.
//!
//! The two front-ends differ only in how they carry a prompt to a model and how
//! they bring the answer back:
//!
//! * the TUI puts the prompt on the clipboard and reads the answer back from it;
//! * [`ask`] sends the prompt through `bm_core::digest::generate` to whatever
//!   analyzer the settings name.
//!
//! Both end at [`report`], which is the same `POST /api/complete` a worker makes
//! — under the reserved `operator` id, which is what makes the inductor accept
//! it as authoritative for the chapter.

use bm_core::config::Settings;
use bm_core::Layout;
use serde_json::Value;

/// What to do next for one chapter, after a prompt or an answer.
#[derive(Debug)]
pub(crate) enum Next {
    /// Ask for this round's answer. Round 1 is the cast, round 2 the script.
    Prompt {
        round: bm_core::digest::Round,
        text: String,
        /// Round 1's validated answer, set on round 2's prompt and nowhere
        /// else — it is what round 2 was rendered against and what its answer
        /// must be checked against, so the caller has to carry it forward.
        cast: Option<Value>,
    },
    /// Both rounds are in and accepted: this is the finished chapter.
    Done(bm_core::digest::DigestOutcome),
}

impl Next {
    /// The round a prompt is for, or `None` when the chapter finished.
    pub(crate) fn round(&self) -> Option<bm_core::digest::Round> {
        match self {
            Next::Prompt { round, .. } => Some(*round),
            Next::Done(_) => None,
        }
    }

    /// The prompt text, or the empty string once the chapter is done.
    pub(crate) fn text(&self) -> &str {
        match self {
            Next::Prompt { text, .. } => text,
            Next::Done(_) => "",
        }
    }

    /// Round 1's cast, once round 2 is being asked for.
    pub(crate) fn cast(&self) -> Option<&Value> {
        match self {
            Next::Prompt { cast, .. } => cast.as_ref(),
            Next::Done(_) => None,
        }
    }
}

/// Start a chapter: round 1's prompt, or a refusal.
///
/// Manual digest re-digests — plus the one next chapter past the digested run,
/// so the operator can work ahead of a bottlenecked digest queue. Its delta then
/// lands on top of its predecessor's, which is the bible order the workers keep.
/// Anything further ahead is refused: skipping would merge deltas out of order.
pub(crate) fn open(
    layout: &Layout,
    n: u32,
    digested: &dyn Fn(u32) -> bool,
) -> Result<Next, String> {
    if !digested(n) && !(n == 1 || digested(n - 1)) {
        return Err(format!(
            "ch{n} is not next — manual digest does the chapter after the last digested one"
        ));
    }
    let step = bm_core::digest::manual_prompt(layout, n, None).map_err(|e| format!("{e:#}"))?;
    Ok(Next::Prompt {
        round: step.round,
        text: step.text,
        cast: None,
    })
}

/// Take one pasted (or generated) answer.
///
/// `Ok(Prompt)` means the round was accepted and the next one is ready —
/// round 1 buys round 2's prompt, which is why the caller must carry `cast`
/// forward. `Ok(Done)` means the chapter finished.
pub(crate) fn advance(
    layout: &Layout,
    n: u32,
    round: bm_core::digest::Round,
    pasted: &str,
    cast: Option<&Value>,
) -> Result<Next, String> {
    let answer = bm_core::digest::manual_accept(layout, n, round, pasted, cast)
        .map_err(|e| format!("{e:#}"))?;

    if let Some(context) = answer.cast {
        // Round 1 done. Round 2's prompt is rendered *against this cast*, which
        // is why the context is carried rather than re-derived — the worker
        // makes exactly this hand-off between its two calls.
        let step = bm_core::digest::manual_prompt(layout, n, Some(&context))
            .map_err(|e| format!("{e:#}"))?;
        return Ok(Next::Prompt {
            round: step.round,
            text: step.text,
            cast: Some(context),
        });
    }

    match answer.outcome {
        Some(outcome) => Ok(Next::Done(outcome)),
        None => Err("the answer carried neither a cast nor a script".into()),
    }
}

/// Send one prompt to the configured analyzer, retrying rate limits.
///
/// The same retry shape the worker's rounds use — a round that gave up sooner
/// than the other would fail chapters for a reason that has nothing to do with
/// the round. One attempt per call; the caller decides whether a refused answer
/// is worth one repair.
pub(crate) async fn ask(
    prompt: &str,
    analyzer: &str,
    settings: &Settings,
) -> Result<String, String> {
    use bm_core::digest::GenError;
    let mut last = String::new();
    for attempt in 0..6 {
        match bm_core::digest::generate(prompt, analyzer, settings).await {
            Ok((text, _backend)) => return Ok(text),
            Err(GenError::RateLimited(msg)) => {
                let wait = bm_core::digest::parse_retry_delay(&msg)
                    .unwrap_or_else(|| (30.0 * 2f64.powi(attempt)).min(300.0));
                eprintln!("rate-limited, sleeping {wait:.0}s");
                tokio::time::sleep(std::time::Duration::from_secs_f64(wait)).await;
                last = msg;
            }
            Err(GenError::Fatal(e)) => return Err(format!("{e:#}")),
        }
    }
    Err(format!("analyzer {analyzer} still rate-limited: {last}"))
}

/// The one repair a refused answer gets, phrased the way the worker's is.
pub(crate) fn repair_prompt(prompt: &str, complaint: &str) -> String {
    format!("{prompt}\n\nYour last output was invalid: {complaint}. Return ONLY the corrected JSON object.")
}

/// The one line an operator needs when the control API is not up.
///
/// Returned rather than waited on: the backend is a **precondition**, the same
/// way it is for `make tui`. A digest whose report has nowhere to land is work
/// on nobody's disk, so this says what to enter and stops.
pub(crate) fn backend_down(api: &str) -> String {
    format!(
        "the inductor at {api} is not answering — start it first (`make serve`, or press :B in \
         the TUI), then run this again; a digest with no inductor to report to is not kept"
    )
}

/// Is the control API answering at all?
///
/// `GET /api/state` is the one call that reads nothing and writes nothing, so
/// it answers "is there an inductor there" without touching a task.
pub(crate) async fn reachable(api: &str, http: &reqwest::Client) -> bool {
    let url = format!("{}/api/state", api.trim_end_matches('/'));
    matches!(http.get(&url).send().await, Ok(r) if r.status().is_success())
}

/// The backend must already be up. Checked before the first chapter rather than
/// after it, so a missing inductor costs a second instead of a whole range.
pub(crate) async fn require_inductor(api: &str, http: &reqwest::Client) -> Result<(), String> {
    if reachable(api, http).await {
        Ok(())
    } else {
        Err(backend_down(api))
    }
}

/// Hand a finished digest to a running inductor.
///
/// The body is a worker's, field for field — the same `Complete` the agent
/// posts — so the inductor cannot tell the two apart except by who is claiming
/// the work, which is the one thing that legitimately differs. The reserved
/// `operator` id is what makes the report authoritative: the row may be
/// assigned to a box grinding on the same chapter, and this answer still wins.
pub(crate) async fn report(
    api: &str,
    http: &reqwest::Client,
    chapter: u32,
    script: &Value,
    delta: &Value,
    detail: String,
) -> Result<String, String> {
    let url = format!("{}/api/complete", api.trim_end_matches('/'));
    let body = bm_proto::Complete {
        worker_id: bm_proto::MANUAL_WORKER.to_string(),
        task_id: format!("{}:{chapter}", bm_proto::Stage::Digest.as_str()),
        ok: true,
        detail,
        duration_secs: 0.0,
        bible_delta: Some(delta.clone()),
        crawl: None,
        units: 0,
        script: Some(script.clone()),
        text: None,
        mp3_b64: None,
        unit_files: Vec::new(),
    };
    match http.post(&url).json(&body).send().await {
        Ok(r) if r.status().is_success() => match r.text().await {
            Ok(line) => Ok(line.trim().to_string()),
            Err(e) => Err(format!("the inductor's answer did not arrive: {e}")),
        },
        Ok(r) => Err(format!("the inductor answered HTTP {}", r.status())),
        Err(e) => Err(format!("could not reach the inductor: {e}")),
    }
}

/// Report, and name the remedy when the backend has gone away mid-run.
///
/// Same precondition as at startup, checked at the one moment it matters: a
/// report that cannot land is the difference between a digested chapter and a
/// model call nobody paid for.
pub(crate) async fn report_or_stop(
    api: &str,
    http: &reqwest::Client,
    chapter: u32,
    script: &Value,
    delta: &Value,
    detail: String,
) -> Result<String, String> {
    match report(api, http, chapter, script, delta, detail).await {
        Ok(line) => Ok(line),
        // The cause and the remedy, together: "could not reach" alone is a
        // shrug, and a bare "start the inductor" hides why it went away.
        Err(e) if !reachable(api, http).await => {
            Err(format!("ch{chapter}: {e} — {}", backend_down(api)))
        }
        Err(e) => Err(format!("ch{chapter}: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_digest_refuses_a_chapter_past_the_next_one() {
        // Skipping ahead would merge bible deltas out of order: only the
        // chapter right after the digested run may be worked by hand.
        let d = tempfile::tempdir().unwrap();
        let layout = Layout::new(d.path());
        let err = open(&layout, 7, &|n| layout.digested(n)).unwrap_err();
        assert!(err.contains("not next"), "{err}");
    }

    #[test]
    fn manual_digest_opens_the_chapter_after_the_last_digested_one() {
        // The digest queue's bottleneck case: ch1 done, ch2 fresh — ch2 opens,
        // ch3 still refused.
        let d = tempfile::tempdir().unwrap();
        bm_core::profile::install_fixture(d.path()).unwrap();
        let layout = Layout::new(d.path());
        std::fs::create_dir_all(layout.chapters()).unwrap();
        std::fs::write(layout.chapter_txt(2), "text").unwrap();
        std::fs::write(layout.chapter_txt(3), "text").unwrap();
        let digested = |n: u32| n == 1;
        assert!(open(&layout, 2, &digested).is_ok());
        assert!(open(&layout, 3, &digested).is_err());
    }
}
