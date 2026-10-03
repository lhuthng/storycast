use super::*;

/// How often the account is read while a launch is in flight.
pub(crate) const AWS_WATCH_SECS: u64 = 15;

/// One account read, folded into the registry: every repair `relink_drifted`
pub(crate) async fn relink_once(
    shared: &std::sync::Arc<tokio::sync::Mutex<state::Inner>>,
    root: &std::path::Path,
) {
    let root = root.to_path_buf();
    let pool = tokio::task::spawn_blocking(move || crate::aws_ops::pool(&root)).await;
    if let Ok(Ok((_cfg, instances))) = pool {
        let mut inner = shared.lock().await;
        for line in inner.relink_drifted(&instances) {
            inner.push_event("info", line.clone());
            println!("[inductor] {line}");
        }
    }
}

/// Pick the binaries matching the target platform. `os`/`arch` are the probe's
pub(crate) fn tts_binary_for(
    os: &str,
    arch: &str,
    layout: &Layout,
    log: &mut bm_core::provision::LiveLog,
    addr: &str,
) -> anyhow::Result<std::path::PathBuf> {
    match tts_binary_staged(os, arch, layout) {
        Ok(b) => {
            if tts_is_stale(&b, layout) {
                log.push(format!(
                    "[{addr}] staged bm-tts is older than its sources — this box runs the last sidecar built; `make tts` rebuilds it"
                ));
            }
            Ok(b)
        }
        Err(staged) => {
            // The cross candidates are buildable, and a `:prov` clicked in the
            if let Some(cand) = buildable_tts_candidates(os, arch, layout)
                .into_iter()
                .next()
            {
                log.push(format!(
                    "[{addr}] no TTS sidecar built yet — cross-building it now (release build, several minutes; the first one also fetches the ONNX Runtime)"
                ));
                build_tts_binary(&cand, layout)?;
                return Ok(cand);
            }
            Err(staged)
        }
    }
}

/// The pick among sidecar binaries already on disk. Platform-pure, no side
pub(crate) fn tts_binary_staged(
    os: &str,
    arch: &str,
    layout: &Layout,
) -> anyhow::Result<std::path::PathBuf> {
    for cand in tts_candidates(os, arch, layout) {
        if cand.is_file() {
            return Ok(cand);
        }
    }
    anyhow::bail!(
        "no TTS sidecar binary for {os}/{arch} at {} (linux/x86_64: `make tts`; linux/aarch64: `cargo zigbuild --release --target aarch64-unknown-linux-gnu -p bm-tts` after staging its runtime)",
        tts_candidates(os, arch, layout)
            .into_iter()
            .map(|c| c.display().to_string())
            .collect::<Vec<_>>()
            .join(" or ")
    )
}

/// True when a staged sidecar is older than the workspace sources it was built
pub(crate) fn tts_is_stale(bin: &std::path::Path, layout: &Layout) -> bool {
    !staged_is_fresh_against(bin, &["crates/bm-tts/src"], layout)
}

/// The sidecar targets this host can actually cross-build: exactly one.
pub(crate) fn buildable_tts_candidates(
    os: &str,
    arch: &str,
    layout: &Layout,
) -> Vec<std::path::PathBuf> {
    if (os, arch) != ("linux", "x86_64") {
        return Vec::new();
    }
    tts_candidates(os, arch, layout)
        .into_iter()
        .take(1)
        .collect()
}

/// Cross-build the sidecar into the exact path a candidate names, staging the
fn build_tts_binary(cand: &std::path::Path, layout: &Layout) -> anyhow::Result<()> {
    let target = cross_target_of(cand)?;
    let rust_dir = workspace_dir_above_target(cand)?;
    // The runtime goes where `tts_runtime_dir` will look for it, so the box
    let runtime = rust_dir.join("target").join("ort-linux-x64");
    // A staging failure is not fatal on its own: a runtime already sitting
    let stage_note = stage_onnx_runtime(layout, &runtime)
        .err()
        .map(|e| e.to_string());
    let mut cmd = std::process::Command::new("cargo-zigbuild");
    for tool in ["zig", "cargo-zigbuild"] {
        if tool_on_path(tool).is_none() {
            anyhow::bail!(
                "{tool} not found: the TTS cross-build needs it (`cargo install cargo-zigbuild`; zig from `brew install zig` or https://ziglang.org/download){}",
                stage_note
                    .map(|n| format!("; staging the ONNX Runtime also failed: {n}"))
                    .unwrap_or_default()
            );
        }
    }
    let shim_dir = tool_on_path("cargo-zigbuild")
        .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
        .expect("probed above");
    cmd.args([
        "zigbuild",
        "--release",
        "--target",
        &target,
        "-p",
        "bm-tts",
        "--bin",
        "bm-tts",
    ]);
    // The engine's own cargo features, or the sidecar is built for the wrong
    let engine = bm_core::config::Settings::load(&layout.settings()).engine;
    let features: Vec<&str> = bm_core::voices::tts_features(engine.trim())
        .unwrap_or(&[])
        .to_vec();
    if !features.is_empty() {
        cmd.arg("--features").arg(features.join(","));
    }
    cmd.arg("--manifest-path")
        .arg(rust_dir.join("Cargo.toml"))
        .env("ORT_LIB_LOCATION", &runtime)
        .env("ORT_PREFER_DYNAMIC_LINK", "1")
        // A GUI launch (or a desktop shortcut) inherits a PATH without
        .env("PATH", path_with_shim(shim_dir));
    let out = cmd
        .output()
        .map_err(|e| anyhow::anyhow!("running cargo-zigbuild: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail: String = stderr
            .lines()
            .filter(|l| !l.trim().is_empty())
            .rev()
            .take(12)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n  ");
        anyhow::bail!("cross build of bm-tts for {target} failed:\n  {tail}");
    }
    if !cand.is_file() {
        anyhow::bail!(
            "cross build reported success but {} is still missing",
            cand.display()
        )
    }
    Ok(())
}

/// `make runtime`, unless the shared library is already staged. Idempotent in
pub(crate) fn stage_onnx_runtime(layout: &Layout, dir: &std::path::Path) -> anyhow::Result<()> {
    if dir.join("libonnxruntime.so").is_file() && dir.join("libonnxruntime.so.1").is_file() {
        return Ok(());
    }
    let out = std::process::Command::new("make")
        .arg("-C")
        .arg(&layout.root)
        .arg("runtime")
        .output()
        .map_err(|e| anyhow::anyhow!("running `make runtime`: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!(
            "staging the ONNX Runtime failed: {}",
            bm_core::util::head_chars(err.trim(), 300)
        );
    }
    Ok(())
}

/// Where `make runtime` staged the shared ONNX Runtime to push alongside the
pub(crate) fn tts_runtime_dir(os: &str, arch: &str, layout: &Layout) -> Option<std::path::PathBuf> {
    match (os, arch) {
        ("linux", "x86_64") => Some(layout.root.join("rust/target/ort-linux-x64")),
        ("linux", "aarch64") => Some(layout.root.join("rust/target/ort-linux-aarch64")),
        _ => None,
    }
}

pub(crate) fn agent_binary_for(
    os: &str,
    arch: &str,
    layout: &Layout,
) -> anyhow::Result<std::path::PathBuf> {
    match agent_binary_staged(os, arch, layout) {
        Ok(b) => Ok(b),
        Err(staged) => {
            // The cross targets are cheap to produce on demand (a debug
            if let Some(cand) = buildable_agent_candidates(os, arch, layout)
                .into_iter()
                .next()
            {
                build_agent_binary(&cand)?;
                return Ok(cand);
            }
            Err(staged)
        }
    }
}

/// The pick among binaries already on disk. Platform-pure, no side effects
pub(crate) fn agent_binary_staged(
    os: &str,
    arch: &str,
    layout: &Layout,
) -> anyhow::Result<std::path::PathBuf> {
    for cand in agent_candidates(os, arch, layout) {
        if cand.is_file() && staged_is_fresh(&cand, layout) {
            return Ok(cand);
        }
    }
    anyhow::bail!(
        "no agent binary for {os}/{arch} at {} (linux/x86_64 is cross-built; linux/aarch64: `cargo zigbuild --target aarch64-unknown-linux-gnu -p bm-agent`; macOS: provision from a same-arch Mac so the native build matches)",
        agent_candidates(os, arch, layout)
            .into_iter()
            .map(|c| c.display().to_string())
            .collect::<Vec<_>>()
            .join(" or ")
    )
}

/// Cross candidates only, the native build is never something we can conjure
pub(crate) fn buildable_agent_candidates(
    os: &str,
    arch: &str,
    layout: &Layout,
) -> Vec<std::path::PathBuf> {
    let native = layout.root.join("rust/target/debug/bm-agent");
    agent_candidates(os, arch, layout)
        .into_iter()
        .filter(|c| c != &native)
        .collect()
}

/// True when no workspace source the agent builds from is newer than the
pub(crate) fn staged_is_fresh(bin: &std::path::Path, layout: &Layout) -> bool {
    staged_is_fresh_against(
        bin,
        &[
            "crates/bm-agent/src",
            "crates/bm-core/src",
            "crates/bm-proto/src",
        ],
        layout,
    )
}

/// The same question for a different set of sources, the sidecar is built
pub(crate) fn staged_is_fresh_against(
    bin: &std::path::Path,
    dirs: &[&str],
    layout: &Layout,
) -> bool {
    let built = match std::fs::metadata(bin).and_then(|m| m.modified()) {
        Ok(t) => t,
        Err(_) => return false,
    };
    for dir in dirs {
        if sources_newer_than(&layout.root.join("rust").join(dir), built) {
            return false;
        }
    }
    true
}

fn sources_newer_than(dir: &std::path::Path, built: std::time::SystemTime) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return false;
    };
    for e in rd.filter_map(|e| e.ok()) {
        let p = e.path();
        if p.is_dir() {
            if sources_newer_than(&p, built) {
                return true;
            }
        } else if p.extension().and_then(|x| x.to_str()) == Some("rs")
            && std::fs::metadata(&p)
                .and_then(|m| m.modified())
                .map(|t| t > built)
                .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

/// Build the agent binary into the exact path a candidate names, so the
fn build_agent_binary(cand: &std::path::Path) -> anyhow::Result<()> {
    let target = cross_target_of(cand)?;
    let rust_dir = workspace_dir_above_target(cand)?;
    for tool in ["zig", "cargo-zigbuild"] {
        if tool_on_path(tool).is_none() {
            anyhow::bail!(
                "{tool} not found: the linux agent cross-build needs it (`cargo install cargo-zigbuild`; zig from `brew install zig` or https://ziglang.org/download)"
            );
        }
    }
    let mut cmd = std::process::Command::new(tool_on_path("cargo-zigbuild").expect("probed above"));
    // The rustup shim dir is missing from a non-login shell's PATH, and `zig cc`
    let shim_dir = tool_on_path("cargo-zigbuild")
        .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
        .expect("probed above");
    let out = cmd
        .args([
            "zigbuild",
            "--target",
            &target,
            "-p",
            "bm-agent",
            "--manifest-path",
        ])
        .arg(rust_dir.join("Cargo.toml"))
        .env("PATH", path_with_shim(shim_dir))
        .output()
        .map_err(|e| anyhow::anyhow!("running cargo-zigbuild: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail: String = stderr
            .lines()
            .filter(|l| !l.trim().is_empty())
            .rev()
            .take(12)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n  ");
        anyhow::bail!("cross build of bm-agent for {target} failed:\n  {tail}");
    }
    if !cand.is_file() {
        anyhow::bail!(
            "cross build reported success but {} is still missing",
            cand.display()
        )
    }
    Ok(())
}

/// This process's PATH with `dir` in front, the shape a build needs when the
fn path_with_shim(dir: std::path::PathBuf) -> std::ffi::OsString {
    let old = std::env::var_os("PATH").unwrap_or_default();
    let mut dirs = vec![dir];
    dirs.extend(std::env::split_paths(&old));
    std::env::join_paths(dirs).unwrap_or(old)
}

/// Where a build tool actually is, looking in `~/.cargo/bin` as well as PATH:
fn tool_on_path(tool: &str) -> Option<std::path::PathBuf> {
    let dirs = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default();
    if let Some(found) = dirs.iter().map(|d| d.join(tool)).find(|c| c.is_file()) {
        return Some(found);
    }
    std::env::var("HOME")
        .ok()
        .map(|h| std::path::Path::new(&h).join(".cargo/bin").join(tool))
        .filter(|c| c.is_file())
}

/// The cross target a candidate names: its grandparent directory under
pub(crate) fn cross_target_of(cand: &std::path::Path) -> anyhow::Result<String> {
    cand.parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("cannot infer target from {}", cand.display()))
}

/// Walk up from a candidate binary to the workspace directory, the candidate
pub(crate) fn workspace_dir_above_target(
    cand: &std::path::Path,
) -> anyhow::Result<std::path::PathBuf> {
    cand.ancestors()
        .find(|a| {
            a.file_name().is_some_and(|n| n == "target")
                && a.parent()
                    .is_some_and(|p| p.file_name().is_some_and(|n| n == "rust"))
        })
        .and_then(|a| a.parent())
        .map(|p| p.to_path_buf())
        .ok_or_else(|| anyhow::anyhow!("cannot locate the rust workspace above {}", cand.display()))
}

/// Cross builds first (they target older glibc and run anywhere), then the
pub(crate) fn agent_candidates(os: &str, arch: &str, layout: &Layout) -> Vec<std::path::PathBuf> {
    let dir = layout.root.join("rust/target");
    let mut cands = Vec::new();
    match (os, arch) {
        ("linux", "x86_64") => cands.push(dir.join("x86_64-unknown-linux-gnu/debug/bm-agent")),
        ("linux", "aarch64") => cands.push(dir.join("aarch64-unknown-linux-gnu/debug/bm-agent")),
        _ => {}
    }
    if os == std::env::consts::OS && arch == std::env::consts::ARCH {
        cands.push(dir.join("debug/bm-agent"));
    }
    cands
}

/// Same order as the agent: cross first, native when this machine matches.
fn tts_candidates(os: &str, arch: &str, layout: &Layout) -> Vec<std::path::PathBuf> {
    let dir = layout.root.join("rust/target");
    let mut cands = Vec::new();
    match (os, arch) {
        ("linux", "x86_64") => cands.push(dir.join("x86_64-unknown-linux-gnu/release/bm-tts")),
        ("linux", "aarch64") => cands.push(dir.join("aarch64-unknown-linux-gnu/release/bm-tts")),
        _ => {}
    }
    if os == std::env::consts::OS && arch == std::env::consts::ARCH {
        cands.push(dir.join("release/bm-tts"));
    }
    cands
}
