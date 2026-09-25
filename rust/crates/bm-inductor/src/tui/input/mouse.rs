//! Mouse input. Keyboard bindings remain the source of truth; this module maps
//! visible rows and panes to the same actions without duplicating key parsing.

use super::handle_key;
use crate::tui::{
    app::{App, HitTarget, ListTarget, Panel},
    jobs::Job,
    model::{beat_backed, live_beats},
    screen::{PickStage, Screen},
    style::Level,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use std::time::{Duration, Instant};

pub(crate) async fn handle_mouse(
    app: &mut App,
    mouse: MouseEvent,
    http: &reqwest::Client,
    job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
) -> bool {
    let Some(region) = app.hit_region(mouse.column, mouse.row) else {
        return false;
    };

    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            let now = Instant::now();
            let double_click = app.last_click.is_some_and(|(x, y, at)| {
                x == mouse.column
                    && y == mouse.row
                    && now.duration_since(at) < Duration::from_millis(450)
            });
            app.last_click = Some((mouse.column, mouse.row, now));

            match region.target {
                HitTarget::Panel {
                    panel,
                    row_start,
                    row_y,
                } => {
                    app.focused_panel = panel;
                    // Clicking the log means "I want to read this, and copy
                    // it" — so the click itself hands the mouse back to the
                    // terminal. Without mouse reporting the drag becomes a
                    // selection, which is the only way to get a failure out of
                    // a dashboard.
                    //
                    // Sticky, and only `M` brings it back. It cannot be
                    // "click anywhere else to restore", because once reporting
                    // is off the app receives no clicks at all — a mid-drag
                    // restore would also yank the selection out from under the
                    // pointer. The status line says how, and what was given up.
                    if panel == Panel::Events && app.mouse_capture {
                        app.mouse_capture = false;
                        app.mouse_toggle = true;
                        app.set_status(
                            Level::Info,
                            "mouse off — drag to select and copy the log · M restores \
                             click-to-select and the wheel",
                        );
                    }
                    if panel == Panel::Machines && mouse.row >= row_y {
                        let index = row_start + usize::from(mouse.row - row_y);
                        if index < app.machines.len() {
                            app.selected = index;
                            if double_click {
                                return handle_key(
                                    app,
                                    KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE),
                                    http,
                                    job_tx,
                                )
                                .await;
                            }
                        }
                    } else if panel == Panel::Workers && mouse.row >= row_y {
                        let index = row_start + usize::from(mouse.row - row_y);
                        let workers: Vec<_> = live_beats(&app.beats, bm_proto::now_secs())
                            .into_iter()
                            .filter(|b| beat_backed(&app.machines, b))
                            .collect();
                        if let Some(worker) = workers.get(index) {
                            if let Some(machine) =
                                app.machines.iter().position(|m| m.addr == worker.addr)
                            {
                                app.selected = machine;
                            }
                        }
                    } else if panel == Panel::Tasks && double_click {
                        return handle_key(
                            app,
                            KeyEvent::new(KeyCode::Char('K'), KeyModifiers::NONE),
                            http,
                            job_tx,
                        )
                        .await;
                    }
                }
                HitTarget::List {
                    kind,
                    row_start,
                    row_y,
                } => {
                    if mouse.row >= row_y {
                        let row = usize::from(mouse.row - row_y);
                        let requested = if kind == ListTarget::Digest {
                            let col = usize::from(
                                mouse.column.saturating_sub(region.area.x.saturating_add(2)) / 6,
                            );
                            row * crate::tui::screen::DIGEST_COLS + col
                        } else {
                            row_start + row
                        };
                        set_list_cursor(app, kind, requested);
                        if double_click {
                            let code = if kind == ListTarget::Policy {
                                KeyCode::Char(' ')
                            } else {
                                KeyCode::Enter
                            };
                            return handle_key(
                                app,
                                KeyEvent::new(code, KeyModifiers::NONE),
                                http,
                                job_tx,
                            )
                            .await;
                        }
                    }
                }
                HitTarget::Confirm { confirm } => {
                    let code = if confirm {
                        KeyCode::Char('y')
                    } else {
                        KeyCode::Char('n')
                    };
                    return handle_key(app, KeyEvent::new(code, KeyModifiers::NONE), http, job_tx)
                        .await;
                }
                HitTarget::SoundTabs if !double_click => {
                    return handle_key(
                        app,
                        KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
                        http,
                        job_tx,
                    )
                    .await;
                }
                HitTarget::SoundTabs => {}
            }
        }
        MouseEventKind::Down(MouseButton::Right) => {
            if matches!(app.screen, Screen::Normal)
                && matches!(
                    region.target,
                    HitTarget::Panel {
                        panel: Panel::Machines,
                        ..
                    }
                )
            {
                return handle_key(
                    app,
                    KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE),
                    http,
                    job_tx,
                )
                .await;
            }
        }
        MouseEventKind::ScrollUp => scroll(app, region.target, -1),
        MouseEventKind::ScrollDown => scroll(app, region.target, 1),
        _ => {}
    }
    false
}

fn list_len(app: &App, kind: ListTarget) -> usize {
    match (&app.screen, kind) {
        (Screen::Cast(v), ListTarget::Cast) => {
            crate::tui::model::filtered_cast_rows(&app.cast_rows(), &v.filter).len()
        }
        (Screen::Tasks(v), ListTarget::Tasks) => {
            crate::tui::model::filtered_tasks(&app.tasks, &v.filter).len()
        }
        (Screen::Sound(v), ListTarget::Sound) => app
            .sound
            .as_ref()
            .map(|d| crate::tui::sound::rows(d, v.layer).len())
            .unwrap_or(0),
        (Screen::Pick(v), ListTarget::Picker) => match v.stage {
            PickStage::Character => crate::tui::model::filtered_characters(app, &v.filter).len(),
            PickStage::Voice => crate::tui::model::filtered_voices(app, &v.filter).len(),
        },
        (Screen::Digest(v), ListTarget::Digest) => v.rows(&|n| app.layout.digested(n)).len(),
        (Screen::Cloud(_), ListTarget::Cloud) => app.cloud.len(),
        (Screen::Policy(v), ListTarget::Policy) => v.prefs.len(),
        _ => 0,
    }
}

fn set_list_cursor(app: &mut App, kind: ListTarget, requested: usize) {
    let len = list_len(app, kind);
    if let Some(cursor) = list_cursor_mut(app, kind) {
        *cursor = if len == 0 { 0 } else { requested.min(len - 1) };
    }
}

fn list_cursor_mut(app: &mut App, kind: ListTarget) -> Option<&mut usize> {
    match (&mut app.screen, kind) {
        (Screen::Cast(v), ListTarget::Cast) => Some(&mut v.cursor),
        (Screen::Tasks(v), ListTarget::Tasks) => Some(&mut v.cursor),
        (Screen::Sound(v), ListTarget::Sound) => Some(&mut v.cursor),
        (Screen::Pick(v), ListTarget::Picker) => Some(&mut v.cursor),
        (Screen::Digest(v), ListTarget::Digest) => Some(&mut v.cursor),
        (Screen::Cloud(v), ListTarget::Cloud) => Some(&mut v.cursor),
        (Screen::Policy(v), ListTarget::Policy) => Some(&mut v.cursor),
        _ => None,
    }
}

fn scroll(app: &mut App, target: HitTarget, direction: i8) {
    let step = direction as isize;
    match target {
        HitTarget::Panel { panel, .. } => match panel {
            Panel::Machines => {
                if !app.machines.is_empty() {
                    let next = app.selected as isize + step * 3;
                    app.selected = next.clamp(0, app.machines.len() as isize - 1) as usize;
                }
            }
            Panel::Events => {
                // The wheel is line-granular, the page keys are a screenful —
                // both clamped to the buffer by the same two doorways.
                if direction < 0 {
                    app.scroll_events_older(3);
                } else {
                    app.scroll_events_newer(3);
                }
            }
            Panel::Workers => {
                if direction < 0 {
                    app.worker_scroll = app.worker_scroll.saturating_add(3);
                } else {
                    app.worker_scroll = app.worker_scroll.saturating_sub(3);
                }
            }
            Panel::Tasks | Panel::Footer => {}
        },
        HitTarget::Confirm { .. } | HitTarget::SoundTabs => {}
        HitTarget::List { kind, .. } => {
            match kind {
                ListTarget::Jobs => {
                    if let Screen::Jobs { scroll, .. } = &mut app.screen {
                        *scroll = if direction < 0 {
                            scroll.saturating_add(3)
                        } else {
                            scroll.saturating_sub(3)
                        };
                    }
                }
                ListTarget::Help => {
                    if let Screen::Help { scroll } = &mut app.screen {
                        *scroll = if direction < 0 {
                            scroll.saturating_add(3)
                        } else {
                            scroll.saturating_sub(3)
                        };
                    }
                }
                ListTarget::Crawl => {
                    // Keys only: the crawl view is read with ↑↓ and PgUp/PgDn.
                }
                ListTarget::TaskDetail => {
                    if let Screen::TaskDetail(v) = &mut app.screen {
                        v.scroll = if direction < 0 {
                            v.scroll.saturating_add(3)
                        } else {
                            v.scroll.saturating_sub(3)
                        };
                    }
                }
                _ => {}
            }
            if matches!(
                kind,
                ListTarget::Jobs | ListTarget::Help | ListTarget::Crawl | ListTarget::TaskDetail
            ) {
                return;
            }
            let len = list_len(app, kind);
            if let Some(cursor) = list_cursor_mut(app, kind) {
                if len > 0 {
                    let next = *cursor as isize + step * 3;
                    *cursor = next.clamp(0, len as isize - 1) as usize;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;

    #[test]
    fn hit_testing_prefers_the_topmost_overlay_region() {
        let mut app = App::new("http://unused");
        app.add_hit_region(
            Rect::new(0, 0, 80, 24),
            HitTarget::Panel {
                panel: Panel::Events,
                row_start: 0,
                row_y: 1,
            },
        );
        app.add_hit_region(
            Rect::new(10, 5, 20, 5),
            HitTarget::Panel {
                panel: Panel::Machines,
                row_start: 3,
                row_y: 7,
            },
        );
        assert!(matches!(
            app.hit_region(12, 7).unwrap().target,
            HitTarget::Panel {
                panel: Panel::Machines,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn clicking_a_machine_row_selects_and_focuses_that_pane() {
        let mut app = App::new("http://unused");
        app.machines.push(bm_proto::Machine::new(
            "10.0.0.1", "operator", 22, None, "worker",
        ));
        app.machines.push(bm_proto::Machine::new(
            "10.0.0.2", "operator", 22, None, "worker",
        ));
        app.add_hit_region(
            Rect::new(0, 2, 80, 5),
            HitTarget::Panel {
                panel: Panel::Machines,
                row_start: 0,
                row_y: 2,
            },
        );
        let (job_tx, _job_rx) = tokio::sync::mpsc::unbounded_channel();
        let http = reqwest::Client::new();
        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 4,
                row: 3,
                modifiers: KeyModifiers::NONE,
            },
            &http,
            &job_tx,
        )
        .await;
        assert_eq!(app.selected, 1);
        assert_eq!(app.focused_panel, Panel::Machines);
    }

    /// A helper so each test can say "click here" instead of spelling out a
    /// whole `MouseEvent`.
    async fn click(app: &mut App, column: u16, row: u16) -> bool {
        let (job_tx, _job_rx) = tokio::sync::mpsc::unbounded_channel();
        handle_mouse(
            app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                modifiers: KeyModifiers::NONE,
            },
            &reqwest::Client::new(),
            &job_tx,
        )
        .await
    }

    fn logs_region(app: &mut App) {
        app.add_hit_region(
            Rect::new(0, 0, 80, 24),
            HitTarget::Panel {
                panel: Panel::Events,
                row_start: 0,
                row_y: 0,
            },
        );
    }

    #[tokio::test]
    async fn clicking_the_log_hands_the_mouse_back_so_it_can_be_selected() {
        let mut app = App::new("http://unused");
        logs_region(&mut app);
        app.mouse_capture = true;

        click(&mut app, 10, 9).await;

        // The click itself has to turn reporting off — the user asked for it
        // by clicking the thing they want to read, not by finding a key.
        assert!(!app.mouse_capture, "the click must release the mouse");
        // And the event loop must be told to act on it, not merely told.
        assert!(app.mouse_toggle, "the event loop needs the release request");
        let status = app.status.text.to_lowercase();
        assert!(
            status.contains("drag") && status.contains("select") && status.contains("copy"),
            "the status must say what to do next, got {status:?}"
        );
        // The escape hatch is named, because it is the only way back.
        assert!(
            status.contains("m restores"),
            "the status must name the way back"
        );
        // Clicking the log is also just focusing it.
        assert_eq!(app.focused_panel, Panel::Events);
    }

    #[tokio::test]
    async fn clicking_the_log_again_does_not_ask_for_the_mouse_back() {
        let mut app = App::new("http://unused");
        logs_region(&mut app);
        // The app only gets here with reporting already off, so the click
        // the user makes to "copy" is really the terminal's own selection,
        // and any second request would be a toggle fighting the user.
        app.mouse_capture = false;
        app.mouse_toggle = false;

        click(&mut app, 10, 9).await;

        assert!(
            !app.mouse_toggle,
            "a second click on the log must not re-request the release"
        );
    }

    #[tokio::test]
    async fn clicking_a_machine_does_not_take_the_mouse_away() {
        let mut app = App::new("http://unused");
        app.mouse_capture = true;
        app.add_hit_region(
            Rect::new(0, 0, 80, 24),
            HitTarget::Panel {
                panel: Panel::Machines,
                row_start: 0,
                row_y: 0,
            },
        );

        click(&mut app, 4, 3).await;

        assert!(app.mouse_capture, "only the log gives the mouse back");
        assert!(!app.mouse_toggle);
    }

    #[test]
    fn wheel_over_logs_moves_toward_older_events() {
        let mut app = App::new("http://unused");
        // The wheel clamps to the buffer, so the pane needs lines for the
        // scroll to have anywhere to go.
        for i in 0..10 {
            app.log_at(Level::Info, format!("line {i}"));
        }
        scroll(
            &mut app,
            HitTarget::Panel {
                panel: Panel::Events,
                row_start: 0,
                row_y: 0,
            },
            -1,
        );
        assert_eq!(app.events_scroll, 3);
    }

    #[test]
    fn the_wheel_never_scrolls_past_the_top_of_the_log() {
        let mut app = App::new("http://unused");
        app.log_at(Level::Info, "only one line");
        for _ in 0..10 {
            scroll(
                &mut app,
                HitTarget::Panel {
                    panel: Panel::Events,
                    row_start: 0,
                    row_y: 0,
                },
                -1,
            );
        }
        // Ten wheel spins over a one-line log: the title may not claim
        // "30 line(s) back" against a buffer of 1.
        assert_eq!(app.events_scroll, 1);
    }
}
