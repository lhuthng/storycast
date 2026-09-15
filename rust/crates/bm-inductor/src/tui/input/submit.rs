//! Prompt submission: validate, never silently default.
use bm_proto::{Machine, Op, OpRequest};
use crate::tui::{
    app::App,
    input::runconfig::parse_range,
    jobs::{Job, op_job},
    screen::{TextKind, TextPrompt},
};

/// Validate and dispatch a submitted text prompt.
///
/// Returns `Err(message)` to keep the prompt open with the problem stated,
/// rather than silently substituting a default.
pub(crate) fn submit_text(app: &mut App, prompt: &TextPrompt) -> Result<Job, String> {
    match prompt.kind {
        // `:` commands run through the key handler, never dispatch: reaching
        // here means a bug, and the prompt staying open says so.
        TextKind::Command => Err("commands run from the command line, not submit".into()),
        // Save-only prompt, persisted from the run screen's Enter branch:
        // reaching dispatch would launch without saving, so refuse.
        TextKind::RunConfig => Err("run config is saved from the run screen".into()),
        TextKind::AddMachine => {
            let addr = prompt.buf.trim().to_string();
            if addr.is_empty() {
                return Err("address is empty — enter an IP or hostname".into());
            }
            if addr.contains(char::is_whitespace) {
                return Err(format!("“{addr}” contains whitespace — one address only"));
            }
            let key = std::env::var("SSH_KEY").ok();
            let m = Machine::new(&addr, "thang", 22, key, "worker");
            Ok(Job::AddMachine {
                api: app.api.clone(),
                http: app.http.clone(),
                m,
            })
        }
        TextKind::AddSample => {
            // Pooled sample only: the whole buffer is the path, tags come
            // from the filename. Anything with `as` belongs to N (named) —
            // say so instead of filing it under a nonsense filename.
            if prompt.buf.contains(" as ") {
                return Err("that looks like a named voice — press N and use `path as Name`".into());
            }
            let path = prompt.buf.trim().to_string();
            if path.is_empty() {
                return Err("path is empty — point at a clip, e.g. ~/dl/young-female-4.mp3".into());
            }
            Ok(Job::AddSample { layout_root: app.layout_root.clone(), path, name: None, tags: None })
        }
        TextKind::AddNamed => {
            // `refs/narrator.mp3 as Narrator`: the name is required, the tags
            // stay empty — a named voice answers by hand, never auto-rolls.
            let (path, name) = match prompt.buf.rsplit_once(" as ") {
                Some((p, n)) if !p.trim().is_empty() && !n.trim().is_empty() => {
                    (p.trim().to_string(), n.trim().to_string())
                }
                _ => return Err("named voices need `path as Name` — e.g. refs/narrator.mp3 as Narrator".into()),
            };
            Ok(Job::AddSample {
                layout_root: app.layout_root.clone(),
                path,
                name: Some(name),
                tags: Some(Vec::new()),
            })
        }
        TextKind::Translate => {
            let (start, count) = parse_range(&prompt.buf)?;
            Ok(op_job(
                app,
                &app.http,
                OpRequest {
                    op: Op::Translate,
                    start: Some(start),
                    count: Some(count),
                    ..Default::default()
                },
            ))
        }
        TextKind::CrawlTemplate => {
            let template = prompt.buf.trim().to_string();
            if template.is_empty() {
                return Err("URL template is empty".into());
            }
            if !template.contains("{n}") {
                return Err("template must contain {n} — that is where the chapter number goes".into());
            }
            Ok(op_job(
                app,
                &app.http,
                OpRequest {
                    op: Op::CrawlSetup,
                    url_template: Some(template),
                    start: Some(app.setting_u32("start", 1)),
                    ..Default::default()
                },
            ))
        }
    }
}
