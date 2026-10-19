//! Voice packs a workspace installs.
//!
//! `voices/packs.json` names them. Each pack keeps its preset beside a clone per
//! slot, and an id is `<pack>/<unnamed-N>` — the string the TTS store, the pool
//! and the cast all carry, so nothing downstream may rename it. The pack itself
//! is read from the bucket (`preset/voices/<name>/latest.json`), verified by its
//! archive sha256 and its tree manifest hash, and never from a working copy.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::preset::{PresetVoice, VoicePreset};
use crate::Layout;

/// The workspace file naming the packs it holds.
pub const PACKS_FILE: &str = "packs.json";

/// The workspace directory the packs live under.
pub const DIR: &str = "voices";

/// A pack read from the bucket, and the only source a workspace trusts.
pub const PUBLISHED: &str = "published";

/// A pack read from this checkout, for a pack nobody has published yet.
pub const CHECKOUT: &str = "checkout";

/// The take a publish ships, and the one a render speaks.
const TAKE: &str = "vieneu-clean.wav";

/// One pack a workspace holds, with what it was verified against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackRef {
    pub name: String,
    /// The version the copy came from; empty for a checkout install.
    #[serde(default)]
    pub version: String,
    /// The archive's sha256, so a later fetch of this version is still checked.
    #[serde(default)]
    pub sha256: String,
    /// The tree hash the unpacked copy was verified against.
    #[serde(default)]
    pub manifest_hash: String,
    /// [`PUBLISHED`] or [`CHECKOUT`].
    #[serde(default)]
    pub source: String,
}

/// `voices/packs.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Packs {
    #[serde(default = "version")]
    pub version: u32,
    #[serde(default)]
    pub packs: Vec<PackRef>,
}

fn version() -> u32 {
    super::preset::VERSION
}

impl Default for Packs {
    fn default() -> Self {
        Self {
            version: super::preset::VERSION,
            packs: Vec::new(),
        }
    }
}

impl Packs {
    /// The packs this workspace holds. A missing or broken file is no packs.
    pub fn load(layout: &Layout) -> Self {
        std::fs::read_to_string(layout.voice_packs())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    /// Write the manifest, keeping a stable order on disk.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        crate::util::atomic_write(path, &serde_json::to_string_pretty(self)?)
            .with_context(|| format!("writing {}", path.display()))
    }
}

/// One slot of an installed pack, resolved as far as the copy allows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Voice {
    /// `<pack>/<unnamed-N>`, the identity everything else uses.
    pub id: String,
    pub pack: String,
    pub slot: String,
    pub tags: Vec<String>,
    /// Workspace-relative take; empty when the slot has no clone behind it.
    pub file: String,
    /// What the clone calls itself, for display only.
    pub label: String,
}

/// Every slot the workspace's packs declare, in pack then preset order.
pub fn voices(layout: &Layout) -> Vec<Voice> {
    let mut out = Vec::new();
    for pack in Packs::load(layout).packs {
        let dir = layout.voices_dir().join(&pack.name);
        let Ok(preset) = VoicePreset::load(&dir.join("preset.json")) else {
            continue;
        };
        for slot in preset.voices {
            let clone = dir.join("clones").join(&slot.voice);
            let take = clone.join(TAKE);
            out.push(Voice {
                id: format!("{}/{}", pack.name, slot.voice),
                pack: pack.name.clone(),
                slot: slot.voice.clone(),
                tags: slot.tags,
                file: if take.is_file() {
                    relative(&layout.work, &take)
                } else {
                    String::new()
                },
                label: clone_label(&clone).unwrap_or_default(),
            });
        }
    }
    out
}

/// Where a pack was read from, and what `packs.json` records about it.
#[derive(Debug, Clone, Default)]
pub struct Origin {
    pub source: String,
    pub version: String,
    pub sha256: String,
    pub manifest_hash: String,
}

impl Origin {
    pub fn checkout() -> Self {
        Self {
            source: CHECKOUT.to_string(),
            ..Default::default()
        }
    }

    pub fn published(version: &str, sha256: &str, manifest_hash: &str) -> Self {
        Self {
            source: PUBLISHED.to_string(),
            version: version.to_string(),
            sha256: sha256.to_string(),
            manifest_hash: manifest_hash.to_string(),
        }
    }
}

/// What an install should read.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Take whatever `latest.json` points at, instead of the pinned version.
    pub latest: bool,
    /// Fetch this version instead of the pinned one.
    pub version: Option<String>,
    /// Read the pack from this tree instead of the bucket.
    pub tree: Option<PathBuf>,
}

/// What one install did, for the caller to print.
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub pack: String,
    pub source: String,
    pub version: String,
    /// `<slot>` -> the clone that filled it.
    pub pairs: Vec<(String, String)>,
    /// Slots no clone answered.
    pub unfilled: Vec<String>,
    /// Cast values rewritten to pack ids.
    pub retargeted: usize,
}

/// Install `name` into the workspace, from the bucket.
///
/// The pinned version is re-fetched and checked against the hashes this
/// workspace recorded; `--latest` (or a version it has no pin for) reads
/// `latest.json` instead.
pub fn install(layout: &Layout, name: &str, opts: &Options) -> Result<Report> {
    super::preset::ensure_slug(name)?;
    if let Some(tree) = &opts.tree {
        return install_tree(layout, name, tree, &Origin::checkout());
    }
    let pinned = Packs::load(layout).packs.into_iter().find(|p| p.name == name);
    // A pin is only reusable while the caller is not asking for another version.
    let mut pin = pinned
        .filter(|p| !p.version.is_empty() && !p.sha256.is_empty() && !opts.latest)
        .filter(|p| opts.version.as_deref().is_none_or(|v| v == p.version));

    let base = crate::config::Settings::load(&layout.settings()).packs_release;
    anyhow::ensure!(
        crate::artifact::is_preset_url(&base),
        "no `packs_release` URL in settings.json, so voices/{name} cannot be fetched"
    );
    let file = format!("{name}.tar.zst");
    if pin.is_none() {
        match crate::artifact::preset_latest_opt(&base, "voices", name, &file)? {
            Some(f) => {
                if let Some(want) = &opts.version {
                    anyhow::ensure!(
                        &f.version == want,
                        "voices/{name}@{want} is not what latest.json pins ({}); \
                         install it while it is latest, or drop --version",
                        f.version
                    );
                }
                pin = Some(PackRef {
                    name: name.to_string(),
                    version: f.version,
                    sha256: f.hash,
                    manifest_hash: f.manifest_hash,
                    source: PUBLISHED.to_string(),
                });
            }
            None => anyhow::bail!("voices/{name} is not published at {base}"),
        }
    }
    let pin = pin.expect("resolved above");
    let url = format!(
        "{}/preset/voices/{name}/{}/{file}",
        base.trim_end_matches('/'),
        pin.version
    );
    let scratch = layout
        .bm_state()
        .join("tmp")
        .join(format!("voices-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch)
        .with_context(|| format!("creating {}", scratch.display()))?;
    let archive = scratch.join(&file);
    let tree = scratch.join("tree");
    let report = (|| -> Result<Report> {
        crate::artifact::download(&url, &archive, &mut |_, _| {})
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let got = crate::artifact::sha256_of(&archive)?;
        anyhow::ensure!(
            got == pin.sha256,
            "{url}: sha256 {got} does not match the pinned {}",
            pin.sha256
        );
        crate::artifact::unpack_verified(&archive, &tree, &pin.manifest_hash)
            .map_err(|e| anyhow::anyhow!("{url}: {e}"))?;
        install_tree(
            layout,
            name,
            &tree,
            &Origin::published(&pin.version, &pin.sha256, &pin.manifest_hash),
        )
    })();
    let _ = std::fs::remove_dir_all(&scratch);
    report
}

/// Install from a pack tree already on disk: `voices/<name>/preset.json` beside
/// `voices/clones/`, or the combined `voices/presets/<name>.json`.
pub fn install_tree(layout: &Layout, name: &str, tree: &Path, origin: &Origin) -> Result<Report> {
    super::preset::ensure_slug(name)?;
    let pack = read_pack(tree, name)?;
    let pair = pair_slots(&pack.slots.voices, &pack.clones);

    let dest = layout.voices_dir().join(name);
    // A reinstall replaces the pack whole: a slot it no longer declares is gone.
    if dest.is_dir() {
        std::fs::remove_dir_all(&dest).with_context(|| format!("clearing {}", dest.display()))?;
    }
    let mut report = Report {
        pack: name.to_string(),
        source: origin.source.clone(),
        version: origin.version.clone(),
        ..Default::default()
    };
    let mut slots: Vec<PresetVoice> = Vec::new();
    for (i, slot) in pack.slots.voices.iter().enumerate() {
        // The workspace's own id: a position, never a clone name, so a
        // republished pack cannot rename a voice a cast already speaks.
        let id = format!("unnamed-{}", i + 1);
        slots.push(PresetVoice {
            voice: id.clone(),
            tags: slot.tags.clone(),
        });
        let Some(filled_by) = pair.get(i).and_then(|n| n.as_ref()) else {
            report.unfilled.push(id);
            continue;
        };
        let Some(clone) = pack.clones.get(filled_by) else {
            report.unfilled.push(id);
            continue;
        };
        let to = dest.join("clones").join(&id);
        std::fs::create_dir_all(&to)?;
        for file in ["voice.json", TAKE] {
            let src = clone.join(file);
            if src.is_file() {
                std::fs::copy(&src, to.join(file))
                    .with_context(|| format!("copying {}", src.display()))?;
            }
        }
        report.pairs.push((id, filled_by.clone()));
    }
    let mut preset = VoicePreset {
        version: pack.slots.version,
        label: pack.slots.label.clone(),
        category: pack.slots.category.clone(),
        voices: slots,
    };
    preset.normalize();
    preset.save(&dest.join("preset.json"))?;

    let mut packs = Packs::load(layout);
    packs.packs.retain(|p| p.name != name);
    packs.packs.push(PackRef {
        name: name.to_string(),
        version: origin.version.clone(),
        sha256: origin.sha256.clone(),
        manifest_hash: origin.manifest_hash.clone(),
        source: origin.source.clone(),
    });
    packs.packs.sort_by(|a, b| a.name.cmp(&b.name));
    packs.save(&layout.voice_packs())?;

    write_registries(layout)?;
    let aliases = aliases(name, &report.pairs, &pack.clones);
    report.retargeted = retarget_cast(layout, &aliases);
    Ok(report)
}

/// A pack's own content: its preset, and the clones its slots may name.
struct Pack {
    slots: VoicePreset,
    /// Clone name and slug -> its directory.
    clones: BTreeMap<String, PathBuf>,
}

/// Read a pack the way a publish writes it: its own tree first, the combined
/// one second. Its takes are its own; a pack that ships none installs unfilled.
fn read_pack(tree: &Path, name: &str) -> Result<Pack> {
    let candidates = [
        tree.join("voices").join(name).join("preset.json"),
        tree.join("voices/presets").join(format!("{name}.json")),
    ];
    let path = candidates
        .iter()
        .find(|p| p.is_file())
        .with_context(|| format!("no {name} preset under {}", tree.display()))?;
    Ok(Pack {
        slots: VoicePreset::load(path)?,
        clones: clone_dirs(&tree.join("voices/clones")),
    })
}

/// Slot index -> the clone that fills it: the clone the slot names, when the
/// pack carries one.
fn pair_slots(slots: &[PresetVoice], clones: &BTreeMap<String, PathBuf>) -> Vec<Option<String>> {
    slots
        .iter()
        .map(|slot| {
            clones
                .contains_key(&slot.voice)
                .then(|| slot.voice.clone())
        })
        .collect()
}

/// Clone name -> its directory, with the slug as a second key. Only a clone
/// with a take counts: a half-built one has nothing to ship.
fn clone_dirs(root: &Path) -> BTreeMap<String, PathBuf> {
    let mut out = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.join(TAKE).is_file() {
            continue;
        }
        if let Some(slug) = dir.file_name().and_then(|s| s.to_str()) {
            out.insert(slug.to_string(), dir.clone());
        }
        if let Some(name) = clone_label(&dir) {
            out.insert(name, dir);
        }
    }
    out
}

/// The display name a clone's `voice.json` carries.
fn clone_label(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("voice.json")).ok()?;
    let doc: serde_json::Value = serde_json::from_str(&text).ok()?;
    let name = doc.get("name")?.as_str()?.trim().to_string();
    (!name.is_empty()).then_some(name)
}

fn relative(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// `voices.json` and `voice-pool.json`, rebuilt from every pack the workspace
/// holds. Entries outside any pack are left alone: that is the manual add.
fn write_registries(layout: &Layout) -> Result<()> {
    let packs = Packs::load(layout);
    let all = voices(layout);
    let ours = |key: &str| {
        packs
            .packs
            .iter()
            .any(|p| key.starts_with(&format!("{}/", p.name)))
    };

    let mut manifest = read_object(&layout.voices_manifest());
    manifest.retain(|k, _| k.starts_with('_') || !ours(k));
    for voice in all.iter().filter(|v| !v.file.is_empty()) {
        manifest.insert(
            voice.id.clone(),
            serde_json::Value::String(voice.file.clone()),
        );
    }
    crate::util::atomic_write(
        &layout.voices_manifest(),
        &serde_json::to_string_pretty(&manifest)?,
    )?;

    let mut pool = read_object(&layout.voice_pool());
    pool.retain(|k, _| k.starts_with('_') || !ours(k));
    for voice in all.iter().filter(|v| !v.file.is_empty()) {
        pool.insert(
            voice.id.clone(),
            serde_json::to_value(crate::pool::PoolEntry {
                file: voice.file.clone(),
                tags: voice.tags.clone(),
                label: voice.label.clone(),
            })?,
        );
    }
    crate::util::atomic_write(&layout.voice_pool(), &serde_json::to_string_pretty(&pool)?)?;
    Ok(())
}

/// A registry as a JSON object, creating the "nothing here yet" note when the
/// file does not exist.
fn read_object(path: &Path) -> serde_json::Map<String, serde_json::Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_else(|| {
            let mut out = serde_json::Map::new();
            out.insert(
                "_note".into(),
                serde_json::Value::String(
                    "Voice registry: `<pack>/<slot>` -> clip. Rebuilt from voices/packs.json; \
                     entries outside a pack are manual adds."
                        .into(),
                ),
            );
            out
        })
}

/// Old name -> pack id, from the clone that filled each slot. An alias two
/// packs both claim is dropped rather than guessed.
fn aliases(
    pack: &str,
    pairs: &[(String, String)],
    clones: &BTreeMap<String, PathBuf>,
) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    let claim = |alias: &str, id: &str, out: &mut BTreeMap<String, String>| {
        if alias.is_empty() {
            return;
        }
        match out.get(alias) {
            Some(existing) if existing != id => {
                out.remove(alias);
            }
            _ => {
                out.insert(alias.to_string(), id.to_string());
            }
        }
    };
    for (slot, name) in pairs {
        let id = format!("{pack}/{slot}");
        claim(slot, &id, &mut out);
        claim(name, &id, &mut out);
        if let Some(slug) = clones.get(name).and_then(|d| d.file_name()?.to_str()) {
            claim(slug, &id, &mut out);
        }
    }
    out
}

/// Rewrite every cast file's values that name a voice by its old name.
fn retarget_cast(layout: &Layout, aliases: &BTreeMap<String, String>) -> usize {
    let Ok(entries) = std::fs::read_dir(layout.data()) else {
        return 0;
    };
    let mut changed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
        if !stem.starts_with("cast-") || path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(mut cast) = serde_json::from_str::<BTreeMap<String, String>>(&text) else {
            continue;
        };
        let mut touched = false;
        for voice in cast.values_mut() {
            if let Some(id) = aliases.get(voice) {
                if id != voice {
                    *voice = id.clone();
                    touched = true;
                }
            }
        }
        if touched {
            let body = serde_json::to_string_pretty(&cast).unwrap_or_default();
            if crate::util::atomic_write(&path, &body).is_ok() {
                changed += 1;
            }
        }
    }
    changed
}

#[cfg(test)]
mod tests;
