//! Confirm: swallow everything but Enter/y and Esc/n.
use crate::tui::{
    app::App,
    input::{dispatch, dispatch_op},
    jobs::Job,
    screen::{Confirm, ConfirmAction, Screen},
    style::Level,
};
use crate::tui::input::Flow;
use bm_proto::{Op, OpRequest};
use crossterm::event::{KeyCode, KeyEvent};
use std::sync::atomic::Ordering;

pub(crate) async fn key_confirm(
    app: &mut App,
    c: Confirm,
    key: KeyEvent,
    http: &reqwest::Client,
    job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
) -> Flow {
    match key.code {
        KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
            app.screen = Screen::Normal;
            match c.action {
                ConfirmAction::Quit => return Flow::Quit,
                ConfirmAction::Provision { addr, force } => {
                    if let Some(m) = app.machine_by_addr(&addr).cloned() {
                        app.set_status(
                            Level::Info,
                            format!("provisioning {addr} in the background…"),
                        );
                        app.log_at(Level::Info, format!("[{addr}] provisioning started"));
                        dispatch(
                            app,
                            job_tx,
                            Job::Provision {
                                layout: app.layout.clone(),
                                api: app.api.clone(),
                                machine: m,
                                force,
                                settings_key: app.ssh_defaults().key,
                                // The operator asked for this one by hand, so
                                // no `B` start's flag may stop it.
                                cancel: None,
                            },
                        );
                    } else {
                        app.set_status(Level::Warn, format!("{addr} is no longer in the registry"));
                    }
                }
                ConfirmAction::AwsDown { ids } => {
                    let n = ids.len();
                    dispatch(
                        app,
                        job_tx,
                        Job::AwsDown {
                            root: app.layout.root.clone(),
                            ids,
                        },
                    );
                    app.set_status(
                        Level::Info,
                        format!("terminating {n} box(es) — watch events"),
                    );
                }
                ConfirmAction::DropMachine { addr } => {
                    dispatch(
                        app,
                        job_tx,
                        Job::DropMachine {
                            api: app.api.clone(),
                            http: http.clone(),
                            addr,
                        },
                    );
                }
                ConfirmAction::RelinkMachine { old_addr } => {
                    let Some(m) = app.machine_by_addr(&old_addr).cloned() else {
                        app.set_status(
                            Level::Warn,
                            format!("{old_addr} is no longer in the registry"),
                        );
                        return Flow::Quit;
                    };
                    dispatch(
                        app,
                        job_tx,
                        Job::RelinkMachine {
                            layout: app.layout.clone(),
                            api: app.api.clone(),
                            http: http.clone(),
                            machine: m,
                        },
                    );
                    app.set_status(Level::Info, "relinking — reading the account…");
                }
                ConfirmAction::SwapVoice { character, voice } => {
                    dispatch_op(
                        app,
                        job_tx,
                        http,
                        OpRequest {
                            op: Op::SwapVoice,
                            character: Some(character.clone()),
                            voice: Some(voice.clone()),
                            ..Default::default()
                        },
                    );
                    app.set_status(Level::Info, format!("swapping {character} → {voice}…"));
                }
                ConfirmAction::StopBackend => {
                    // Cluster-wide and slow (ssh sweeps) — a background job,
                    // never inline, so the dashboard keeps drawing. The catch-up
                    // provisions a `B` handed out are **not** queued behind it
                    // any more: they hold one box each, the stop holds the
                    // cluster, and the two run together. So the stop does not
                    // wait for them; it sets the flag they read before they
                    // launch a worker, and an in-flight push then finishes and
                    // stays quiet rather than relaunching what X is killing.
                    if let Some(flag) = app.start_cancel.take() {
                        flag.store(true, Ordering::Relaxed);
                    }
                    app.set_status(Level::Info, "stopping everything, everywhere…");
                    dispatch(
                        app,
                        job_tx,
                        Job::StopBackend {
                            layout: app.layout.clone(),
                            machines: app.effective_machines(),
                            api: app.api.clone(),
                            settings_key: app.ssh_defaults().key,
                        },
                    );
                }
                ConfirmAction::Reconcile => {
                    dispatch_op(
                        app,
                        job_tx,
                        http,
                        OpRequest {
                            op: Op::Reconcile,
                            ..Default::default()
                        },
                    );
                    app.set_status(
                        Level::Info,
                        "reconciling duplicate characters — watch events",
                    );
                }
                ConfirmAction::Merge { survivor, absorbed } => {
                    dispatch_op(
                        app,
                        job_tx,
                        http,
                        OpRequest {
                            op: Op::Merge,
                            survivor: Some(survivor.clone()),
                            absorbed: absorbed.clone(),
                            ..Default::default()
                        },
                    );
                    app.set_status(
                        Level::Info,
                        format!(
                            "merging {} into {survivor} — watch events",
                            absorbed.join(", ")
                        ),
                    );
                }
                ConfirmAction::Rerender => {
                    dispatch_op(
                        app,
                        job_tx,
                        http,
                        OpRequest {
                            op: Op::Rerender,
                            ..Default::default()
                        },
                    );
                    app.set_status(Level::Info, "re-rendering everything — watch events");
                }
                ConfirmAction::ReleaseWorker {
                    worker,
                    count,
                    beating,
                    list,
                } => {
                    // `beating` *is* the force flag: releasing from a box that
                    // is still answering is exactly the case the op refuses
                    // without one, and the dialog the operator just answered is
                    // where that decision was made.
                    dispatch_op(
                        app,
                        job_tx,
                        http,
                        OpRequest {
                            op: Op::Release,
                            worker: Some(worker.clone()),
                            force: Some(beating),
                            ..Default::default()
                        },
                    );
                    app.set_status(
                        Level::Info,
                        format!("releasing {count} row(s) from {worker} — watch the ledger"),
                    );
                    // Back to the ledger, not the dashboard: the rows leaving
                    // *are* the confirmation, and this is the only screen where
                    // they can be seen leaving. The same reason the sound
                    // editor's removal returns to its own tab.
                    app.screen = Screen::Tasks(list);
                }
                ConfirmAction::SoundRemove(r) => {
                    // Sets the screen itself: answering this dialog returns to
                    // the pool tab it was asked from, not to the dashboard.
                    if crate::tui::input::sound::apply_removal(app, r.layer, &r.name, r.view) {
                        // A clip left the registry, so a published chapter that
                        // played it no longer matches the design on disk.
                        dispatch_op(
                            app,
                            job_tx,
                            http,
                            OpRequest {
                                op: Op::SoundChanged,
                                ..Default::default()
                            },
                        );
                    }
                }
            }
        }
        // Back to whatever raised the dialog, not to the dashboard. Every
        // caller that answers `y` returns to its own screen (the sound editor
        // to its tab, `W` to the ledger), and answering `n` has to land in the
        // same place — otherwise the two answers to one question leave the
        // operator in two different rooms.
        KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
            app.screen = app.back_out();
            app.set_status(Level::Info, "cancelled — nothing changed");
        }
        _ => {}
    }
    Flow::KeepRunning
}
