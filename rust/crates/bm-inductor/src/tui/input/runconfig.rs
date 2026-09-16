//! Run-config: what the run screen previews, edits and saves.
use crate::tui::app::App;

/// What the run screen previews and launches with: the live settings while the
/// backend answers, the saved file while it doesn't, defaults when neither
/// exists. The source rides along and is shown — a compiled-in default must
/// read differently from a range somebody saved.
pub(crate) struct RunPreview {
    pub(crate) start: u32,
    pub(crate) count: u32,
    pub(crate) analyzer: String,
    pub(crate) models: Vec<String>,
    pub(crate) engine: String,
    pub(crate) live: bool,
    /// A settings file exists (vs compiled defaults standing in).
    pub(crate) saved: bool,
}

pub(crate) fn run_preview(app: &App) -> RunPreview {
    let saved = !app.layout_root.as_os_str().is_empty()
        && bm_core::Layout::new(&app.layout_root).settings().is_file();
    if let Some(s) = &app.settings {
        let models = s
            .get("analyze_models")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|m| m.as_str())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        return RunPreview {
            start: s.get("start").and_then(|v| v.as_u64()).unwrap_or(1) as u32,
            count: s.get("count").and_then(|v| v.as_u64()).unwrap_or(1) as u32,
            analyzer: s
                .get("analyzer")
                .and_then(|v| v.as_str())
                .unwrap_or("opencode")
                .to_string(),
            models,
            engine: s
                .get("engine")
                .and_then(|v| v.as_str())
                .unwrap_or("vieneu")
                .to_string(),
            live: true,
            saved,
        };
    }
    let s = if app.layout_root.as_os_str().is_empty() {
        bm_core::config::Settings::default()
    } else {
        bm_core::config::Settings::load(&bm_core::Layout::new(&app.layout_root).settings())
    };
    RunPreview {
        start: s.start,
        count: s.count,
        analyzer: s.analyzer,
        models: s.analyze_models,
        engine: s.engine,
        live: false,
        saved,
    }
}

/// Parse `<start> <count> [analyzer] [models,comma,separated]` — the run
/// configuration shape. `Err` keeps the prompt open; omitted trailing fields
/// keep their current values (clearing a model chain is a settings-file edit,
/// not something a blank field should do by accident).
pub(crate) type RunConfig = (u32, u32, String, Option<Vec<String>>);

pub(crate) fn parse_run_config(buf: &str, current_analyzer: &str) -> Result<RunConfig, String> {
    let (start, count) = parse_range(buf)?;
    let tokens: Vec<&str> = buf.split_whitespace().collect();
    let analyzer = match tokens.get(2) {
        None => current_analyzer.to_string(),
        Some(a) if ["opencode", "openrouter", "local", "gemini"].contains(a) => a.to_string(),
        Some(a) => {
            return Err(format!(
                "analyzer “{a}” unknown — opencode|openrouter|local|gemini"
            ))
        }
    };
    // Everything past the analyzer is the model list, rejoined: `3.8-flash,
    // 3.7-flash` (natural spacing) works exactly like `3.8-flash,3.7-flash`.
    // A model name never contains a space, so a spaced piece is a typo.
    let models: Option<Vec<String>> = {
        let rest = tokens.get(3..).unwrap_or(&[]).join(" ");
        if rest.trim().is_empty() {
            None
        } else {
            let v: Vec<String> = rest
                .split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect();
            if v.is_empty() {
                return Err("models list is empty — e.g. 3.8-flash,3.7-flash,3.5-flash".into());
            }
            if v.iter().any(|m| m.contains(char::is_whitespace)) {
                return Err("models are comma-separated — e.g. 3.8-flash,3.7-flash (no spaces outside commas)".into());
            }
            Some(v)
        }
    };
    Ok((start, count, analyzer, models))
}

/// Persist run configuration to the settings file. Returns a status line.
pub(crate) fn save_run_config(app: &App, buf: &str) -> Result<String, String> {
    let (start, count, analyzer, models) =
        parse_run_config(buf, &app.setting_str("analyzer", "opencode"))?;
    if app.layout_root.as_os_str().is_empty() {
        return Err("no repo root — restart the TUI from a checkout".into());
    }
    let settings_path = bm_core::Layout::new(&app.layout_root).settings();
    let mut settings = bm_core::config::Settings::load(&settings_path);
    // Everything on the line is saved: the file is the single source the run
    // screen previews, the footer shows and the next backend boots with.
    settings.start = start;
    settings.count = count;
    settings.analyzer = analyzer.clone();
    if let Some(m) = models {
        settings.analyze_models = m;
    }
    settings
        .save(&settings_path)
        .map_err(|e| format!("saving settings: {e:#}"))?;
    Ok(format!(
        "run config saved: ch{start}×{count}, digest {analyzer}"
    ))
}

/// Persist one app-wide ssh default to the settings file. Returns a status
/// line; `Err` keeps the prompt open. Applies to machines bound afterwards
/// (and to a running inductor after its next restart, like every setting).
pub(crate) fn save_ssh_setting(
    app: &App,
    kind: crate::tui::screen::TextKind,
    buf: &str,
) -> Result<String, String> {
    use crate::tui::screen::TextKind;
    if app.layout_root.as_os_str().is_empty() {
        return Err("no repo root — restart the TUI from a checkout".into());
    }
    let settings_path = bm_core::Layout::new(&app.layout_root).settings();
    let mut settings = bm_core::config::Settings::load(&settings_path);
    let msg = match kind {
        TextKind::SshKey => {
            let key = buf.trim();
            if key.is_empty() {
                settings.ssh.key = None;
                "ssh key default cleared — ssh decides per machine".to_string()
            } else {
                let expanded = bm_core::util::expand_tilde(key);
                if !expanded.is_file() {
                    return Err(format!(
                        "no such key: {} — check the path, or clear it to let ssh decide",
                        expanded.display()
                    ));
                }
                settings.ssh.key = Some(key.to_string());
                format!("ssh key default saved: {key}")
            }
        }
        TextKind::SshUser => {
            let user = buf.trim();
            if user.is_empty() {
                return Err("user is empty — enter a login name".into());
            }
            settings.ssh.user = user.to_string();
            format!("ssh user default saved: {user}")
        }
        TextKind::SshPort => {
            let port: u16 = buf
                .trim()
                .parse()
                .map_err(|_| format!("port “{}” is not a number", buf.trim()))?;
            settings.ssh.port = port;
            format!("ssh port default saved: {port}")
        }
        _ => return Err("not an ssh setting prompt".into()),
    };
    settings
        .save(&settings_path)
        .map_err(|e| format!("saving settings: {e:#}"))?;
    Ok(msg)
}

/// Parse `<start> <count>` — the shape the `t` prompt takes.
/// `Err` keeps the prompt open with the problem stated, never a silent default.
pub(crate) fn parse_range(buf: &str) -> Result<(u32, u32), String> {
    let mut it = buf.split_whitespace();
    let start: u32 = match it.next() {
        Some(s) => s
            .parse()
            .map_err(|_| format!("start “{s}” is not a chapter number"))?,
        None => return Err("expected: <start> <count>, e.g. 1 1".into()),
    };
    let count: u32 = match it.next() {
        Some(s) => s
            .parse()
            .map_err(|_| format!("count “{s}” is not a number"))?,
        None => return Err("expected: <start> <count>, e.g. 1 1".into()),
    };
    if count == 0 {
        return Err("count must be at least 1".into());
    }
    Ok((start, count))
}
