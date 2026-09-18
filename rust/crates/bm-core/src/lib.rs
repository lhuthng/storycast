//! Pipeline core for storycast.
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
pub mod audio_pool;
pub mod cast;
pub mod config;
pub mod crawl;
pub mod digest;
pub mod eta;
pub mod paths;
pub mod pool;
pub mod provision;
pub mod segments;
pub mod util;
pub mod voices;

pub use paths::Layout;
pub use util::{atomic_write, read_json, write_json};

/// The inductor's own node, by address. One predicate, one place: the
/// provisioner's `Ssh.local`, the reconcile seed and the offer's `local_node`
/// flag must never disagree about it.
pub fn is_local_node(addr: &str) -> bool {
    matches!(addr, "127.0.0.1" | "localhost" | "::1")
}

/// Serializes the tests that touch process-global `HOME`: `pool`'s tilde
/// test overrides it while `ssh`/`util` tilde tests read it, and libtest
/// runs them on parallel threads — without this the ssh argv test
/// intermittently expands against the pool test's temp dir.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use crate::is_local_node;

    #[test]
    fn local_node_covers_loopback_spellings() {
        for a in ["127.0.0.1", "localhost", "::1"] {
            assert!(is_local_node(a), "{a}");
        }
        for a in ["192.168.2.2", "", "127.0.0.2", "LOCALHOST"] {
            assert!(!is_local_node(a), "{a}");
        }
    }
}
