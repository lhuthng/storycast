//! Prompt submission: validate, never silently default.
use crate::tui::{
    app::App,
    input::runconfig::parse_range,
    jobs::{op_job, Job, ProfileReq, WorkspaceReq},
    screen::{TextKind, TextPrompt},
};
use bm_proto::{Machine, Op, OpRequest};

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
        // Same for the ssh defaults: text.rs saves them on Enter, so
        // reaching dispatch means a bug, and the prompt staying open says so.
        TextKind::SshKey | TextKind::SshUser | TextKind::SshPort => {
            Err("ssh defaults save from the prompt, not submit".into())
        }
        TextKind::Mix => Err("mix saves from the prompt, not submit".into()),
        // The sound-design prompts write a registry and launch nothing, so
        // reaching dispatch means a bug — same as the two above.
        TextKind::SoundAdd(_) | TextKind::SoundEdit(..) | TextKind::SoundLevel(..) => {
            Err("sound pools save from the prompt, not submit".into())
        }
        TextKind::AddMachine => {
            // Bind tuple: `addr [user [port [key...]]]` — the key is the
            // remainder of the line so paths with spaces survive. Missing
            // fields fall back to the app-wide ssh defaults; no key at all
            // means ssh decides (agent / ~/.ssh/config).
            let buf = prompt.buf.trim();
            let toks: Vec<&str> = buf.split_whitespace().collect();
            let Some(addr) = toks.first().map(|s| s.to_string()) else {
                return Err("address is empty — enter an IP or hostname".into());
            };
            let def = app.ssh_defaults();
            let user = toks.get(1).map(|s| s.to_string()).unwrap_or(def.user);
            let port: u16 = match toks.get(2) {
                None => def.port,
                Some(p) => p
                    .parse()
                    .map_err(|_| format!("port “{p}” is not a number"))?,
            };
            // Byte offset of the fourth field: skip three fields and the gaps
            // between them. Internal spacing of the key is preserved.
            let key = if toks.len() > 3 {
                let mut idx = 0;
                for _ in 0..3 {
                    idx += buf[idx..]
                        .split_whitespace()
                        .next()
                        .map(|t| t.len())
                        .unwrap_or(0);
                    idx += buf[idx..]
                        .chars()
                        .take_while(|c| c.is_whitespace())
                        .map(|c| c.len_utf8())
                        .sum::<usize>();
                }
                let typed = buf[idx..].trim().to_string();
                let expanded = bm_core::util::expand_tilde(&typed);
                if !expanded.is_file() {
                    return Err(format!(
                        "no such key: {} — check the path, or clear it to let ssh decide",
                        expanded.display()
                    ));
                }
                Some(typed)
            } else {
                def.key
            };
            let m = Machine::new(&addr, &user, port, key, "worker");
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
                return Err(
                    "that looks like a named voice — press N and use `path as Name`".into(),
                );
            }
            let path = prompt.buf.trim().to_string();
            if path.is_empty() {
                return Err("path is empty — point at a clip, e.g. ~/dl/young-female-4.mp3".into());
            }
            Ok(Job::AddSample {
                layout: app.layout.clone(),
                path,
                name: None,
                tags: None,
            })
        }
        TextKind::AddNamed => {
            // `refs/narrator.mp3 as Narrator`: the name is required, the tags
            // stay empty — a named voice answers by hand, never auto-rolls.
            let (path, name) = match prompt.buf.rsplit_once(" as ") {
                Some((p, n)) if !p.trim().is_empty() && !n.trim().is_empty() => {
                    (p.trim().to_string(), n.trim().to_string())
                }
                _ => {
                    return Err(
                        "named voices need `path as Name` — e.g. refs/narrator.mp3 as Narrator"
                            .into(),
                    )
                }
            };
            Ok(Job::AddSample {
                layout: app.layout.clone(),
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
                return Err(
                    "template must contain {n} — that is where the chapter number goes".into(),
                );
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
        TextKind::Workspace => {
            // `new <name>` creates and switches; a bare name switches; empty
            // lists. Parsed here so a typo is refused with the prompt still
            // open, rather than queued as a job that fails a second later.
            let buf = prompt.buf.trim();
            let req = if buf.is_empty() {
                WorkspaceReq::List
            } else if buf == "new" {
                return Err("`new` needs a name — `new <name>`".into());
            } else if let Some(name) = buf.strip_prefix("new ") {
                WorkspaceReq::New(workspace_name(name)?)
            } else {
                WorkspaceReq::Use(workspace_name(buf)?)
            };
            Ok(Job::Workspace {
                layout: app.layout.clone(),
                api: app.api.clone(),
                req,
            })
        }
        TextKind::Profile => {
            // Same grammar: `pack <name>` bundles the live tree, a bare name
            // loads it, empty lists.
            let buf = prompt.buf.trim();
            let req = if buf.is_empty() {
                ProfileReq::List
            } else if buf == "pack" {
                return Err("`pack` needs a name — `pack <name>`".into());
            } else if let Some(name) = buf.strip_prefix("pack ") {
                ProfileReq::Pack(workspace_name(name)?)
            } else {
                ProfileReq::Load(workspace_name(buf)?)
            };
            Ok(Job::Profile {
                layout: app.layout.clone(),
                api: app.api.clone(),
                req,
            })
        }
    }
}

/// A workspace or profile name: one path segment, no escapes.
///
/// `workspace_cmd` enforces the same rule, but by then the job is queued and
/// the prompt is closed — the operator would have to retype it. Refusing here
/// keeps the prompt open with the name still on screen.
fn workspace_name(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return Err(format!(
            "“{name}” is not a name — one path segment, no slashes"
        ));
    }
    Ok(name.to_string())
}
