//! InstaBox's own words: the machine record, the workspace's specs and caps,
//! the create body, published ports, and its error catalog. Pure — the HTTP
//! client is the inductor's half.

use crate::cloud::{Liveness, ProviderView, Reason, Shortfall};
use bm_proto::now_secs;
use serde_json::{json, Value};

/// The cloud-side delete the API accepts, in seconds: 60 is its floor and one
/// day its ceiling.
pub const MIN_DELETE_AFTER_SECS: u64 = 60;
pub const MAX_DELETE_AFTER_SECS: u64 = 86_400;
/// The longest one `exec` may run before it is killed; anything longer has to
/// be started detached and polled.
pub const MAX_EXEC_SECS: u64 = 600;
/// Where a machine's user starts: every current template says so, and the
/// files API wants an absolute path.
pub const HOME: &str = "/data/home";

/// One size a machine can be, as both `allowedSpecs` and a machine's `spec`
/// report it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spec {
    pub vcpus: u32,
    pub mem_mib: u32,
    pub disk_gib: u32,
}

impl Spec {
    /// `4/8`: vCPUs and their memory in GiB, which is how an operator says it.
    pub fn label(self) -> String {
        format!("{}/{}", self.vcpus, self.mem_mib / 1024)
    }

    /// The same form back. A size that does not parse is no size at all, so it
    /// is refused against the workspace's own list rather than guessed at.
    pub fn parse(raw: &str) -> Option<Spec> {
        let (vcpus, mem) = raw.trim().split_once('/')?;
        let vcpus: u32 = vcpus.trim().parse().ok()?;
        let mem: u32 = mem.trim().parse().ok()?;
        // The label is in GiB; a MiB figure is what the API answers with.
        let mem_mib = mem.checked_mul(1024)?;
        (vcpus > 0 && mem_mib > 0).then_some(Spec {
            vcpus,
            mem_mib,
            disk_gib: 0,
        })
    }

    fn from_json(v: &Value) -> Option<Spec> {
        let n = |k: &str| v.get(k).and_then(|x| x.as_u64()).map(|x| x as u32);
        Some(Spec {
            vcpus: n("vcpus")?,
            mem_mib: n("memMiB")?,
            disk_gib: n("persistentDiskGiB").unwrap_or_default(),
        })
    }
}

/// One machine, as `GET /v1/machines` (and `/{id}`) describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Box {
    pub id: String,
    pub name: String,
    /// The provider's own word, kept verbatim.
    pub state: String,
    pub spec: Spec,
    /// Unix seconds the API will delete it at, when a deadline was set.
    pub expires_at: Option<u64>,
    /// `runtime.ready`: it runs and passed its own checks.
    pub ready: bool,
    /// Why an `error` box errored.
    pub detail: Option<String>,
}

impl Box {
    /// The shared verdict, with the provider's word kept for the operator.
    pub fn view(&self) -> ProviderView {
        let state = match self.state.as_str() {
            "creating" | "starting" | "running" | "resuming" => Liveness::Running,
            "stopping" | "stopped" | "suspending" | "suspended" => Liveness::Stopped,
            "error" => Liveness::Dead,
            "deleting" => Liveness::Gone,
            // A word this build has not met is never read as dead: a delete
            // taken on an unknown state is worse than one taken a minute late.
            _ => Liveness::Unknown,
        };
        let mut view = ProviderView::new(self.id.clone(), state, self.state.clone());
        if let Some(detail) = &self.detail {
            view = view.saying(detail.clone());
        }
        view
    }

    /// The registry row's note: the id, the size and the provider's word.
    pub fn note(&self) -> String {
        let mut note = format!(
            "InstaBox {} ({} {})",
            self.id,
            self.state,
            self.spec.label()
        );
        if let Some(detail) = &self.detail {
            note.push_str(&format!(" · {detail}"));
        }
        note
    }

    /// Running *and* through its checks: nothing can be done to it before.
    pub fn usable(&self) -> bool {
        self.ready && matches!(self.state.as_str(), "running")
    }
}

/// Read a machine listing or a single machine object.
pub fn parse_machines(json: &str) -> Option<Vec<Box>> {
    let doc: Value = serde_json::from_str(json).ok()?;
    // `GET /v1/machines` pages (`{machines, nextCursor}`); `GET /{id}` is one
    // object, and both must read here.
    let list: Vec<Value> = match doc.get("machines").and_then(|m| m.as_array()) {
        Some(m) => m.clone(),
        None if doc.is_array() => doc.as_array().cloned().unwrap_or_default(),
        None => vec![doc],
    };
    let mut out: Vec<Box> = list.iter().filter_map(parse_machine).collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Some(out)
}

/// One machine, from either a listing entry or `GET /v1/machines/{id}`.
pub fn parse_machine(v: &Value) -> Option<Box> {
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    let id = s("id").or_else(|| s("machineId"))?;
    let state = s("state").unwrap_or_else(|| "unknown".into());
    let detail = v
        .get("error")
        .and_then(|e| e.get("message").and_then(|m| m.as_str()).or(e.as_str()))
        .map(str::to_string)
        .or_else(|| s("pauseReason"))
        .or_else(|| s("reason"))
        .filter(|d| !d.trim().is_empty());
    Some(Box {
        name: s("name").unwrap_or_else(|| id.clone()),
        spec: v.get("spec").and_then(Spec::from_json).unwrap_or(Spec {
            vcpus: v.get("vcpus").and_then(|c| c.as_u64()).unwrap_or_default() as u32,
            mem_mib: 0,
            disk_gib: 0,
        }),
        ready: v
            .get("runtime")
            .and_then(|r| r.get("ready"))
            .and_then(|r| r.as_bool())
            .unwrap_or(false),
        // `deleteAfterSeconds` is what a create was given; `deleteAt` is what
        // the machine itself reports back, as RFC 3339.
        expires_at: s("deleteAt")
            .or_else(|| s("archiveAt"))
            .and_then(|t| rfc3339_secs(&t))
            .or_else(|| {
                v.get("deleteAfterSeconds")
                    .and_then(|d| d.as_u64())
                    .filter(|d| *d > 0)
                    .map(|d| now_secs() + d)
            }),
        state,
        id,
        detail,
    })
}

/// A published port: the only way into a machine, which has no address of its
/// own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Port {
    pub port: u16,
    pub url: String,
}

/// `GET /v1/machines/{id}/ports` answers `{enabled, ports: [...]}`.
pub fn parse_ports(json: &str) -> Vec<Port> {
    let Ok(doc) = serde_json::from_str::<Value>(json) else {
        return Vec::new();
    };
    let list = doc
        .get("ports")
        .and_then(|p| p.as_array())
        .or_else(|| doc.as_array())
        .cloned()
        .unwrap_or_default();
    list.iter()
        .filter_map(|p| {
            let port = p.get("port").and_then(|n| n.as_u64())? as u16;
            let url = p
                .get("url")
                .and_then(|u| u.as_str())
                .map(str::trim)
                .filter(|u| !u.is_empty())?;
            let url = url.trim_end_matches('/');
            Some(Port {
                port,
                url: if url.starts_with("http") {
                    url.to_string()
                } else {
                    format!("https://{url}")
                },
            })
        })
        .collect()
}

/// The URL of the published worker port, if one was published.
pub fn worker_url(ports: &[Port]) -> Option<String> {
    ports
        .iter()
        .find(|p| p.port == bm_proto::DEFAULT_TASK_PORT)
        .map(|p| p.url.clone())
}

/// `MM-DDTHH:MM:SS[.fff]Z` → unix seconds. The API sends RFC 3339 in UTC and
/// always with a `Z`, which is all this reads.
fn rfc3339_secs(text: &str) -> Option<u64> {
    let s = text.trim().trim_end_matches('Z');
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-');
    let (y, m, day): (i64, i64, i64) = (
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
    );
    let time = time.split('.').next()?;
    let mut t = time.split(':');
    let (hh, mm, ss): (i64, i64, i64) = (
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
    );
    // Days from civil, Howard Hinnant's algorithm: no calendar crate for one
    // field.
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hh * 3600 + mm * 60 + ss;
    u64::try_from(secs).ok()
}

/// One workspace, from `GET /v1/orgs`: `{"orgs": [...]}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Org {
    pub name: String,
    pub tier: String,
    /// Every size this workspace may create, with its own numbers.
    pub allowed_specs: Vec<Spec>,
}

impl Org {
    pub fn parse(json: &str) -> Option<Self> {
        let doc: Value = serde_json::from_str(json).ok()?;
        let org = match doc.get("orgs").and_then(|o| o.as_array()) {
            Some(list) => list.first()?.clone(),
            None => doc.get("org").cloned().unwrap_or(doc),
        };
        let specs = org
            .get("allowedSpecs")
            .and_then(|s| s.as_array())
            .map(|list| list.iter().filter_map(Spec::from_json).collect())
            .unwrap_or_default();
        Some(Org {
            name: org
                .get("name")
                .and_then(|t| t.as_str())
                .unwrap_or("unknown")
                .to_string(),
            tier: org
                .get("tier")
                .or_else(|| org.get("plan"))
                .and_then(|t| t.as_str())
                .unwrap_or("unknown")
                .to_string(),
            allowed_specs: specs,
        })
    }

    /// The smallest size this workspace allows, which is what an unset `spec`
    /// asks for.
    pub fn smallest(&self) -> Option<Spec> {
        self.allowed_specs
            .iter()
            .min_by_key(|s| (s.vcpus, s.mem_mib))
            .copied()
    }

    /// The size a configured `spec` names, or why it names none.
    pub fn resolve(&self, raw: &str) -> Result<Option<Spec>, String> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Ok(self.smallest());
        }
        let want = Spec::parse(raw).ok_or_else(|| {
            format!("`{raw}` is not a size: write it `<vcpus>/<mem GiB>`, e.g. 4/8")
        })?;
        if self.allowed_specs.is_empty() {
            return Ok(Some(want));
        }
        self.allowed_specs
            .iter()
            .find(|s| s.vcpus == want.vcpus && s.mem_mib == want.mem_mib)
            .copied()
            .map(Some)
            .ok_or_else(|| {
                format!(
                    "{} is not a size this workspace allows — it has {}",
                    want.label(),
                    self.labels()
                )
            })
    }

    pub fn labels(&self) -> String {
        if self.allowed_specs.is_empty() {
            return "(none listed)".to_string();
        }
        self.allowed_specs
            .iter()
            .map(|s| s.label())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// What this workspace is, said once, before anything is created.
    pub fn notes(&self, spec: Option<Spec>) -> Vec<String> {
        vec![format!(
            "workspace {} · tier {} · sizes: {}",
            self.name,
            self.tier,
            self.labels()
        )]
        .into_iter()
        .chain(spec.map(|s| {
            format!(
                "creating {}{}",
                s.label(),
                if s.disk_gib > 0 {
                    format!(" with a {} GiB /data", s.disk_gib)
                } else {
                    String::new()
                }
            )
        }))
        .collect()
    }
}

/// The account's caps, from `GET /v1/usage`: the machine count is the
/// provider's, never a literal in our code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_machines: u32,
    pub max_snapshots: u32,
    pub stored_gib: u32,
}

impl Limits {
    pub fn parse(json: &str) -> Option<Self> {
        let doc: Value = serde_json::from_str(json).ok()?;
        let limits = doc.get("limits").unwrap_or(&doc);
        let n = |k: &str| limits.get(k).and_then(|v| v.as_u64()).map(|v| v as u32);
        Some(Limits {
            max_machines: n("maxMachines")?,
            max_snapshots: n("maxSnapshots").unwrap_or_default(),
            stored_gib: n("maxStoredGiB").unwrap_or_default(),
        })
    }

    /// The caps that decide a plan, said as the numbers they are.
    pub fn notes(&self) -> Vec<String> {
        vec![format!(
            "at most {} machine(s), {} snapshot(s), {} GiB stored",
            self.max_machines, self.max_snapshots, self.stored_gib
        )]
    }
}

/// The deadline a create carries, and why it may carry none.
///
/// InstaBox deletes on its own only between a minute and a day. Beyond that the
/// value is left off and the caller is told, rather than silently clamped to a
/// deadline the operator did not ask for.
pub fn delete_after_seconds(minutes: Option<u32>) -> (Option<u64>, Option<String>) {
    let Some(minutes) = minutes else {
        return (None, None);
    };
    if minutes == 0 {
        return (None, None);
    }
    let secs = u64::from(minutes) * 60;
    if secs > MAX_DELETE_AFTER_SECS {
        return (
            None,
            Some(format!(
                "an InstaBox deadline tops out at {}h, so this box has no cloud-side delete — \
                 the inductor's watchdog still shuts it down",
                MAX_DELETE_AFTER_SECS / 3600
            )),
        );
    }
    (Some(secs.max(MIN_DELETE_AFTER_SECS)), None)
}

/// The `POST /v1/machines` body. The pointer to the port is not one of its
/// fields — a port is published after the machine runs, by its own call.
pub fn create_body(
    spec: Option<Spec>,
    template: &str,
    name: &str,
    delete_after: Option<u64>,
) -> Value {
    let mut body = json!({
        "name": name,
        "templateId": template,
    });
    if let Some(spec) = spec {
        body["vcpus"] = json!(spec.vcpus);
        body["memMiB"] = json!(spec.mem_mib);
    }
    if let Some(secs) = delete_after {
        body["deleteAfterSeconds"] = json!(secs);
    }
    body
}

/// The machine a create answered with: the body carries the operation and the
/// id it is about, not the machine itself.
pub fn created_machine_id(json: &str) -> Option<String> {
    let doc: Value = serde_json::from_str(json).ok()?;
    doc.get("machineId")
        .or_else(|| doc.get("id"))
        .and_then(|m| m.as_str())
        .map(str::to_string)
}

/// The `POST /v1/machines/{id}/ports` body.
pub fn port_body(port: u16, name: &str) -> Value {
    json!({ "port": port, "name": name })
}

/// InstaBox's error body → a shortfall in our vocabulary, with a fix built from
/// the facts we already read.
///
/// A code this build has not met is passed through with the provider's own
/// words and docs, and **no invented advice**.
pub fn shortfall(
    status: u16,
    body: &str,
    org: Option<&Org>,
    limits: Option<&Limits>,
    want: u32,
) -> Shortfall {
    let doc: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let nested = doc.get("error");
    let field = |k: &str| {
        doc.get(k)
            .or_else(|| nested.and_then(|e| e.get(k)))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let code = field("code").unwrap_or_else(|| match status {
        400 => "bad_request".into(),
        401 => "invalid_key".into(),
        402 => "payment_required".into(),
        403 => "permission_denied".into(),
        404 => "not_found".into(),
        409 => "operation_conflict".into(),
        429 => "quota_exceeded".into(),
        503 => "no_capacity".into(),
        s if s >= 500 => "provider_error".into(),
        _ => "http_error".into(),
    });
    // `error.message` is the provider's words; a bare body is still its answer.
    let message = field("message")
        .or_else(|| {
            doc.get("message")
                .and_then(|m| m.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.trim().chars().take(200).collect());
    let docs = field("docs_url").or_else(|| field("docs"));
    let retry_after = doc
        .get("retryAfterSeconds")
        .or_else(|| nested.and_then(|e| e.get("retryAfterSeconds")))
        .and_then(|v| v.as_u64());
    let retryable = doc
        .get("retryable")
        .or_else(|| nested.and_then(|e| e.get("retryable")))
        .and_then(|v| v.as_bool())
        .unwrap_or(matches!(status, 429 | 503) || status >= 500);

    let mut s = match code.as_str() {
        "missing_key" | "malformed_key" | "invalid_key" | "expired_key" | "revoked_key" => {
            Shortfall::new(Reason::Stop, code, message).with_fix(
                "`ibx login` (`:ibx`) stores a fresh key from the workspace's settings page",
            )
        }
        "permission_denied" | "missing_scope" => Shortfall::new(Reason::Stop, code, message)
            .with_fix("the key lacks a scope this route needs; the message above names it"),
        "payment_required" | "no_credit" | "credit_exhausted" => {
            Shortfall::new(Reason::Stop, code, message)
                .with_fix("the workspace's billing page on instabox.sh")
        }
        "spec_not_allowed" => {
            let mut s = Shortfall::new(Reason::Clamp, code, message.clone());
            if let Some(org) = org {
                s = s.with_fix(format!(
                    "this workspace allows: {}; set `spec` in the InstaBox definition",
                    org.labels()
                ));
            }
            s.covering(want)
        }
        "template_not_allowed" => Shortfall::new(Reason::Stop, code, message).with_fix(
            "the free plan creates from `agent-browser`, `android` or `ci`, or from a snapshot",
        ),
        "quota_exceeded" => {
            let mut s = Shortfall::new(Reason::Clamp, code, message).covering(want);
            if let Some(limits) = limits {
                s = s.with_fix(format!(
                    "this workspace allows {} machine(s) — delete one (`:down`), or raise the plan",
                    limits.max_machines
                ));
            }
            if retryable {
                s = s.retry_in(retry_after);
            }
            s
        }
        "no_capacity" | "host_unavailable" => {
            Shortfall::new(Reason::Capacity, code, message).retry_in(retry_after)
        }
        "rate_limited" | "too_many_requests" | "machine_cooldown" => {
            Shortfall::new(Reason::Capacity, code, message).retry_in(retry_after)
        }
        // Ours to fix, not the operator's: say what was refused and stop.
        "unknown_field"
        | "bad_request"
        | "idempotency_conflict"
        | "delete_confirmation_required"
        | "delete_not_confirmed" => Shortfall::new(Reason::Stop, code, message),
        "machine_not_running" => Shortfall::new(Reason::Capacity, code, message)
            .with_fix("a stopped box is started first; a paused one is woken by the same call"),
        "operation_conflict" => Shortfall::new(Reason::Capacity, code, message)
            .retry_in(retry_after)
            .with_fix("a machine runs one operation at a time — wait for the one in flight"),
        "not_found" => Shortfall::new(Reason::Stop, code, message),
        s if status >= 500 => Shortfall::new(Reason::Capacity, s, message).retry_in(retry_after),
        _ => Shortfall::new(Reason::Unknown, code, message),
    };
    if let Some(docs) = docs {
        s = s.with_docs(docs);
    }
    if retryable && !s.retryable {
        s = s.retry_in(retry_after);
    }
    s
}

#[cfg(test)]
mod tests;
