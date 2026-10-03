//! Text prompt: the single-line editor plus the `:` command recursion.
use crate::tui::input::Flow;
use crate::tui::{
    app::App,
    input::{
        command::{command_key, do_command, Command},
        dispatch,
        runconfig::parse_mix_config,
        runconfig::save_app_setting,
        runconfig::save_run_config,
        submit::submit_text,
    },
    jobs::{op_job, Job},
    screen::{Screen, TextKind, TextPrompt},
    style::Level,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub(crate) async fn key_text(
    app: &mut App,
    prompt: TextPrompt,
    key: KeyEvent,
    http: &reqwest::Client,
    job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
) -> Flow {
    let mut p = prompt;
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Esc => {
            // Back to the screen this prompt was opened over. The layer stack
            app.screen = app.back_out();
            app.set_status(Level::Info, "cancelled — nothing was submitted");
        }
        KeyCode::Enter => {
            // `:` command line: press the named key for the operator.
            if p.kind == TextKind::Command {
                let buf = p.buf.trim().to_string();
                app.screen = app.command_return.take().unwrap_or(Screen::Normal);
                if buf.is_empty() {
                    app.set_status(Level::Info, "cancelled — nothing was submitted");
                    return Flow::KeepRunning;
                }
                // From here the prompt is spent, not stacked, and the command
                app.prompt_spent = Some(app.screen.clone());
                return match command_key(&buf) {
                    // Read-only commands press a still-live key, so a
                    Some(Command::Key(code)) => {
                        if matches!(app.screen, Screen::Pick(_) | Screen::Cast(_)) {
                            app.screen = Screen::Normal;
                        }
                        // Straight to the owning screen, skipping the wrapper
                        Box::pin(super::route(
                            app,
                            KeyEvent::new(code, KeyModifiers::empty()),
                            http,
                            job_tx,
                        ))
                        .await
                    }
                    Some(cmd) => {
                        do_command(app, cmd, http, job_tx);
                        Flow::KeepRunning
                    }
                    None => {
                        app.set_status(Level::Error, format!("unknown command :{buf} — try :help"));
                        Flow::KeepRunning
                    }
                };
            }
            // Run-config edits save a file and launch nothing: handled
            if p.kind == TextKind::RunConfig {
                match save_run_config(app, &p.buf) {
                    Ok(msg) => {
                        app.screen = Screen::Normal;
                        app.set_status(Level::Ok, msg);
                    }
                    Err(msg) => app.set_status(Level::Error, msg),
                }
            } else if p.kind == TextKind::RenderBatch {
                // Save-only, like the ssh defaults: the scheduler reads the
                match crate::tui::input::runconfig::save_render_batch(app, &p.buf) {
                    Ok(msg) => {
                        app.screen = Screen::Normal;
                        app.set_status(Level::Ok, msg);
                    }
                    Err(msg) => app.set_status(Level::Error, msg),
                }
            } else if matches!(
                p.kind,
                TextKind::SshKey
                    | TextKind::SshUser
                    | TextKind::SshPort
                    | TextKind::Advertise
                    | TextKind::ModelsRelease
                    | TextKind::PacksRelease
            ) {
                match save_app_setting(app, p.kind.clone(), &p.buf) {
                    Ok(msg) => {
                        app.screen = Screen::Normal;
                        app.set_status(Level::Ok, msg);
                    }
                    Err(msg) => app.set_status(Level::Error, msg),
                }
            } else if matches!(
                p.kind,
                TextKind::SoundAdd(_) | TextKind::SoundEdit(..) | TextKind::SoundLevel(..)
            ) {
                // The three sound-design prompts all write a registry and launch
                match crate::tui::input::sound::submit(app, &p.kind, &p.buf) {
                    Ok((msg, view)) => {
                        app.log_at(Level::Ok, msg.clone());
                        app.set_status(Level::Ok, msg);
                        app.screen = Screen::Sound(view);
                        // The registry write above is local and needs nothing
                        dispatch(
                            app,
                            job_tx,
                            op_job(
                                app,
                                http,
                                bm_proto::OpRequest {
                                    op: bm_proto::Op::SoundChanged,
                                    ..Default::default()
                                },
                            ),
                        );
                    }
                    Err(msg) => app.set_status(Level::Error, msg),
                }
            } else if matches!(
                p.kind,
                TextKind::LlmKey(_) | TextKind::LlmUrl(_) | TextKind::LlmModel(_)
            ) {
                // One provider field: validated in one place so a typo keeps
                let provider = match &p.kind {
                    TextKind::LlmKey(id) | TextKind::LlmUrl(id) | TextKind::LlmModel(id) => {
                        id.clone()
                    }
                    _ => unreachable!(),
                };
                match super::llm::save_llm_field(app, &provider, &p.kind, &p.buf) {
                    Ok(msg) => {
                        let mut v = super::super::screen::LlmView::new();
                        v.cursor = app.llm_cursor;
                        app.screen = Screen::Llm(v);
                        app.set_status(Level::Ok, msg);
                    }
                    Err(msg) => app.set_status(Level::Error, msg),
                }
            } else if p.kind == TextKind::Mix {
                // Validated here so a typo keeps the prompt open; the op
                match parse_mix_config(&p.buf) {
                    Ok((speed, fx, music, inj)) => {
                        app.screen = Screen::Normal;
                        app.set_status(Level::Ok, format!("submitted: {}", p.buf.trim()));
                        dispatch(
                            app,
                            job_tx,
                            op_job(
                                app,
                                http,
                                bm_proto::OpRequest {
                                    op: bm_proto::Op::Remix,
                                    speed: Some(speed),
                                    effect_volume: Some(fx),
                                    music_volume: Some(music),
                                    inject_volume: inj,
                                    ..Default::default()
                                },
                            ),
                        );
                    }
                    Err(msg) => app.set_status(Level::Error, msg),
                }
            } else if p.kind == TextKind::Workspace {
                // `new` or `new <name>` opens the guided flow: pick a profile,
                let buf = p.buf.trim();
                if buf.is_empty() {
                    // Nothing typed means "which book?" — so the books are
                    app.screen =
                        Screen::WorkspaceList(crate::tui::screen::WsList::read(&app.layout.root));
                    app.set_status(
                        Level::Info,
                        "workspace: ↑↓ to move, Enter to switch, Esc to close",
                    );
                    // Handled, so: not the main loop's exit.
                    return Flow::KeepRunning;
                }
                let guided = if buf == "new" {
                    Some(String::new())
                } else if let Some(rest) = buf.strip_prefix("new ") {
                    (!rest.contains("--profile")).then(|| rest.trim().to_string())
                } else {
                    None
                };
                match guided {
                    Some(name) => {
                        let profiles = super::workspace_new::preset_items(&app.layout.root);
                        app.screen = Screen::WorkspaceNew(crate::tui::screen::WorkspaceNew::new(
                            name, profiles,
                        ));
                        app.set_status(Level::Info, "workspace: pick a name, then a profile");
                    }
                    None => match submit_text(app, &p) {
                        Ok(job) => {
                            app.set_status(Level::Ok, format!("submitted: {}", buf));
                            app.screen = Screen::Normal;
                            dispatch(app, job_tx, job);
                        }
                        Err(msg) => app.set_status(Level::Error, msg),
                    },
                }
            } else {
                match submit_text(app, &p) {
                    Ok(job) => {
                        app.set_status(Level::Ok, format!("submitted: {}", p.buf.trim()));
                        app.screen = Screen::Normal;
                        dispatch(app, job_tx, job);
                    }
                    // Keep the prompt open: the operator's typing is preserved and
                    Err(msg) => app.set_status(Level::Error, msg),
                }
            }
        }
        KeyCode::Backspace => p.backspace(),
        KeyCode::Delete => p.delete(),
        KeyCode::Left => p.left(),
        KeyCode::Right => p.right(),
        KeyCode::Home => p.home(),
        KeyCode::End => p.end(),
        KeyCode::Char(c) if ctrl => match c.to_ascii_lowercase() {
            'u' => p.kill_to_start(),
            'w' => p.kill_word(),
            'a' => p.home(),
            'e' => p.end(),
            _ => {}
        },
        KeyCode::Char(c) if !alt => p.insert(c),
        _ => {}
    }
    // Write back the edited prompt — unless an arm above already closed it
    if matches!(app.screen, Screen::Text(_)) {
        app.screen = Screen::Text(p);
    }
    Flow::KeepRunning
}
