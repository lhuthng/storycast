//! `bm-tts` — the VieNeu-TTS v3 Turbo inference path, without Python.
//!
//! Layout:
//!
//! * [`engine`] — the generator: prompt build, prefill, the per-frame loop.
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
pub mod sample;
pub mod server;
pub mod simd;
pub mod stats;
pub mod synth;
pub mod text;
pub mod voice;

pub use sea_g2p_rs;
