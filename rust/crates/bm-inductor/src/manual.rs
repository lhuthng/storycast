//! The manual digest flow, shared by the TUI's `:digest` manager and the

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
        cast: Option<Value>,
        /// The part of the chapter this round is for, when the chapter is
        part: Option<bm_core::digest::ManualPart>,
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

    /// Which part of the chapter this round is for, when it is one of several.
    pub(crate) fn part(&self) -> Option<bm_core::digest::ManualPart> {
        match self {
            Next::Prompt { part, .. } => *part,
            Next::Done(_) => None,
        }
    }
}

/// Start a chapter: round 1's prompt, or a refusal.
pub(crate) fn open(
    layout: &Layout,
    engine: &str,
    n: u32,
    digested: &dyn Fn(u32) -> bool,
) -> Result<Next, String> {
    if !digested(n) && !(n == 1 || digested(n - 1)) {
        return Err(format!(
            "ch{n} is not next — manual digest does the chapter after the last digested one"
        ));
    }
    let step =
        bm_core::digest::manual_prompt(layout, engine, n, None).map_err(|e| format!("{e:#}"))?;
    Ok(Next::Prompt {
        round: step.round,
        text: step.text,
        cast: None,
        part: step.part,
    })
}

/// Take one pasted (or generated) answer.
pub(crate) fn advance(
    layout: &Layout,
    engine: &str,
    n: u32,
    round: bm_core::digest::Round,
    pasted: &str,
    cast: Option<&Value>,
) -> Result<Next, String> {
    let answer = bm_core::digest::manual_accept(layout, n, round, pasted, cast)
        .map_err(|e| format!("{e:#}"))?;

    if let Some(context) = answer.cast {
        // Round 1 done. Round 2's prompt is rendered *against this cast*, which
        let step = bm_core::digest::manual_prompt(layout, engine, n, Some(&context))
            .map_err(|e| format!("{e:#}"))?;
        return Ok(Next::Prompt {
            round: step.round,
            text: step.text,
            cast: Some(context),
            part: step.part,
        });
    }

    match answer.outcome {
        Some(outcome) => Ok(Next::Done(outcome)),
        // A part's script was accepted and the chapter has more parts: the next
        None => match answer.prompt {
            Some(step) => Ok(Next::Prompt {
                round: step.round,
                text: step.text,
                cast: None,
                part: step.part,
            }),
            None => Err("the answer carried neither a prompt nor a finished chapter".into()),
        },
    }
}

/// Send one prompt to the configured analyzer, retrying rate limits.
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
pub(crate) fn backend_down(api: &str) -> String {
    format!(
        "the inductor at {api} is not answering — start it first (`make serve`, or press :B in \
         the TUI), then run this again; a digest with no inductor to report to is not kept"
    )
}

/// Is the control API answering at all?
pub(crate) async fn reachable(api: &str, http: &reqwest::Client) -> bool {
    let url = format!("{}/api/state", api.trim_end_matches('/'));
    matches!(http.get(&url).send().await, Ok(r) if r.status().is_success())
}

/// The backend must already be up. Checked before the first chapter rather than
pub(crate) async fn require_inductor(api: &str, http: &reqwest::Client) -> Result<(), String> {
    if reachable(api, http).await {
        Ok(())
    } else {
        Err(backend_down(api))
    }
}

/// Hand a finished digest to a running inductor.
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
        let d = tempfile::tempdir().unwrap();
        let layout = Layout::new(d.path());
        let err = open(&layout, "vieneu", 7, &|n| layout.digested(n)).unwrap_err();
        assert!(err.contains("not next"), "{err}");
    }

    #[test]
    fn manual_digest_opens_the_chapter_after_the_last_digested_one() {
        // The digest queue's bottleneck case: ch1 done, ch2 fresh — ch2 opens,
        let d = tempfile::tempdir().unwrap();
        bm_core::profile::install_fixture(d.path()).unwrap();
        let layout = Layout::new(d.path());
        std::fs::create_dir_all(layout.chapters()).unwrap();
        std::fs::write(layout.chapter_txt(2), "text").unwrap();
        std::fs::write(layout.chapter_txt(3), "text").unwrap();
        let digested = |n: u32| n == 1;
        assert!(open(&layout, "vieneu", 2, &digested).is_ok());
        assert!(open(&layout, "vieneu", 3, &digested).is_err());
    }
}
