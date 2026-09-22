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
    /// Takes per render offer (`:batch`). Shown because it is the one knob that
    /// changes how *often* a worker is spoken to rather than what it produces.
    pub(crate) render_batch: u32,
    pub(crate) speed: f64,
    pub(crate) effect_volume: f64,
    pub(crate) music_volume: f64,
    pub(crate) inject_volume: f64,
    pub(crate) live: bool,
    /// A settings file exists (vs compiled defaults standing in).
    pub(crate) saved: bool,
}

pub(crate) fn run_preview(app: &App) -> RunPreview {
    // One source, read once: `App::effective_settings` already resolves
    // live → file → compiled default, so this function is no longer a second
    // implementation of that precedence with its own two branches to keep in
    // step. The `live`/`saved` flags are about *provenance* — what to print —
    // not about which value wins.
    let s = app.effective_settings();
    let saved = !app.layout.root.as_os_str().is_empty() && app.layout.settings().is_file();
    let num = |key: &str, default: f64| s.get(key).and_then(|v| v.as_f64()).unwrap_or(default);
    let u32_of = |key: &str, default: u32| {
        s.get(key)
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
            .unwrap_or(default)
    };
    RunPreview {
        start: u32_of("start", 1),
        count: u32_of("count", 1),
        analyzer: s
            .get("analyzer")
            .and_then(|v| v.as_str())
            .unwrap_or("opencode")
            .to_string(),
        models: s
            .get("analyze_models")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|m| m.as_str())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default(),
        engine: s
            .get("engine")
            .and_then(|v| v.as_str())
            .unwrap_or("vieneu")
            .to_string(),
        render_batch: u32_of("render_batch", bm_core::config::DEFAULT_RENDER_BATCH),
        speed: num("speed", 1.25),
        effect_volume: num("effect_volume", 1.0),
        music_volume: num("music_volume", 1.0),
        inject_volume: num("inject_volume", 1.0),
        live: app.settings.is_some(),
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

/// Parse the `:batch` prompt: one whole number of takes per offer.
///
/// Bounded here rather than in the scheduler so the operator learns the range
/// while their typing is still on screen — the scheduler clamps as a last
/// resort, not as the first answer. `Err` keeps the prompt open.
pub(crate) fn parse_render_batch(buf: &str) -> Result<u32, String> {
    let t = buf.trim();
    let n: u32 = t
        .parse()
        .map_err(|_| format!("“{t}” is not a number of takes"))?;
    if n < 1 {
        return Err("at least 1 — 0 would offer nothing and the chapter would never render".into());
    }
    if n > bm_core::config::MAX_RENDER_BATCH {
        return Err(format!(
            "at most {} — a batch is a lease on one box, not a queue",
            bm_core::config::MAX_RENDER_BATCH
        ));
    }
    Ok(n)
}

/// Persist the render batch size to this workspace's settings file. Returns a
/// status line. Save-only: nothing is dispatched, because the value is read
/// when the next offer is built.
pub(crate) fn save_render_batch(app: &App, buf: &str) -> Result<String, String> {
    if app.layout.root.as_os_str().is_empty() {
        return Err("no repo root — restart the TUI from a checkout".into());
    }
    let n = parse_render_batch(buf)?;
    let settings_path = app.layout.settings();
    let mut settings = bm_core::config::Settings::load(&settings_path);
    settings.render_batch = n;
    settings
        .save(&settings_path)
        .map_err(|e| format!("saving settings: {e:#}"))?;
    Ok(format!(
        "render batch saved: {n} take(s) per offer — takes effect on the next offer"
    ))
}

/// Persist run configuration to the settings file. Returns a status line.
pub(crate) fn save_run_config(app: &App, buf: &str) -> Result<String, String> {
    let (start, count, analyzer, models) =
        parse_run_config(buf, &app.setting_str("analyzer", "opencode"))?;
    if app.layout.root.as_os_str().is_empty() {
        return Err("no repo root — restart the TUI from a checkout".into());
    }
    let settings_path = app.layout.settings();
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

/// Parse `<speed> <effect-volume> <music-volume> <inject-volume>` — the mix
/// shape. Speed is the story tempo (0.5–2.0, the single-`atempo` range);
/// volumes are master gains over the scene map's own levels (0.0–2.0, 0 mutes,
/// 1 as authored).
pub(crate) type MixConfig = (f64, f64, f64, Option<f64>);

pub(crate) fn parse_mix_config(buf: &str) -> Result<MixConfig, String> {
    let parts: Vec<&str> = buf.split_whitespace().collect();
    if !(3..=4).contains(&parts.len()) {
        return Err(
            "expected: <speed> <fx-vol> <music-vol> [inject-vol], e.g. 1.25 1.0 1.0 1.0".into(),
        );
    }
    let num = |s: &str, what: &str, lo: f64, hi: f64| {
        s.parse::<f64>()
            .map_err(|_| format!("{what} “{s}” is not a number"))
            .and_then(|v| {
                if v.is_finite() && (lo..=hi).contains(&v) {
                    Ok(v)
                } else {
                    Err(format!("{what} must be {lo}–{hi}, got “{s}”"))
                }
            })
    };
    Ok((
        num(parts[0], "speed", 0.5, 2.0)?,
        num(parts[1], "fx volume", 0.0, 2.0)?,
        num(parts[2], "music volume", 0.0, 2.0)?,
        parts
            .get(3)
            .map(|s| num(s, "inject volume", 0.0, 2.0))
            .transpose()?,
    ))
}

/// Prefill for the `:mix` prompt from the settings in force.
///
/// The three-branch dance this used to do (live / the file / the compiled
/// defaults) now lives in `App::effective_settings`, so this is just the four
/// reads — and it cannot drift from the run screen's own numbers.
pub(crate) fn mix_prefill(app: &App) -> String {
    format!(
        "{} {} {} {}",
        app.setting_f64("speed", 1.25),
        app.setting_f64("effect_volume", 1.0),
        app.setting_f64("music_volume", 1.0),
        app.setting_f64("inject_volume", 1.0)
    )
}

/// Persist one app-wide ssh default to the settings file. Returns a status
/// line; `Err` keeps the prompt open. Applies to machines bound afterwards
/// (and to a running inductor after its next restart, like every setting).
/// Save-only prompts that write app-wide settings and launch nothing: the ssh
/// defaults and the advertised address. Validated here so a typo keeps the
/// prompt open with the operator's own typing still in it.
pub(crate) fn save_app_setting(
    app: &App,
    kind: crate::tui::screen::TextKind,
    buf: &str,
) -> Result<String, String> {
    use crate::tui::screen::TextKind;
    if app.layout.root.as_os_str().is_empty() {
        return Err("no repo root — restart the TUI from a checkout".into());
    }
    let settings_path = app.layout.settings();
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
        TextKind::Advertise => {
            // Empty clears it back to the sentinel, which is the only way to
            // undo a wrong address without hand-editing settings.json.
            let host = buf.trim();
            if host.is_empty() {
                settings.advertise = "127.0.0.1".into();
                "advertised address cleared — the launcher asks the routing table again".to_string()
            } else if host.contains(char::is_whitespace) {
                return Err("an address has no spaces — host or host:port".into());
            } else {
                settings.advertise = host.to_string();
                format!("workers will dial http://{host} — reachable from the worker side")
            }
        }
        _ => return Err("not an app setting prompt".into()),
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
