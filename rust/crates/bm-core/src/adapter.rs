//! The adapter manifest: the language, declared rather than deduced.
//!   differs *per engine* rather than per language. Empty means "any engine
//!   that can voice the language", which is every adapter shipped today.

use crate::paths::Layout;
use crate::voices;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The manifest's file name inside an adapter's home.
pub const MANIFEST: &str = "adapter.json";

/// What an adapter declares about itself.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Manifest {
    /// The genre these prompts are written for (`xianxia`). Empty: no claim.
    pub pack: String,
    /// The BCP-47 language the prompts write in, which is also the language the
    pub language: String,
    /// The engine this adapter is written for, when its wording depends on the
    pub engine: String,
}

/// The manifest beside an adapter's trees.
pub fn path(home: &Path) -> PathBuf {
    home.join(MANIFEST)
}

/// Read the manifest in one adapter's home.
pub fn read(home: &Path) -> Result<Option<Manifest>> {
    let file = path(home);
    if !file.is_file() {
        return Ok(None);
    }
    let text =
        std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
    let manifest: Manifest =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", file.display()))?;
    Ok(Some(manifest))
}

/// The manifest of the adapter in force for this layout, if it has one.
pub fn in_force(layout: &Layout) -> Result<Option<Manifest>> {
    match layout.adapter_home() {
        Some(home) => read(&home),
        None => Ok(None),
    }
}

/// The language this checkout's adapter writes.
pub fn language(layout: &Layout, pack: &str) -> Result<Option<String>> {
    if let Some(declared) = in_force(layout)?.map(|m| m.language) {
        let declared = declared.trim();
        if !declared.is_empty() {
            return Ok(Some(declared.to_string()));
        }
    }
    Ok(voices::adapter_language(pack, &layout.adapter).map(String::from))
}

/// What the adapter, the binding and the bound engine say about each other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// The language in force, or `None` when nothing claims one.
    pub language: Option<String>,
    /// One sentence per disagreement, in the order they are worth reading.
    pub problems: Vec<String>,
}

impl Verdict {
    pub fn agrees(&self) -> bool {
        self.problems.is_empty()
    }

    /// The problems as one line, for an event or a refusal.
    pub fn reason(&self) -> String {
        self.problems.join("; ")
    }
}

/// Read the adapter in force and say everything wrong with the triple.
pub fn inspect(layout: &Layout, pack: &str, engine: &str) -> Verdict {
    let manifest = match in_force(layout) {
        Ok(m) => m.unwrap_or_default(),
        Err(e) => {
            return Verdict {
                language: None,
                problems: vec![format!("{e:#}")],
            }
        }
    };
    let declared = |s: &str| {
        let s = s.trim();
        if s.is_empty() {
            None
        } else {
            Some(s.to_string())
        }
    };
    let mut problems = Vec::new();
    if let Some(want) = declared(&manifest.pack) {
        let have = pack.trim();
        if !have.is_empty() && !have.eq_ignore_ascii_case(&want) {
            problems.push(format!(
                "adapter '{}' declares pack '{want}' and the binding is '{have}'",
                layout.adapter
            ));
        }
    }
    if let Some(want) = declared(&manifest.engine) {
        let have = engine.trim();
        if !have.is_empty() && !have.eq_ignore_ascii_case(&want) {
            problems.push(format!(
                "adapter '{}' declares engine '{want}' and the binding is '{have}'",
                layout.adapter
            ));
        }
    }
    let language = declared(&manifest.language)
        .or_else(|| voices::adapter_language(pack, &layout.adapter).map(String::from));
    if let Some(language) = &language {
        if !voices::voices_language(engine, language) {
            problems.push(format!(
                "adapter '{}' writes {language} and engine '{engine}' cannot voice it",
                layout.adapter
            ));
        }
    }
    Verdict { language, problems }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::ADAPTERS_DIR;

    /// A layout whose adapter has a home, with `manifest` written into it.
    fn fixture(tag: &str, adapter: &str, manifest: Option<&str>) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("bm-adapter-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join(ADAPTERS_DIR).join(adapter);
        std::fs::create_dir_all(&home).unwrap();
        if let Some(text) = manifest {
            std::fs::write(path(&home), text).unwrap();
        }
        root
    }

    fn layout(root: &Path, adapter: &str) -> Layout {
        Layout {
            adapter: adapter.to_string(),
            ..Layout::new(root)
        }
    }

    #[test]
    fn a_declared_language_is_believed_over_the_id() {
        // The point of the file: the id is a convention, so a manifest that
        let root = fixture("lang", "xianxia-en-US", Some(r#"{"language":"vi-VN"}"#));
        let l = layout(&root, "xianxia-en-US");
        assert_eq!(
            language(&l, "xianxia").unwrap().as_deref(),
            Some("vi-VN"),
            "the declaration wins over the id's suffix"
        );
        // …and the id is what a pre-manifest adapter falls back to.
        let bare = fixture("id", "xianxia-en-US", None);
        let b = layout(&bare, "xianxia-en-US");
        assert_eq!(language(&b, "xianxia").unwrap().as_deref(), Some("en-US"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&bare);
    }

    #[test]
    fn the_pre_split_checkout_claims_no_language() {
        // `default` is every existing workspace's adapter. It makes no claim,
        let root = fixture("default", "default", None);
        let l = layout(&root, "default");
        let v = inspect(&l, "xianxia", "vieneu");
        assert_eq!(v.language, None);
        assert!(v.agrees(), "{:?}", v.problems);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_english_adapter_behind_vieneu_is_named_as_a_mismatch() {
        // The case the whole file exists for. VieNeu declares `vi-VN`, so an
        let root = fixture("mismatch", "xianxia-en-US", Some(r#"{"language":"en-US"}"#));
        let l = layout(&root, "xianxia-en-US");
        let v = inspect(&l, "xianxia", "vieneu");
        assert_eq!(v.language.as_deref(), Some("en-US"));
        assert!(!v.agrees());
        assert!(v.reason().contains("xianxia-en-US"), "{}", v.reason());
        assert!(v.reason().contains("en-US"), "{}", v.reason());
        assert!(v.reason().contains("vieneu"), "{}", v.reason());

        // The same adapter behind an engine that declares it: nothing to say.
        let ok = inspect(&l, "xianxia", "gemini");
        assert!(ok.agrees(), "{:?}", ok.problems);
        assert_eq!(ok.language.as_deref(), Some("en-US"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_pack_or_an_engine_pin_that_disagrees_is_a_problem() {
        let root = fixture(
            "pins",
            "xianxia-vi-VN",
            Some(r#"{"pack":"xianxia","engine":"gemini","language":"vi-VN"}"#),
        );
        let l = layout(&root, "xianxia-vi-VN");
        // Agreeing on all three: silent.
        assert!(inspect(&l, "xianxia", "gemini").agrees());
        // Another genre's prompts behind this binding.
        let pack = inspect(&l, "romance", "gemini");
        assert_eq!(pack.problems.len(), 1, "{:?}", pack.problems);
        assert!(pack.reason().contains("romance"), "{}", pack.reason());
        // Authored for one engine, running on another.
        let engine = inspect(&l, "xianxia", "vieneu");
        assert!(
            engine.reason().contains("engine 'gemini'"),
            "{}",
            engine.reason()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_manifest_that_cannot_be_read_is_a_problem_not_silence() {
        // An operator who wrote a declaration and cannot see it is being
        let root = fixture("broken", "xianxia-vi-VN", Some("{ not json"));
        let l = layout(&root, "xianxia-vi-VN");
        let v = inspect(&l, "xianxia", "vieneu");
        assert!(!v.agrees());
        assert!(v.reason().contains(MANIFEST), "{}", v.reason());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_empty_manifest_is_the_same_as_no_manifest() {
        let empty = fixture("empty", "xianxia-en-US", Some("{}"));
        let l = layout(&empty, "xianxia-en-US");
        // No claim, so the id convention answers — and it still refuses
        let v = inspect(&l, "xianxia", "vieneu");
        assert_eq!(v.language.as_deref(), Some("en-US"));
        assert!(!v.agrees());
        let _ = std::fs::remove_dir_all(&empty);
    }
}
