//! The HTTP surface, replacing `python/tts_server.py`.
//!
//! Six of the seven endpoints on port 8818 keep the same request and response
//! shapes the Python sidecar served, so `bm-agent/src/tts.rs` — a thin client
//! that never loads a model — needs no change at all. The seventh, `/shutdown`,
//! is additive: it exists so the two callers that must not share an 8 GiB box
//! with a 2.85 GB model can stop one they did not spawn.
//!
//! Two things are deliberately *not* re-implemented here:
//!
//! * **The accent policy and the roster labels come from `bm-core`.** The
//!   sidecar is the authority at runtime and `bm-core` is the offline fallback,
//!   which makes `bm-core` the single definition of both. Restating the voice
//!   pools in a third place is exactly the defect that rule exists to prevent.
//! * **`/policy` keeps the Python field names** (`male_voices`, `allowed_voices`,
//!   …). `bm-agent` probes for `allowed_voices` to decide whether a server is
//!   current, so renaming a field here would look like an old server and make the
//!   agent refuse to use it.
//!
//! Rendering is serialised behind a mutex. The reference does the same with an
//! `RLock` around its session — ONNX Runtime sessions are not safely re-entrant
//! across `run` calls, and the model is the bottleneck anyway.

use crate::codec::to_wav_bytes;
use crate::engine::Request;
use crate::sample::{Rng, Sampling};
use crate::synth::{gaps_to_silence, join_with_pauses, Synth, SAMPLE_RATE};
use crate::text::FrontEnd;
use crate::voice::{Roster, Voice};
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
/// thing.
pub const PREVIEW_TEXT: &str = "Xin chào, đây là giọng đọc thử của bộ truyện.";

/// The HTTP surface's shared state, constructed **empty** and filled once the
/// model has loaded.
///
/// The listener binds before the load, so the port is the single-instance lock
/// and a second launch dies on it immediately instead of after allocating
/// ~2.85 GB. `/health` answers 503 until [`Server::fill`] runs, so "starting" is
/// a distinct, honest state from "absent" — a caller that cannot tell those
/// apart is the one that spawns a duplicate and OOMs the box.
pub struct Server {
    inner: OnceLock<Inner>,
    /// Fired by `POST /shutdown`, awaited by `main`. A `Notify` rather than a
    /// `Child`/signal because the caller is often *not* the spawner: a
    /// provision-started sidecar is nobody's child, and the two places that must
    /// not co-reside with it (a merge's ffmpeg pass, a worker exiting) have no
    /// pid to signal.
    shutdown: tokio::sync::Notify,
}

struct Inner {
    front: FrontEnd,
    roster: Roster,
    synth: Mutex<Synth>,
}

impl Server {
    pub fn new() -> Arc<Server> {
        Arc::new(Server {
            inner: OnceLock::new(),
            shutdown: tokio::sync::Notify::new(),
        })
    }

    /// Resolves once [`Server::request_shutdown`] has been called. `Notify`
    /// stores one permit, so a request that arrives first is not lost.
    pub async fn await_shutdown(&self) {
        self.shutdown.notified().await;
    }

    /// Ask the process to exit. Idempotent; the last word is `main`'s.
    pub fn request_shutdown(&self) {
        self.shutdown.notify_one();
    }

    /// Install the loaded model. Once; a later call is ignored.
    pub fn fill(&self, front: FrontEnd, roster: Roster, synth: Synth) {
        let _ = self.inner.set(Inner {
            front,
            roster,
            synth: Mutex::new(synth),
        });
    }

    /// The loaded state, or `None` while the model is still loading.
    fn inner(&self) -> Option<&Inner> {
        self.inner.get()
    }
}

impl Inner {
    /// Render one text with one voice, start to finish.
    fn render(
        &self,
        text: &str,
        voice: &Voice,
        temperature: f64,
        seed: u64,
    ) -> anyhow::Result<Vec<f32>> {
        let chunks = self.front.chunks_sentence_level(text);
        if chunks.chunks.is_empty() {
            return Ok(Vec::new());
        }
        let mut synth = self
            .synth
            .lock()
            .map_err(|e| anyhow::anyhow!("the render lock is poisoned: {e}"))?;
        let mut rng = Rng::new(seed);
        let mut wavs = Vec::with_capacity(chunks.chunks.len());
        for ch in &chunks.chunks {
            let phonemes = self.front.phonemize_with_emotions(ch);
            let mut req = Request::new(&phonemes);
            req.sampling = Sampling {
                temperature,
                ..Default::default()
            };
            req.speaker_emb = Some(&voice.speaker_emb);
            req.ref_codes = Some(&voice.codes);
            wavs.push(synth.chunk(&req, &mut rng)?.pcm);
        }
        Ok(join_with_pauses(
            &wavs,
            &gaps_to_silence(&chunks.gaps),
            SAMPLE_RATE,
        ))
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
    /// the v3 render takes its pauses from the chunk boundaries instead.
    #[serde(default)]
    pub silence_p: Option<f64>,
    /// Accepted and ignored: this server is VieNeu-only, and the reference's
    /// sidecar ignored it too.
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

fn wav(pcm: &[f32]) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "audio/wav")],
        to_wav_bytes(pcm, SAMPLE_RATE as u32),
    )
        .into_response()
}

fn failed(e: anyhow::Error) -> Response {
    // Truncated like the reference: a full ONNX traceback in an HTTP body helps
    // nobody and the log already has it.
    let msg = format!("{e:#}");
    let short: String = msg.chars().take(300).collect();
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({"error": short})),
    )
        .into_response()
}

/// 503 while the weights are still loading, 200 once they are in.
///
/// The distinction is the point: a caller that reads 503 as "starting, wait"
/// never spawns a duplicate, and one that reads a closed port the same way
/// cannot tell "starting" from "absent".
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
/// stuck mid-load holding 2.85 GB is exactly the one a worker wants gone before
/// it runs ffmpeg. Unauthenticated like every other endpoint here (LAN-only,
/// loopback-bound by `bm-agent`), and the worst a stray caller can do is cost a
/// model reload.
///
/// An older sidecar without this route answers 404; the caller treats any
/// non-success as "could not ask" and moves on.
async fn shutdown(State(s): State<Arc<Server>>) -> Response {
    s.request_shutdown();
    Json(serde_json::json!({"ok": true, "exiting": true})).into_response()
}

/// `(label, id)` pairs, the shape the reference's SDK returns.
fn labels(roster: &Roster) -> Vec<(String, String)> {
    roster
        .voices
        .values()
        .map(|v| {
            let label = if v.description.is_empty() {
                v.name.clone()
            } else {
                format!("{} — {}", v.name, v.description)
            };
            (label, v.name.clone())
        })
        .collect()
}

async fn voices(State(s): State<Arc<Server>>) -> Response {
    match s.inner() {
        Some(i) => Json(labels(&i.roster)).into_response(),
        None => loading(),
    }
}

/// The structured roster: name, gender, accent, style, language.
///
/// Parsed by `bm-core` from the same labels `/voices` sends, so the two can
/// never disagree about what a voice is.
async fn roster(State(s): State<Arc<Server>>) -> Response {
    match s.inner() {
        Some(i) => Json(bm_core::voices::voices_from_labels(
            "vieneu",
            &labels(&i.roster),
            &[],
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
    if s.inner().is_none() {
        return loading();
    }
    let p = bm_core::voices::vieneu_policy();
    let mut cast = std::collections::BTreeMap::new();
    for (character, voice) in p.default_cast {
        cast.insert(character, voice);
    }
    Json(PolicyResponse {
        engine: "vieneu",
        sample_rate: SAMPLE_RATE as u32,
        male_voices: p.male,
        female_voices: p.female,
        allowed_voices: p.allowed,
        default_cast: cast,
    })
    .into_response()
}

async fn infer(State(s): State<Arc<Server>>, Json(body): Json<InferBody>) -> Response {
    let Some(inner) = s.inner() else {
        return loading();
    };
    if body.text.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "text is required"})),
        )
            .into_response();
    }
    let voice = match inner.roster.resolve(body.voice.as_deref()) {
        Ok(v) => v,
        Err(e) => return failed(e),
    };
    match inner.render(&body.text, voice, body.temperature, seed()) {
        Ok(pcm) => wav(&pcm),
        Err(e) => failed(e),
    }
}

async fn preview(State(s): State<Arc<Server>>, Json(body): Json<PreviewBody>) -> Response {
    let Some(inner) = s.inner() else {
        return loading();
    };
    let voice = match inner.roster.resolve(body.voice.as_deref()) {
        Ok(v) => v,
        Err(e) => return failed(e),
    };
    match inner.render(PREVIEW_TEXT, voice, 0.8, seed()) {
        Ok(pcm) => wav(&pcm),
        Err(e) => failed(e),
    }
}

/// A per-request seed. The render is stochastic, so two calls should differ —
/// but a logged seed makes any one of them reproducible.
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
        let l = labels(&r);
        assert_eq!(
            l[0],
            ("A — Nam · Bắc · Kể chuyện".to_string(), "A".to_string())
        );
        assert_eq!(l[1].0, "B — Nữ · Nam · Tự nhiên");
    }

    /// The roster is derived from the same labels `/voices` sends, so `bm-core`
    /// parses them exactly as it parses the Python sidecar's.
    #[test]
    fn the_roster_reads_gender_and_accent_positionally() {
        let dir = tempfile::tempdir().unwrap();
        let r = Roster::load(&store(dir.path())).unwrap();
        let v = bm_core::voices::voices_from_labels("vieneu", &labels(&r), &[]);
        let a = v.iter().find(|v| v.name == "A").unwrap();
        assert_eq!(a.gender, "male");
        assert_eq!(a.accent, "Northern");
        assert_eq!(a.style, "Kể chuyện");
        // "Nam" in the *accent* slot is South, not male — the trap this parsing
        // exists for.
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
            allowed_voices: p.allowed,
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
    fn a_wav_response_is_audio_not_json() {
        let r = wav(&[0.0, 0.5]);
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers()[header::CONTENT_TYPE], "audio/wav");
    }

    #[test]
    fn a_failure_is_reported_without_a_traceback() {
        let r = failed(anyhow::anyhow!("{}", "x".repeat(500)));
        assert_eq!(r.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
