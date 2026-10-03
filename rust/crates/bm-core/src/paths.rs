//! Where everything lives. Every path the pipeline touches is derived here, so

use crate::util::squeeze_ws;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Layout {
    pub root: PathBuf,
    /// The active workspace: `workspaces/<name>/`, holding this book's
    pub work: PathBuf,
    /// The adapter (language) caches are keyed by, taken from the load pointer.
    pub adapter: String,
    /// The voice engine this checkout runs: the name of its `engines/<name>/`
    pub engine: String,
}

/// What a checkout with no adapter bundle keys its caches under.
pub const DEFAULT_ADAPTER: &str = "default";

/// What a checkout with no engine named runs: the local engine that was there
pub const DEFAULT_ENGINE: &str = "vieneu";

/// The directory every engine's own files hang off, at the root: `engines/`.
pub const ENGINES_DIR: &str = "engines";

/// The directory every language's own trees hang off, at the scope root:
pub const ADAPTERS_DIR: &str = "adapters";

/// The one engine that ever had a *flat* tree at the root.
pub const LEGACY_ENGINE: &str = "vieneu";

/// The one language that ever had a *flat* tree at the root.
pub const LEGACY_ADAPTER: &str = "vi-VN";

/// The engine's name as it appears in a cache path.
fn engine_key(engine: &str) -> &str {
    match engine {
        "" => "unknown",
        "gemini" => "gemini-v2",
        other => other,
    }
}

/// The pre-split spelling of `engine`'s cache, for the one-time rename.
fn legacy_engine_key(engine: &str) -> Option<&'static str> {
    match engine {
        "vieneu" => Some("vieneu"),
        "gemini" => Some("gemini-v2"),
        _ => None,
    }
}

/// The engines that have a pre-split spelling on disk to move.
pub const LEGACY_CACHE_ENGINES: [&str; 2] = ["vieneu", "gemini"];

impl Layout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Layout {
            work: root.clone(),
            root,
            adapter: DEFAULT_ADAPTER.to_string(),
            engine: DEFAULT_ENGINE.to_string(),
        }
    }

    /// The adapter the load pointer names, or [`DEFAULT_ADAPTER`].
    fn bound_adapter(root: &Path) -> String {
        crate::profile::read_binding(root)
            .map(|b| b.cache_adapter())
            .unwrap_or_else(|_| DEFAULT_ADAPTER.to_string())
    }

    /// The engine the load pointer names, or [`DEFAULT_ENGINE`].
    fn bound_engine(root: &Path) -> String {
        crate::profile::read_binding(root)
            .map(|b| b.cache_engine())
            .unwrap_or_else(|_| DEFAULT_ENGINE.to_string())
    }

    /// Resolve the active workspace: `.bm/active-workspace` names a directory
    pub fn resolve(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let adapter = Self::bound_adapter(&root);
        let engine = Self::bound_engine(&root);
        let pointer = Self::active_workspace_file(&root);
        if !pointer.is_file() {
            // No pointer, so this root **is** the workspace (or a bare
            let ws = crate::config::Settings::load(&root.join("settings.json"));
            let adapter = if ws.profile.adapter.name.is_empty() {
                adapter
            } else {
                ws.profile.adapter.name.clone()
            };
            let engine = if ws.profile.engine.name.is_empty() {
                engine
            } else {
                ws.profile.engine.name.clone()
            };
            return Ok(Layout {
                adapter,
                engine,
                work: root.clone(),
                root,
            });
        }
        let name = std::fs::read_to_string(&pointer)
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let work = root.join("workspaces").join(&name);
        if name.is_empty() || !work.is_dir() {
            anyhow::bail!(
                "workspace pointer {:?} names {:?}, which is not a directory — recreate it (`workspace new {}`) or point elsewhere (`workspace use <name>`)",
                pointer.display(),
                work.display(),
                name,
            );
        }
        // The active workspace's own binding is the authority for what it
        let ws = crate::config::Settings::load(&work.join("settings.json"));
        let adapter = if ws.profile.adapter.name.is_empty() {
            adapter
        } else {
            ws.profile.adapter.name.clone()
        };
        let engine = if ws.profile.engine.name.is_empty() {
            engine
        } else {
            ws.profile.engine.name.clone()
        };
        Ok(Layout {
            root,
            work,
            adapter,
            engine,
        })
    }

    /// This layout, re-bound to the adapter and engine a task arrives with.
    pub fn rebind(&self, adapter: &str, engine: &str) -> Self {
        let mut bound = self.clone();
        if !adapter.trim().is_empty() {
            bound.adapter = adapter.trim().to_string();
        }
        if !engine.trim().is_empty() {
            bound.engine = engine.trim().to_string();
        }
        bound
    }

    /// Every adapter home this checkout carries: `(name, scope)`, the tree
    pub fn adapter_homes(&self) -> Vec<(String, PathBuf)> {
        let mut out: Vec<(String, PathBuf)> = Vec::new();
        let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for scope in [&self.work, &self.root] {
            let Ok(entries) = std::fs::read_dir(scope.join(ADAPTERS_DIR)) else {
                continue;
            };
            let mut names: Vec<String> = entries
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .filter_map(|e| e.file_name().into_string().ok())
                .collect();
            names.sort();
            for name in names {
                if seen.insert(name.clone()) {
                    out.push((name, scope.clone()));
                }
            }
        }
        out
    }

    /// Resolve, or fall back to the bare root when the pointer is stale.
    pub fn resolve_or_root(root: impl Into<PathBuf>) -> (Self, Option<String>) {
        let root = root.into();
        match Self::resolve(&root) {
            Ok(layout) => (layout, None),
            Err(e) => (Layout::new(root), Some(e.to_string())),
        }
    }

    /// The workspace pointer file. One line: the workspace name.
    pub fn active_workspace_file(root: &Path) -> PathBuf {
        root.join(".bm").join("active-workspace")
    }

    /// The bm root above the current directory, resolving nothing.
    /// the repo marker made every remote worker die at startup with "no repo
    /// root found".
    pub fn find_root() -> Result<PathBuf> {
        let cwd = std::env::current_dir().context("reading current dir")?;
        let mut cur: Option<&Path> = Some(cwd.as_path());
        while let Some(dir) = cur {
            if dir.join("rust/Cargo.toml").is_file() || dir.join(".bm/profile").is_file() {
                return Ok(dir.to_path_buf());
            }
            cur = dir.parent();
        }
        anyhow::bail!(
            "no bm root found above {} (expected rust/Cargo.toml in a checkout, .bm/profile on a provisioned worker)",
            cwd.display()
        )
    }

    /// Walk up from the current directory looking for a bm root, and resolve
    pub fn discover() -> Result<Self> {
        Self::resolve(Self::find_root()?)
    }
}
mod assets;
mod book;
mod migrate;
mod state;
mod workspaces;
pub use workspaces::{chapter_of, workspaces, WorkspaceConfig, WorkspaceEntry};

#[cfg(test)]
mod tests;
