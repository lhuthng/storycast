use super::provision::carry_task_policy;
use super::provision::provision_machine;
use super::*;

/// Rewrite interjections into engine tags via the live inductor API.
pub(crate) async fn cmd_retag(api: &str, dry_run: bool) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?;
    let res: bm_proto::OpResult = client
        .post(format!("{}/api/op", api.trim_end_matches('/')))
        .json(&bm_proto::OpRequest {
            op: bm_proto::Op::Retag,
            dry_run: Some(dry_run),
            ..Default::default()
        })
        .send()
        .await?
        .json()
        .await?;
    println!("{}", res.message);
    if !res.ok {
        anyhow::bail!("retag refused");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn cmd_provision(
    layout: Layout,
    addr: String,
    user: String,
    port: u16,
    key: Option<String>,
    api_port: u16,
    force: bool,
    release_repo: Option<String>,
) -> anyhow::Result<()> {
    // The blocking SSH/rsync flow runs off the async runtime; registration
    // afterwards needs the live API client.
    let mut m = Machine::new(&addr, &user, port, key.clone(), "worker");
    m.tts_url = Some("http://127.0.0.1:8818".into());
    carry_task_policy(&mut m, &layout);
    let out = tokio::task::spawn_blocking({
        let (layout, addr, user) = (layout.clone(), addr.clone(), user.clone());
        move || provision_machine(&layout, &addr, &user, port, key, force, None, release_repo)
    })
    .await?;
    for line in &out.lines {
        println!("{line}");
    }
    if !out.ready {
        println!("[{addr}] provision INCOMPLETE — fix the errors above and run it again");
    }
    // Register the machine so the scheduler sees it: prefer the live API,
    // fall back to merging the ledger file (safe only when no inductor runs
    // the API attempt failing is exactly that signal).
    let api = format!("http://127.0.0.1:{api_port}/api/machines");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    match client.post(&api).json(&m).send().await {
        Ok(r) if r.status().is_success() => println!("[{}] registered with live inductor", addr),
        _ => {
            // No live inductor: merge into the ledger file (safe only when no
            // inductor runs, the API attempt failing is exactly that signal).
            // New shape is `machine_state` (runtime); a pre-migration file
            // still carrying the `machines` array gets both, so the boot
            // migration sees one coherent story.
            let path = layout.bm_state().join("ledger.json");
            let mut doc: serde_json::Value = std::fs::read_to_string(&path)
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or(serde_json::json!({"tasks": [], "machine_state": {}}));
            let (_, rt) = bm_core::provision::split_machine(&m, "");
            if let Some(st) = doc.get_mut("machine_state").and_then(|v| v.as_object_mut()) {
                st.insert(addr.clone(), serde_json::to_value(&rt)?);
            }
            if let Some(ms) = doc.get_mut("machines").and_then(|v| v.as_array_mut()) {
                ms.retain(|x| x.get("addr").and_then(|a| a.as_str()) != Some(addr.as_str()));
                ms.push(serde_json::to_value(&m)?);
            }
            std::fs::create_dir_all(layout.bm_state())?;
            bm_core::atomic_write(&path, &serde_json::to_string_pretty(&doc)?)?;
            // Config side: a provision is a bind, so the box lands in
            // machines.json under its stored name (or the address, first time).
            let boxes_path = layout.machines();
            let name = bm_core::provision::load_boxes(&boxes_path)
                .iter()
                .find(|b| b.addr == addr)
                .map(|b| b.name.clone())
                .unwrap_or_else(|| addr.clone());
            let (bxo, _) = bm_core::provision::split_machine(&m, &name);
            bm_core::provision::save_box(&boxes_path, &bxo)?;
            println!(
                "[{}] recorded in ledger file (no live inductor found)",
                addr
            );
        }
    }
    Ok(())
}

/// Add one clip to the sample pool, reporting what the filename suggested.
pub(crate) fn cmd_roster_add_sample(
    layout: &Layout,
    path: &std::path::Path,
    tags: Vec<String>,
    name: Option<String>,
) -> anyhow::Result<()> {
    // A reference clip is only useful to an engine that clones from one, and
    // whether it can is the engine's own declaration rather than an assumption
    // every caller makes. Refusing here names the engine and the fix, instead
    // of enrolling a voice no render can ever speak through.
    if !bm_core::voices::clones(&layout.engine) {
        anyhow::bail!(
            "engine '{}' declares no voice cloning — a reference clip has nothing to enrol from; \
             switch to an engine that clones (`settings.engine`), or pick one of its presets",
            layout.engine
        );
    }
    let tags = if tags.is_empty() { None } else { Some(tags) };
    for line in bm_core::pool::add_sample(layout, path, tags, name)? {
        println!("{line}");
    }
    Ok(())
}

/// Report a `migrate-cast` run: what changed, what could not, and where the
/// backup went.
pub(crate) fn cmd_roster_migrate_cast(layout: &Layout, dry_run: bool) -> anyhow::Result<()> {
    let runs = roster::migrate_cast(layout, dry_run)?;
    if runs.is_empty() {
        println!("no cast files under {}", layout.data().display());
        return Ok(());
    }
    for r in runs {
        let outcome = if r.written {
            ", rewritten"
        } else if dry_run {
            " [dry run]"
        } else if r.changed.is_empty() {
            ", already keyed"
        } else {
            ", unchanged"
        };
        println!(
            "{} ({}) — {} entries, {} keyed{}",
            r.path.display(),
            r.engine,
            r.entries,
            r.keyed,
            outcome
        );
        for (character, old, new) in &r.changed {
            println!("  {character}: {old} -> {new}");
        }
        // Never silent: an entry with no key is a clone, or a voice the
        // catalogue has dropped, and the operator is the one who can tell which.
        for line in &r.unmigratable {
            println!("  no catalogue key, left as a name: {line}");
        }
        if r.written {
            println!("  backup: {}", r.backup().display());
        }
    }
    Ok(())
}
