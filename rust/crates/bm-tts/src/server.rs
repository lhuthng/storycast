//! The HTTP surface, replacing `python/tts_server.py`.

use crate::codec::to_wav_bytes;
use crate::engine::Request;
#[cfg(feature = "pocket")]
use crate::pocket::Pocket;
use crate::sample::{Rng, Sampling};
use crate::synth::{gaps_to_silence, join_with_pauses, Synth, SAMPLE_RATE};
use crate::text::FrontEnd;
use crate::voice::Roster;
use axum::{
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex, OnceLock};

/// Fixed on purpose: two voice samples are only comparable if both say the same
pub const PREVIEW_TEXT_VIENEU: &str = "Xin chào, đây là giọng đọc thử của bộ truyện.";
pub const PREVIEW_TEXT_POCKET: &str =
    "Hello, this is a storycast voice preview. The narrator reads the chapters.";

/// The HTTP surface's shared state, constructed **empty** and filled once the
pub struct Server {
    inner: OnceLock<Inner>,
    /// Fired by `POST /shutdown`, awaited by `main`. A `Notify` rather than a
    shutdown: tokio::sync::Notify,
}

struct Inner {
    backend: Backend,
}

/// The two engines the one HTTP surface serves. The contract — `/infer` in,
#[allow(clippy::large_enum_variant)] // one Backend per process, behind a OnceLock
pub enum Backend {
    Vieneu {
        front: FrontEnd,
        roster: Roster,
        synth: Mutex<Synth>,
    },
    // Absent entirely without the `pocket` feature, so a VieNeu-only build
    #[cfg(feature = "pocket")]
    Pocket { engine: Box<Mutex<Pocket>> },
}
// The size difference between the variants is real but irrelevant: an `Inner`

impl Server {
    pub fn new() -> Arc<Server> {
        Arc::new(Server {
            inner: OnceLock::new(),
            shutdown: tokio::sync::Notify::new(),
        })
    }

    /// Resolves once [`Server::request_shutdown`] has been called. `Notify`
    pub async fn await_shutdown(&self) {
        self.shutdown.notified().await;
    }

    /// Ask the process to exit. Idempotent; the last word is `main`'s.
    pub fn request_shutdown(&self) {
        self.shutdown.notify_one();
    }

    /// Install the loaded model. Once; a later call is ignored.
    pub fn fill(&self, backend: Backend) {
        let _ = self.inner.set(Inner { backend });
    }

    /// The loaded state, or `None` while the model is still loading.
    fn inner(&self) -> Option<&Inner> {
        self.inner.get()
    }
}

/// One voice's picker label: the description after an em dash, or bare.
fn label(name: &str, description: &str) -> String {
    if description.is_empty() {
        name.to_string()
    } else {
        format!("{name} — {description}")
    }
}

impl Inner {
    /// Render one text with one voice, start to finish — whichever engine is
    fn render(
        &self,
        text: &str,
        voice: Option<&str>,
        temperature: f64,
        seed: u64,
    ) -> anyhow::Result<(Vec<f32>, usize)> {
        match &self.backend {
            Backend::Vieneu {
                front,
                roster,
                synth,
            } => {
                let voice = roster.resolve(voice)?;
                let chunks = front.chunks_sentence_level(text);
                if chunks.chunks.is_empty() {
                    return Ok((Vec::new(), SAMPLE_RATE));
                }
                let mut synth = synth
                    .lock()
                    .map_err(|e| anyhow::anyhow!("the render lock is poisoned: {e}"))?;
                let mut rng = Rng::new(seed);
                let mut wavs = Vec::with_capacity(chunks.chunks.len());
                for ch in &chunks.chunks {
                    let phonemes = front.phonemize_with_emotions(ch);
                    let mut req = Request::new(&phonemes);
                    req.sampling = Sampling {
                        temperature,
                        ..Default::default()
                    };
                    req.speaker_emb = Some(&voice.speaker_emb);
                    req.ref_codes = Some(&voice.codes);
                    wavs.push(synth.chunk(&req, &mut rng)?.pcm);
                }
                Ok((
                    join_with_pauses(&wavs, &gaps_to_silence(&chunks.gaps), SAMPLE_RATE),
                    SAMPLE_RATE,
                ))
            }
            #[cfg(feature = "pocket")]
            Backend::Pocket { engine } => {
                let mut engine = engine
                    .lock()
                    .map_err(|e| anyhow::anyhow!("the render lock is poisoned: {e}"))?;
                engine.generate(text, voice.unwrap_or_default(), temperature, seed)
            }
        }
    }

    /// The engine's name, for `/policy` and the roster.
    fn engine_name(&self) -> &'static str {
        match &self.backend {
            Backend::Vieneu { .. } => "vieneu",
            #[cfg(feature = "pocket")]
            Backend::Pocket { .. } => "pocket",
        }
    }

    /// `(label, id)` pairs, the shape the reference's SDK returns.
    fn labels(&self) -> Vec<(String, String)> {
        match &self.backend {
            Backend::Vieneu { roster, .. } => roster
                .voices
                .values()
                .map(|v| (label(&v.name, &v.description), v.name.clone()))
                .collect(),
            #[cfg(feature = "pocket")]
            Backend::Pocket { engine } => match engine.try_lock() {
                Ok(engine) => engine
                    .voices
                    .values()
                    .map(|v| (label(&v.name, &v.description), v.name.clone()))
                    .collect(),
                Err(_) => Vec::new(),
            },
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct InferBody {
    pub text: String,
    #[serde(default)]
    pub voice: Option<String>,
    #[serde(default = "default_temperature")]
    pub temperature: f64,
    /// Accepted and ignored, exactly as the reference ignores it on this path:
    #[serde(default)]
    pub silence_p: Option<f64>,
    /// Accepted and ignored: this server is VieNeu-only, and the reference's
    #[serde(default)]
    pub engine: Option<String>,
}

fn default_temperature() -> f64 {
    0.8
}

#[derive(Debug, Deserialize)]
pub struct PreviewBody {
    #[serde(default)]
    pub voice: Option<String>,
}

fn wav(pcm: &[f32], sample_rate: usize) -> Response {
    if pcm.is_empty() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": "TTS produced no audio samples (input may contain only punctuation)"
            })),
        )
            .into_response();
    }
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "audio/wav")],
        to_wav_bytes(pcm, sample_rate as u32),
    )
        .into_response()
}

fn validate_infer_text(text: &str) -> std::result::Result<(), &'static str> {
    if text.trim().is_empty() {
        Err("text is required")
    } else if !bm_core::util::has_speakable_content(text) {
        Err("text contains no speakable content (punctuation-only)")
    } else {
        Ok(())
    }
}

fn failed(e: anyhow::Error) -> Response {
    // Truncated like the reference: a full ONNX traceback in an HTTP body helps
    let msg = format!("{e:#}");
    let short: String = msg.chars().take(300).collect();
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({"error": short})),
    )
        .into_response()
}

/// 503 while the weights are still loading, 200 once they are in.
fn loading() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"status": "loading"})),
    )
        .into_response()
}

async fn health(State(s): State<Arc<Server>>) -> Response {
    if s.inner().is_some() {
        Json(serde_json::json!({"ok": true})).into_response()
    } else {
        loading()
    }
}

/// Ask the process to exit. **Deliberately works while loading too** — a sidecar
async fn shutdown(State(s): State<Arc<Server>>) -> Response {
    s.request_shutdown();
    Json(serde_json::json!({"ok": true, "exiting": true})).into_response()
}

async fn voices(State(s): State<Arc<Server>>) -> Response {
    match s.inner() {
        Some(i) => Json(i.labels()).into_response(),
        None => loading(),
    }
}

/// The structured roster: name, gender, accent, style, language.
async fn roster(State(s): State<Arc<Server>>) -> Response {
    match s.inner() {
        Some(i) => Json(bm_core::voices::voices_from_labels(
            i.engine_name(),
            &i.labels(),
        ))
        .into_response(),
        None => loading(),
    }
}

#[derive(Serialize)]
struct PolicyResponse {
    engine: &'static str,
    sample_rate: u32,
    male_voices: Vec<String>,
    female_voices: Vec<String>,
    allowed_voices: Vec<String>,
    default_cast: std::collections::BTreeMap<String, String>,
}

async fn policy(State(s): State<Arc<Server>>) -> Response {
    let Some(i) = s.inner() else {
        return loading();
    };
    let (name, rate, pools) = match &i.backend {
        Backend::Vieneu { .. } => (
            "vieneu",
            SAMPLE_RATE as u32,
            bm_core::voices::vieneu_policy(),
        ),
        #[cfg(feature = "pocket")]
        Backend::Pocket { .. } => ("pocket", 24_000, bm_core::voices::pocket_policy()),
    };
    let mut cast = std::collections::BTreeMap::new();
    for (character, voice) in pools.default_cast {
        cast.insert(character, voice);
    }
    Json(PolicyResponse {
        engine: name,
        sample_rate: rate,
        male_voices: pools.male,
        female_voices: pools.female,
        // Always empty: the field stays because `bm-agent` probes for it to
        allowed_voices: Vec::new(),
        default_cast: cast,
    })
    .into_response()
}

async fn infer(State(s): State<Arc<Server>>, Json(body): Json<InferBody>) -> Response {
    let Some(inner) = s.inner() else {
        return loading();
    };
    if let Err(error) = validate_infer_text(&body.text) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": error})),
        )
            .into_response();
    }
    match inner.render(&body.text, body.voice.as_deref(), body.temperature, seed()) {
        Ok((pcm, rate)) => wav(&pcm, rate),
        Err(e) => failed(e),
    }
}

async fn preview(State(s): State<Arc<Server>>, Json(body): Json<PreviewBody>) -> Response {
    let Some(inner) = s.inner() else {
        return loading();
    };
    let text = match inner.backend {
        Backend::Vieneu { .. } => PREVIEW_TEXT_VIENEU,
        #[cfg(feature = "pocket")]
        Backend::Pocket { .. } => PREVIEW_TEXT_POCKET,
    };
    match inner.render(text, body.voice.as_deref(), 0.8, seed()) {
        Ok((pcm, rate)) => wav(&pcm, rate),
        Err(e) => failed(e),
    }
}

/// A per-request seed. The render is stochastic, so two calls should differ —
fn seed() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

pub fn router(server: Arc<Server>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/voices", get(voices))
        .route("/roster", get(roster))
        .route("/policy", get(policy))
        .route("/infer", post(infer))
        .route("/preview", post(preview))
        .route("/shutdown", post(shutdown))
        .with_state(server)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(dir: &std::path::Path) -> std::path::PathBuf {
        let p = dir.join("voices.json");
        std::fs::write(
            &p,
            r#"{"default_voice":"B","presets":{
                "A":{"description":"Nam · Bắc · Kể chuyện","gender":"male","style":"doc_truyen",
                     "speaker_emb":[0.1],"codes":[[1]]},
                "B":{"description":"Nữ · Nam · Tự nhiên","gender":"female","style":"tu_nhien",
                     "speaker_emb":[0.2],"codes":[[2]]}}}"#,
        )
        .unwrap();
        p
    }

    #[test]
    fn labels_carry_the_description_after_an_em_dash() {
        let dir = tempfile::tempdir().unwrap();
        let r = Roster::load(&store(dir.path())).unwrap();
        let l: Vec<(String, String)> = r
            .voices
            .values()
            .map(|v| (label(&v.name, &v.description), v.name.clone()))
            .collect();
        assert_eq!(
            l[0],
            ("A — Nam · Bắc · Kể chuyện".to_string(), "A".to_string())
        );
        assert_eq!(l[1].0, "B — Nữ · Nam · Tự nhiên");
    }

    /// The roster is derived from the same labels `/voices` sends, so `bm-core`
    #[test]
    fn the_roster_reads_gender_and_accent_positionally() {
        let dir = tempfile::tempdir().unwrap();
        let r = Roster::load(&store(dir.path())).unwrap();
        let l: Vec<(String, String)> = r
            .voices
            .values()
            .map(|v| (label(&v.name, &v.description), v.name.clone()))
            .collect();
        let v = bm_core::voices::voices_from_labels("vieneu", &l);
        let a = v.iter().find(|v| v.name == "A").unwrap();
        assert_eq!(a.gender, "male");
        assert_eq!(a.accent, "Northern");
        assert_eq!(a.style, "Kể chuyện");
        // "Nam" in the *accent* slot is South, not male — the trap this parsing
        let b = v.iter().find(|v| v.name == "B").unwrap();
        assert_eq!(b.gender, "female");
        assert_eq!(b.accent, "South");
    }

    #[test]
    fn the_policy_keeps_the_field_names_the_agent_probes_for() {
        let p = bm_core::voices::vieneu_policy();
        let body = serde_json::to_value(PolicyResponse {
            engine: "vieneu",
            sample_rate: SAMPLE_RATE as u32,
            male_voices: p.male,
            female_voices: p.female,
            // Always empty: the field stays because `bm-agent` probes for it to
            allowed_voices: Vec::new(),
            default_cast: Default::default(),
        })
        .unwrap();
        // `bm-agent` decides a server is current by finding this array.
        assert!(body
            .get("allowed_voices")
            .and_then(|v| v.as_array())
            .is_some());
        assert!(body.get("male_voices").and_then(|v| v.as_array()).is_some());
        assert_eq!(body["sample_rate"], 48_000);
    }

    #[test]
    fn infer_rejects_empty_or_punctuation_only_text_before_synthesis() {
        assert_eq!(validate_infer_text("  "), Err("text is required"));
        assert_eq!(
            validate_infer_text(","),
            Err("text contains no speakable content (punctuation-only)")
        );
        assert_eq!(validate_infer_text("Ừm!"), Ok(()));
        assert_eq!(validate_infer_text("[cười]"), Ok(()));
    }

    #[test]
    fn a_wav_response_is_audio_not_json() {
        let r = wav(&[0.0, 0.5], 48_000);
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers()[header::CONTENT_TYPE], "audio/wav");
    }

    #[test]
    fn empty_audio_is_not_wrapped_as_a_valid_44_byte_wav() {
        let r = wav(&[], 48_000);
        assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[test]
    fn a_failure_is_reported_without_a_traceback() {
        let r = failed(anyhow::anyhow!("{}", "x".repeat(500)));
        assert_eq!(r.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
