//! Asset composition: a genre's art, built from the assets it depends on.

use crate::profile;
pub use graph::DepRecord;
pub use graph::{read_marker, read_pack, set_release_versions};
pub(crate) use patch::tree_hash;
pub use patch::Report;
pub use resolve::resolve;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The authored dependency list, at `assets/pack.json`.
pub const PACK_FILE: &str = "pack.json";

/// Where the dependencies are unpacked: `assets/_extends/<name>/`.
pub const EXTENDS_DIR: &str = "_extends";

/// The resolution record, at `assets/_extends.json`. Generated, never authored.
pub const MARKER_FILE: &str = "_extends.json";

pub fn pack_path(assets: &Path) -> PathBuf {
    assets.join(PACK_FILE)
}

pub fn extends_dir(assets: &Path) -> PathBuf {
    assets.join(EXTENDS_DIR)
}

pub fn marker_path(assets: &Path) -> PathBuf {
    assets.join(MARKER_FILE)
}

mod graph;
mod layer;
mod patch;
mod resolve;

#[cfg(test)]
mod tests;
