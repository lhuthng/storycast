use super::*;

/// How this workspace crawls, and what a crawl script may spend.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CrawlSettings {
    /// `script` | `manual`. **`manual` is the default** (see the type docs); a
    pub mode: String,
    /// The crawler. A relative name is resolved against the active workspace
    pub script: String,
    /// Passed to the script verbatim and never validated: a new site should be
    pub params: serde_json::Map<String, serde_json::Value>,
    /// Extra headers on every fetch: a `Referer` some sites require, or a
    pub headers: std::collections::BTreeMap<String, String>,
    /// Empty means the built-in browser-ish default.
    pub user_agent: String,
    /// Minimum gap between two fetches of the same host, in milliseconds.
    pub pace_ms: u64,
    /// Per-request timeout.
    pub timeout_secs: u64,
    /// Wall-clock budget for one chapter, engine time included: what stops an
    pub max_seconds: u64,
    /// Network round trips one chapter may make. A listing walk spends several.
    pub max_fetches: u32,
}

impl Default for CrawlSettings {
    /// The default for **`Settings::default()`** — a workspace being created
    fn default() -> Self {
        CrawlSettings {
            mode: "manual".into(),
            script: String::new(),
            params: serde_json::Map::new(),
            headers: std::collections::BTreeMap::new(),
            user_agent: String::new(),
            pace_ms: 750,
            timeout_secs: 60,
            max_seconds: 180,
            max_fetches: 64,
        }
    }
}

impl CrawlSettings {
    /// What a settings file written before `crawl` existed deserializes to:
    pub fn legacy_default() -> Self {
        CrawlSettings {
            mode: "script".into(),
            script: crate::crawl::DEFAULT_SCRIPT.into(),
            ..Default::default()
        }
    }

    /// Whether anything is fetched at all.
    pub fn is_manual(&self) -> bool {
        self.mode.eq_ignore_ascii_case("manual")
    }
}
