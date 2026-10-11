//! Live check: a provider the operator **names**, end to end — the entry the
//! `L` screen's `n` writes, routed to a real endpoint.
//!
//! Run by hand (nothing in CI reaches a network):
//!
//! ```text
//! cd rust
//! BM_LLM_URL=https://tokenharbor.ai BM_LLM_KEY=… BM_LLM_MODEL=… \
//!   cargo test -p bm-core --test live_llm -- --ignored --nocapture
//! ```
//!
//! One URL serves both shapes, because they are protocols rather than vendors:
//! the OpenAI-compatible one posts `{url}/v1/chat/completions`, the Gemini one
//! `{url}/v1beta/models/{model}:generateContent`. `BM_LLM_MODEL` defaults to the
//! Gemini chat model the proxy in the notes serves.
#![allow(clippy::print_stdout)]

use bm_core::config::{LlmConfig, ProviderEntry, Settings};
use bm_core::digest::{fetch_models, generate};

/// The endpoint root: a gateway, a proxy, or a local server.
fn url() -> String {
    std::env::var("BM_LLM_URL")
        .expect("BM_LLM_URL — the endpoint root, e.g. https://tokenharbor.ai")
        .trim_end_matches('/')
        .to_string()
}

fn key() -> String {
    std::env::var("BM_LLM_KEY").expect("BM_LLM_KEY — what the endpoint accepts as its key")
}

fn model() -> String {
    std::env::var("BM_LLM_MODEL").unwrap_or_else(|_| "gemini-3.1-flash-lite".to_string())
}

/// The provider as the screen would have written it, resolved the same way.
fn named(kind: &str, base: &str, model: &str, key: &str) -> LlmConfig {
    LlmConfig {
        active: "live-gateway".into(),
        providers: [(
            "live-gateway".to_string(),
            ProviderEntry {
                kind: kind.into(),
                base_url: base.into(),
                api_key: key.into(),
                model: model.into(),
            },
        )]
        .into_iter()
        .collect(),
    }
}

/// Ask it for JSON, because that is what the digest lane asks for.
const PROMPT: &str = "Answer with a JSON object of one field: {\"word\": \"pong\"}";

async fn digests(cfg: &LlmConfig, base: &str, kind: &str) -> String {
    let (analyzer, block) = cfg.offer_analyzer(&Settings::default());
    assert_eq!(analyzer, "live-gateway", "the id is what the offer names");
    // What a provisioned box does with the offer: no settings file of its own.
    let settings = Settings::default().with_analyzer_settings(&block);

    match fetch_models(&analyzer, kind, base, &key()).await {
        Ok(models) => println!(
            "listing: {} model(s), first {:?}",
            models.len(),
            models.first()
        ),
        Err(e) => println!("listing refused ({e:#}) — the screen would say the same"),
    }
    let (text, backend) = generate(PROMPT, &analyzer, &settings)
        .await
        .expect("the endpoint answers");
    println!("{backend:?} via {base}: {text}");
    text
}

#[tokio::test]
#[ignore = "live: reaches a real endpoint, run by hand"]
async fn an_openai_compatible_gateway_the_operator_named_answers() {
    // The key rides the environment, exactly as it does on a worker: the offer
    // carries `OPENROUTER_API_KEY` for anything whose kind is not gemini.
    std::env::set_var("OPENROUTER_API_KEY", key());
    let base = format!("{}/v1", url());
    let cfg = named("openai", &base, &model(), &key());
    let text = digests(&cfg, &base, "openai").await;
    assert!(text.to_lowercase().contains("pong"), "{text}");
}

#[tokio::test]
#[ignore = "live: reaches a real endpoint, run by hand"]
async fn a_gemini_shaped_gateway_the_operator_named_answers() {
    std::env::set_var("GEMINI_API_KEY", key());
    let base = url();
    let m = model();
    let cfg = named("gemini", &base, &m, &key());
    assert_eq!(
        cfg.resolve().map(|r| r.key_var),
        Some("GEMINI_API_KEY"),
        "the protocol picks the credential slot"
    );
    let text = digests(&cfg, &base, "gemini").await;
    assert!(text.to_lowercase().contains("pong"), "{text}");
}
