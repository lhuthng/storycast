use super::*;

/// List, switch or create a workspace.
pub(crate) async fn job_workspace(
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    layout: bm_core::Layout,
    api: String,
    req: WorkspaceReq,
) {
    // Read before the request is consumed: only a switch moves anything, and
    let listing = matches!(req, WorkspaceReq::List);
    let run = |cmd: crate::WorkspaceCmd| {
        let root = layout.root.clone();
        async move {
            tokio::task::spawn_blocking(move || crate::workspace_cmd(&root, cmd))
                .await
                .unwrap_or_else(|e| Err(anyhow::anyhow!("workspace task failed: {e}")))
        }
    };
    let out = match req {
        WorkspaceReq::List => run(crate::WorkspaceCmd::List).await,
        WorkspaceReq::Use(name) => match cluster_busy(&api).await {
            Some(why) => Err(anyhow::anyhow!("{why}")),
            None => run(crate::WorkspaceCmd::Use { name }).await,
        },
        WorkspaceReq::New {
            name,
            profile,
            crawler,
        } => match cluster_busy(&api).await {
            Some(why) => Err(anyhow::anyhow!("{why}")),
            None => {
                run(crate::WorkspaceCmd::New {
                    name,
                    profile,
                    crawler,
                })
                .await
            }
        },
    };
    let switched = out.is_ok() && !listing;
    match out {
        Ok(lines) => {
            for l in lines {
                send(&tx, Level::Info, l);
            }
        }
        Err(e) => send(&tx, Level::Error, format!("workspace: {e:#}")),
    }
    // Every arm owes exactly one Done; see `job_load_lines`. A successful
    let _ = tx.send(Ev::Done(if switched {
        DoneKind::Relayout
    } else {
        DoneKind::Other
    }));
}

/// List, load or pack a profile bundle, through `tools/profile.sh`.
pub(crate) async fn job_profile(
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    layout: bm_core::Layout,
    api: String,
    req: ProfileReq,
) {
    let root = layout.root.clone();
    let script = root.join("tools/profile.sh");
    let (verb, arg) = match &req {
        ProfileReq::List => ("list", None),
        ProfileReq::Load(name) => ("unpack", Some(name.clone())),
        ProfileReq::Pack(name) => ("pack", Some(name.clone())),
    };
    // A load replaces the live tree; a pack only reads it and writes into
    let loading = matches!(req, ProfileReq::Load(_));
    if loading {
        if let Some(why) = cluster_busy(&api).await {
            send(&tx, Level::Error, format!("profile: {why}"));
            let _ = tx.send(Ev::Done(DoneKind::Other));
            return;
        }
    }
    if !script.is_file() {
        send(
            &tx,
            Level::Error,
            format!("no {} — the bundle format lives there", script.display()),
        );
        let _ = tx.send(Ev::Done(DoneKind::Other));
        return;
    }
    send(
        &tx,
        Level::Info,
        match &arg {
            Some(a) => format!("profile {verb} {a}"),
            None => format!("profile {verb}"),
        },
    );
    let out = tokio::task::spawn_blocking(move || {
        let mut cmd = std::process::Command::new("bash");
        cmd.arg(&script).arg(verb);
        if let Some(a) = arg {
            cmd.arg(a);
        }
        cmd.output()
    })
    .await;
    match out {
        Ok(Ok(o)) => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
            let mut any = false;
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                send(&tx, Level::Info, line.to_string());
                any = true;
            }
            if !any {
                send(&tx, Level::Warn, "profile.sh said nothing".into());
            }
            if !o.status.success() {
                send(
                    &tx,
                    Level::Error,
                    format!("profile.sh {verb} exited {}", o.status.code().unwrap_or(-1)),
                );
            }
        }
        Ok(Err(e)) => send(&tx, Level::Error, format!("profile.sh failed to run: {e}")),
        Err(e) => send(&tx, Level::Error, format!("profile task failed: {e}")),
    }
    // A load changes which profile the live tree claims to be; the UI re-reads
    let _ = tx.send(Ev::Done(if loading {
        DoneKind::Relayout
    } else {
        DoneKind::Other
    }));
}
