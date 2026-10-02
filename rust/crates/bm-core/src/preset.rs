//! Profile presets: a name for a pack × adapter × engine triple.
//!
//! Creating a workspace used to mean inheriting whatever the checkout had
//! loaded — `.bm/profile` stamped the new book's settings, and a second book
//! wanting another pack, language or engine had to re-load the checkout first,
//! create, then load back. A preset is the data that removes the dance:
//!
//! ```json
//! "jnovel-en": { "pack": "", "pack_deps": ["common", "craft",
//!               "court-mystery"], "adapter": "jnovel-en-US", "engine": "pocket" }
//! ```
//!
//! `bm-inductor workspace new <book> --profile jnovel-en` reads this file and
//! stamps the workspace's own binding from it — the checkout's pointer is
//! never touched, so a book being worked on this minute cannot notice.
//!
//! The pieces, and what each answers for:
//!
//! * **`pack`** — the name the binding's pack piece carries. With
//!   **`pack_deps`** the workspace composes its OWN pack from those roots
//!   ([`compose_workspace_pack`]: a `pack.json` naming them, the dependency
//!   trees linked in from the checkout's `assets/_extends/`, resolved in the
//!   workspace) — the workspace-pack shape [ROADMAP.md](../../../docs/ROADMAP.md)
//!   §3 asks for, where two books on one checkout do not share a score. With
//!   no `pack_deps` the workspace shares the checkout's live pack, which is
//!   the shape every existing workspace has.
//! * **`adapter`** — which `adapters/<id>/` home the prompts come from. The
//!   binding stamps the adapter's *home* hash, the same claim
//!   `verify_binding` checks; the workspace's caches key on the name.
//! * **`engine`** — the engine's name. Like every binding, the engine piece is
//!   a declaration and never a digest: `settings.engine` is the whole fact.
//! * **`crawler`** — a `{ type, file }` selection, wired into settings, so a
//!   book that needs no thought about sites (an EPUB, say) still says where its
//!   chapters come from. `known` picks a site from the global registry, and
//!   `example` picks the global EPUB crawler; both are **referenced in place**
//!   (`crawlers/…`), not copied, so an edit reaches every book that selected
//!   them. `custom` means the book drops its own script into `crawl/`, which
//!   [`crate::crawl::provider::resolve_script`] searches first and a profile
//!   release cannot reach.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

/// The preset file, under `profiles/`.
pub const PRESETS_FILE: &str = "presets.json";

/// One named triple. Every field is a claim about what a workspace created
/// from it will bind; the CLI is the only writer of that binding, so the
/// struct is read-only data.
#[derive(Debug, Clone, Deserialize)]
pub struct Preset {
    /// What a human calls it — printed by the picker, never parsed.
    pub label: String,
    /// The binding's pack name. Empty with `pack_deps` set means "name the
    /// composition after the workspace", which is what a book's own pack is.
    #[serde(default)]
    pub pack: String,
    /// The roots the workspace's OWN pack is composed from, weakest first —
    /// `common` must be in the chain or the pack has no world (see
    /// ASSET-PACKS.md). Empty means the checkout's live pack is shared.
    #[serde(default)]
    pub pack_deps: Vec<String>,
    /// Which adapter home the prompts come from.
    pub adapter: String,
    /// The engine `settings.engine` names.
    pub engine: String,
    /// The crawler a workspace created from this preset starts with, as a
    /// `{ type, file }` pair. Absent means no crawler.
    #[serde(default)]
    pub crawler: PresetCrawler,
}

/// A preset's crawler, as a **type and a file** rather than a path to copy.
///
/// The known-site crawlers are global now (`crawlers/`), so a preset selects one
/// rather than carrying a copy: `{ "type": "known", "file": "storya.click" }`
/// names the registry entry, `{ "type": "example", "file": "epub.lua" }` names
/// the unknown-structure crawler, and `{ "type": "custom" }` leaves the book to
/// drop its own script into `crawl/`. `none` (or an absent block) is no crawler.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct PresetCrawler {
    /// `known` | `example` | `custom` | `none`.
    #[serde(rename = "type")]
    pub kind: String,
    /// `known`: the site host (`storya.click`). `example`: the file under
    /// `crawlers/examples/` (`epub.lua`). `custom`: the file the book owns under
    /// its own `crawl/` (empty to leave the naming to the operator).
    pub file: String,
}

impl PresetCrawler {
    /// Whether this preset names no crawler at all.
    pub fn is_none(&self) -> bool {
        self.kind.is_empty() || self.kind == "none"
    }
}

/// A crawler to install in a new workspace: the script to copy in, the URL
/// template it runs against, and the fetch budget the site needs.
///
/// This is the **guided create** flow's answer, and it is the reason a preset
/// alone cannot carry it: a preset names one script path, while the picker lets
/// the operator choose a known site (whose template, params and budget come from
/// the registry) or a local file, and none of those is a property of the pack ×
/// adapter × engine triple. Deliberately not `serde`: it never travels further
/// than the in-process job that builds it and the function that applies it.
#[derive(Debug, Clone, Default)]
pub struct CrawlerSetup {
    /// The value to write into `settings.crawl.script`. A global crawler is
    /// referenced in place (`crawlers/known/storya.lua`); a custom one names the
    /// workspace's own copy (`crawl/mysite.lua`) and comes with `source` set.
    /// Empty means "leave the settings' script alone".
    pub script: String,
    /// Absolute path of a script to copy into the workspace's own `crawl/`, for a
    /// crawler the book owns. Empty for a global crawler that is only referenced.
    pub source: std::path::PathBuf,
    /// The chapter URL template, or empty to leave the settings' alone.
    pub url_template: String,
    /// `crawl.params` the site needs (`epub` path, `book` URL, …).
    pub params: serde_json::Map<String, serde_json::Value>,
    /// `crawl.max_fetches`, or `0` for the built-in default.
    pub max_fetches: u32,
    /// `crawl.max_seconds`, or `0` for the built-in default.
    pub max_seconds: u64,
    /// Absolute path of a local **EPUB** to copy into the new workspace at
    /// `tmp/book.epub`. Set only by the guided create flow's "Local file
    /// (EPUB)" choice, whose script reads `crawl.params.epub`; empty for every
    /// site crawler. The book is copied, never referenced, because the crawl's
    /// read root is the workspace — a path outside it is refused.
    pub book: std::path::PathBuf,
    /// Absolute path of a local **directory of volumes**, when the operator
    /// handed the guided flow a folder instead of one file. Every `.epub`
    /// inside is copied into the workspace's own `books/`, which is what
    /// `crawl.params.books` names. Mutually exclusive with [`Self::book`]: one
    /// is a book, the other is a shelf.
    pub books: std::path::PathBuf,
}

/// `profiles/presets.json`.
pub fn presets_path(root: &Path) -> std::path::PathBuf {
    root.join("profiles").join(PRESETS_FILE)
}

/// Every preset the checkout ships, keyed by name.
pub fn read_presets(root: &Path) -> Result<BTreeMap<String, Preset>> {
    let path = presets_path(root);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let presets = read_presets_str(&text)
        .with_context(|| format!("parsing {}", path.display()))?;
    Ok(presets)
}

/// [`read_presets`], from a string — the seam the tests parse through.
fn read_presets_str(text: &str) -> Result<BTreeMap<String, Preset>> {
    #[derive(Deserialize)]
    struct File {
        #[serde(default, rename = "_note")]
        _note: String,
        #[serde(flatten)]
        presets: BTreeMap<String, Preset>,
    }
    let file: File = serde_json::from_str(text)?;
    Ok(file.presets)
}

/// Compose the workspace's own pack: `pack.json` naming `deps`, the dependency
/// trees linked in from the checkout's `assets/_extends/`, and one resolve.
///
/// The link is a symlink on purpose. A composition input is never shipped and
/// never hashed (the manifest skips `_extends/` by name), so copying tens of
/// megabytes of dependency clips into every book would buy nothing but disk —
/// the *resolved result* is what the workspace owns, and that is written as
/// real files by the resolve. A workspace that needs a real tree (to rsync
/// whole, say) replaces the link with a copy and nothing else changes.
///
/// An existing `pack.json` is left alone: creating a workspace twice is
/// refused before this runs, so the file can only be one this call wrote —
/// or one an operator replaced, which is theirs to keep.
pub fn compose_workspace_pack(
    work: &Path,
    root_extends: &Path,
    deps: &[String],
) -> Result<()> {
    let assets = work.join("assets");
    std::fs::create_dir_all(&assets)?;
    let pack_path = assets.join(crate::compose::PACK_FILE);
    if !pack_path.is_file() {
        let pack = serde_json::json!({
            "_note": "This workspace's own pack, composed at creation from its profile preset's pack_deps — the workspace-pack shape ROADMAP §3 asks for. Edit the registries here as the book's own taste; the deps are what it was built on.",
            "deps": deps,
        });
        crate::atomic_write(&pack_path, &serde_json::to_string_pretty(&pack)?)?;
    }
    let extends = assets.join(crate::compose::EXTENDS_DIR);
    if !extends.exists() {
        std::os::unix::fs::symlink(root_extends, &extends).with_context(|| {
            format!(
                "linking {} -> {}",
                extends.display(),
                root_extends.display()
            )
        })?;
    }
    for dep in deps {
        let dep_dir = root_extends.join(dep);
        anyhow::ensure!(
            dep_dir.is_dir(),
            "'{dep}' is not unpacked: {} is missing — unpack it under the checkout's assets/_extends/ first",
            dep_dir.display()
        );
    }
    crate::compose::resolve(&assets, false)
        .context("resolving the workspace's own pack composition")?;
    Ok(())
}

/// The workspace pack's content hash: the manifest hash over the resolved
/// `assets/` tree, exactly the number [`crate::profile::verify_binding`]
/// computes for a checkout's pack — relative to the *workspace* here, because
/// the tree is the workspace's.
pub fn workspace_pack_hash(work: &Path) -> Result<String> {
    let files = crate::profile::files_under(work, &["assets"]);
    anyhow::ensure!(
        !files.is_empty(),
        "the workspace pack is empty: {} holds nothing",
        work.join("assets").display()
    );
    Ok(crate::profile::manifest_hash(
        &crate::profile::hash_files(work, files)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("bm-preset-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_shipped_file_parses_and_names_both_shapes() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
        let presets = read_presets(&root).unwrap();
        let xianxia = presets.get("xianxia-vi").expect("the sharing preset");
        assert_eq!(xianxia.pack, "xianxia");
        assert!(
            xianxia.pack_deps.is_empty(),
            "the checkout's live pack is the pre-preset shape"
        );
        assert_eq!(xianxia.adapter, "vi-VN");
        assert_eq!(xianxia.engine, "vieneu");

        let jnovel = presets.get("jnovel-en").expect("the composing preset");
        assert_eq!(jnovel.pack_deps, vec!["common", "craft", "court-mystery"]);
        assert_eq!(jnovel.engine, "pocket");
        assert!(
            jnovel.pack.is_empty(),
            "the composition is named after the workspace — the preset names no genre pack"
        );
        assert_eq!(jnovel.crawler.kind, "custom", "the EPUB book picks its own");
        let xianxia_crawler = &xianxia.crawler;
        assert_eq!(xianxia_crawler.kind, "known");
        assert_eq!(xianxia_crawler.file, "storya.click");
    }

    #[test]
    fn a_garbled_preset_file_is_an_error_that_names_the_file() {
        let root = dir("garbled");
        std::fs::create_dir_all(root.join("profiles")).unwrap();
        std::fs::write(root.join("profiles/presets.json"), "{ not json").unwrap();
        let err = read_presets(&root).unwrap_err();
        assert!(err.to_string().contains("presets.json"), "{err:#}");
    }

    #[test]
    fn a_workspace_pack_composes_from_linked_deps_and_hashes_stably() {
        // A checkout with one dependency, and a workspace composing it.
        let root = dir("compose");
        let extends = root.join("assets/_extends/common");
        std::fs::create_dir_all(extends.join("effects")).unwrap();
        std::fs::write(extends.join("effect-pool.json"), r#"{ "wind": { "tags": ["wind"], "files": ["effects/wind-1.mp3"] } }"#).unwrap();
        std::fs::write(extends.join("effects/wind-1.mp3"), b"clip").unwrap();

        let work = root.join("workspaces/book");
        compose_workspace_pack(&work, &root.join("assets/_extends"), &["common".into()]).unwrap();

        // The resolve wrote the dependency's registry and clip into the
        // workspace as real files; the input is the link.
        let assets = work.join("assets");
        assert!(assets.join("effect-pool.json").is_file());
        assert!(assets.join("effects/wind-1.mp3").is_file());
        assert!(
            assets.join(crate::compose::PACK_FILE).is_file(),
            "the workspace's own pack.json names its deps"
        );
        assert!(assets.join(crate::compose::MARKER_FILE).is_file());

        // The hash is over the RESOLVED tree: the link itself must not be in
        // it (a copy in place of the link must hash the same), and it must be
        // stable across a re-resolve.
        let linked = workspace_pack_hash(&work).unwrap();
        std::fs::remove_dir_all(assets.join("effects")).unwrap();
        std::fs::remove_file(assets.join(crate::compose::EXTENDS_DIR)).unwrap();
        std::fs::create_dir_all(assets.join("effects")).unwrap();
        std::fs::write(assets.join("effects/wind-1.mp3"), b"clip").unwrap();
        let copied = workspace_pack_hash(&work).unwrap();
        assert_eq!(linked, copied, "a link and its copy are one tree");
    }

    #[test]
    fn a_workspace_pack_names_the_dep_that_is_not_unpacked() {
        let root = dir("no-common");
        // One real dependency, and one the checkout never unpacked.
        let extends = root.join("assets/_extends/weapons");
        std::fs::create_dir_all(extends.join("effects")).unwrap();
        std::fs::write(
            root.join("assets/_extends/weapons/effect-pool.json"),
            r#"{ "clash": { "tags": ["clash"], "files": ["effects/clash-1.mp3"] } }"#,
        ).unwrap();
        let work = root.join("workspaces/book");
        compose_workspace_pack(&work, &root.join("assets/_extends"), &["weapons".into()])
            .expect("an unpacked dep composes");

        // The world-vocabulary rule — a pack whose chain reaches no `common`
        // has no place words, no moods, no beds — is a property ASSET-PACKS.md
        // documents and the pack gate tests; what THIS layer refuses is a
        // dependency name nobody unpacked, and it names the path it looked at.
        let work2 = root.join("workspaces/book2");
        let err = compose_workspace_pack(&work2, &root.join("assets/_extends"), &["magic".into()])
            .unwrap_err();
        assert!(err.to_string().contains("not unpacked"), "{err:#}");
        assert!(err.to_string().contains("magic"), "{err:#}");
    }
}
