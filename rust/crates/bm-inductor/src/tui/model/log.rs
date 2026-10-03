use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum LogFilter {
    #[default]
    All,
    Crawl,
    Digest,
    Render,
    Merge,
    Warn,
    Error,
}

impl LogFilter {
    pub(crate) const ALL: [LogFilter; 7] = [
        LogFilter::All,
        LogFilter::Crawl,
        LogFilter::Digest,
        LogFilter::Render,
        LogFilter::Merge,
        LogFilter::Warn,
        LogFilter::Error,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            LogFilter::All => "all",
            LogFilter::Crawl => "crawl",
            LogFilter::Digest => "digest",
            LogFilter::Render => "render",
            LogFilter::Merge => "merge",
            LogFilter::Warn => "warn",
            LogFilter::Error => "error",
        }
    }

    pub(crate) fn step(self, forward: bool) -> LogFilter {
        let n = Self::ALL.len();
        let i = Self::ALL.iter().position(|f| *f == self).unwrap_or(0);
        Self::ALL[if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        }]
    }

    pub(crate) fn matches(self, l: &LogLine) -> bool {
        match self {
            LogFilter::All => true,
            LogFilter::Warn => l.level == Level::Warn,
            LogFilter::Error => l.level == Level::Error,
            stage => l.text.to_lowercase().contains(stage.label()),
        }
    }
}
pub(crate) enum TaskEvent<'a> {
    Done {
        worker: &'a str,
        stage: &'a str,
        task: &'a str,
        secs: &'a str,
    },
    Failed {
        stage: &'a str,
        task: &'a str,
        note: Option<&'a str>,
        reason: &'a str,
    },
    Shelved {
        task: &'a str,
        stage: &'a str,
        reason: &'a str,
    },
}

/// Split `stage:chapter[:take]` off a task word, with the stage validated.
fn task_stage(task: &str) -> Option<&str> {
    let (stage, rest) = task.split_once(':')?;
    if rest.is_empty() || rest.contains(' ') || rest.contains(']') {
        return None;
    }
    Stage::parse(stage).map(|_| stage)
}

fn bracket_head(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix('[')?;
    let end = rest.find("] ")?;
    let head = rest[..end].trim();
    if head.is_empty() {
        return None;
    }
    Some((head, &rest[end + 2..]))
}

/// `[worker] stage:ch done in 14.6s — detail` → [`TaskEvent::Done`], and the
pub(crate) fn task_event(text: &str) -> Option<TaskEvent<'_>> {
    if let Some((worker, rest)) = bracket_head(text) {
        if let Some((task, tail)) = rest.split_once(" done in ") {
            let stage = task_stage(task)?;
            let secs = tail.split_whitespace().next()?;
            if secs.strip_suffix('s').map(|n| n.parse::<f64>().is_ok()) != Some(true) {
                return None;
            }
            return Some(TaskEvent::Done {
                worker,
                stage,
                task,
                secs,
            });
        }
        if let Some((task, tail)) = rest.split_once(" FAILED") {
            let stage = task_stage(task)?;
            let (note, reason) = {
                let after = tail.strip_prefix(' ')?;
                if let Some(n) = after.strip_prefix('(') {
                    let (note, reason) = n.split_once("): ")?;
                    (Some(note), reason)
                } else {
                    (None, after.strip_prefix(": ")?)
                }
            };
            return Some(TaskEvent::Failed {
                stage,
                task,
                note,
                reason,
            });
        }
        return None;
    }
    // No worker head: `{task} SHELVED without retry: {reason} (press u …)`.
    if let Some((task, tail)) = text.split_once(" SHELVED without retry: ") {
        let stage = task_stage(task)?;
        let reason = tail.strip_suffix(" (press u to requeue)")?;
        return Some(TaskEvent::Shelved {
            task,
            stage,
            reason,
        });
    }
    None
}
