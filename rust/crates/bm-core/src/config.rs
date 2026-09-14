//! Runtime settings and a tiny `.env` loader.
//!
//! Settings live in `.bm/settings.json` so the inductor can be reconfigured
//! from the TUI and survive restarts. Secrets stay in `.env` — never here.

use crate::util::{atomic_write, read_json};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Where chapters come from, e.g. `https://site/truyen/x/chuong-{n}`.
    pub url_template: String,
    /// `vieneu` (local, unlimited) or `gemini` (cloud, quota-limited).
    pub engine: String,
    /// Chapter range the cluster is currently working on.
    pub start: u32,
    pub count: u32,
    /// Final-mix tempo and inter-line silence.
    pub speed: f64,
    pub gap_ms: u32,
    /// Per-scene ambience beds + reverb under the voice mix.
    pub ambience: bool,
    /// `opencode` | `openrouter` | `local` | `gemini`.
    pub analyzer: String,
    pub opencode_model: String,
    pub openrouter_model: String,
    pub local_model: String,
    pub ollama_url: String,
    pub analyze_model: String,
    /// Gemini TTS fallback chain, newest first.
    pub model_order: Vec<String>,
    /// Port the inductor's control API listens on.
    pub control_port: u16,
    /// Where the inductor is reachable from workers.
    pub advertise: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            url_template: "https://storya.click/truyen/nguoi-tren-van-nguoi/chuong-{n}".into(),
            engine: "vieneu".into(),
            start: 21,
            count: 80,
            speed: 1.25,
            gap_ms: 300,
            ambience: true,
            analyzer: "opencode".into(),
            opencode_model: "opencode/muse-spark-1.3-contributor-free".into(),
            openrouter_model: "google/gemma-4-31b-it:free".into(),
            local_model: "gemma-4-12b".into(),
            ollama_url: "http://localhost:11434".into(),
            analyze_model: "gemini-3.5-flash".into(),
            model_order: vec![
                "gemini-3.1-flash-tts-preview".into(),
                "gemini-2.5-pro-preview-tts".into(),
                "gemini-2.5-flash-preview-tts".into(),
            ],
            control_port: 8901,
            advertise: "127.0.0.1".into(),
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Settings {
        read_json::<Settings>(path).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        atomic_write(path, &serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Expand `{n}` in the chapter URL template.
    pub fn chapter_url(&self, n: u32) -> String {
        self.url_template.replace("{n}", &n.to_string())
    }
}

/// Minimal `.env` reader: `KEY=value`, `#` comments, optional quotes.
///
/// Deliberately not a dependency — the format is trivial and we only ever read
/// a handful of keys. Existing process env always wins, so `FOO=bar cargo run`
/// still overrides the file.
pub fn load_dotenv(path: &Path) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || std::env::var_os(key).is_some() {
            continue;
        }
        let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
        std::env::set_var(key, value);
    }
}

/// Read an env var, falling back to a default.
pub fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chapter_url_substitutes_every_n() {
        let s = Settings {
            url_template: "https://x/chuong-{n}?page={n}".into(),
            ..Default::default()
        };
        assert_eq!(s.chapter_url(12), "https://x/chuong-12?page=12");
    }

    #[test]
    fn settings_roundtrip_and_default_on_missing() {
        let dir = std::env::temp_dir().join("bm-settings-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("settings.json");
        assert_eq!(Settings::load(&p).engine, "vieneu");

        let s = Settings {
            engine: "gemini".into(),
            count: 42,
            ..Default::default()
        };
        s.save(&p).unwrap();
        let back = Settings::load(&p);
        assert_eq!(back.engine, "gemini");
        assert_eq!(back.count, 42);
    }

    #[test]
    fn dotenv_does_not_clobber_existing_env() {
        let dir = std::env::temp_dir().join("bm-dotenv-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(".env");
        std::fs::write(
            &p,
            "# comment\nBM_TEST_KEY=\"from-file\"\nBM_TEST_QUOTED='quoted'\n",
        )
        .unwrap();
        std::env::set_var("BM_TEST_KEY", "from-env");
        load_dotenv(&p);
        assert_eq!(std::env::var("BM_TEST_KEY").unwrap(), "from-env");
        assert_eq!(std::env::var("BM_TEST_QUOTED").unwrap(), "quoted");
    }

    #[test]
    fn missing_dotenv_is_not_an_error() {
        load_dotenv(Path::new("/nonexistent/.env"));
    }
}
