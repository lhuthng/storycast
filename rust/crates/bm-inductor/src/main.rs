//! Inductor: control API + scheduler. Workers report facts; this decides.
use crate::aws_cli::aws_cmd;
use crate::book::cmd_backup;
use crate::book::cmd_check;
use crate::book::cmd_crawl;
use crate::book::cmd_digest;
use crate::book::cmd_excerpts;
use crate::book::BackupOpts;
use crate::book::ExcerptOpts;
use crate::commands::cmd_provision;
use crate::commands::cmd_retag;
use crate::commands::cmd_roster_add_sample;
use crate::commands::cmd_roster_migrate_cast;
use crate::preset::workspace_cmd;
use crate::provision::check_bins;
use crate::serve::cmd_serve;

mod api;
mod aws_ops;
mod backend;
mod dispatch;
mod manual;
mod releases;
mod roster;
mod segments;
mod state;
mod tui;
mod tunnel;

use bm_core::{config::Settings, Layout};
use bm_proto::Machine;
use clap::{Parser, Subcommand};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // The dashboard and `workspace` are the management plane: they must open
    // on a root whose pointer is *stale*, because re-pointing is exactly how
    // that gets repaired. Everything else resolves first and refuses. The
    // problem is carried, not swallowed, the dashboard prints it, and
    // `workspace list` shows which names are still there.
    let manages = matches!(&cli.cmd, Cmd::Workspace { .. } | Cmd::Tui { .. });
    let (mut layout, mut pointer_problem) = match cli.root {
        Some(r) if manages => Layout::resolve_or_root(r),
        Some(r) => (Layout::resolve(r)?, None),
        None if manages => Layout::resolve_or_root(Layout::find_root()?),
        None => (Layout::discover()?, None),
    };
    // One-time history migration, before anything resolves a path through the
    // layout: a language that was flat at the root — `prompts/` plus the pack's
    // `assets/crawl/` — gets its own home, and the binding is stamped with its
    // name. Silent and idempotent once done, which is every start after this
    // one, and it does nothing at all on a checkout that never loaded a profile.
    if let Some(name) = layout.migrate_adapter_tree().unwrap_or(None) {
        println!(
            "adapter '{name}': prompts/ moved under adapters/{name}/ — the language has its own tree now"
        );
        // Re-read: the name the pointer now carries is what every path below
        // resolves through, including the ones already computed above.
        let (relaid, problem) = Layout::resolve_or_root(layout.root.clone());
        layout = relaid;
        if problem.is_some() {
            pointer_problem = problem;
        }
    }
    let settings = Settings::load(&layout.settings());
    // One-time migration: first run after the upgrade seeds `.bm/llm.json`
    // from the legacy workspace settings + environment, then saves it.
    bm_core::config::LlmConfig::load_or_seed(&layout.root, &settings);
    // `roster` and `workspace` are local file work: requiring ssh/rsync/ffmpeg
    // to rewrite JSON would make them unusable on exactly the machine that
    // needs them. Same for `digest`, which is one HTTP call to an analyzer
    // and touches no worker — and for `crawl`, which is one book lookup or
    // a few polite HTTP calls and touches no worker either.
    if !matches!(
        &cli.cmd,
        Cmd::Roster { .. }
            | Cmd::Digest { .. }
            | Cmd::Crawl { .. }
            | Cmd::Backup { .. }
            | Cmd::Excerpts { .. }
            | Cmd::Workspace { .. }
            | Cmd::Aws { .. }
            | Cmd::Asset { .. }
            | Cmd::Profile { .. }
    ) {
        check_bins()?;
    }
    match cli.cmd {
        Cmd::Serve {
            port,
            bind,
            start,
            count,
            go,
        } => cmd_serve(layout, settings, port, &bind, start, count, go).await,
        Cmd::Provision {
            r#box,
            addr,
            user,
            port,
            key,
            api_port,
            force,
            release_repo,
        } => {
            // A linked box fills every flag it stored; explicit flags win for
            // the rest. Neither is an error until both are missing an address.
            let linked = r#box
                .as_deref()
                .map(|name| {
                    bm_core::provision::load_boxes(&layout.machines())
                        .into_iter()
                        .find(|b| b.name == name)
                        .ok_or_else(|| {
                            anyhow::anyhow!("no linked box {name:?} (see `link --help`)")
                        })
                })
                .transpose()?;
            let addr = addr
                .or_else(|| linked.as_ref().map(|b| b.addr.clone()))
                .ok_or_else(|| anyhow::anyhow!("provision needs --box or --addr"))?;
            // clap's defaults must not shadow a linked value: only an
            // explicitly passed flag wins over the box.
            let user = if user != "thang" {
                user
            } else {
                linked.as_ref().map(|b| b.user.clone()).unwrap_or(user)
            };
            let port = if port != 22 {
                port
            } else {
                linked.as_ref().map(|b| b.port).unwrap_or(port)
            };
            let key = key
                .or_else(|| linked.as_ref().and_then(|b| b.key.clone()))
                .or_else(|| settings.ssh.key.clone());
            cmd_provision(layout, addr, user, port, key, api_port, force, release_repo).await
        }
        Cmd::Segments {
            from,
            collect,
            prune,
            dry_run,
        } => {
            // Separate paths: the report hashes every local byte (~100s on a
            // full store in debug builds), while prune only lists names.
            // Neither piggybacks on the other.
            if prune {
                segments::cmd_prune(&layout, &settings, !dry_run)
            } else {
                segments::cmd_segments(&layout, &settings, &from, collect, dry_run)
            }
        }
        Cmd::Retag { api, dry_run } => cmd_retag(&api, dry_run).await,
        Cmd::Digest {
            chapter,
            analyzer,
            write,
            json,
        } => {
            cmd_digest(
                &layout,
                &settings,
                chapter,
                analyzer.as_deref(),
                write,
                json,
            )
            .await
        }
        Cmd::Crawl {
            start,
            count,
            force,
        } => cmd_crawl(&layout, &settings, start, count, force).await,
        Cmd::Backup {
            start,
            through,
            analyzer,
            api,
            inductor,
            model,
            retries,
            dry_run,
        } => {
            cmd_backup(
                &layout,
                settings,
                BackupOpts {
                    start,
                    through,
                    analyzer,
                    model,
                    model_api: api,
                    inductor,
                    retries,
                    dry_run,
                },
            )
            .await
        }
        Cmd::Excerpts {
            start,
            through,
            analyzer,
            api,
            model,
            retries,
            force,
            dry_run,
        } => {
            cmd_excerpts(
                &layout,
                settings,
                ExcerptOpts {
                    start,
                    through,
                    analyzer,
                    model,
                    model_api: api,
                    retries,
                    force,
                    dry_run,
                },
            )
            .await
        }
        Cmd::Link {
            name,
            addr,
            user,
            port,
            key,
        } => {
            let bxo = bm_core::provision::LinkedBox {
                name: name.clone(),
                addr,
                user,
                port,
                key,
                role: "worker".into(),
                task_policy: None,
                accepting_work: true,
                tts_threads: None,
            };
            bm_core::provision::save_box(&layout.machines(), &bxo)?;
            println!("linked {name} -> {}", layout.machines().display());
            Ok(())
        }
        Cmd::Tui { api, once } => {
            // A stale pointer is why this dashboard opened on the root: say so
            // before the alternate screen takes the terminal, or the operator
            // sees the wrong book's panes with no explanation.
            if let Some(problem) = &pointer_problem {
                eprintln!("warning: {problem}");
            }
            if once {
                tui::snapshot(&api).await
            } else {
                tui::run(&api, layout).await
            }
        }
        Cmd::Roster { cmd } => match cmd {
            RosterCmd::MigrateCast { dry_run } => cmd_roster_migrate_cast(&layout, dry_run),
            RosterCmd::AddSample { path, tags, name } => {
                cmd_roster_add_sample(&layout, &path, tags, name)
            }
        },
        Cmd::Workspace { cmd } => {
            for line in workspace_cmd(&layout.root, cmd)? {
                println!("{line}");
            }
            Ok(())
        }
        Cmd::Aws { cmd } => {
            for line in aws_cmd(&layout.root, cmd)? {
                println!("{line}");
            }
            Ok(())
        }
        Cmd::Asset { cmd } => match cmd {
            AssetCmd::Resolve { dry_run } => {
                let report = bm_core::compose::resolve(&layout.assets(), dry_run)?;
                println!("assets/ — {}", report.summary());
                if !dry_run && !report.deps.is_empty() {
                    println!(
                        "  built on {}",
                        report
                            .deps
                            .iter()
                            .map(|d| format!("{} ({})", d.name, &d.hash[..8.min(d.hash.len())]))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
                Ok(())
            }
        },
        Cmd::Profile { cmd } => match cmd {
            ProfileCmd::Update {
                dry_run,
                force,
                repo,
            } => {
                // `reqwest::blocking` builds a runtime of its own, and dropping
                // one from inside this one panics, so the update runs on a thread
                // that is not a runtime worker. It is synchronous, socket-bound
                // work either way, and the alternative — an async release source —
                // would be a second implementation of the verified unpack.
                let layout = layout.clone();
                let settings = settings.clone();
                let task = tokio::task::spawn_blocking(move || {
                    releases::cmd_update(&layout, &settings, repo.as_deref(), dry_run, force)
                });
                match task.await {
                    Ok(result) => result,
                    Err(e) => Err(anyhow::anyhow!("the update did not finish: {e}")),
                }
            }
            ProfileCmd::Check => {
                let (lines, ok) = profile_check(&layout, &settings)?;
                for line in &lines {
                    println!("{line}");
                }
                if ok {
                    return Ok(());
                }
                anyhow::bail!(
                    "the adapter, the binding and the engine disagree — no digest or \
                     render will be offered until one of them changes"
                );
            }
            ProfileCmd::Manifest {
                name,
                piece,
                version,
                force,
                dep: dep_manifest,
            } => {
                let Some(piece) = bm_core::profile::Piece::from_noun(&piece) else {
                    anyhow::bail!("unknown piece '{piece}' (pack|adapter)");
                };
                // The gate first, so a stale tree is refused before anything is
                // staged: a release is a claim about what it was built on, and
                // packing one behind a parent that has moved would make that
                // claim false in the file that exists to record it.
                let stale = bm_core::profile::stale_dependencies(&layout)?;
                if !stale.is_empty() && !force {
                    anyhow::bail!(
                        "{} moved since the last resolve — `asset resolve` to rebuild this tree, or --force to pack what is on disk",
                        stale.join(", ")
                    );
                }
                // `--dep` releases a dependency tree itself (`assets/_extends/<dep>`
                // unpacked to `assets/`), the sanitized self-contained root pack —
                // content only, no composition inputs, with a generated
                // `assets/pack.json` for whatever extends it later. Without it,
                // `name` is the live composition and `piece` picks its trees.
                let manifest = if dep_manifest {
                    bm_core::profile::compute_dep_manifest(&layout, &name, &version)?
                } else {
                    bm_core::profile::compute_manifest(&layout, piece, &name, &version)?
                };
                println!("{}", serde_json::to_string_pretty(&manifest)?);
                if !stale.is_empty() {
                    eprintln!("warning: packed while {} had moved", stale.join(", "));
                }
                Ok(())
            }
        },
        Cmd::Check { url, timeout } => cmd_check(settings.clone(), url, timeout).await,
    }
}

/// The adapter, the binding and the engine, as printable lines, plus whether
/// they agree.
///
/// **The lines come back rather than being printed here**, so a test can read
/// them — the same reason `workspace_cmd` and `aws_cmd` return their output.
/// The verdict comes back separately for the one thing that cannot be a line:
/// a disagreement has to print every fact it read *before* the process exits
/// non-zero, because a check that exits with nothing printed is a check nobody
/// can act on.
///
/// The engine is the one a run will name (`settings.engine`, what every offer
/// builds on) and the load pointer's own `engines/<name>/` tree is reported
/// beside it when the two differ, since that is a second, quieter way to get
/// the wrong voice: the run names one engine while the weights on disk are
/// another's.
fn profile_check(layout: &Layout, settings: &Settings) -> anyhow::Result<(Vec<String>, bool)> {
    // The binding **in force**, not the checkout's pointer: a workspace owns
    // its pack, language and engine, and `profile check` has to answer for the
    // book that will run rather than the root it sits on.
    let binding = bm_core::profile::in_force(layout)?;
    let declared = bm_core::adapter::in_force(layout)?;
    let verdict = bm_core::adapter::inspect(layout, &binding.pack.name, &settings.engine);
    let home = format!("{}/{}", bm_core::paths::ADAPTERS_DIR, layout.adapter);
    let mut lines = vec![format!("profile   {}", bm_core::profile::label(&binding))];
    lines.push(match &declared {
        Some(m) => {
            let mut claims = Vec::new();
            for (what, value) in [
                ("language", &m.language),
                ("pack", &m.pack),
                ("engine", &m.engine),
            ] {
                if !value.trim().is_empty() {
                    claims.push(format!("{what} {}", value.trim()));
                }
            }
            format!(
                "adapter   {home}/{} — declares {}",
                bm_core::adapter::MANIFEST,
                if claims.is_empty() {
                    "nothing".to_string()
                } else {
                    claims.join(", ")
                }
            )
        }
        None => format!(
            "adapter   {home}/ — no {}, so the language is the id's suffix",
            bm_core::adapter::MANIFEST
        ),
    });
    let from_manifest = declared
        .as_ref()
        .map(|m| !m.language.trim().is_empty())
        .unwrap_or(false);
    lines.push(format!(
        "language  {}",
        match &verdict.language {
            Some(l) if from_manifest => format!("{l} (declared)"),
            Some(l) => format!("{l} (from the id)"),
            None => "unclaimed — a pre-split adapter, which cannot be mismatched".to_string(),
        }
    ));
    let langs = bm_core::voices::languages(&settings.engine);
    lines.push(format!(
        "engine    {}{}",
        settings.engine,
        if langs.is_empty() {
            " — declares no languages".to_string()
        } else {
            format!(" — declares {}", langs.join(", "))
        }
    ));
    if layout.engine != settings.engine {
        lines.push(format!(
            "tree      engines/{} — what the load pointer names, and not the engine a run names",
            layout.engine
        ));
    }
    if verdict.agrees() {
        lines.push("verdict   ok".to_string());
        return Ok((lines, true));
    }
    for problem in &verdict.problems {
        lines.push(format!("problem   {problem}"));
    }
    Ok((lines, false))
}

#[allow(unused_imports)]
pub(crate) use aws_cli::set_json;
#[allow(unused_imports)]
pub(crate) use aws_cli::{
    aws_cli_instances, aws_cli_opt, aws_cli_raw, aws_cli_text, aws_cli_with, read_pool_doc,
    seed_pool,
};
#[allow(unused_imports)]
pub(crate) use cli::{AssetCmd, AwsCmd, Cli, Cmd, ProfileCmd, RosterCmd, WorkspaceCmd};
#[allow(unused_imports)]
pub(crate) use provision::provision_machine;
#[allow(unused_imports)]
pub(crate) use provision::{carry_task_policy, stopped, ProvisionOutcome};
#[allow(unused_imports)]
pub(crate) use stage::stage_onnx_runtime;
#[allow(unused_imports)]
pub(crate) use stage::{
    agent_binary_for, agent_binary_staged, agent_candidates, buildable_agent_candidates,
    buildable_tts_candidates, cross_target_of, staged_is_fresh, staged_is_fresh_against,
    tts_binary_staged, tts_is_stale, tts_runtime_dir, workspace_dir_above_target,
};

mod aws_cli;
mod book;
mod cli;
mod commands;
mod preset;
mod provision;
mod serve;
mod stage;

#[cfg(test)]
mod tests;

