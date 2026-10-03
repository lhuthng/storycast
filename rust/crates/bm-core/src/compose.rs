//! Asset composition: a genre's art, built from the assets it depends on.
//!
//! A pack used to be one flat tree, so a sound every genre wants — a door slam,
//! wind, a body hitting the floor — had to exist once per genre, and a
//! correction had to be made once per copy. An asset now names its
//! dependencies in `assets/pack.json`:
//!
//! ```json
//! { "deps": ["common", "xianxia-base"] }
//! ```
//!
//! and this module folds them into the live `assets/` tree, **weakest first**,
//! so `xianxia-base` overrides `common` on a shared key and the asset's own
//! entries override both. A single-parent chain is the one-element case; the
//! list is ordered rather than a set so the precedence is written down rather
//! than inferred.
//!
//! **The dependencies are unpacked, not referenced.** `assets/_extends/<name>/`
//! holds each one's tree — an unpacked release, put there by whoever cut it —
//! so the whole composition lives inside `assets/`, which is the tree the
//! binding hashes and the tree provisioning ships. Nothing resolves through a
//! reference at run time: a reader sees one tree, exactly as it always did.
//!
//! **Composition is fill-in, and whole entry by key.** A sound is a key in a
//! pool registry, so a dependency's `wind` is inherited only if the asset ships
//! no `wind` — tags, files, `mode`, `hold` and `level` together. Fields are not
//! merged: `serde`'s defaults cannot tell "unset" from "set to the default", and
//! a half-inherited entry is a clip whose `mode` came from a sound it is not.
//! Everything that is not a pool registry (a clip, `scene-map.json`,
//! `tag-aliases.json`) is inherited per file, on the same missing-wins rule.
//!
//! **And the resolution remembers what it did.** Fill-in alone is not enough to
//! be regenerable: without a record, a second resolve cannot tell an entry *it*
//! inserted from an entry the asset has always had, so it could never withdraw
//! one a dependency has since dropped. [`Inherited`] (`assets/_extends.json`) is
//! that record — per registry, the keys inserted and a hash of the value
//! inserted; per file, the paths copied in and their content hashes. On the next
//! resolve an inherited entry whose hash *still matches* is withdrawn and
//! re-filled from the dependency, so a parent's change propagates; one whose
//! hash **differs** has been edited by the operator, who has adopted it — it
//! stays, and it stops being tracked. Nothing is ever silently thrown away,
//! which is what makes the merge both idempotent (no change in, no change out)
//! and safe to run at any time.
//!
//! Nothing here runs by itself. `asset resolve` is the verb, and a checkout with
//! no `pack.json` has no dependencies, so a resolve leaves it byte for byte as
//! it was.

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
///
/// Inside `assets/` on purpose. A composition that lived beside the pack would
/// be two trees to hash and two to ship, and the pack hash would then be a
/// claim about only half of what a reader resolves.
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
