use super::*;

/// How this workspace crawls, and what a crawl script may spend.
///
/// `mode` is the only switch that changes *where the text comes from*:
///
/// * `script` (the default) — the crawler named by `script` decides. An empty
///   `script` means the built-in fetcher, which is `url_template` plus the
///   element `params.extract` names and nothing else: it knows no site, so it
///   is the fallback for a workspace whose script cannot be read;
/// * `manual` (the default) — nothing is fetched. Chapters are adopted from
///   files the operator supplies (`:import`), one at a time. A workspace that
///   wants the automatic path says `mode: "script"`.
/// * `script` — the crawler named by `script` decides. An empty `script` means
///   the built-in fetcher, which is `url_template` plus the element
///   `params.extract` names and nothing else: it knows no site.
///
/// **Why manual is the default.** Every workspace starts with no novel in it —
/// the operator has not chosen a site yet, and the wrong default fetches:
/// `mode: script` + a template would have a fresh workspace crawl the bundled
/// crawler's home site the first time `:translate` ran. Starting closed makes
/// the first crawl a decision the operator makes about *their* site, and the
/// import path needs no crawler at all.
///
/// **Migration is still the identity for old workspaces.** The bundled default
/// is what a settings file written before `crawl` existed *named implicitly*,
/// so deserializing a file without a `crawl` block keeps it: such a workspace
/// loads `mode: script` with the bundled Storya crawler, and its book crawls
/// byte-identically on the first run after upgrading. The manual default
/// applies only to a workspace created *after* this change — whose settings
/// say `manual` explicitly, or nothing yet at all.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CrawlSettings {
    /// `script` | `manual`. **`manual` is the default** (see the type docs); a
    /// settings file that predates the whole `crawl` block keeps `script` —
    /// see [`CrawlSettings::legacy_default`].
    pub mode: String,
    /// The crawler. A relative name is resolved against the active workspace
    /// first (`crawl/mysite.lua` there is this book's own crawler, synced to
    /// every worker with the sources), then the root, then `assets/` — which
    /// is why a profile can ship one. `.js`/`.mjs` picks the JavaScript
    /// engine, anything else Lua. Empty means the built-in fetcher.
    pub script: String,
    /// Passed to the script verbatim and never validated: a new site should be
    /// zero changes to Rust.
    pub params: serde_json::Map<String, serde_json::Value>,
    /// Extra headers on every fetch: a `Referer` some sites require, or a
    /// session cookie the operator pasted in. Secrets here live in the
    /// workspace's `settings.json`, not with the provider keys — a deliberate
    /// trade, since a crawl header is per-book configuration.
    pub headers: std::collections::BTreeMap<String, String>,
    /// Empty means the built-in browser-ish default.
    pub user_agent: String,
    /// Minimum gap between two fetches of the same host, in milliseconds.
    ///
    /// **On by default, and that is the point.** A cluster pointed at one site
    /// is the thing that gets a novel scraper banned: ten workers leasing crawl
    /// tasks all arrive at once. Rotating an address is not the fix — the new one
    /// is throttled the same, because the pacing was the problem. `0` disables
    /// it for a local fixture server.
    pub pace_ms: u64,
    /// Per-request timeout.
    pub timeout_secs: u64,
    /// Wall-clock budget for one chapter, engine time included: what stops an
    /// infinite `next`-link loop from owning a worker.
    pub max_seconds: u64,
    /// Network round trips one chapter may make. A listing walk spends several.
    pub max_fetches: u32,
}

impl Default for CrawlSettings {
    /// The default for **`Settings::default()`** — a workspace being created
    /// now, which has named no site yet: `manual`, so nothing is fetched until
    /// the operator says how chapters arrive.
    ///
    /// This is deliberately **not** the default a *deserialized* settings file
    /// gets for an absent `crawl` block — see [`Self::legacy_default`].
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
    /// **the bundled scripted crawler**, byte-identically to what the old Rust
    /// path fetched for it. That file named a `url_template` and nothing else,
    /// and the whole point of the crawl-script rework is that the same book
    /// crawls the same way after upgrading — a manual default here would turn
    /// every existing workspace off the moment the binary was replaced.
    ///
    /// Reached only when the block is *missing*: a file that says `"crawl":
    // {"mode": "manual"}` said so and is honoured, like any explicit value.
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
