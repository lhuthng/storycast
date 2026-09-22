use crate::config::Settings;
use crate::util::head_chars;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::time::Duration;

/// Why a generation attempt failed.
#[derive(Debug)]
pub enum GenError {
    /// The provider asked us to slow down. Retry after a delay.
    RateLimited(String),
    /// Anything else — do not retry, the prompt or the credentials are wrong.
    Fatal(anyhow::Error),
}

impl std::fmt::Display for GenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GenError::RateLimited(m) => write!(f, "rate limited: {m}"),
            GenError::Fatal(e) => write!(f, "{e}"),
        }
    }
}

/// Which backend actually answered.
///
/// Returned alongside the text because the *configured* analyzer and the one
/// that ran are not the same thing: `gemini` falls back to `opencode` when its
/// chain is exhausted, so a caller that labelled its progress with the
/// configured name would say "via gemini" while opencode was the thing running
/// — and, on 2026-09-22, the thing hanging for twenty-six minutes. This is what
/// lets the caller say which one it actually got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Gemini,
    Opencode,
    Openrouter,
    Ollama,
}

impl Backend {
    pub fn as_str(self) -> &'static str {
        match self {
            Backend::Gemini => "gemini",
            Backend::Opencode => "opencode",
            Backend::Openrouter => "openrouter",
            Backend::Ollama => "ollama",
        }
    }
}

/// How long one HTTP attempt against a Gemini model may take.
///
/// The digest lease is 1200 s (`bm-inductor/src/state.rs::LEASE_SECS`) and the
/// chain makes up to three attempts per model, so a per-attempt budget that is
/// too generous makes the **lease** the deadline that fires first — and a lease
/// expiry is strike-free, so the task is silently requeued while the request is
/// still in flight. 180 s keeps a whole chain inside the lease.
const GEMINI_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(180);

/// How long the `opencode` child may run before it is killed.
///
/// **This is the one that was missing, and it took a cluster's worth of work with
/// it.** `Command::output()` has no deadline of its own, so a CLI that stalls
/// blocks the worker for ever: the stage sits at whatever percentage it had
/// reached, prints nothing, and the only thing that ever happens is the lease
/// expiring — silently, without a strike — and the task being handed to another
/// box, which stalls the same way. The whole loop is invisible from the TUI.
///
/// **Two minutes, not ten.** This is a *fallback*: if it cannot answer a 32 KB
/// prompt in two minutes it is not coming back, and every second past that is a
/// box held hostage. The cost is that a legitimately slow run now fails instead
/// of finishing — which is the right trade for a path whose failure mode is a
/// silent infinite hang, and it is one constant if that proves too tight.
///
/// Still **under** the 1200 s digest lease, for the reason that matters: the
/// deadline that fires must be the child's, not the lease's, or the task is
/// requeued while the request is still in flight.
const OPENCODE_TIMEOUT: Duration = Duration::from_secs(120);

// ---------------------------------------------------------------------------
// generation backends
// ---------------------------------------------------------------------------

/// Pull a delay out of a provider error body: `retry in 53.2s` or `"retryDelay": "53s"`.
pub fn parse_retry_delay(s: &str) -> Option<f64> {
    if let Some(idx) = s.find("retry in ") {
        let tail = &s[idx + "retry in ".len()..];
        let num: String = tail
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        if let Ok(v) = num.parse::<f64>() {
            return Some(v);
        }
    }
    if let Some(idx) = s.find("retryDelay") {
        let tail = &s[idx..];
        let num: String = tail
            .chars()
            .skip_while(|c| !c.is_ascii_digit())
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        if let Ok(v) = num.parse::<f64>() {
            return Some(v);
        }
    }
    None
}

fn extract_json_object(text: &str) -> Result<String> {
    let start = text
        .find('{')
        .ok_or_else(|| anyhow!("no JSON object in output: {:?}", head_chars(text, 200)))?;
    let end = text
        .rfind('}')
        .ok_or_else(|| anyhow!("no closing brace in output: {:?}", head_chars(text, 200)))?;
    if end <= start {
        anyhow::bail!("malformed JSON span in output: {:?}", head_chars(text, 200));
    }
    Ok(text[start..=end].to_string())
}

/// The error for a provider key this process does not hold.
///
/// Naming both routes is the point. On the inductor `.env` is the file to
/// edit, but a provisioned worker has **no `.env` at all** — it is personal and
/// git-ignored, so provisioning copies `prompts/`, `python/`, `assets/` and
/// `refs/` and never that. "copy .env.example to .env" sent the operator
/// looking for a file that does not exist on the box that was failing. The key
/// normally arrives with the task (`bm_proto::Credentials`), so the fix is to
/// set it where it travels from.
fn missing_key(var: &str) -> GenError {
    GenError::Fatal(anyhow!(
        "{var} missing — the task carried no key and this machine has none in .env; \
         set it in the inductor's .env (the copy that travels) and retry"
    ))
}

async fn generate_opencode(
    prompt: &str,
    settings: &Settings,
) -> Result<(String, Backend), GenError> {
    generate_opencode_within(prompt, settings, OPENCODE_TIMEOUT).await
}

/// The same call with the deadline supplied.
///
/// Split out so the deadline is **testable**: a test that has to wait ten minutes
/// is a test nobody runs, and a deadline nobody tests is exactly the one that was
/// missing here. The caller above is the only production path.
async fn generate_opencode_within(
    prompt: &str,
    settings: &Settings,
    deadline: Duration,
) -> Result<(String, Backend), GenError> {
    let full = format!(
        "Do not use any tools. Answer with the requested output and nothing else.\n\n{prompt}"
    );
    let model = settings.opencode_model.clone();
    // `kill_on_drop` is what makes the deadline below real: without it the future
    // is dropped at the timeout and the child keeps running, orphaned, holding
    // the prompt and whatever it was waiting on.
    let child = tokio::process::Command::new("opencode")
        .args(["run", "-m", &model, &full])
        .kill_on_drop(true)
        .output();
    println!(
        "opencode run -m {model} — started ({} bytes of prompt, deadline {}s)",
        full.len(),
        // `as_secs_f64`, not `as_secs`: truncating printed "deadline 0s" for the
        // sub-second deadline a test passes, and a log line that misreports the
        // deadline it applied is the one thing this fix exists to stop.
        deadline.as_secs_f64()
    );
    let started = std::time::Instant::now();
    let out = match tokio::time::timeout(deadline, child).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(GenError::Fatal(anyhow!(
                "opencode CLI not found — install it first"
            )))
        }
        Ok(Err(e)) => return Err(GenError::Fatal(anyhow!(e).context("running opencode"))),
        // The deadline. Named out loud, because "the digest is stuck" is exactly
        // what this looked like from the outside and the message has to say
        // otherwise — and it has to say what to do next.
        Err(_) => {
            eprintln!(
                "opencode run -m {model} — KILLED after {}s with no answer",
                deadline.as_secs_f64()
            );
            return Err(GenError::Fatal(anyhow!(
                "opencode run -m {model} produced nothing in {}s and was killed. The digest is \
                 not at fault: run `opencode run -m {model} hello` by hand to see what the CLI \
                 does on its own",
                deadline.as_secs_f64()
            )));
        }
    };
    let took = started.elapsed().as_secs_f64();
    if !out.status.success() {
        eprintln!(
            "opencode run -m {model} — failed after {took:.1}s ({})",
            out.status
        );
        return Err(GenError::Fatal(anyhow!(
            "opencode run failed after {took:.1}s: {}",
            head_chars(&String::from_utf8_lossy(&out.stderr), 500)
        )));
    }
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    println!(
        "opencode run -m {model} — ok in {took:.1}s ({} bytes out)",
        stdout.len()
    );
    extract_json_object(&stdout)
        .map(|t| (t, Backend::Opencode))
        .map_err(GenError::Fatal)
}

async fn generate_ollama(prompt: &str, settings: &Settings) -> Result<(String, Backend), GenError> {
    let body = json!({
        "model": settings.local_model,
        "messages": [{"role": "user", "content": prompt}],
        "stream": false,
        "format": "json",
        "options": {"temperature": 0, "num_ctx": 16384},
    });
    // NOTE: `ollama_url` is a loopback endpoint by default and this client
    // honours `HTTP_PROXY`, unlike the sidecar clients (`sidecar_client()` sets
    // `.no_proxy()`). No failure has been reproduced from that here — reqwest
    // skips the proxy for loopback — so it is left as it is rather than
    // "fixed" on a guess. It would matter for an operator who points
    // `ollama_url` at a LAN box while a proxy is configured.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(1800))
        .build()
        .map_err(|e| GenError::Fatal(anyhow!(e)))?;
    let resp = client
        .post(format!("{}/api/chat", settings.ollama_url))
        .json(&body)
        .send()
        .await
        .map_err(|e| {
            GenError::Fatal(anyhow!(
                "cannot reach Ollama at {} ({e}); run: ollama serve",
                settings.ollama_url
            ))
        })?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(GenError::Fatal(anyhow!(
            "ollama error {status}: {}",
            head_chars(&text, 300)
        )));
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| GenError::Fatal(anyhow!(e)))?;
    v.pointer("/message/content")
        .and_then(|c| c.as_str())
        .map(|c| (c.to_string(), Backend::Ollama))
        .ok_or_else(|| GenError::Fatal(anyhow!("ollama response had no message.content")))
}

async fn generate_openrouter(
    prompt: &str,
    settings: &Settings,
) -> Result<(String, Backend), GenError> {
    let key = std::env::var("OPENROUTER_API_KEY").map_err(|_| missing_key("OPENROUTER_API_KEY"))?;
    let body = json!({
        "model": settings.openrouter_model,
        "messages": [{"role": "user", "content": prompt}],
        "temperature": 0,
        "max_tokens": 16384,
        "response_format": {"type": "json_object"},
    });
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(|e| GenError::Fatal(anyhow!(e)))?;
    let resp = client
        .post("https://openrouter.ai/api/v1/chat/completions")
        .header("Authorization", format!("Bearer {key}"))
        .header("HTTP-Referer", "https://github.com/lhuthng/storycast")
        .header("X-Title", "storycast")
        .json(&body)
        .send()
        .await
        .map_err(|e| GenError::Fatal(anyhow!("cannot reach OpenRouter ({e})")))?;
    let status = resp.status();
    let retry_after = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<f64>().ok());
    let text = resp.text().await.unwrap_or_default();
    if status.as_u16() == 429 {
        let delay = retry_after.map(|d| d + 2.0).unwrap_or(60.0);
        return Err(GenError::RateLimited(format!(
            "retry in {delay}s: {}",
            head_chars(&text, 200)
        )));
    }
    if !status.is_success() {
        return Err(GenError::Fatal(anyhow!(
            "OpenRouter error {status}: {}",
            head_chars(&text, 200)
        )));
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| GenError::Fatal(anyhow!(e)))?;
    v.pointer("/choices/0/message/content")
        .and_then(|c| c.as_str())
        .map(|c| (c.to_string(), Backend::Openrouter))
        .ok_or_else(|| GenError::Fatal(anyhow!("OpenRouter response had no content")))
}

/// Gemini model chain over REST, ending in opencode as the last resort.
///
/// Skipped fast, never retried: 401/403 (the key is wrong for every model)
/// and 400 (the request itself is bad) — retrying those anywhere is burning
/// quota for nothing. Everything else walks on: 429s (after sleeping the
/// provider's own delay), 5xx, transport errors, unknown-model 404s and spent
/// day-quotas.
async fn generate_gemini(prompt: &str, settings: &Settings) -> Result<(String, Backend), GenError> {
    let key = std::env::var("GEMINI_API_KEY").map_err(|_| missing_key("GEMINI_API_KEY"))?;
    let mut last = String::from("no models configured");
    let chain = analyze_chain(settings);
    if chain.is_empty() {
        // Say so rather than falling through with a reason that reads like a
        // provider fault: an empty chain is a settings mistake.
        eprintln!(
            "gemini: no models configured (analyze_models is empty) — falling back to opencode"
        );
    }
    for model in &chain {
        match try_gemini_model(prompt, &key, model).await {
            ModelNext::Text(t) => return Ok((t, Backend::Gemini)),
            ModelNext::Abort(e) => return Err(GenError::Fatal(e)),
            ModelNext::Skip(reason) => {
                eprintln!("gemini {model} exhausted ({reason}) — next model");
                last = format!("{model}: {reason}");
            }
        }
    }
    // **The line that names the real backend.** Whatever the caller's progress
    // label says, this is what is about to run, and it is the only record of the
    // fallback that survives into the log. It also states the deadline, because
    // "opencode is running" and "opencode is running and will be killed in ten
    // minutes" are very different things to read at 2 a.m.
    eprintln!(
        "gemini chain exhausted ({last}) — running opencode -m {} instead (deadline {}s)",
        settings.opencode_model,
        OPENCODE_TIMEOUT.as_secs()
    );
    match generate_opencode(prompt, settings).await {
        Ok(t) => Ok(t),
        Err(GenError::RateLimited(m)) => Err(GenError::RateLimited(m)),
        Err(GenError::Fatal(e)) => Err(GenError::Fatal(anyhow!(
            "gemini chain exhausted ({last}); opencode fallback failed: {e:#}"
        ))),
    }
}

/// What one model attempt resolved to: text, the next model, or give up now.
enum ModelNext {
    Text(String),
    Skip(String),
    Abort(anyhow::Error),
}

/// Up to three attempts against one Gemini model.
async fn try_gemini_model(prompt: &str, key: &str, model: &str) -> ModelNext {
    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent?key={key}"
    );
    let body = json!({
        "contents": [{"parts": [{"text": prompt}]}],
        "generationConfig": {"responseMimeType": "application/json", "maxOutputTokens": 16384},
    });
    // The default client has **no deadline at all** — the same class of bug as
    // the opencode child below, and just as invisible when it fires.
    let client = match reqwest::Client::builder()
        .timeout(GEMINI_ATTEMPT_TIMEOUT)
        .build()
    {
        Ok(c) => c,
        // `Abort`, not `Skip`: a client that will not build will not build for
        // the next model either, and walking the chain would only burn time.
        Err(e) => return ModelNext::Abort(anyhow!(e).context("building the Gemini HTTP client")),
    };
    let mut last = String::from("no attempts ran");
    for attempt in 0..3 {
        let resp = client.post(&url).json(&body).send().await;
        let (status, text) = match resp {
            Ok(r) => {
                let status = r.status();
                (status, r.text().await.unwrap_or_default())
            }
            Err(e) => {
                // Logged per attempt rather than only summarised at the end:
                // three silent timeouts and three silent 503s produce the same
                // aggregate line, and they are different problems.
                eprintln!("gemini {model} attempt {}/3 — transport: {e}", attempt + 1);
                last = format!("transport error: {e}");
                continue;
            }
        };
        if status.is_success() {
            let v: Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(e) => return ModelNext::Abort(anyhow!(e).context("gemini response not JSON")),
            };
            return match v
                .pointer("/candidates/0/content/parts/0/text")
                .and_then(|t| t.as_str())
                .map(String::from)
            {
                Some(t) => ModelNext::Text(t),
                None => ModelNext::Abort(anyhow!(
                    "gemini response had no text part: {}",
                    head_chars(&text, 300)
                )),
            };
        }
        match status.as_u16() {
            // Wrong key or bad request: identical for every model, stop now.
            401 | 403 => {
                return ModelNext::Abort(anyhow!(
                    "gemini error {status} on {model}: key or project rejected — {}",
                    head_chars(text.trim(), 200)
                ))
            }
            400 => {
                return ModelNext::Abort(anyhow!(
                    "gemini error 400 on {model}: {}",
                    head_chars(text.trim(), 200)
                ))
            }
            // Unknown model name or its day quota spent: the next model is
            // exactly what the chain is for.
            404 => return ModelNext::Skip(format!("{status} ({})", head_chars(text.trim(), 120))),
            _ if text.contains("PerDay") => return ModelNext::Skip("day quota spent".to_string()),
            429 => {
                let wait = parse_retry_delay(&text).map(|d| d + 2.0).unwrap_or(30.0);
                last = format!("429, retry in {wait:.0}s");
                eprintln!(
                    "gemini {model} attempt {}/3 rate-limited, sleeping {:.0}s",
                    attempt + 1,
                    wait
                );
                tokio::time::sleep(Duration::from_secs_f64(wait)).await;
            }
            _ => {
                // A 5xx used to be silent here — the reason only ever appeared in
                // the aggregate "chain exhausted" line, which is why a 503 storm
                // read as "nothing happened".
                eprintln!(
                    "gemini {model} attempt {}/3 — {status}: {}",
                    attempt + 1,
                    head_chars(text.trim(), 160)
                );
                last = format!("{status}: {}", head_chars(text.trim(), 200));
            }
        }
    }
    ModelNext::Skip(last)
}

fn analyze_chain(settings: &Settings) -> Vec<String> {
    settings
        .analyze_models
        .iter()
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
        .collect()
}

/// One generation attempt against the configured backend.
///
/// Returns the text **and the backend that produced it**: the configured name is
/// not the one that ran whenever `gemini` falls back, and a caller that labelled
/// its progress with the configured name would be describing a backend that had
/// already given up.
pub async fn generate(
    prompt: &str,
    analyzer: &str,
    settings: &Settings,
) -> Result<(String, Backend), GenError> {
    match analyzer {
        "local" => generate_ollama(prompt, settings).await,
        "openrouter" => generate_openrouter(prompt, settings).await,
        "opencode" => generate_opencode(prompt, settings).await,
        "gemini" => generate_gemini(prompt, settings).await,
        other => Err(GenError::Fatal(anyhow!(
            "unknown analyzer {other:?} (expected opencode | openrouter | local | gemini)"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analyze_chain_uses_only_configured_models() {
        let plain = Settings::default();
        assert_eq!(analyze_chain(&plain), vec!["gemini-3.5-flash"]);
        for models in [vec![], vec![" ".into()]] {
            let empty = Settings {
                analyze_models: models,
                ..Settings::default()
            };
            assert!(analyze_chain(&empty).is_empty());
        }

        let chained = Settings {
            analyze_models: vec![
                " gemini-3.8-flash ".into(),
                " ".into(),
                "gemini-3.5-flash".into(),
            ],
            ..Settings::default()
        };
        assert_eq!(
            analyze_chain(&chained),
            vec!["gemini-3.8-flash", "gemini-3.5-flash"]
        );
    }

    #[test]
    fn gemini_without_a_key_fails_before_touching_the_network() {
        let saved = std::env::var("GEMINI_API_KEY").ok();
        std::env::remove_var("GEMINI_API_KEY");
        let err = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(generate_gemini("{}", &Settings::default()))
            .unwrap_err();
        assert!(err.to_string().contains("GEMINI_API_KEY missing"), "{err}");
        // The wording is load-bearing: the old one told a provisioned worker
        // (which has no `.env` at all) to copy a `.env.example` that is not
        // there. Both routes have to be named — the key travels with the task.
        assert!(err.to_string().contains("inductor"), "{err}");
        assert!(
            !err.to_string().contains("copy .env.example"),
            "the instruction that sent the operator to a missing file: {err}"
        );
        if let Some(k) = saved {
            std::env::set_var("GEMINI_API_KEY", k);
        }
    }

    #[test]
    fn both_missing_key_errors_name_their_own_variable() {
        // One helper, two callers: the message must not be able to drift into
        // blaming the wrong provider, and it must stay short enough for the
        // TUI's 200-char event line.
        for var in ["GEMINI_API_KEY", "OPENROUTER_API_KEY"] {
            let msg = missing_key(var).to_string();
            assert!(msg.starts_with(var), "{msg}");
            assert!(msg.contains("inductor"), "{msg}");
            assert!(msg.len() < 200, "{} chars: {msg}", msg.len());
        }
    }

    #[test]
    fn a_worker_with_no_settings_file_runs_the_inductors_model() {
        // The reported outage, in one assertion. A provisioned box has no
        // `.bm/settings.json` (provisioning never copies `.bm/`), so
        // `Settings::load` returns the compiled default — whose `analyze_models`
        // is the literal below. The operator had switched to `-lite`, the box
        // kept calling the old model, and the only trace was a 503 naming it.
        let remote_box = Settings::default();
        assert_eq!(
            analyze_chain(&remote_box),
            vec!["gemini-3.5-flash"],
            "this is the compiled default that caused the outage — if you are \
             changing it, change this test with it"
        );

        // What the offer carries now.
        let inductor = Settings {
            analyze_models: vec!["gemini-3.5-flash-lite".into()],
            ..Settings::default()
        };
        let effective = remote_box.with_analyzer_settings(&inductor.analyzer_settings());
        assert_eq!(analyze_chain(&effective), vec!["gemini-3.5-flash-lite"]);

        // And an inductor that says nothing leaves the box exactly as it was.
        let silent = remote_box.with_analyzer_settings(&bm_proto::AnalyzerSettings::default());
        assert_eq!(analyze_chain(&silent), analyze_chain(&remote_box));
    }

    #[tokio::test]
    async fn the_opencode_child_is_killed_at_its_deadline() {
        // The bug that cost ~80 minutes of silent looping on 2026-09-22:
        // `Command::output()` has no deadline of its own, so a CLI that stalls
        // blocks the worker for ever. The stage sits at whatever percentage it
        // had reached, prints nothing, and the only thing that ever happens is
        // the lease expiring — silently, without a strike — and the task being
        // handed to another box, which stalls the same way.
        //
        // A 500 ms deadline against the **real** CLI, because a fake one would
        // only prove the fake hangs. Measured: `opencode run -m <model> "say hi"`
        // prints its banner and then produces nothing at all, so this exercises
        // the deadline branch rather than pretending to.
        //
        // **Guarded, and loudly.** `opencode` is installed by provisioning rather
        // than by cargo, so on a machine without it this prints what it did *not*
        // check instead of failing for the environment's sake.
        if std::process::Command::new("opencode")
            .arg("--version")
            .output()
            .is_err()
        {
            println!("SKIP: no `opencode` on PATH — the deadline branch was NOT checked here");
            return;
        }
        let err =
            generate_opencode_within("say hi", &Settings::default(), Duration::from_millis(500))
                .await
                .expect_err("half a second is not enough for the CLI to answer");
        let msg = err.to_string();
        assert!(msg.contains("produced nothing"), "{msg}");
        assert!(msg.contains("was killed"), "{msg}");
        assert!(
            msg.contains("by hand"),
            "and names the next step, because the operator's instinct is to blame the \
             digest rather than the CLI: {msg}"
        );
        assert!(
            matches!(err, GenError::Fatal(_)),
            "a deadline is fatal for this round — retrying a hung CLI is what made it \
             invisible in the first place"
        );
    }

    #[test]
    fn retry_delay_parses_both_provider_shapes() {
        assert_eq!(
            parse_retry_delay("... retry in 53.262507263s"),
            Some(53.262507263)
        );
        assert_eq!(parse_retry_delay(r#"{"retryDelay": "53s"}"#), Some(53.0));
        assert_eq!(parse_retry_delay("no hint here"), None);
    }

    #[test]
    fn json_object_is_extracted_from_surrounding_prose() {
        let t = "Sure! Here you go:\n{\"a\": 1}\nHope that helps.";
        assert_eq!(extract_json_object(t).unwrap(), "{\"a\": 1}");
        assert!(extract_json_object("no braces").is_err());
    }
}
