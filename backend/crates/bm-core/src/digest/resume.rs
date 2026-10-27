//! What a failed digest hands the next attempt: the proofread it already
//! accepted, so a later step's failure resumes there instead of reading the
//! whole chapter again. A new digest, one with no failure behind it, starts
//! clean and proofreads as always.

use super::quotes::{proofread, text_hash};
use super::*;

#[derive(Serialize, Deserialize)]
struct Resume {
    source: String,
    step: String,
}

fn path(layout: &Layout, n: u32) -> PathBuf {
    layout.data().join(format!(".digest-resume-ch{n}.json"))
}

/// The kept proofread for `n` and the step the last run stopped at, when that
/// run got past its proofread and the sidecar still answers for this source.
pub(crate) fn kept(layout: &Layout, n: u32, original: &str) -> Option<(String, String)> {
    let raw = std::fs::read_to_string(path(layout, n)).ok()?;
    let marker: Resume = serde_json::from_str(&raw).ok()?;
    if marker.source != text_hash(original) {
        clear(layout, n);
        return None;
    }
    match proofread(layout, n, original) {
        Some(text) => Some((text, marker.step)),
        None => {
            clear(layout, n);
            None
        }
    }
}

/// The proofread is accepted: a failure from here resumes at the step it
/// happened in, and never asks for the repair again.
pub(crate) fn keep(layout: &Layout, n: u32, original: &str) {
    write(layout, n, original, "proofread");
}

/// Name the step a run stopped at, so the retry says where it resumes.
pub(crate) fn stop(layout: &Layout, n: u32, original: &str, step: &str) {
    write(layout, n, original, step);
}

pub(crate) fn clear(layout: &Layout, n: u32) {
    let _ = std::fs::remove_file(path(layout, n));
}

fn write(layout: &Layout, n: u32, original: &str, step: &str) {
    let marker = Resume {
        source: text_hash(original),
        step: step.to_string(),
    };
    let _ = atomic_write(
        &path(layout, n),
        &serde_json::to_string(&marker).unwrap_or_default(),
    );
}

#[cfg(test)]
mod tests;
