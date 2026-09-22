//! Text prompt: the single-line editor plus the `:` command recursion.
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
) -> bool {
    let mut p = prompt;
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Esc => {
            app.screen = app.command_return.take().unwrap_or(Screen::Normal);
            app.set_status(Level::Info, "cancelled — nothing was submitted");
        }
        KeyCode::Enter => {
            // `:` command line: press the named key for the operator.
            // Back to the screen it was typed in first, then recurse —
            // the mapped key runs exactly what it always runs, prompts
            // and confirms included.
            if p.kind == TextKind::Command {
                let buf = p.buf.trim().to_string();
                app.screen = app.command_return.take().unwrap_or(Screen::Normal);
                if buf.is_empty() {
                    app.set_status(Level::Info, "cancelled — nothing was submitted");
                    return false;
                }
                return match command_key(&buf) {
                    // Read-only commands press a still-live key, so a
                    // context (task list) reacts exactly as if it had been
                    // typed there. Operator actions run directly via
                    // `do_command` — their keys were removed.
                    // From the picker/cast the filter owns every letter, so
                    // a global key (`q` `?` `K` …) would type instead of
                    // acting — run those via Normal first.
                    Some(Command::Key(code)) => {
                        if matches!(app.screen, Screen::Pick(_) | Screen::Cast(_)) {
                            app.screen = Screen::Normal;
                        }
                        Box::pin(super::handle_key(
                            app,
                            KeyEvent::new(code, KeyModifiers::empty()),
                            http,
                            job_tx,
                        ))
                        .await
                    }
                    Some(cmd) => {
                        do_command(app, cmd, http, job_tx);
                        false
                    }
                    None => {
                        app.set_status(Level::Error, format!("unknown command :{buf} — try :help"));
                        false
                    }
                };
            }
            // Run-config edits save a file and launch nothing: handled
            // here rather than in `submit_text`, which can only dispatch.
            // The ssh defaults work the same way.
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
                // value when it builds its next offer, so there is nothing to
                // dispatch and nothing to invalidate.
                match crate::tui::input::runconfig::save_render_batch(app, &p.buf) {
                    Ok(msg) => {
                        app.screen = Screen::Normal;
                        app.set_status(Level::Ok, msg);
                    }
                    Err(msg) => app.set_status(Level::Error, msg),
                }
            } else if matches!(
                p.kind,
                TextKind::SshKey | TextKind::SshUser | TextKind::SshPort | TextKind::Advertise
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
                // nothing. Validated in one place so a typo keeps the prompt
                // open with the operator's own typing still in it, and so the
                // three paths cannot disagree about what they check.
                match crate::tui::input::sound::submit(app, &p.kind, &p.buf) {
                    Ok((msg, view)) => {
                        app.log_at(Level::Ok, msg.clone());
                        app.set_status(Level::Ok, msg);
                        app.screen = Screen::Sound(view);
                        // The registry write above is local and needs nothing
                        // from the scheduler; the *invalidation* is the
                        // scheduler's. Say what happened rather than letting a
                        // retuned effect leave every mp3 that uses it looking
                        // current — the whole reason this op exists.
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
            } else if p.kind == TextKind::Mix {
                // Validated here so a typo keeps the prompt open; the op
                // itself saves the mix and requeues every merge — live via
                // the API, offline against the files when the inductor is
                // down — so this branch dispatches instead of writing.
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
            } else {
                match submit_text(app, &p) {
                    Ok(job) => {
                        app.set_status(Level::Ok, format!("submitted: {}", p.buf.trim()));
                        app.screen = Screen::Normal;
                        dispatch(app, job_tx, job);
                    }
                    // Keep the prompt open: the operator's typing is preserved and
                    // the problem is stated in place.
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
    // (Esc / successful submit). Doing this unconditionally re-opened the
    // prompt on every close.
    if matches!(app.screen, Screen::Text(_)) {
        app.screen = Screen::Text(p);
    }
    false
}
