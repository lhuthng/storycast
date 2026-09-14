//! Pipeline core for beyond-myriads-converter.
//!
//! This is a 1:1 behavioural port of the original Python stages so that a
//! chapter rendered by a Rust worker is byte-comparable with one rendered by
//! the legacy pipeline:
//!
//! | module        | ported from      |
//! |---------------|------------------|
//! | [`crawl`]     | `ingest.py`      |
//! | [`digest`]    | `analyze.py`     |
//! | [`cast`]      | `synthesize.py`  |
//! | [`assemble`]  | `synthesize.py`  |
//! | [`ambience`]  | `ambience.py`    |
//!
//! [`eta`], [`provision`] and [`config`] are new: they exist because the
//! pipeline now runs across a cluster instead of on one box.

pub mod ambience;
pub mod assemble;
pub mod cast;
pub mod config;
pub mod crawl;
pub mod digest;
pub mod eta;
pub mod paths;
pub mod provision;
pub mod util;
pub mod voices;

pub use paths::Layout;
pub use util::{atomic_write, read_json, write_json};
