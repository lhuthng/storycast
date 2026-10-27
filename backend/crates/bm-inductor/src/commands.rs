use super::provision::carry_task_policy;
use super::provision::provision_machine;
use super::*;

/// One proofread sidecar, with the static scan's findings and whether a script
/// was digested from it.
pub(crate) struct ProofreadSidecar {
    pub(crate) chapter: u32,
    pub(crate) text: String,
    pub(crate) findings: Vec<bm_core::digest::QuoteFinding>,
    pub(crate) script: bool,
}

/// Every proofread sidecar in the workspace, in chapter order.
pub(crate) fn scan_proofreads(layout: &Layout) -> anyhow::Result<Vec<ProofreadSidecar>> {
    let data = layout.data();
    if !data.is_dir() {
        return Ok(Vec::new());
    }
    let mut found = Vec::new();
    for entry in std::fs::read_dir(&data)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(n) = name
            .strip_prefix("ch")
            .and_then(|s| s.strip_suffix("-repaired.txt"))
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let text = std::fs::read_to_string(&path)?;
        let findings = bm_core::digest::quote_findings(&text);
        found.push(ProofreadSidecar {
            chapter: n,
            text,
            findings,
            script: layout.script(n).is_file(),
        });
    }
    found.sort_by_key(|c| c.chapter);
    Ok(found)
}

/// Report proofread sidecars that fail the static scan, and with `semantic`
/// also ask the analyzer whether each structurally-clean sidecar kept the
/// original chapter's speakers and words. Read-only: it names the chapters to
/// re-digest instead of re-digesting them itself.
pub(crate) async fn cmd_quote_scan(
    layout: &Layout,
    settings: &Settings,
    analyzer: Option<&str>,
    verbose: bool,
    semantic: bool,
) -> anyhow::Result<()> {
    let (analyzer, settings) = if semantic {
        let llm = bm_core::config::LlmConfig::load_or_seed(&layout.root, settings);
        if let Some(flag) = analyzer {
            if llm.backend_for(flag).is_none() {
                anyhow::bail!("unknown analyzer {flag:?} — pick one with `tui` (L)");
            }
        }
        let (active, asettings) = llm.offer_analyzer(settings);
        let analyzer = analyzer.unwrap_or(active.as_str()).to_string();
        if analyzer.is_empty() {
            anyhow::bail!("no LLM provider is active — add a key with `tui` (L), then retry");
        }
        (analyzer, settings.with_analyzer_settings(&asettings))
    } else {
        (String::new(), settings.clone())
    };
    let rows = scan_proofreads(layout)?;
    if semantic && !layout.repair_judge_prompt().is_file() {
        anyhow::bail!(
            "no proofread judge template at {} — this adapter cannot judge \
             semantically; add the file or scan without --semantic",
            layout.repair_judge_prompt().display()
        );
    }
    let mut structural = 0;
    let mut semantic_bad = 0;
    let mut clean = 0;
    let mut unjudged = 0;
    for row in &rows {
        let state = if row.script {
            "script on disk"
        } else {
            "no script yet"
        };
        if !row.findings.is_empty() {
            structural += 1;
            println!(
                "ch{}: structural — {} finding(s) — {state}",
                row.chapter,
                row.findings.len()
            );
            if verbose {
                for f in &row.findings {
                    println!(
                        "    paragraph {} ({}): {}",
                        f.paragraph,
                        f.kind,
                        f.text.chars().take(120).collect::<String>()
                    );
                }
            }
            continue;
        }
        if !semantic {
            clean += 1;
            if verbose {
                println!("ch{}: structurally clean", row.chapter);
            }
            continue;
        }
        let original = match std::fs::read_to_string(layout.chapter_txt(row.chapter)) {
            Ok(text) => text,
            Err(e) => {
                unjudged += 1;
                println!(
                    "ch{}: unjudged — no chapter text to compare ({e})",
                    row.chapter
                );
                continue;
            }
        };
        match bm_core::digest::judge_proofread(layout, &original, &row.text, &analyzer, &settings)
            .await
        {
            bm_core::digest::ProofreadVerdict::Faithful => {
                clean += 1;
                if verbose {
                    println!("ch{}: faithful", row.chapter);
                }
            }
            bm_core::digest::ProofreadVerdict::Unfaithful(problems) => {
                semantic_bad += 1;
                println!("ch{}: semantic — {state}", row.chapter);
                for problem in &problems {
                    println!("    {problem}");
                }
            }
            bm_core::digest::ProofreadVerdict::Unjudged(why) => {
                unjudged += 1;
                println!("ch{}: unjudged — {why}", row.chapter);
            }
        }
    }
    if semantic {
        println!(
            "quote scan: {structural} structural, {semantic_bad} semantic, {clean} clean, \
             {unjudged} unjudged ({} sidecar(s))",
            rows.len()
        );
    } else {
        println!(
            "quote scan: {structural} structural, {clean} clean ({} sidecar(s); --semantic \
             asks the analyzer)",
            rows.len()
        );
    }
    Ok(())
}

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

/// Install one voice pack and say what it paired, what it left open, and which
/// cast files it retargeted.
/// The fetch is `reqwest::blocking` all the way down, which cannot be dropped
/// inside a runtime — so it runs where the provision flow runs: off it.
pub(crate) async fn cmd_roster_pack_install(
    layout: &Layout,
    name: &str,
    latest: bool,
    version: Option<String>,
    tree: Option<std::path::PathBuf>,
) -> anyhow::Result<()> {
    let opts = bm_core::voices::packs::Options {
        latest,
        version,
        tree,
    };
    let name = name.to_string();
    let (layout, fetching) = (layout.clone(), name.clone());
    let (report, enrolled) = tokio::task::spawn_blocking(move || {
        let report = bm_core::voices::packs::install(&layout, &fetching, &opts)?;
        let enrolled = bm_core::pool::enroll_manifest(&layout);
        Ok::<_, anyhow::Error>((report, enrolled))
    })
    .await??;
    println!(
        "{name} {} {} — {} slot(s)",
        report.version,
        report.source,
        report.pairs.len()
    );
    for (slot, clone) in &report.pairs {
        println!("  {name}/{slot} <- {clone}");
    }
    if !report.unfilled.is_empty() {
        println!(
            "  {} slot(s) with no clone behind them: {}",
            report.unfilled.len(),
            report.unfilled.join(", ")
        );
    }
    println!(
        "  voices.json and voice-pool.json rebuilt; {} cast file(s) retargeted",
        report.retargeted
    );
    for line in enrolled {
        println!("  {line}");
    }
    Ok(())
}

/// The packs this workspace holds, with every slot and the clone behind it.
pub(crate) fn cmd_roster_pack_list(layout: &Layout) -> anyhow::Result<()> {
    let packs = bm_core::voices::packs::Packs::load(layout);
    if packs.packs.is_empty() {
        println!(
            "no voice packs in {} — `roster pack-install <name> <tree>` adds one",
            layout.voices_dir().display()
        );
        return Ok(());
    }
    let voices = bm_core::voices::packs::voices(layout);
    for pack in &packs.packs {
        let slots: Vec<&bm_core::voices::packs::Voice> =
            voices.iter().filter(|v| v.pack == pack.name).collect();
        let open = slots.iter().filter(|v| v.file.is_empty()).count();
        println!(
            "{} {} — {} slot(s){}",
            pack.name,
            pack.version,
            slots.len(),
            if open == 0 {
                String::new()
            } else {
                format!(", {open} with no take")
            }
        );
        for v in slots {
            let label = if v.label.is_empty() {
                String::new()
            } else {
                format!(" ({})", v.label)
            };
            println!(
                "  {}{label} [{}]{}",
                v.id,
                v.tags.join(", "),
                if v.file.is_empty() {
                    " — no take"
                } else {
                    ""
                }
            );
        }
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
    let mut bible: serde_json::Value =
        bm_core::read_json(&bible_path).unwrap_or_else(|_| serde_json::json!({"characters": []}));
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
            let Some(entry) = chars
                .iter_mut()
                .find(|c| c.get("name").and_then(|n| n.as_str()) == Some(fix.character.as_str()))
            else {
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
