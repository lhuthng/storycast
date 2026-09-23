//! Segment inventory across the cluster: report, collect, prune.
//!
//! Phase 1 of the inductor-owned-segments migration. The inductor's
//! `data/audio/` is the authoritative store; this command proves it per
//! chapter (report, the default — dry-run always) and pulls what remotes hold
//! that it lacks (`--collect`). Proof is `expected_wavs`
//! (`assemble::plan`), the single namer the renderer, the completeness check
//! and the merger already share — never a file count, and never the
//! completeness boolean, because the migration needs the *missing names*.

use anyhow::{Context, Result};
use bm_core::provision::{load_boxes, resolve_key, Ssh, REMOTE_DIR};
use bm_core::segments::SegmentEntry;
use bm_core::{config::Settings, Layout};
use std::collections::{BTreeMap, BTreeSet};

/// One box the inventory covers: how to reach it plus a display label.
struct Target {
    label: String,
    addr: String,
    ssh: Ssh,
}

/// Every registered machine: linked boxes plus ledger-only addresses (a box
/// provisioned before `link` existed, or whose link was dropped). Filtered by
/// `--from` (address or link name) when given.
fn targets(layout: &Layout, settings: &Settings, only: &[String]) -> Vec<Target> {
    let mut addrs: BTreeMap<String, (String, u16, Option<String>, String)> = BTreeMap::new();
    for b in load_boxes(&layout.machines()) {
        addrs.insert(
            b.addr.clone(),
            (b.user.clone(), b.port, b.key.clone(), b.name.clone()),
        );
    }
    if let Ok(doc) = bm_core::read_json::<serde_json::Value>(&layout.bm_state().join("ledger.json"))
    {
        if let Some(rt) = doc.get("machine_state").and_then(|v| v.as_object()) {
            for addr in rt.keys() {
                addrs.entry(addr.clone()).or_insert_with(|| {
                    (
                        settings.ssh.user.clone(),
                        settings.ssh.port,
                        settings.ssh.key.clone(),
                        addr.clone(),
                    )
                });
            }
        }
    }
    // The inductor's own box is always in scope, even with no link and no
    // ledger entry yet — its manifest is walked, never ssh'd.
    addrs.entry("127.0.0.1".into()).or_insert_with(|| {
        (
            settings.ssh.user.clone(),
            settings.ssh.port,
            settings.ssh.key.clone(),
            "127.0.0.1".into(),
        )
    });
    let filter = |t: &Target| {
        // The local box is always in scope: it is the reference every remote
        // diffs against, so `--from` only narrows remotes.
        t.ssh.local || only.is_empty() || only.iter().any(|w| w == &t.addr || w == &t.label)
    };
    addrs
        .into_iter()
        .map(|(addr, (user, port, key, name))| {
            let (key, _) = resolve_key(key.as_deref(), settings.ssh.key.as_deref());
            Target {
                label: name,
                ssh: Ssh {
                    target: format!("{user}@{addr}"),
                    port,
                    key: key.map(|p| p.to_string_lossy().to_string()),
                    local: bm_core::is_local_node(&addr),
                },
                addr,
            }
        })
        .filter(filter)
        .collect()
}

/// Chapters the inductor can prove anything about: scripts present locally.
fn chapters(layout: &Layout) -> Vec<u32> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(layout.data()) else {
        return out;
    };
    for e in rd.filter_map(|e| e.ok()) {
        let n = e.file_name().to_string_lossy().into_owned();
        if let Some(num) = n
            .strip_prefix("script-")
            .and_then(|s| s.strip_suffix(".json"))
        {
            if let Ok(ch) = num.parse::<u32>() {
                if layout.script(ch).is_file() {
                    out.push(ch);
                }
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Names the expected set holds that the local store lacks. Empty means the
/// chapter verifies; the completion gate (`complete()`) treats a non-empty
/// list as a failed report. `None` when the chapter cannot be planned here at
/// all — also a failure, with nothing to name.
pub(crate) fn missing_wavs(layout: &Layout, engine: &str, chapter: u32) -> Option<Vec<String>> {
    let expected = expected_names(layout, engine, chapter)?;
    let dir = layout.seg_dir(engine, chapter);
    let mut missing: Vec<String> = expected
        .into_iter()
        .filter(|name| {
            !dir.join(name)
                .metadata()
                .map(|m| m.len() > 1000)
                .unwrap_or(false)
        })
        .collect();
    missing.sort();
    Some(missing)
}

/// The file names `(chapter, engine)` must hold.
///
/// **The recorded plan first.** A take's name is content-addressed and its
/// identity is a hash of the inputs that produced it, so the plan is the only
/// thing that knows what the set is without recomputing it from a script and a
/// cast that may have moved since — the same reason the renderer no longer
/// derives names at speak time. A chapter with no stored plan (never
/// reconciled, or written by an older build) falls back to evaluating
/// `expected_wavs` against the local script, cast and bible.
///
/// `None` when the chapter cannot be planned (unparseable script, uncast
/// speaker) — callers say so instead of guessing. Shared by the report, the
/// completion gate and the collect pass, so the writer and the prover can
/// never disagree about the set.
pub(crate) fn expected_names(
    layout: &Layout,
    engine: &str,
    chapter: u32,
) -> Option<BTreeSet<String>> {
    if let Some(plan) = bm_core::assemble::RenderPlan::load(&layout.plan(chapter)) {
        if plan.engine == engine {
            return Some(plan.files().into_iter().collect());
        }
    }
    let script_path = layout.script(chapter);
    let text = std::fs::read_to_string(&script_path).ok()?;
    let data: serde_json::Value = serde_json::from_str(&text).ok()?;
    let segments = data.get("segments")?.as_array()?;
    let policy = bm_core::cast::policy_for_bible(engine);
    let cast = bm_core::cast::load_cast(
        &script_path,
        &layout.cast(engine),
        &layout.bible(),
        &policy,
        false,
    )
    .ok()?;
    let local = engine == "vieneu";
    let title = bm_core::assemble::title_speech_for_script(&script_path, &cast, segments);
    let planned = bm_core::assemble::Planned::plan(segments);
    let wavs = bm_core::assemble::expected_wavs(
        &planned,
        &cast,
        &layout.seg_dir(engine, chapter),
        local,
        title.as_ref(),
    )
    .ok()?;
    Some(
        wavs.iter()
            .filter_map(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|s| s.to_string())
            })
            .collect(),
    )
}

/// How much of the expected set a box holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Coverage {
    Complete,
    Partial { have: usize, want: usize },
    NoDirectory,
}

fn coverage(expected: &BTreeSet<String>, have: &BTreeSet<String>) -> Coverage {
    let n = have.intersection(expected).count();
    if n == expected.len() {
        Coverage::Complete
    } else if n == 0 {
        Coverage::NoDirectory
    } else {
        Coverage::Partial {
            have: n,
            want: expected.len(),
        }
    }
}

fn show(c: Coverage) -> String {
    match c {
        Coverage::Complete => "complete".into(),
        Coverage::Partial { have, want } => format!("partial ({have}/{want})"),
        Coverage::NoDirectory => "no-directory".into(),
    }
}

/// This inductor's own manifest: walked, never ssh'd.
fn local_manifest(layout: &Layout) -> Vec<SegmentEntry> {
    bm_core::segments::manifest(&layout.audio())
}

/// A remote box's manifest, via the box's own agent over the existing ssh
/// transport. An agent too old to have the subcommand fails loudly here —
/// that is the staged-rollout signal, not a silent empty.
fn remote_manifest(ssh: &Ssh) -> Result<Vec<SegmentEntry>> {
    let script = format!("cd $HOME/{REMOTE_DIR} && ./bm-agent segments --json");
    let (code, stdout, stderr) = ssh.run(&script, 120)?;
    if code != 0 {
        anyhow::bail!(
            "agent segments failed (exit {code}): {}",
            bm_core::util::head_chars(stderr.trim(), 200)
        );
    }
    serde_json::from_str(stdout.trim()).with_context(|| "parsing agent segments output".to_string())
}

/// Delete files in a seg dir that `expected_wavs` never names (stale voices
/// from swaps the surgical invalidation could not name). Report-only unless
/// `prune` is passed — this is destructive and gets an explicit flag, never a
/// side effect. Chapters with a live render task are skipped: a local worker
/// may be writing into the dir right now.
///
/// Prints every path before deleting.
pub fn cmd_prune(layout: &Layout, settings: &Settings, prune: bool) -> Result<()> {
    let engine = settings.engine.clone();
    // Live renders first: never sweep a directory a worker may be writing.
    let mut live: BTreeSet<u32> = BTreeSet::new();
    if let Ok(doc) = bm_core::read_json::<serde_json::Value>(&layout.bm_state().join("ledger.json"))
    {
        if let Some(tasks) = doc.get("tasks").and_then(|t| t.as_array()) {
            for t in tasks {
                let active = t
                    .get("state")
                    .and_then(|s| s.as_str())
                    .is_some_and(|s| s == "assigned" || s == "running");
                let is_render = t.get("stage").and_then(|s| s.as_str()) == Some("render");
                if active && is_render {
                    if let Some(n) = t.get("chapter").and_then(|c| c.as_u64()) {
                        live.insert(n as u32);
                    }
                }
            }
        }
    }
    let mut total = 0u32;
    for n in chapters(layout) {
        if live.contains(&n) {
            println!("ch{n}: skipped — render task live");
            continue;
        }
        let Some(expected) = expected_names(layout, &engine, n) else {
            continue;
        };
        let dir = layout.seg_dir(&engine, n);
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut stale: Vec<String> = rd
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|f| !expected.contains(f))
            .collect();
        stale.sort();
        if stale.is_empty() {
            continue;
        }
        for f in &stale {
            println!("ch{n}: stale {}", dir.join(f).display());
        }
        if prune {
            for f in &stale {
                let _ = std::fs::remove_file(dir.join(f));
                total += 1;
            }
        }
    }
    if prune {
        println!("pruned {total} stale files");
    } else {
        println!("report only — pass --prune to delete");
    }
    Ok(())
}

/// Report per chapter per machine, then collect what remotes hold and the
/// inductor lacks.
pub fn cmd_segments(
    layout: &Layout,
    settings: &Settings,
    from: &[String],
    collect: bool,
    dry_run: bool,
) -> Result<()> {
    let writing = collect && !dry_run;
    let engine = settings.engine.clone();
    let boxes = targets(layout, settings, from);
    if boxes.is_empty() {
        println!("no machines in scope — link one first");
        return Ok(());
    }
    // Manifests once: the diff below reuses them per chapter.
    let mut manifests: BTreeMap<String, Vec<SegmentEntry>> = BTreeMap::new();
    for t in &boxes {
        if t.ssh.local {
            manifests.insert(t.addr.clone(), local_manifest(layout));
            continue;
        }
        match remote_manifest(&t.ssh) {
            Ok(m) => {
                manifests.insert(t.addr.clone(), m);
            }
            Err(e) => println!("{}: manifest unavailable ({e:#})", t.label),
        }
    }
    let local_names = |chapter: u32| -> BTreeSet<String> {
        manifests
            .get("127.0.0.1")
            .map(|m| {
                m.iter()
                    .filter(|e| e.chapter == chapter && e.engine == engine)
                    .map(|e| e.name.clone())
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut verified = 0u32;
    for n in chapters(layout) {
        let Some(expected) = expected_names(layout, &engine, n) else {
            println!("ch{n} ({engine}): cannot plan — script unparseable or speaker uncast");
            continue;
        };
        if expected.is_empty() {
            println!("ch{n} ({engine}): no segments expected");
            continue;
        }
        let local: BTreeSet<String> = local_names(n);
        let local_c = coverage(&expected, &local);
        if !writing && local_c == Coverage::Complete {
            verified += 1;
        }
        println!("ch{n} ({} expected):", expected.len());
        println!("  inductor   {}", show(local_c));
        for t in &boxes {
            if t.ssh.local {
                continue;
            }
            let Some(m) = manifests.get(&t.addr) else {
                println!("  {:<10} manifest unavailable", t.label);
                continue;
            };
            let have: BTreeSet<String> = m
                .iter()
                .filter(|e| e.chapter == n && e.engine == engine)
                .map(|e| e.name.clone())
                .collect();
            let c = coverage(&expected, &have);
            // Files worth pulling: expected, held remotely, missing locally.
            let mut pull: Vec<&String> = have
                .intersection(&expected)
                .filter(|f| !local.contains(*f))
                .collect();
            pull.sort();
            print!("  {:<10} {}", t.label, show(c));
            if !pull.is_empty() {
                let names: Vec<&str> = pull.iter().map(|s| s.as_str()).collect();
                print!(" — inductor lacks: {}", names.join(", "));
            }
            if c != Coverage::Complete {
                // Expected names the box does NOT hold: caps the repair
                // (retry renders exactly these) and explains a partial that
                // collection alone cannot close.
                let mut gone: Vec<&String> =
                    expected.iter().filter(|f| !have.contains(*f)).collect();
                gone.sort();
                if !gone.is_empty() {
                    let shown: Vec<&str> = gone.iter().take(5).map(|s| s.as_str()).collect();
                    print!(" — remote lacks: {}", shown.join(", "));
                    if gone.len() > 5 {
                        print!(" (+{} more)", gone.len() - 5);
                    }
                }
            }
            println!();
            if writing && !pull.is_empty() {
                // Whole-dir pull (the only rsync shape); anything unexpected
                // it brings along is `--prune`'s job in step 4, not this one's.
                let rel = format!("data/audio/segments-{engine}-{n:02}/");
                let dst = layout.seg_dir(&engine, n);
                match t.ssh.rsync_pull(&rel, &dst) {
                    Ok(()) => println!(
                        "  pulled {} from {} ({} needed files); still missing: {}",
                        rel,
                        t.label,
                        pull.len(),
                        expected.len() - local.len() - pull.len()
                    ),
                    Err(e) => println!("  collect {} from {} failed: {e:#}", rel, t.label),
                }
            }
        }
    }
    // Verify what collection claims: completeness per chapter, not file counts.
    // A half-collected chapter is no cache at all to every consumer.
    if writing {
        for n in chapters(layout) {
            if !bm_core::assemble::segments_complete(
                &layout.script(n),
                &layout.cast(&engine),
                &layout.bible(),
                &layout.seg_dir(&engine, n),
                &engine,
            ) {
                continue;
            }
            verified += 1;
        }
        println!("verified complete afterwards: {verified} chapters");
    } else if collect {
        println!("dry run — pass --collect without --dry-run to pull");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coverage_splits_complete_partial_and_missing() {
        let e: BTreeSet<String> = ["a".into(), "b".into(), "c".into()].into_iter().collect();
        assert_eq!(
            coverage(
                &e,
                &["a".into(), "b".into(), "c".into(), "x".into()]
                    .into_iter()
                    .collect()
            ),
            Coverage::Complete,
            "extras never demote"
        );
        assert_eq!(
            coverage(&e, &["a".into()].into_iter().collect()),
            Coverage::Partial { have: 1, want: 3 }
        );
        assert_eq!(
            coverage(&e, &["x".into()].into_iter().collect()),
            Coverage::NoDirectory
        );
        assert_eq!(coverage(&e, &BTreeSet::new()), Coverage::NoDirectory);
    }
}
