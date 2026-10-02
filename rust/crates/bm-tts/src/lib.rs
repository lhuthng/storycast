//! `bm-tts` — the sidecar's inference paths, without Python.
//!
//! Two engines behind one HTTP surface (see [`server`]):
//!
//! * [`engine`] — VieNeu-TTS v3 Turbo: prompt build, prefill, the per-frame
//!   loop.
//! * [`pocket`] — Kyutai's Pocket TTS, ported from the community ONNX runtime
//!   whose bundle the engine ships (`engines/pocket/`).
//!
//! Plus the parts both share:
//!
//! * [`sample`] — top-k / top-p / repetition-penalty sampling and its history.
//! * [`framecap`] — how long a chunk may run before the generator is suspect.
//! * [`npz`] — reading the model's embedding tables out of a `.npz`.
//!
//! The text front end is the vendored `sea_g2p_rs` (see
//! `rust/vendor/sea-g2p/VENDORED.md`); it is re-exported here so callers do not
//! have to know it is a path dependency.

pub mod babble;
pub mod codec;
pub mod engine;
pub mod f32le;
pub mod framecap;
pub mod npz;
// Gated on the `pocket` feature: the module is the only thing in this crate
// that needs Candle, and a VieNeu-only build should not compile it at all.
// See the feature's comment in Cargo.toml.
#[cfg(feature = "pocket")]
pub mod pocket;
pub mod sample;
pub mod server;
pub mod simd;
pub mod stats;
pub mod synth;
pub mod text;
pub mod voice;

pub use sea_g2p_rs;
