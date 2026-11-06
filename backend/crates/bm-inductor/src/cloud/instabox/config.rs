//! `.bm/instabox.json`: the key, the spec and what a launch may ask for.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Where the workspace keeps its InstaBox definition.
pub(crate) const FILE: &str = "instabox.json";

fn default_api_url() -> String {
    "https://api.instabox.sh".into()
}

fn default_max_workers() -> u32 {
    1
}

fn default_ttl_hours() -> u32 {
    7
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct InstaboxConfig {
    /// From the workspace's settings page; empty until `ibx login` stores one.
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_api_url")]
    pub api_url: String,
    /// The size to create, as `<vcpus>/<mem GiB>` (`4/8`). Empty asks the
    /// workspace's own list for the smallest it allows.
    #[serde(default)]
    pub spec: String,
    /// The baked template to launch. Empty uses [`DEFAULT_TEMPLATE`].
    #[serde(default)]
    pub template: String,
    #[serde(default = "default_max_workers")]
    pub max_workers: u32,
    /// Hours a box lives unless the command says otherwise. `0` is none.
    #[serde(default = "default_ttl_hours")]
    pub ttl_hours: u32,
    /// Publish the worker port after the create: a machine has no address of
    /// its own, so without this it is not dialable at all.
    #[serde(default = "default_true")]
    pub publish_worker_port: bool,
}

/// The template that carries the worker stack; see `docs/cloud.md`.
pub(crate) const DEFAULT_TEMPLATE: &str = "agent-browser";

impl Default for InstaboxConfig {
    fn default() -> Self {
        InstaboxConfig {
            api_key: String::new(),
            api_url: default_api_url(),
            spec: String::new(),
            template: String::new(),
            max_workers: default_max_workers(),
            ttl_hours: default_ttl_hours(),
            publish_worker_port: true,
        }
    }
}

impl InstaboxConfig {
    pub(crate) fn load(root: &Path) -> Self {
        let path = Self::path(root);
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    /// `.bm/`, like every other file that holds a credential.
    pub(crate) fn path(root: &Path) -> std::path::PathBuf {
        root.join(".bm").join(FILE)
    }

    /// Write the whole definition, restricting the file: it holds a key.
    pub(crate) fn save(&self, root: &Path) -> anyhow::Result<()> {
        let path = Self::path(root);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| anyhow::anyhow!("creating {}: {e}", parent.display()))?;
        }
        bm_core::write_json(&path, self)?;
        bm_core::util::restrict(&path)
    }

    /// Set the key alone, leaving every other field as the file had it. An
    /// empty key clears it, which is how an operator turns the provider off.
    pub(crate) fn store_key(root: &Path, key: &str) -> anyhow::Result<()> {
        let mut cfg = Self::load(root);
        cfg.api_key = key.trim().to_string();
        cfg.save(root)
    }

    /// What a launch cannot do without. The key is the only thing that has to
    /// be filled in: a spec and a template have sensible defaults.
    pub(crate) fn missing(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.api_key.trim().is_empty() {
            out.push("api_key is empty — the workspace's settings page has one".to_string());
        }
        if self.api_url.trim().is_empty() {
            out.push("api_url is empty".to_string());
        }
        if self.max_workers == 0 {
            out.push("max_workers is 0, no launch can ever be within the cap".to_string());
        }
        out
    }

    pub(crate) fn template(&self) -> &str {
        let t = self.template.trim();
        if t.is_empty() {
            DEFAULT_TEMPLATE
        } else {
            t
        }
    }

    /// The key, or the environment's, so a CI run needs no file.
    pub(crate) fn key(&self) -> String {
        let env = std::env::var("INSTABOX_API_KEY").unwrap_or_default();
        if !self.api_key.trim().is_empty() {
            self.api_key.trim().to_string()
        } else {
            env.trim().to_string()
        }
    }

    pub(crate) fn summary(&self) -> String {
        format!(
            "{} · spec {} · template {} · max {} · {}{}",
            self.api_url.trim_end_matches('/'),
            if self.spec.trim().is_empty() {
                "(tier's smallest)"
            } else {
                self.spec.trim()
            },
            self.template(),
            self.max_workers,
            if self.ttl_hours == 0 {
                "no deadline".to_string()
            } else {
                format!("{}h", self.ttl_hours)
            },
            if self.publish_worker_port {
                " · worker port published"
            } else {
                " · no port published (unreachable)"
            },
        )
    }
}
