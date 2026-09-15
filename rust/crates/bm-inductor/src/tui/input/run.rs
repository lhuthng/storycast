//! Run screen: preview, launch, edit config.
use crossterm::event::{KeyCode, KeyEvent};
use std::sync::{Arc, atomic::AtomicBool};
use crate::tui::{
    app::App,
    input::{dispatch, runconfig::run_preview},
    jobs::Job,
    screen::{Screen, TextKind, TextPrompt},
    style::{Conn, Level},
};

pub(crate) async fn key_run(app: &mut App, key: KeyEvent, _http: &reqwest::Client, job_tx: &tokio::sync::mpsc::UnboundedSender<Job>) -> bool {
        match key.code {
            KeyCode::Esc => {
                app.screen = Screen::Normal;
            }
            KeyCode::Enter => {
                let cfg = run_preview(app);
                if app.backend_start_outstanding {
                    app.set_status(Level::Warn, "backend start already running — watch events");
                } else {
                    let cancel = Arc::new(AtomicBool::new(false));
                    app.start_cancel = Some(cancel.clone());
                    app.backend_start_outstanding = true;
                    dispatch(
                        app,
                        job_tx,
                        Job::StartBackend {
                            layout_root: app.layout_root.clone(),
                            api: app.api.clone(),
                            api_up: app.conn == Conn::Up,
                            start: cfg.start,
                            count: cfg.count,
                            enqueue: true,
                            machines: app.effective_machines(),
                            cancel,
                        },
                    );
                }
                app.screen = Screen::Normal;
                app.set_status(Level::Info, format!("starting backend, then launching ch{}×{}…", cfg.start, cfg.count));
            }
            KeyCode::Char('e') | KeyCode::Char('E') => {
                let cfg = run_preview(app);
                let models = cfg.models.join(",");
                let prefill = if models.is_empty() {
                    format!("{} {} {}", cfg.start, cfg.count, cfg.analyzer)
                } else {
                    format!("{} {} {} {}", cfg.start, cfg.count, cfg.analyzer, models)
                };
                app.screen = Screen::Text(TextPrompt::new(
                    TextKind::RunConfig,
                    "Run config",
                    "range as <start> <count> [analyzer] [models,comma,separated]. Saved to settings.",
                    &prefill,
                ));
            }
            _ => {}
        }
        false
}
