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
    let mut m = Machine::new(&addr, &user, port, key.clone(), "worker");
    m.tts_url = Some("http://127.0.0.1:8818".into());
    carry_task_policy(&mut m, &layout);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    // The run rsyncs the box under itself, so the scheduler must not hand it
    // work while it happens: this stamp is what withholds it (a beating worker
    // no longer lifts `provisioning`), and the registration below ends it.
    // Best effort — no live inductor means nothing is dispatching.
    let _ = client
        .post(format!("http://127.0.0.1:{api_port}/api/machines/state"))
        .json(&serde_json::json!({
            "addr": addr,
            "state": bm_proto::MachineState::Provisioning.as_str(),
            "note": "provisioning (cli)",
        }))
        .send()
        .await;
    // The blocking SSH/rsync flow runs off the async runtime; registration
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
    let api = format!("http://127.0.0.1:{api_port}/api/machines");
    match client.post(&api).json(&m).send().await {
        Ok(r) if r.status().is_success() => println!("[{}] registered with live inductor", addr),
        _ => {
            // No live inductor: merge into the ledger file (safe only when no
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
        for line in &r.unmigratable {
            println!("  no catalogue key, left as a name: {line}");
        }
        if r.written {
            println!("  backup: {}", r.backup().display());
        }
    }
    Ok(())
}

/// Swap every cast voice that contradicts a stated gender, then write back the
/// bible hints that stated the wrong one. The bible's real hint and the
/// adapter's own `gender.json` are the only sources — the old `adult male`
/// placeholder states nothing, so the lexicon repairs it rather than the stamp.
pub(crate) fn cmd_roster_repair_gender(layout: &Layout, dry_run: bool) -> anyhow::Result<()> {
    let settings = bm_core::config::Settings::load(&layout.settings());
    let engine = settings.engine.clone();
    let policy = bm_core::cast::policy_for_bible(&engine, layout);
    let bible_path = layout.bible();
    let mut bible: serde_json::Value = bm_core::read_json(&bible_path)
        .unwrap_or_else(|_| serde_json::json!({"characters": []}));
    let cast = bm_core::cast::read_cast(&engine, &layout.cast(&engine));
    let pool = bm_core::pool::load_pool(&layout.voice_pool());
    let lexicon = bm_core::gender::load_lexicon(layout.adapter_home().as_deref());
    let fixes = bm_core::gender::plan(&bible, &cast, &policy, &pool, &lexicon);

    if fixes.is_empty() {
        println!("gender repair: every cast voice agrees with a stated gender");
        return Ok(());
    }
    for fix in &fixes {
        let to = fix.new_voice.as_deref().unwrap_or("<no free voice>");
        let note = if fix.new_hint.is_some() {
            " (bible hint corrected)"
        } else {
            ""
        };
        println!(
            "{}: {} -> {to} ({}){note}",
            fix.character, fix.old_voice, fix.gender
        );
    }
    if dry_run {
        println!("[dry run] {} swap(s); nothing written", fixes.len());
        return Ok(());
    }

    // The swaps go through the same op the TUI uses: cast write, segment
    // invalidation and re-render queue in one move.
    let mut inner = crate::state::Inner::new(layout.clone(), settings);
    inner.load_ledger();
    inner.adopt_render_plans();
    let mut swapped = 0;
    for fix in &fixes {
        let Some(voice) = fix.new_voice.as_deref() else {
            println!(
                "  {}: no free {} voice — left alone",
                fix.character, fix.gender
            );
            continue;
        };
        match inner.op_swap_voice(&fix.character, voice) {
            Ok(msg) => {
                println!("  {msg}");
                swapped += 1;
            }
            Err(e) => println!("  {}: refused: {e}", fix.character),
        }
    }

    // A placeholder the lexicon settled is written back, so the next digest
    // reads a real gender instead of re-deriving the old one.
    let mut corrected = 0;
    if let Some(chars) = bible.get_mut("characters").and_then(|c| c.as_array_mut()) {
        for fix in &fixes {
            let Some(hint) = fix.new_hint.as_deref() else {
                continue;
            };
            let Some(entry) = chars.iter_mut().find(|c| {
                c.get("name").and_then(|n| n.as_str()) == Some(fix.character.as_str())
            }) else {
                continue;
            };
            entry["voice_hint"] = serde_json::json!(hint);
            if let Some(tags) = entry.get_mut("tags").and_then(|t| t.as_array_mut()) {
                for tag in tags.iter_mut() {
                    if matches!(tag.as_str(), Some("male" | "female")) {
                        *tag = serde_json::json!(fix.gender);
                    }
                }
            }
            corrected += 1;
        }
    }
    if corrected > 0 {
        bm_core::write_json(&bible_path, &bible)?;
    }
    println!("gender repair: {swapped} swap(s), {corrected} bible hint(s) corrected");
    Ok(())
}
