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

async fn generate_opencode(prompt: &str, settings: &Settings) -> Result<String, GenError> {
    let full = format!(
        "Do not use any tools. Answer with the requested output and nothing else.\n\n{prompt}"
    );
    let out = tokio::process::Command::new("opencode")
        .args(["run", "-m", &settings.opencode_model, &full])
        .output()
        .await
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                GenError::Fatal(anyhow!("opencode CLI not found — install it first"))
            } else {
                GenError::Fatal(anyhow!(e).context("running opencode"))
            }
        })?;
    if !out.status.success() {
        return Err(GenError::Fatal(anyhow!(
            "opencode run failed: {}",
            head_chars(&String::from_utf8_lossy(&out.stderr), 500)
        )));
    }
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    extract_json_object(&stdout).map_err(GenError::Fatal)
}

async fn generate_ollama(prompt: &str, settings: &Settings) -> Result<String, GenError> {
    let body = json!({
        "model": settings.local_model,
        "messages": [{"role": "user", "content": prompt}],
        "stream": false,
        "format": "json",
        "options": {"temperature": 0, "num_ctx": 16384},
    });
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
        .map(String::from)
        .ok_or_else(|| GenError::Fatal(anyhow!("ollama response had no message.content")))
}

async fn generate_openrouter(prompt: &str, settings: &Settings) -> Result<String, GenError> {
    let key = std::env::var("OPENROUTER_API_KEY")
        .map_err(|_| GenError::Fatal(anyhow!("OPENROUTER_API_KEY missing — add it to .env")))?;
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
        .map(String::from)
        .ok_or_else(|| GenError::Fatal(anyhow!("OpenRouter response had no content")))
}

/// Gemini model chain over REST, ending in opencode as the last resort.
///
/// `analyze_models` (when set) IS the chain; otherwise the legacy single
/// `analyze_model` stands alone, which is today's behavior. Each model gets a
/// few attempts, then the next one, then opencode.
///
/// Skipped fast, never retried: 401/403 (the key is wrong for every model)
/// and 400 (the request itself is bad) — retrying those anywhere is burning
/// quota for nothing. Everything else walks on: 429s (after sleeping the
/// provider's own delay), 5xx, transport errors, unknown-model 404s and spent
/// day-quotas.
async fn generate_gemini(prompt: &str, settings: &Settings) -> Result<String, GenError> {
    let key = std::env::var("GEMINI_API_KEY").map_err(|_| {
        GenError::Fatal(anyhow!(
            "GEMINI_API_KEY missing — copy .env.example to .env"
        ))
    })?;
    let mut last = String::from("no models configured");
    for model in analyze_chain(settings) {
        match try_gemini_model(prompt, &key, &model).await {
            ModelNext::Text(t) => return Ok(t),
            ModelNext::Abort(e) => return Err(GenError::Fatal(e)),
            ModelNext::Skip(reason) => {
                eprintln!("gemini {model} exhausted ({reason}) — next model");
                last = format!("{model}: {reason}");
            }
        }
    }
    eprintln!("gemini chain exhausted ({last}) — falling back to opencode");
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
    let client = reqwest::Client::new();
    let mut last = String::from("no attempts ran");
    for attempt in 0..3 {
        let resp = client.post(&url).json(&body).send().await;
        let (status, text) = match resp {
            Ok(r) => {
                let status = r.status();
                (status, r.text().await.unwrap_or_default())
            }
            Err(e) => {
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
                last = format!("{status}: {}", head_chars(text.trim(), 200));
            }
        }
    }
    ModelNext::Skip(last)
}

/// Models to try, in order: `analyze_models` when set, else the legacy single.
fn analyze_chain(settings: &Settings) -> Vec<String> {
    let chain: Vec<String> = settings
        .analyze_models
        .iter()
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
        .collect();
    if chain.is_empty() {
        vec![settings.analyze_model.clone()]
    } else {
        chain
    }
}

/// One generation attempt against the configured backend.
pub async fn generate(
    prompt: &str,
    analyzer: &str,
    settings: &Settings,
) -> Result<String, GenError> {
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
    fn analyze_chain_defaults_to_the_single_model() {
        let plain = Settings::default();
        assert!(plain.analyze_models.is_empty());
        assert_eq!(analyze_chain(&plain), vec![plain.analyze_model.clone()]);

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
        if let Some(k) = saved {
            std::env::set_var("GEMINI_API_KEY", k);
        }
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
