//! Text prompt: the single-line editor plus the `:` command recursion.
use crate::tui::{
    app::App,
    input::{
        command::{command_key, do_command, Command},
        dispatch,
        runconfig::save_run_config,
        runconfig::save_ssh_setting,
        submit::submit_text,
    },
    jobs::Job,
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
                    // context (task list, picker) reacts exactly as if it
                    // had been typed there. Operator actions run directly
                    // via `do_command` — their keys were removed.
                    Some(Command::Key(code)) => {
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
            } else if matches!(
                p.kind,
                TextKind::SshKey | TextKind::SshUser | TextKind::SshPort
            ) {
                match save_ssh_setting(app, p.kind, &p.buf) {
                    Ok(msg) => {
                        app.screen = Screen::Normal;
                        app.set_status(Level::Ok, msg);
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
