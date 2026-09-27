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
/// that ran are not the same thing only while a chain walks: the caller must
/// label its progress with the backend that actually produced the answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Gemini,
    Openrouter,
    Ollama,
}

impl Backend {
    pub fn as_str(self) -> &'static str {
        match self {
            Backend::Gemini => "gemini",
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

/// The error for a provider key this process does not hold.
///
/// The key arrives with the task (`bm_proto::Credentials`): the inductor is
/// the single machine whose `.bm/llm.json` the operator maintains (TUI: `L`),
/// and each offer carries the active provider's key to the box that runs it.
/// A worker has no key file of its own, so "set it where it travels from".
fn missing_key(var: &str) -> GenError {
    GenError::Fatal(anyhow!(
        "{var} missing — the task carried no key; add one on the inductor with L (:llm) and retry"
    ))
}

async fn generate_ollama(prompt: &str, settings: &Settings) -> Result<(String, Backend), GenError> {
    let body = json!({
        "model": settings.local_model,
        "messages": [{"role": "user", "content": prompt}],
        "stream": false,
        // A schema where the pass has one, else JSON mode. Ollama enforces a
        // schema in `format`, so a malformed answer is not emitted to repair.
        "format": digest_schema(prompt).unwrap_or_else(|| json!("json")),
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

/// The base a path is appended to: trailing slashes go, and so does a
/// pasted full endpoint (`…/v1/chat/completions` from a provider's docs) —
/// the code appends the path itself, so keeping it would double it.
fn normalize_base(url: &str) -> String {
    let u = url.trim_end_matches('/');
    u.strip_suffix("/chat/completions").unwrap_or(u).to_string()
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
    // The endpoint is settings, not a constant: the public API by default, a
    // gateway or a proxy where the key actually lives. The path is appended, so
    // `API=https://openrouter.ai/api/v1` is the whole address.
    //
    // `who` names the provider id for every message below: this one function
    // serves OpenRouter and every custom gateway, and "OpenRouter error 502"
    // for a TokenHarbor outage sends the operator to the wrong dashboard.
    let who = settings.analyzer.trim();
    let who = if who.is_empty() { "OpenRouter" } else { who };
    let url = format!("{}/chat/completions", normalize_base(&settings.openrouter_url));
    let resp = client
        .post(url)
        .header("Authorization", format!("Bearer {key}"))
        .header("HTTP-Referer", "https://github.com/lhuthng/storycast")
        .header("X-Title", "storycast")
        .json(&body)
        .send()
        .await
        .map_err(|e| GenError::Fatal(anyhow!("cannot reach {who} ({e})")))?;
    let status = resp.status();
    let retry_after = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<f64>().ok());
    let text = resp.text().await.unwrap_or_default();
    // 429 is quota; 502/503/529 is the provider at peak demand — both are
    // "try again shortly", and the digest sleeps out the provider's own
    // delay (or a minute) and retries. Anything else is fatal for the round:
    // a 401 is a dead key, a 400/404 a dead request, and retrying those
    // strikes the chapter for nothing.
    if matches!(status.as_u16(), 429 | 502 | 503 | 529) {
        let delay = retry_after.map(|d| d + 2.0).unwrap_or(60.0);
        return Err(GenError::RateLimited(format!(
            "retry in {delay}s: {}",
            head_chars(&text, 200)
        )));
    }
    if !status.is_success() {
        return Err(GenError::Fatal(anyhow!(
            "{who} error {status}: {}",
            head_chars(&text, 200)
        )));
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| GenError::Fatal(anyhow!(e)))?;
    v.pointer("/choices/0/message/content")
        .and_then(|c| c.as_str())
        .map(|c| (c.to_string(), Backend::Openrouter))
        .ok_or_else(|| GenError::Fatal(anyhow!("{who} response had no content")))
}

/// Gemini model chain over REST.
///
/// Skipped fast, never retried: 401/403 (the key is wrong for every model)
/// and 400 (the request itself is bad) — retrying those anywhere is burning
/// quota for nothing. Everything else walks on: 429s (after sleeping the
/// provider's own delay), 5xx, transport errors, unknown-model 404s and spent
/// day-quotas. An exhausted chain is fatal: there is no fallback backend.
async fn generate_gemini(prompt: &str, settings: &Settings) -> Result<(String, Backend), GenError> {
    let key = std::env::var("GEMINI_API_KEY").map_err(|_| missing_key("GEMINI_API_KEY"))?;
    let mut last = String::from("no models configured");
    let chain = analyze_chain(settings);
    if chain.is_empty() {
        // Say so rather than falling through with a reason that reads like a
        // provider fault: an empty chain is a settings mistake.
        return Err(GenError::Fatal(anyhow!(
            "gemini: no models configured — pick one with L (:llm) on the inductor"
        )));
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
    Err(GenError::Fatal(anyhow!("gemini chain exhausted ({last})")))
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
    // `responseSchema` where the pass has one, so Gemini constrains decoding to
    // the staging shape instead of merely promising JSON. The schema is built to
    // Gemini's OpenAPI subset (no `additionalProperties`, no unions).
    let mut config = json!({
        "responseMimeType": "application/json",
        "maxOutputTokens": 16384,
    });
    if let Some(schema) = digest_schema(prompt) {
        config["responseSchema"] = schema;
    }
    let body = json!({
        "contents": [{"parts": [{"text": prompt}]}],
        "generationConfig": config,
    });
    // The default client has **no deadline at all** — without the timeout
    // below a stalled request blocks the worker until the digest lease fires.
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

/// A decoder-enforced JSON Schema for the staging pass, or `None` for any
/// prompt that is not one.
///
/// Only the staging pass is offered a schema. Its answer is the one with the
/// repeated per-line keys, and its shape is a plain array of objects with no maps,
/// so it fits the OpenAPI subset both Ollama and Gemini accept. The attribution
/// pass carries `mentions`/`speakers` maps, which that subset does not express
/// portably, so it keeps plain JSON mode.
///
/// Selected from the prompt text rather than threaded through every call site:
/// the automatic and manual paths share [`generate`], and a pass identity it
/// does not otherwise use would touch all of them to say one bit.
///
/// Backends that advertise schema enforcement get it; the free OpenRouter model
/// documents JSON mode *without* schema enforcement, so a schema sent there
/// would be ignored at best. Both keep the parse-and-repair path.
pub fn digest_schema(prompt: &str) -> Option<Value> {
    prompt
        .contains("---STAGING OUTPUT CONTRACT---")
        .then(staging_schema)
}

/// The staging answer's strict schema: `segments` plus `fixes`, the fields the
/// contract names and nothing else a decoder might invent.
///
/// `text`, `mood`, `scene` and `music` are optional on purpose: the contract
/// asks for them only where they change and the carry-forward pass fills the
/// rest, so requiring them here would undo the very saving the shape exists for.
fn staging_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "segments": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "source_id": {"type": "string"},
                        "text": {"type": "string"},
                        "mood": {"type": "string"},
                        "scene": {"type": "string"},
                        "music": {"type": "string"},
                        "sound_after": {"type": "string"},
                        "stop_after": {"type": "string"}
                    },
                    "required": ["source_id", "sound_after", "stop_after"]
                }
            },
            "fixes": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "before": {"type": "string"},
                        "after": {"type": "string"}
                    },
                    "required": ["before", "after"]
                }
            }
        },
        "required": ["segments"]
    })
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
/// `analyzer` is the provider id and names the progress lines only. Routing
/// reads `settings.analyzer_backend` — the slot the inductor resolved from
/// the entry's `kind` and sent with the offer. An empty slot means an older
/// inductor, which is routed off its retired `analyzer` value instead (`gemini`
/// | `local` | `openrouter`; anything else, including nothing, refuses).
/// No provider id is matched here, so renaming one never reroutes it.
pub async fn generate(
    prompt: &str,
    analyzer: &str,
    settings: &Settings,
) -> Result<(String, Backend), GenError> {
    let backend = if !settings.analyzer_backend.trim().is_empty() {
        settings.analyzer_backend.clone()
    } else {
        match analyzer.trim() {
            "gemini" => "gemini".into(),
            "local" => "ollama".into(),
            "openrouter" => "openai".into(),
            "" => {
                return Err(GenError::Fatal(anyhow!(
                    "no LLM provider is active — add a key with L (:llm) on the inductor"
                )))
            }
            other => {
                return Err(GenError::Fatal(anyhow!(
                    "unknown analyzer {other:?} — pick one with L (:llm) on the inductor"
                )))
            }
        }
    };
    match backend.as_str() {
        "gemini" => generate_gemini(prompt, settings).await,
        "ollama" => generate_ollama(prompt, settings).await,
        "openai" => generate_openrouter(prompt, settings).await,
        other => Err(GenError::Fatal(anyhow!(
            "unknown backend {other:?} — pick a provider with L (:llm) on the inductor"
        ))),
    }
}

/// List the models a provider serves, for the `L` screen's picker.
///
/// `kind` is the backend slot (`gemini` | `openai` | `ollama`): Google
/// answers `GET {base}/v1beta/models?key=…`
/// (`{"models":[{"name":"models/…"}]}`); Ollama answers `GET
/// {base}/api/tags`; everything else answers the OpenAI-compatible `GET
/// {base}/models` (`{"data":[{"id":…}]}`).
pub async fn fetch_models(provider: &str, kind: &str, base_url: &str, key: &str) -> Result<Vec<String>> {
    let base = normalize_base(base_url);
    let base = base.as_str();
    if base.is_empty() {
        anyhow::bail!("{provider} has no base URL set");
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    let (url, req): (String, reqwest::RequestBuilder) = if kind == "gemini" {
        let req = client.get(format!("{base}/v1beta/models"));
        let req = if key.trim().is_empty() {
            req
        } else {
            req.query(&[("key", key)])
        };
        (format!("{base}/v1beta/models"), req)
    } else if kind == "ollama" {
        (format!("{base}/api/tags"), client.get(format!("{base}/api/tags")))
    } else {
        let req = client.get(format!("{base}/models"));
        let req = if key.trim().is_empty() {
            req
        } else {
            req.header("Authorization", format!("Bearer {key}"))
        };
        (format!("{base}/models"), req)
    };
    let resp = req.send().await.map_err(|e| anyhow!("cannot reach {url} ({e})"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("{provider} returned {status}: {}", head_chars(text.trim(), 160));
    }
    let v: Value = serde_json::from_str(&text)?;
    let mut out: Vec<String> = if kind == "gemini" {
        v.pointer("/models")
            .and_then(|m| m.as_array())
            .map(|ms| {
                ms.iter()
                    .filter_map(|m| m.pointer("/name").and_then(|n| n.as_str()))
                    .map(|n| n.strip_prefix("models/").unwrap_or(n).to_string())
                    .collect()
            })
            .unwrap_or_default()
    } else if kind == "ollama" {
        v.pointer("/models")
            .and_then(|m| m.as_array())
            .map(|ms| {
                ms.iter()
                    .filter_map(|m| m.pointer("/name").and_then(|n| n.as_str()))
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default()
    } else {
        v.pointer("/data")
            .and_then(|m| m.as_array())
            .map(|ms| {
                ms.iter()
                    .filter_map(|m| m.pointer("/id").and_then(|n| n.as_str()))
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default()
    };
    out.sort();
    out.dedup();
    Ok(out)
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
    fn only_the_staging_prompt_gets_a_decoder_schema() {
        let schema = digest_schema("text\n---STAGING OUTPUT CONTRACT---\n{}")
            .expect("the staging pass has a schema");
        assert_eq!(schema["type"], json!("object"));
        assert_eq!(schema["properties"]["segments"]["type"], json!("array"));
        // The per-line fields the carry-forward pass fills must stay optional,
        // or the decoder forces the model to restate them and the saving is
        // undone at the source.
        let required = schema["properties"]["segments"]["items"]["required"]
            .as_array()
            .expect("items have a required list");
        assert!(required.contains(&json!("source_id")));
        assert!(required.contains(&json!("sound_after")));
        assert!(!required.contains(&json!("text")));
        assert!(!required.contains(&json!("mood")));

        // The attribution answer carries `mentions`/`speakers` maps the OpenAPI
        // subset cannot express portably, so it keeps plain JSON mode. So does
        // an old profile with no contract at all.
        assert!(digest_schema("---ATTRIBUTION OUTPUT CONTRACT---").is_none());
        assert!(digest_schema("an old profile with no contract").is_none());
    }

    #[test]
    fn gemini_without_a_key_fails_before_touching_the_network() {
        let _g = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = std::env::var("GEMINI_API_KEY").ok();
        std::env::remove_var("GEMINI_API_KEY");
        let err = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(generate_gemini("{}", &Settings::default()))
            .unwrap_err();
        assert!(err.to_string().contains("GEMINI_API_KEY missing"), "{err}");
        // The wording is load-bearing: a provisioned worker holds no key file
        // at all, so the message must name where the key travels from — the
        // inductor's `L` screen — not a file on the failing box.
        assert!(err.to_string().contains(":llm"), "{err}");
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
            assert!(msg.contains(":llm"), "{msg}");
            assert!(msg.len() < 200, "{} chars: {msg}", msg.len());
        }
    }

    #[test]
    fn routing_reads_the_slot_never_the_label() {
        // The slot arrives in `analyzer_backend` (the offer's block); the id
        // only names progress lines. Keyless, each slot fails on its own key
        // before any I/O — and a mismatched label does not reroute.
        let _g = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved_g = std::env::var("GEMINI_API_KEY").ok();
        let saved_or = std::env::var("OPENROUTER_API_KEY").ok();
        std::env::remove_var("GEMINI_API_KEY");
        std::env::remove_var("OPENROUTER_API_KEY");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let slot = |backend: &str| Settings {
            analyzer_backend: backend.into(),
            ..Settings::default()
        };
        let err = rt
            .block_on(generate("{}", "tokenharbor", &slot("openai")))
            .unwrap_err();
        assert!(err.to_string().contains("OPENROUTER_API_KEY"), "{err}");
        let err = rt
            .block_on(generate("{}", "tokenharbor", &slot("gemini")))
            .unwrap_err();
        assert!(err.to_string().contains("GEMINI_API_KEY"), "{err}");
        // No slot: the retired wire values still route, anything else refuses.
        let err = rt
            .block_on(generate("{}", "gemini", &Settings::default()))
            .unwrap_err();
        assert!(err.to_string().contains("GEMINI_API_KEY"), "{err}");
        let err = rt
            .block_on(generate("{}", "watson", &Settings::default()))
            .unwrap_err();
        assert!(err.to_string().contains("unknown analyzer"), "{err}");
        if let Some(k) = saved_g {
            std::env::set_var("GEMINI_API_KEY", k);
        }
        if let Some(k) = saved_or {
            std::env::set_var("OPENROUTER_API_KEY", k);
        }
    }

    #[test]
    fn no_active_provider_refuses_rather_than_calling_anything() {
        let err = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(generate("{}", "", &Settings::default()))
            .unwrap_err();
        assert!(err.to_string().contains("no LLM provider"), "{err}");
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

    #[test]
    fn a_pasted_full_endpoint_is_trimmed_to_its_base() {
        // Providers document the full `…/v1/chat/completions` path; the code
        // appends it, so keeping it would double it.
        assert_eq!(
            normalize_base("https://tokenharbor.ai/v1/chat/completions"),
            "https://tokenharbor.ai/v1"
        );
        assert_eq!(
            normalize_base("https://openrouter.ai/api/v1/"),
            "https://openrouter.ai/api/v1"
        );
        assert_eq!(normalize_base(""), "");
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
}
