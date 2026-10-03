use super::*;

/// Run a `:` operator command. `Command::Key` never arrives here — the caller
pub(crate) fn do_command(
    app: &mut App,
    cmd: Command,
    http: &reqwest::Client,
    job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
) {
    match cmd {
        Command::Key(_) => unreachable!("Command::Key is pressed by the caller"),
        // Digest off / on, cluster-wide. **Off takes the snapshot; on is the only
        Command::DigestOff | Command::DigestOn => {
            let restore = matches!(cmd, Command::DigestOn);
            if app.machines.is_empty() {
                app.set_status(Level::Warn, "no machines known — nothing to switch");
                return;
            }
            if restore {
                // A restore with no snapshot would post an empty policy to every
                let path = app.layout.bm_state().join("digest-suspend.json");
                if !path.is_file() {
                    app.set_status(
                        Level::Warn,
                        "no snapshot to restore — `:off` was not run from here; use `P` per box",
                    );
                    return;
                }
            }
            let machines: Vec<(String, Option<Vec<bm_proto::TaskPref>>)> = app
                .machines
                .iter()
                .map(|m| (m.addr.clone(), m.task_policy.clone()))
                .collect();
            let count = machines.len();
            dispatch(
                app,
                job_tx,
                Job::DigestPolicy {
                    api: app.api.clone(),
                    http: http.clone(),
                    layout: app.layout.clone(),
                    machines,
                    restore,
                },
            );
            app.set_status(
                Level::Info,
                if restore {
                    format!("restoring each box's digest policy… ({count} machine(s))")
                } else {
                    format!("turning digest off everywhere… ({count} machine(s))")
                },
            );
        }
        Command::AddMachine => {
            // Bind prompt: `addr [user [port [key]]]`, prefilled from the
            let def = app.ssh_defaults();
            let mut initial = format!("{} {}", def.user, def.port);
            if let Some(k) = &def.key {
                initial.push(' ');
                initial.push_str(k);
            }
            let mut prompt = TextPrompt::new(
                TextKind::AddMachine,
                "Add machine",
                "addr [user [port [key]]] — empty key means ssh decides (agent / ~/.ssh/config)",
                &initial,
            );
            prompt.cursor = 0;
            app.screen = Screen::Text(prompt);
        }
        Command::AddSample => {
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::AddSample,
                "Add pooled sample",
                "clip path, e.g. ~/dl/young-female-4.mp3 — tags come from the filename, voice auto-rolls",
                "",
            ));
        }
        Command::AddNamed => {
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::AddNamed,
                "Add named voice",
                "clip path as Name, e.g. refs/narrator.mp3 as Narrator — manual assignment only, never rotates",
                "",
            ));
        }
        Command::Provision { force } => match app.selected_machine() {
            None => app.set_status(Level::Warn, "no machine selected — :add adds one first"),
            Some(m) => {
                app.screen = Screen::Confirm(Confirm {
                    title: if force {
                        "Re-provision (force)".into()
                    } else {
                        "Provision machine".into()
                    },
                    danger: false,
                    body: vec![
                        format!("Onboard {} over ssh.", m.addr),
                        String::new(),
                        if force {
                            "Force ignores the skip-if-configured check and re-sends the".into()
                        } else {
                            "Already-configured machines are detected and skipped, so this is"
                                .into()
                        },
                        if force {
                            "sidecar and its weights. That is the slow path.".into()
                        } else {
                            "cheap to run again — it will report why it did nothing.".into()
                        },
                    ],
                    action: ConfirmAction::Provision {
                        addr: m.addr.clone(),
                        force,
                    },
                });
            }
        },
        Command::DropMachine => match app.selected_machine() {
            None => app.set_status(Level::Warn, "no machine selected"),
            Some(m) => {
                app.screen = Screen::Confirm(Confirm {
                    title: "Drop machine from the registry".into(),
                    danger: true,
                    body: vec![
                        format!("Remove {} from the cluster registry.", m.addr),
                        String::new(),
                        "This forgets the machine. It does not touch anything on the".into(),
                        "remote box, and re-adding it by address is enough to bring it back."
                            .into(),
                    ],
                    action: ConfirmAction::DropMachine {
                        addr: m.addr.clone(),
                    },
                });
            }
        },
        Command::Relink => match app.selected_machine() {
            None => app.set_status(Level::Warn, "no machine selected"),
            Some(m) => {
                app.screen = Screen::Confirm(Confirm {
                    title: "Relink to the box's current address".into(),
                    danger: false,
                    body: vec![
                        format!(
                            "{} no longer answers — EC2 public IPs change on every",
                            m.addr
                        ),
                        "stop/start and spot relaunch.".into(),
                        String::new(),
                        "The account is read, the box is matched by its EC2 instance id,".into(),
                        "and the registry entry is re-pointed at the address it carries".into(),
                        "now. Nothing is pushed; :prov afterwards onboards it.".into(),
                    ],
                    action: ConfirmAction::RelinkMachine {
                        old_addr: m.addr.clone(),
                    },
                });
            }
        },
        Command::Translate => {
            let start = app.setting_u32("start", 1);
            let count = app.setting_u32("count", 1);
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::Translate,
                "Translate — enqueue crawl + digest",
                "chapter range as <start> <count>. Prefilled from the inductor's settings.",
                &format!("{start} {count}"),
            ));
        }
        Command::CrawlSetup => {
            let current = app.setting_str("url_template", "");
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::CrawlTemplate,
                "Crawl setup — save the URL template, then probe one chapter",
                "must contain {n} (the chapter number) — or leave it empty to probe without \
                 saving one, which is what a crawler with a discover() needs",
                &current,
            ));
        }
        Command::Import => {
            let chapter = app.setting_u32("start", 1);
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::Import,
                "Import — adopt chapter text from a file",
                "`<chapter> <path>`, e.g. `34 /tmp/ch34.txt` — or just the path, if it is named `ch34.txt`. \
                 The text goes through the same cleaning and length check a crawl does.",
                &format!("{chapter} "),
            ));
        }
        Command::SshKey => {
            let cur = app.ssh_defaults().key.unwrap_or_default();
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::SshKey,
                "Default ssh key",
                "key path for machines bound without one — empty clears it (ssh decides). Must exist.",
                &cur,
            ));
        }
        Command::SshUser => {
            let cur = app.ssh_defaults().user;
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::SshUser,
                "Default ssh user",
                "login for machines bound without one",
                &cur,
            ));
        }
        Command::SshPort => {
            let cur = app.ssh_defaults().port.to_string();
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::SshPort,
                "Default ssh port",
                "port for machines bound without one",
                &cur,
            ));
        }
        Command::Advertise => {
            // Prefilled with what is in force — including the `127.0.0.1`
            let cur = app.setting_str("advertise", "127.0.0.1");
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::Advertise,
                "Advertised address for workers",
                "host or host:port as the *workers* see this machine — needed when they are \
                 off the LAN (a cloud box cannot reach a NAT'd 192.168.x.x). Empty restores the guess.",
                &cur,
            ));
        }
        Command::ModelsRelease => {
            let cur = app.setting_str("models_release", "");
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::ModelsRelease,
                "Model artifact release",
                "GitHub owner/name holding the baked weights as a release (tools/models.sh \
                 publish). A provisioned box then fetches that 363 MB bundle and verifies it \
                 itself, instead of receiving 668 MB over this machine's uplink. Empty restores \
                 the push.",
                &cur,
            ));
        }
        Command::PacksRelease => {
            let cur = app.setting_str("packs_release", "");
            // The prompt says where the *tag* comes from, because the obvious
            let which = match bm_core::profile::in_force(&app.layout).map(|b| b.pack) {
                Ok(p) if !p.version.is_empty() => {
                    format!(
                        "The loaded profile is {} v{}, so boxes ask for {}.",
                        p.name,
                        p.version,
                        bm_core::artifact::pack_tag_for(&p.name, &p.version)
                    )
                }
                Ok(p) => format!(
                    "The loaded profile '{}' names no version, so nothing would be fetched yet — \
                     re-publish it with `tools/profile.sh pack {} --version <v>` first.",
                    p.name, p.name
                ),
                Err(_) => "No profile is loaded, so there is no pack to fetch.".to_string(),
            };
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::PacksRelease,
                "Profile pack release",
                &format!(
                    "GitHub owner/name holding the profile pack as a release (tools/profile.sh \
                     pack <name> --version <v>, then `gh release create`). A provisioned box \
                     fetches that ~60 MB of assets/ and verifies it itself, instead of \
                     receiving it over this machine's uplink once per box. {} Empty restores \
                     the push.",
                    which
                ),
                &cur,
            ));
        }
        Command::RenderBatch => {
            // Prefilled from `run_preview` — the *same* precedence the run
            let cur = run_preview(app).render_batch;
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::RenderBatch,
                "Render batch — takes per offer",
                "how many of ONE chapter's takes a single render offer carries, 1-64. \
                 Each take still settles on its own ledger row; this only decides how many \
                 travel together, so a worker pays one round trip per batch instead of one \
                 per segment. Saved to this workspace's settings.",
                &cur.to_string(),
            ));
        }
        Command::TtsThreads { threads } => match app.selected_machine() {
            None => app.set_status(
                Level::Warn,
                "no machine selected — the sidecar thread count is one box's",
            ),
            Some(m) => {
                if let Some(v) = threads {
                    let addr = m.addr.clone();
                    let label = crate::tui::model::machine_label(&m);
                    dispatch(
                        app,
                        job_tx,
                        Job::SetTtsThreads {
                            api: app.api.clone(),
                            http: http.clone(),
                            addr,
                            threads: v,
                        },
                    );
                    app.set_status(
                        Level::Info,
                        match v {
                            Some(n) => format!(
                                "threads {label}: {n} — the sidecar restarts on the next render"
                            ),
                            None => format!(
                                "threads {label}: back to the sidecar default — it restarts on the next render"
                            ),
                        },
                    );
                    return;
                }
                // Prefilled with the box's own override, empty when it has
                let cur = m.tts_threads.map(|t| t.to_string()).unwrap_or_default();
                let label = crate::tui::model::machine_label(&m);
                app.screen = Screen::Text(TextPrompt::new(
                    TextKind::TtsThreads,
                    &format!("TTS threads — {label}"),
                    "ONNX threads for this box's TTS sidecar, 1-64. Empty restores the \
                     sidecar's own default (half the cores, capped at 8). The box restarts \
                     its sidecar on the next render. One model sits behind a mutex, so more \
                     threads run one line faster, never two lines at once.",
                    &cur,
                ));
            }
        },
        Command::Mix => {
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::Mix,
                "Mix — story speed and layer volumes",
                "as <speed 0.5-2.0> <fx 0-2> <music 0-2> [inject 0-2], e.g. 1.25 1.0 1.0 1.0 (0 mutes). \
                 These are the whole layers; per-sound trims and the pools themselves are :sound",
                &crate::tui::input::runconfig::mix_prefill(app),
            ));
        }
        Command::Sound => {
            // The editor is a screen, not a prompt: it reads three registries
            app.screen = Screen::Sound(crate::tui::sound::SoundView::new());
            app.load_sound(job_tx);
        }
        Command::Llm => {
            super::super::llm::open_llm(app);
        }
        Command::WorkspacePick => {
            // `:ws` on its own: read the tree and list it. Rows come from the
            app.screen = Screen::WorkspaceList(crate::tui::screen::WsList::read(&app.layout.root));
            app.set_status(
                Level::Info,
                "workspace: ↑↓ to move, Enter to switch, Esc to close",
            );
        }
        Command::Workspace { prefill } => {
            // Prefilled with what was typed after `:ws`, not with the workspace
            let initial = prefill.clone();
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::Workspace,
                "Workspace — one directory per book",
                "<name> to switch · `new <name>` to create (pick a profile and a crawler) · \
                 `new <name> --profile <id>` to skip the pickers · clear the line to pick \
                 from the list. \
                 Only with the cluster stopped (:X): the ledger, settings and data all move.",
                &initial,
            ));
        }
        Command::Profile => {
            let cur = crate::tui::model::profile_label(app.profile.as_ref());
            let initial = if cur == "none" { "" } else { cur.as_str() };
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::Profile,
                "Profile — the genre bundle behind assets/ + prompts/",
                "<name> to load · `pack <name>` to bundle the live tree · empty to list. \
                 Loading replaces the live tree every worker reads, so only with the cluster stopped (:X).",
                initial,
            ));
        }
        Command::Rerender => {
            // Full re-speak: worth one Enter, like every other destructive
            app.screen = Screen::Confirm(Confirm::rerender());
        }
        Command::Voices => {
            dispatch_op(
                app,
                job_tx,
                http,
                OpRequest {
                    op: Op::Voices,
                    ..Default::default()
                },
            );
        }
        Command::SwapVoice => {
            app.screen = Screen::Pick(Picker::new());
            if app.roster.is_none() {
                app.load_roster(job_tx, http);
            }
            // The audition keys need the line index, and building it takes
            app.ensure_lines(job_tx);
        }
        Command::Cast => {
            app.screen = Screen::Cast(CastView::new());
            if app.roster.is_none() {
                app.load_roster(job_tx, http);
            }
            // Start the audition line index now: it is seconds of file reads, and
            app.ensure_lines(job_tx);
        }
        Command::Eta => {
            dispatch_op(
                app,
                job_tx,
                http,
                OpRequest {
                    op: Op::Eta,
                    ..Default::default()
                },
            );
        }
        Command::FixSpeaker {
            chapter,
            segment,
            expect,
            speaker,
        } => {
            dispatch_op(
                app,
                job_tx,
                http,
                OpRequest {
                    op: Op::FixSpeaker,
                    chapter: Some(chapter),
                    segment: Some(segment),
                    expect: Some(expect),
                    speaker: Some(speaker),
                    ..Default::default()
                },
            );
        }
        Command::Retry { stage, chapter } => {
            dispatch_op(
                app,
                job_tx,
                http,
                OpRequest {
                    op: Op::Retry,
                    stage,
                    chapter,
                    ..Default::default()
                },
            );
        }
        Command::Dispatch { go } => {
            // The line the inductor answers with is the report: it names the
            app.set_status(
                Level::Info,
                if go {
                    "starting distribution…"
                } else {
                    "holding — nothing new will be offered"
                },
            );
            dispatch_op(
                app,
                job_tx,
                http,
                OpRequest {
                    op: Op::Dispatch,
                    go: Some(go),
                    ..Default::default()
                },
            );
        }
        Command::Remerge => {
            // Same blast radius as `:mix` (finished mp3s rebuild from cache),
            dispatch_op(
                app,
                job_tx,
                http,
                OpRequest {
                    op: Op::Remerge,
                    ..Default::default()
                },
            );
            app.set_status(Level::Info, "requeueing every merge — render cache kept");
        }
        Command::AuditionCurrent => {
            audition::audition_word(app, job_tx, http, audition::AuditionKind::Current);
        }
        Command::AuditionTry => {
            audition::audition_word(
                app,
                job_tx,
                http,
                audition::AuditionKind::Pointed { reroll: false },
            );
        }
        Command::AuditionAnother => {
            audition::audition_word(
                app,
                job_tx,
                http,
                audition::AuditionKind::Pointed { reroll: true },
            );
        }
        Command::AwsLogin => {
            // The console's download is the whole prompt. Prefilled with where
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::AwsLogin,
                "AWS login — the IAM user this app runs as",
                "the console's accessKeys.csv: IAM → Users → storycast-operator → Security \
                 credentials → Access keys → Create access key → Download .csv file. \
                 It carries both halves, so no secret is typed here.",
                "~/Downloads/accessKeys.csv",
            ));
        }
        Command::AwsDiscover => {
            // Prefilled with the region already in force, so the common re-run
            let cfg = bm_core::provision::AwsConfig::load_layered(&app.layout.root);
            let initial = if cfg.region.trim().is_empty() {
                String::new()
            } else {
                format!("--region {}", cfg.region.trim())
            };
            app.screen = Screen::Text(TextPrompt::new(
                TextKind::AwsDiscover,
                "AWS discover — read the account into .bm/aws.json",
                "--region <r> [--pem <path>] [--instance-profile <name>] \
                 [--security-group sg-…] [--subnet subnet-…] [--ami ami-…] [--force]. \
                 Everything already set is kept, so re-running is safe; --force only \
                 replaces a `.pem` that is a different key.",
                &initial,
            ));
        }
        Command::AwsPool => {
            // Open the view first, then fill it: the screen renders its own
            app.screen = Screen::Cloud(CloudView::new());
            dispatch(
                app,
                job_tx,
                Job::AwsPool {
                    root: app.layout.root.clone(),
                    api: app.api.clone(),
                    http: http.clone(),
                },
            );
            app.set_status(Level::Info, "reading the EC2 account…");
        }
        Command::AwsUp { count } => {
            // No confirmation: the CLI's review step is `aws up --dry-run`, and
            dispatch(
                app,
                job_tx,
                Job::AwsUp {
                    root: app.layout.root.clone(),
                    api: app.api.clone(),
                    http: http.clone(),
                    count,
                },
            );
            app.set_status(
                Level::Info,
                format!("launching {count} box(es) — watch events"),
            );
        }
        Command::AwsDown { force } => {
            if app.cloud.is_empty() {
                app.set_status(Level::Warn, "no cloud listing yet — run :pool first");
                return;
            }
            let ids: Vec<String> = app
                .cloud
                .iter()
                .filter(|i| is_live_state(&i.state))
                .map(|i| i.id.clone())
                .collect();
            if ids.is_empty() {
                app.set_status(
                    Level::Info,
                    "nothing to terminate — no live box carries the marker tag",
                );
                return;
            }
            // The guard the CLI does not have: a box killed mid-render loses
            let addrs = instance_addresses(&app.cloud);
            let busy = busy_on(&app.beats, &app.tasks, &addrs, bm_proto::now_secs());
            if !busy.is_empty() && !force {
                app.set_status(
                    Level::Warn,
                    format!(
                        "refused: {} task(s) in flight on those boxes — `:down force` to kill a render mid-flight",
                        busy.len()
                    ),
                );
                return;
            }
            let mut body = vec![
                format!("Terminate {} EC2 instance(s):", ids.len()),
                String::new(),
            ];
            for id in &ids {
                body.push(format!("  {id}"));
            }
            body.push(String::new());
            body.push("Terminated boxes cannot be restarted; a replacement is `:up`.".into());
            body.push("Their registry entries stay until you `:drop` them.".into());
            if !busy.is_empty() {
                body.push(String::new());
                // Bounded: one box can hold a whole batch of takes, and the
                body.push(format!(
                    "FORCED past {} in-flight task(s): {}",
                    busy.len(),
                    busy_summary(&busy)
                ));
            }
            app.screen = Screen::Confirm(Confirm {
                title: format!("Terminate {} box(es)?", ids.len()),
                danger: true,
                body,
                action: ConfirmAction::AwsDown { ids },
            });
        }
        Command::Script => {
            // The window reads the scripts off disk itself, so it works
            app.screen = Screen::Script(ScriptView::new(&app.layout));
            let n = match &app.screen {
                Screen::Script(v) => v.chapters.len(),
                _ => 0,
            };
            app.set_status(
                Level::Info,
                format!("script window — {n} chapter(s) · Enter open · s re-point · e excerpt · Esc close"),
            );
        }
        Command::ShutdownWhenIdle => {
            // Graceful and reversible (nothing deleted, `:B` brings workers
            dispatch_op(
                app,
                job_tx,
                http,
                OpRequest {
                    op: Op::ShutdownWhenIdle,
                    ..Default::default()
                },
            );
            app.set_status(
                Level::Info,
                "shutdown armed — workers exit once the queue drains",
            );
        }
        Command::ExclusiveCancel { route } => {
            // Nothing to confirm: dropping a queued write changes nothing
            dispatch_op(
                app,
                job_tx,
                http,
                OpRequest {
                    op: Op::ExclusiveCancel,
                    exclusive: route.map(|r| {
                        bm_proto::ExclusiveOp::parse(&r).unwrap_or(bm_proto::ExclusiveOp::Remerge)
                    }),
                    ..Default::default()
                },
            );
            app.set_status(Level::Info, "dropping the queued write — watch events");
        }
        Command::Reconcile => {
            // Reconcile rewrites cast + scripts and re-renders losers: worth
            app.screen = Screen::Confirm(Confirm {
                title: "Fold duplicate characters?".into(),
                danger: false,
                body: vec![
                    "Certain folds (titles, casing, parentheticals) apply at once;".into(),
                    "ambiguous pairs are only listed, never auto-merged.".into(),
                    String::new(),
                    "Cast rewritten, losers re-rendered. Workers keep working.".into(),
                ],
                action: ConfirmAction::Reconcile,
            });
        }
        Command::Merge { survivor, absorbed } => {
            // The word alone (`:merge` with no names) parses to the
            if survivor.trim().is_empty() || absorbed.is_empty() {
                app.set_status(
                    Level::Warn,
                    "merge needs names: `:merge \"Survivor\" \"Absorbed\"…` — quotes for spaces",
                );
                return;
            }
            // Same bargain as reconcile: worth one Enter. The op itself
            let body = vec![
                format!(
                    "{} keeps its voice; {} join{} its proper_aliases.",
                    survivor,
                    absorbed.join(", "),
                    if absorbed.len() == 1 { "s" } else { "" },
                ),
                String::new(),
                "Scripts rewritten through the folded bible, losers re-rendered.".into(),
                "Workers keep working.".into(),
            ];
            app.screen = Screen::Confirm(Confirm {
                title: "Fold these characters?".into(),
                danger: false,
                body,
                action: ConfirmAction::Merge {
                    survivor: survivor.clone(),
                    absorbed: absorbed.clone(),
                },
            });
        }
        Command::Backend => {
            // Backend up now, boxes join in background — a second press
            if app.backend_start_outstanding {
                app.set_status(Level::Warn, "backend start already running — watch events");
            } else {
                let cfg = run_preview(app);
                let cancel = Arc::new(AtomicBool::new(false));
                app.start_cancel = Some(cancel.clone());
                app.backend_start_outstanding = true;
                dispatch(
                    app,
                    job_tx,
                    Job::StartBackend {
                        layout: app.layout.clone(),
                        api: app.api.clone(),
                        api_up: app.conn == Conn::Up,
                        start: cfg.start,
                        count: cfg.count,
                        enqueue: false,
                        machines: app.effective_machines(),
                        cancel,
                        settings_key: app.ssh_defaults().key,
                    },
                );
                app.set_status(
                    Level::Info,
                    "starting backend now — boxes join in background; watch events",
                );
            }
        }
        Command::Stop => {
            let mut remotes: Vec<String> = app
                .effective_machines()
                .iter()
                .map(|m| m.addr.clone())
                .filter(|a| !["127.0.0.1", "localhost", "::1"].contains(&a.as_str()))
                .collect();
            remotes.sort();
            remotes.dedup();
            let mut body = vec![
                "Stop the local backend AND every worker on every machine.".into(),
                "In-flight tasks return to the queue; the ledger keeps".into(),
                "everything, so nothing is lost.".into(),
                String::new(),
            ];
            if remotes.is_empty() {
                body.push("No remote machines registered — local only.".into());
            } else {
                body.push(format!(
                    "Remote boxes swept over ssh: {}.",
                    remotes.join(", ")
                ));
                body.push("Unreachable boxes report and are skipped.".into());
            }
            app.screen = Screen::Confirm(Confirm {
                title: "Stop everything, everywhere?".into(),
                danger: true,
                body,
                action: ConfirmAction::StopBackend,
            });
        }
    }
}
