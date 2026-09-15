//! Thin HTTP client for the Python TTS sidecar.
//!
//! The agent never loads a model. Every render is one request to whichever
//! machine runs the sidecar — normally the agent's own, so it is a loopback
//! call and the audio never crosses the network.

use anyhow::{Context, Result};
use serde_json::json;
use std::time::Duration;

#[derive(Clone)]
pub struct Tts {
    base: String,
    http: reqwest::Client,
}

impl Tts {
    pub fn new(base: &str) -> Self {
        Tts {
            base: base.trim_end_matches('/').to_string(),
            // Rendering a long run can take minutes on a slow CPU.
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(900))
                .build()
                .unwrap_or_default(),
        }
    }

    pub async fn health(&self) -> bool {
        match self
            .http
            .get(format!("{}/health", self.base))
            .timeout(Duration::from_secs(5))
            .send()
            .await
        {
            Ok(r) => r.status().is_success(),
            Err(_) => false,
        }
    }

    /// The installed roster as `(label, id)` pairs.
    #[allow(dead_code)]
    pub async fn voices(&self) -> Result<Vec<(String, String)>> {
        let resp = self
            .http
            .get(format!("{}/voices", self.base))
            .send()
            .await
            .with_context(|| format!("GET {}/voices", self.base))?;
        let v: Vec<Vec<String>> = resp.json().await.context("parsing /voices")?;
        Ok(v.into_iter()
            .filter_map(|pair| match pair.as_slice() {
                [label, id] => Some((label.clone(), id.clone())),
                _ => None,
            })
            .collect())
    }

    /// Accent policy + roster + default cast. The sidecar is the authority;
    /// `bm_core::voices` is the offline fallback.
    pub async fn policy(&self) -> Result<serde_json::Value> {
        let resp = self
            .http
            .get(format!("{}/policy", self.base))
            .send()
            .await
            .with_context(|| format!("GET {}/policy", self.base))?;
        resp.json()
            .await
            .context("parsing /policy")
    }

    /// Render one utterance to WAV bytes at the engine's native sample rate.
    pub async fn infer(
        &self,
        text: &str,
        voice: &str,
        temperature: f64,
        silence_p: f64,
        engine: &str,
    ) -> Result<Vec<u8>> {
        let body = json!({
            "text": text,
            "voice": voice,
            "temperature": temperature,
            "silence_p": silence_p,
            "engine": engine,
        });
        let resp = self
            .http
            .post(format!("{}/infer", self.base))
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {}/infer", self.base))?;
        let status = resp.status();
        if !status.is_success() {
            let detail = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "TTS worker error {status}: {}",
                bm_core::util::head_chars(&detail, 300)
            );
        }
        let bytes = resp.bytes().await.context("reading wav body")?;
        if bytes.len() < 1000 {
            anyhow::bail!("TTS worker returned a suspiciously small wav ({} bytes)", bytes.len());
        }
        Ok(bytes.to_vec())
    }
}
