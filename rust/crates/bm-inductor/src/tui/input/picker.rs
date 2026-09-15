//! Voice picker: two-stage filter, arrows-only movement.
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use bm_proto::{Op, OpRequest};
use crate::tui::{
    app::App,
    input::dispatch_op,
    jobs::Job,
    model::{filtered_characters, filtered_voices},
    screen::{Confirm, ConfirmAction, PickStage, Picker, Screen},
    style::Level,
};

pub(crate) async fn key_picker(app: &mut App, picker: Picker, key: KeyEvent, http: &reqwest::Client, job_tx: &tokio::sync::mpsc::UnboundedSender<Job>) -> bool {
        let mut p = picker;
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc => {
                match p.stage {
                    PickStage::Voice => {
                        p.stage = PickStage::Character;
                        p.filter.clear();
                        p.cursor = 0;
                        p.scroll = 0;
                        app.screen = Screen::Pick(p);
                    }
                    PickStage::Character => {
                        app.screen = Screen::Normal;
                        app.set_status(Level::Info, "cancelled — nothing changed");
                    }
                }
            }
            KeyCode::Char('R') => {
                app.load_roster(job_tx, http);
            }
            KeyCode::Enter => match p.stage {
                PickStage::Character => {
                    let list = filtered_characters(app, &p.filter);
                    let chosen = list
                        .get(p.cursor)
                        .cloned()
                        .unwrap_or_else(|| p.filter.trim().to_string());
                    if chosen.is_empty() {
                        app.set_status(Level::Error, "pick a character, or type a new name first");
                    } else {
                        p.character = chosen;
                        p.stage = PickStage::Voice;
                        p.filter.clear();
                        p.cursor = 0;
                        p.scroll = 0;
                        app.screen = Screen::Pick(p);
                    }
                }
                PickStage::Voice => {
                    let list = filtered_voices(app, &p.filter);
                    match list.get(p.cursor) {
                        None => app.set_status(Level::Error, "no voice selected"),
                        Some(v) => {
                            if !v.allowed {
                                app.set_status(
                                    Level::Warn,
                                    format!(
                                        "{} is an accent policy concern — pick another",
                                        v.name
                                    ),
                                );
                            } else {
                                app.screen = Screen::Confirm(Confirm {
                                    title: "Confirm voice swap".into(),
                                    danger: true,
                                    body: vec![
                                        format!("Repoint “{}” from its current voice to “{}”.", p.character, v.name),
                                        String::new(),
                                        "This deletes only that speaker's cached segments, drops the".into(),
                                        "stale mp3s for the affected chapters and requeues render +".into(),
                                        "merge. Every other character keeps its cache.".into(),
                                    ],
                                    action: ConfirmAction::SwapVoice {
                                        character: p.character.clone(),
                                        voice: v.name.clone(),
                                    },
                                });
                            }
                        }
                    }
                }
            },
            KeyCode::Tab if p.stage == PickStage::Voice => {
                let list = filtered_voices(app, &p.filter);
                match list.get(p.cursor) {
                    None => app.set_status(Level::Warn, "nothing to audition"),
                    Some(v) => {
                        if p.previewing.is_some() {
                            app.set_status(Level::Warn, "an audition is already running");
                        } else {
                            p.previewing = Some(v.name.clone());
                            app.set_status(Level::Info, format!("auditioning {}…", v.name));
                            dispatch_op(
                                app,
                                job_tx,
                                http,
                                OpRequest {
                                    op: Op::PreviewVoice,
                                    voice: Some(v.name.clone()),
                                    ..Default::default()
                                },
                            );
                        }
                    }
                }
                app.screen = Screen::Pick(p);
            }
            // Movement is arrows only. `j`/`k` used to move too, which meant a
            // filter for a speaker called "Kiên" silently moved the cursor
            // instead of typing — and nothing on screen said why.
            KeyCode::Up => {
                p.cursor = p.cursor.saturating_sub(1);
                app.screen = Screen::Pick(p);
            }
            KeyCode::Down => {
                p.cursor += 1;
                app.screen = Screen::Pick(p);
            }
            KeyCode::PageUp => {
                p.cursor = p.cursor.saturating_sub(8);
                app.screen = Screen::Pick(p);
            }
            KeyCode::PageDown => {
                p.cursor += 8;
                app.screen = Screen::Pick(p);
            }
            KeyCode::Backspace => {
                p.filter.pop();
                p.cursor = 0;
                p.scroll = 0;
                app.screen = Screen::Pick(p);
            }
            KeyCode::Char(c) if ctrl => {
                match c {
                    'u' => {
                        p.filter.clear();
                        p.cursor = 0;
                        p.scroll = 0;
                    }
                    'r' => {
                        app.load_roster(job_tx, http);
                    }
                    _ => {}
                }
                app.screen = Screen::Pick(p);
            }
            KeyCode::Char(c) if !alt => {
                p.filter.push(c);
                p.cursor = 0;
                p.scroll = 0;
                app.screen = Screen::Pick(p);
            }
            _ => {}
        }
        false
}
