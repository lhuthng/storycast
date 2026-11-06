//! InstaBox as one `Cloud`: the workspace's numbers, one create per box, the
//! port that makes it dialable, and a delete the provider itself also makes at
//! the deadline.
//!
//! Every JSON shape lives in `bm_core::cloud::instabox`; this file is the wire.

use super::*;
use bm_core::cloud::instabox::{self, Limits, Org, Spec};
use bm_core::cloud::{CloudBox, Launch, Liveness, Plan, ProviderView, Reason, Shortfall, Smoke};
use bm_proto::{now_secs, MachineState};
use serde_json::Value;
use std::path::{Path, PathBuf};

mod config;

pub(crate) use config::InstaboxConfig;

/// The facts a refusal's fix is built from: what the workspace is, and what it
/// may spend.
#[derive(Clone, Copy, Default)]
struct Facts<'a> {
    org: Option<&'a Org>,
    limits: Option<&'a Limits>,
}

impl<'a> Facts<'a> {
    fn with(org: &'a Org, limits: Option<&'a Limits>) -> Facts<'a> {
        Facts {
            org: Some(org),
            limits,
        }
    }
}

pub(crate) struct Instabox {
    root: PathBuf,
    /// `None` is the provider's own default; `Some(0)` is no deadline at all.
    ttl_minutes: Option<u32>,
}

impl Instabox {
    pub(crate) fn new(root: &Path, ttl_minutes: Option<u32>) -> Self {
        Instabox {
            root: root.to_path_buf(),
            ttl_minutes,
        }
    }

    fn cfg(&self) -> Result<InstaboxConfig, Shortfall> {
        let cfg = InstaboxConfig::load(&self.root);
        let missing = cfg.missing();
        if missing.is_empty() && !cfg.key().is_empty() {
            return Ok(cfg);
        }
        let mut missing = missing;
        if cfg.key().is_empty() && !missing.iter().any(|m| m.contains("api_key")) {
            missing
                .push("no api_key in the file and no INSTABOX_API_KEY in the environment".into());
        }
        Err(
            Shortfall::new(Reason::Stop, "pool_incomplete", missing.join("; ")).with_fix(format!(
                "run `ibx login` (`:ibx` in the dashboard) to store the key in {}, or export \
             INSTABOX_API_KEY; the workspace's settings page on instabox.sh has one",
                InstaboxConfig::path(&self.root).display()
            )),
        )
    }

    /// One request. The status comes back with the body: a provider's refusal is
    /// data, and its own words are what the operator needs.
    fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        facts: Facts<'_>,
        confirm: Option<&str>,
    ) -> Result<(u16, String), Shortfall> {
        let cfg = self.cfg()?;
        let url = format!("{}{}", cfg.api_url.trim_end_matches('/'), path);
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .map_err(|e| {
                Shortfall::new(
                    Reason::Stop,
                    "http_client",
                    format!("could not build a client: {e}"),
                )
            })?;
        let mut req = client
            .request(method.parse().unwrap_or(reqwest::Method::GET), &url)
            .bearer_auth(cfg.key())
            .header("accept", "application/json");
        // A lifecycle call must carry one; a retry with the same key must not
        // be applied twice, which is exactly the accident a lost answer makes.
        if matches!(method, "POST" | "PATCH" | "DELETE") {
            req = req.header("Idempotency-Key", idempotency_key());
        }
        if let Some(confirm) = confirm {
            // The API refuses a delete that does not name what it deletes.
            req = req.header("X-Confirm-Delete", confirm);
        }
        if let Some(body) = body {
            req = req.json(&body);
        }
        let sent = req.send().map_err(|e| {
            Shortfall::new(
                Reason::Capacity,
                "unreachable",
                format!(
                    "{} answered nothing: {e}",
                    cfg.api_url.trim_end_matches('/')
                ),
            )
            .retry_in(None)
            .with_fix("check the network, or `api_url` in the InstaBox definition")
        })?;
        let status = sent.status().as_u16();
        let text = sent.text().unwrap_or_default();
        if (200..300).contains(&status) {
            return Ok((status, text));
        }
        Err(instabox::shortfall(
            status,
            &text,
            facts.org,
            facts.limits,
            1,
        ))
    }

    fn get(&self, path: &str, facts: Facts<'_>) -> Result<String, Shortfall> {
        Ok(self.call("GET", path, None, facts, None)?.1)
    }

    fn org(&self) -> Result<Org, Shortfall> {
        let body = self.get("/v1/orgs", Facts::default())?;
        Org::parse(&body).ok_or_else(|| {
            Shortfall::new(
                Reason::Unknown,
                "unparsable_org",
                "the workspace answer is a shape this does not understand — not planning on a guess",
            )
        })
    }

    fn limits(&self) -> Result<Limits, Shortfall> {
        let body = self.get("/v1/usage", Facts::default())?;
        Limits::parse(&body).ok_or_else(|| {
            Shortfall::new(
                Reason::Unknown,
                "unparsable_usage",
                "the usage answer carries no machine cap this understands",
            )
        })
    }

    fn machines(&self) -> Result<Vec<instabox::Box>, Shortfall> {
        let body = self.get("/v1/machines", Facts::default())?;
        instabox::parse_machines(&body).ok_or_else(|| {
            Shortfall::new(
                Reason::Unknown,
                "unparsable_listing",
                "the machine list is a shape this does not understand",
            )
        })
    }

    /// The size to ask for, or why this workspace has none.
    fn spec(&self, cfg: &InstaboxConfig, org: &Org, want: u32) -> Result<Spec, Shortfall> {
        let spec = org
            .resolve(&cfg.spec)
            .map_err(|why| Shortfall::new(Reason::Stop, "bad_spec", why).covering(want))?
            .ok_or_else(|| {
                Shortfall::new(
                    Reason::Stop,
                    "no_spec",
                    "the workspace lists no size, so there is nothing to create",
                )
                .covering(want)
            })?;
        // A box under the sidecar's footprint would be paid for and never
        // render: the same size the agent measures is refused here instead.
        if spec.mem_mib < bm_core::cloud::MIN_BOX_MIB {
            return Err(
                bm_core::cloud::too_small("box_too_small", spec.mem_mib, fitting_sizes(org))
                    .covering(want),
            );
        }
        Ok(spec)
    }

    fn deadline(&self, cfg: &InstaboxConfig) -> (Option<u64>, Option<String>) {
        let minutes = match self.ttl_minutes {
            Some(0) => None,
            Some(n) => Some(n),
            None => (cfg.ttl_hours > 0).then_some(cfg.ttl_hours * 60),
        };
        instabox::delete_after_seconds(minutes)
    }
}

impl Instabox {
    /// Give a fresh box a bounded wait for it to run: a create answers before
    /// anything can be done to the machine.
    fn wait_ready(&self, id: &str) -> Option<instabox::Box> {
        let deadline = now_secs() + 180;
        loop {
            if let Ok(list) = self.machines() {
                if let Some(m) = list.into_iter().find(|b| b.id == id) {
                    let fate = m.view().state;
                    if m.usable() || matches!(fate, Liveness::Dead | Liveness::Gone) {
                        return Some(m);
                    }
                }
            }
            if now_secs() >= deadline {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
    }

    /// Publish the worker port and answer its URL.
    fn publish(&self, id: &str, facts: Facts<'_>) -> Option<String> {
        let body = instabox::port_body(bm_proto::DEFAULT_TASK_PORT, "bm-worker");
        let path = format!("/v1/machines/{id}/ports");
        self.call("POST", &path, Some(body), facts, None).ok()?;
        self.get(&path, facts)
            .ok()
            .and_then(|body| instabox::worker_url(&instabox::parse_ports(&body)))
    }
}

impl Cloud for Instabox {
    fn provider(&self) -> Provider {
        Provider::InstaBox
    }

    fn smoke(&self) -> Result<Smoke, Shortfall> {
        let cfg = self.cfg()?;
        // The workspace first: a dead key is named before anything else is read.
        let org = self.org()?;
        let limits = self.limits();
        let machines = self.machines()?;
        let live = machines
            .iter()
            .filter(|m| m.view().state != Liveness::Gone)
            .count();
        let mut lines = vec![format!(
            "key accepted · workspace {} ({}) · {} box(es) live",
            org.name, org.tier, live
        )];
        lines.push(cfg.summary());
        match &limits {
            Ok(limits) => {
                // The cap is the account's, not the tier's name.
                if live >= limits.max_machines as usize {
                    lines.push(format!(
                        "no room: {} of {} machine(s) live — delete one (`:down`)",
                        live, limits.max_machines
                    ));
                }
                lines.extend(limits.notes());
            }
            Err(s) => lines.push(format!("usage unread: {}", s.line())),
        }
        match self.spec(&cfg, &org, 1) {
            Ok(spec) => lines.extend(org.notes(Some(spec))),
            // Said here so the facts above still print; `plan` is what refuses.
            Err(s) => lines.push(s.line()),
        }
        lines.push(format!(
            "creating from template `{}`, then publishing port {}",
            cfg.template(),
            bm_proto::DEFAULT_TASK_PORT
        ));
        let (_, said) = self.deadline(&cfg);
        if let Some(said) = said {
            lines.push(said);
        }
        Ok(Smoke {
            provider: Provider::InstaBox,
            lines,
        })
    }

    fn plan(&self, want: u32) -> Result<Plan, Shortfall> {
        let cfg = self.cfg()?;
        let org = self.org()?;
        let limits = self.limits();
        let machines = self.machines()?;
        let live = machines
            .iter()
            .filter(|m| m.view().state != Liveness::Gone)
            .count() as u32;
        let spec = self.spec(&cfg, &org, want)?;

        let mut notes = org.notes(Some(spec));
        let mut shortfalls = Vec::new();
        if let Ok(limits) = &limits {
            let (fit, bound) = fit(want, cfg.max_workers, limits.max_machines, live);
            if let Some(code) = bound {
                let refused = want - fit;
                let why = if code == "max_workers" {
                    format!(
                        "this workspace allows {} InstaBox machine(s) and {live} are live",
                        cfg.max_workers
                    )
                } else {
                    format!(
                        "the {} plan allows {} machine(s) and {live} are live",
                        org.tier, limits.max_machines
                    )
                };
                shortfalls.push(
                    Shortfall::new(Reason::Clamp, code, why)
                        .with_fix("delete a box (`:down`), or raise the plan / max_workers")
                        .covering(refused),
                );
            }
            notes.extend(limits.notes());
            return Ok(Plan {
                want,
                fit,
                live,
                notes,
                shortfalls,
            });
        }
        // Without a cap there is no plan: one is not invented from a tier name.
        shortfalls.push(
            limits
                .err()
                .unwrap_or_else(|| {
                    Shortfall::new(Reason::Unknown, "no_limits", "no machine cap was read")
                })
                .covering(want),
        );
        Ok(Plan {
            want,
            fit: 0,
            live,
            notes,
            shortfalls,
        })
    }

    fn up(&self, plan: &Plan) -> Launch {
        let mut launch = Launch {
            created: Vec::new(),
            skipped: plan.shortfalls.clone(),
        };
        if plan.fit == 0 {
            return launch;
        }
        let cfg = match self.cfg() {
            Ok(c) => c,
            Err(e) => {
                launch.skipped.push(e);
                return launch;
            }
        };
        let org = match self.org() {
            Ok(o) => o,
            Err(e) => {
                launch.skipped.push(e);
                return launch;
            }
        };
        let limits = self.limits().ok();
        let facts = Facts::with(&org, limits.as_ref());
        let spec = match self.spec(&cfg, &org, plan.fit) {
            Ok(s) => s,
            Err(s) => {
                launch.skipped.push(s);
                return launch;
            }
        };
        let (delete_after, _) = self.deadline(&cfg);
        let expires_at = delete_after.map(|s| now_secs() + s);

        // One create per box, for the same reason AWS does one call per box: a
        // refusal at the second must leave the first standing and named.
        for i in 0..plan.fit {
            let name = format!("bm-{}-{i}", bm_core::util::head_chars(cfg.template(), 8));
            let body = instabox::create_body(Some(spec), cfg.template(), &name, delete_after);
            let (_, answer) = match self.call("POST", "/v1/machines", Some(body), facts, None) {
                Ok(ok) => ok,
                Err(mut s) => {
                    s.want = plan.fit - i;
                    launch.skipped.push(s);
                    break;
                }
            };
            let Some(id) = instabox::created_machine_id(&answer) else {
                launch.skipped.push(
                    Shortfall::new(
                        Reason::Unknown,
                        "unparsable_launch",
                        "the create was accepted but named no machine",
                    )
                    .covering(plan.fit - i),
                );
                break;
            };
            // A box that never runs is still registered by id, and its own
            // deadline still ends it.
            let became = self.wait_ready(&id);
            let url = if cfg.publish_worker_port && became.as_ref().is_some_and(|m| m.usable()) {
                self.publish(&id, facts)
            } else {
                None
            };
            let at = expires_at.or_else(|| became.as_ref().and_then(|m| m.expires_at));
            launch
                .created
                .push(box_of(&id, became.as_ref(), url, at, spec, plan.live + i + 1));
        }
        launch
    }

    fn list(&self, ids: &[String]) -> Result<Vec<ProviderView>, Shortfall> {
        let machines = self.machines()?;
        Ok(machines
            .iter()
            .filter(|m| ids.is_empty() || ids.iter().any(|id| id == &m.id))
            .map(instabox::Box::view)
            .collect())
    }

    fn down(&self, ids: &[String]) -> Result<Vec<String>, Shortfall> {
        let mut out = Vec::new();
        for id in ids {
            match self.call(
                "DELETE",
                &format!("/v1/machines/{id}"),
                None,
                Facts::default(),
                Some(id),
            ) {
                Ok(_) => out.push(format!("deleting {id}")),
                // The provider not knowing it is the outcome we asked for.
                Err(s) if s.code == "not_found" => out.push(format!("{id} was already gone")),
                Err(s) => return Err(s),
            }
        }
        Ok(out)
    }
}

/// Which of the workspace's own sizes clear the sidecar's footprint: a fix
/// that names a size which also fails would send the operator in a circle.
fn fitting_sizes(org: &Org) -> String {
    let fits: Vec<String> = org
        .allowed_specs
        .iter()
        .filter(|s| s.mem_mib >= bm_core::cloud::MIN_BOX_MIB)
        .map(|s| s.label())
        .collect();
    if fits.is_empty() {
        format!(
            "this workspace allows no size with {} MiB of RAM (it offers {}) — raise the plan on \
             instabox.sh",
            bm_core::cloud::MIN_BOX_MIB,
            org.labels()
        )
    } else {
        format!(
            "set `spec` in .bm/instabox.json to {} — this workspace allows {}",
            fits.join(" or "),
            org.labels()
        )
    }
}

/// How many boxes fit, and which cap bound it when fewer than asked fit.
/// The plan's limit is the provider's own number; `cap` is ours.
fn fit(want: u32, cap: u32, tier_limit: u32, live: u32) -> (u32, Option<&'static str>) {
    let fits_cap = want.min(cap.saturating_sub(live));
    let fit = fits_cap.min(tier_limit.saturating_sub(live));
    if fit >= want {
        return (fit, None);
    }
    let bound = if fits_cap < want {
        "max_workers"
    } else {
        "machine_limit"
    };
    (fit, Some(bound))
}

fn box_of(
    id: &str,
    became: Option<&instabox::Box>,
    url: Option<String>,
    expires_at: Option<u64>,
    spec: Spec,
    slot: u32,
) -> CloudBox {
    // No public address of its own: the published worker URL is the address,
    // and the id stands in until it appears.
    let note = match became {
        Some(m) => m.note(),
        None => format!("InstaBox {id} ({})", spec.label()),
    };
    let state = match (&url, became.map(|m| m.view().state)) {
        (Some(_), _) => MachineState::Initializing,
        (None, Some(Liveness::Dead)) => MachineState::Offline,
        (None, Some(Liveness::Gone)) => MachineState::Offline,
        _ => MachineState::AwaitingIp,
    };
    CloudBox {
        provider: Provider::InstaBox,
        provider_id: id.to_string(),
        name: format!("instabox-{slot}"),
        addr: url.clone().unwrap_or_else(|| id.to_string()),
        note,
        state,
        // InstaBox has no ssh: the registry still needs a shape, and the
        // provider is what picks the transport.
        ssh_user: "instabox".into(),
        ssh_port: 22,
        key: None,
        expires_at,
        base_url: url,
    }
}

/// Any unique string: the API keeps it for a week and replays the answer for
/// the same one, so a lost create is never applied twice.
fn idempotency_key() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "bm-{}-{}-{}",
        std::process::id(),
        now_secs(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(test)]
mod tests;
