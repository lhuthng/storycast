//! AWS as one `Cloud`: smoke, plan against the account, launch one at a time.
use super::*;
use bm_core::cloud::{CloudBox, Launch, Liveness, Plan, ProviderView, Reason, Shortfall, Smoke};
use bm_core::provision::{
    admits_port, describe_image_args, describe_instances_args, describe_security_group_args,
    machine_from_instance, parse_instances, run_instances_args, terminate_args, AwsConfig,
    AwsInstance, REQUIRED_INGRESS,
};
use bm_proto::now_secs;
use std::path::{Path, PathBuf};

mod errors;
mod plan;

pub(crate) struct Aws {
    root: PathBuf,
    /// `None` is the provider's own default; `Some(0)` is no deadline at all.
    ttl_minutes: Option<u32>,
}

impl Aws {
    pub(crate) fn new(root: &Path, ttl_minutes: Option<u32>) -> Self {
        Aws {
            root: root.to_path_buf(),
            ttl_minutes,
        }
    }

    fn cfg(&self) -> Result<AwsConfig, Shortfall> {
        let cfg = AwsConfig::load_layered(&self.root);
        let missing = cfg.missing();
        if missing.is_empty() {
            return Ok(cfg);
        }
        Err(Shortfall::new(
            Reason::Stop,
            "pool_incomplete",
            missing.join("; "),
        )
        .with_fix("`bm-inductor aws show` names each one; `aws discover --region <r>` fills the lookups"))
    }

    fn instances(&self, cfg: &AwsConfig) -> Result<Vec<AwsInstance>, Shortfall> {
        let json = crate::aws_cli_instances(&self.root, &cfg.region, &cfg.tag_key)
            .map_err(|e| errors::from_error(&format!("{e:#}"), &errors::Facts::of(cfg)))?;
        parse_instances(&json, &cfg.tag_key).ok_or_else(|| {
            Shortfall::new(
                Reason::Unknown,
                "unparsable_listing",
                "the aws CLI answered a shape this does not understand — not launching on a guess",
            )
        })
    }

    /// The profile hash the marker tag carries; provisioning refuses without one.
    fn tag_value(&self) -> Result<String, Shortfall> {
        let layout = bm_core::Layout::resolve_or_root(&self.root).0;
        bm_core::profile::in_force(&layout)
            .ok()
            .map(|b| b.pack.hash)
            .filter(|h| !h.is_empty())
            .ok_or_else(|| {
                Shortfall::new(
                    Reason::Stop,
                    "no_profile",
                    "no profile loaded — the marker tag records the profile a box was built for, and provisioning refuses without one",
                )
                .with_fix(
                    ":profile in the dashboard (pack <name>, then <name>), or \
                     tools/profile.sh fetch/unpack <name>",
                )
            })
    }

    /// The configured type's own numbers, from one call. `None` when the role
    /// cannot describe types: the cap and the quota then stand alone.
    fn type_facts(&self, cfg: &AwsConfig) -> Option<(u32, u32)> {
        let args = plan::type_args(&cfg.region, &cfg.instance_type);
        plan::parse_type(&crate::aws_cli_text(&self.root, &args).ok()?)
    }

    fn vcpus(&self, cfg: &AwsConfig) -> Option<u32> {
        self.type_facts(cfg).map(|(vcpus, _)| vcpus)
    }

    /// Best effort on purpose: a role without the quota permission still
    /// launches, it just cannot warn before AWS refuses.
    fn quota(&self, cfg: &AwsConfig) -> Option<u32> {
        let args = plan::quota_args(&cfg.region, plan::VCPU_QUOTA);
        crate::aws_cli_text(&self.root, &args)
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    fn deadline(&self, cfg: &AwsConfig) -> Option<u64> {
        let minutes = match self.ttl_minutes {
            Some(0) => return None,
            Some(n) => n,
            None => cfg.ttl_hours.saturating_mul(60),
        };
        (minutes > 0).then(|| now_secs() + u64::from(minutes) * 60)
    }
}

impl Cloud for Aws {
    fn provider(&self) -> Provider {
        Provider::Aws
    }

    fn smoke(&self) -> Result<Smoke, Shortfall> {
        let cfg = self.cfg()?;
        let facts = errors::Facts::of(&cfg);
        // The identity first: a dead key is named before anything else is read.
        let account = crate::aws_cli_text(
            &self.root,
            &[
                "sts".into(),
                "get-caller-identity".into(),
                "--query".into(),
                "Account".into(),
                "--output".into(),
                "text".into(),
            ],
        )
        .map_err(|e| errors::from_error(&format!("{e:#}"), &facts))?;
        let instances = self.instances(&cfg)?;
        let live = plan::live_count(&instances);

        // A closed port makes ssh hang rather than refuse, so it is caught here.
        let group = crate::aws_cli_raw(
            &self.root,
            &describe_security_group_args(&cfg.region, &cfg.security_group_id),
        )
        .map_err(|e| errors::from_error(&format!("{e:#}"), &facts))?;
        let closed: Vec<u16> = REQUIRED_INGRESS
            .iter()
            .copied()
            .filter(|p| admits_port(&group, *p) == Some(false))
            .collect();
        if !closed.is_empty() {
            let ports = closed
                .iter()
                .map(u16::to_string)
                .collect::<Vec<_>>()
                .join(" + ");
            return Err(Shortfall::new(
                Reason::Stop,
                "ingress_closed",
                format!(
                    "security group {} admits no inbound traffic on {ports}",
                    cfg.security_group_id
                ),
            )
            .with_fix(format!(
                "EC2 → Security Groups → {} → Edit inbound rules → add the ports above, \
                 source = wherever the inductor runs",
                cfg.security_group_id
            )));
        }

        let kind = self.type_facts(&cfg);
        check_ram(&cfg, kind)?;

        let tag = self.tag_value()?;
        let mut lines = vec![format!(
            "credentials: account {} · region {}",
            account.trim(),
            cfg.region
        )];
        lines.push(format!(
            "{live} box(es) live · max_workers {}",
            cfg.max_workers
        ));
        lines.push(format!(
            "profile {} · marker {}",
            bm_core::util::head_chars(&tag, 12),
            cfg.tag_key
        ));
        if let (Some((per, mib)), Some(total)) = (kind, self.quota(&cfg)) {
            lines.push(format!("{per} vCPU / {mib} MiB per box · quota {total} vCPU"));
        }
        lines.push(format!(
            "ingress open on {}",
            REQUIRED_INGRESS
                .iter()
                .map(u16::to_string)
                .collect::<Vec<_>>()
                .join(" + ")
        ));
        Ok(Smoke {
            provider: Provider::Aws,
            lines,
        })
    }

    fn plan(&self, want: u32) -> Result<Plan, Shortfall> {
        let cfg = self.cfg()?;
        let instances = self.instances(&cfg)?;
        let kind = self.type_facts(&cfg);
        check_ram(&cfg, kind)?;
        let per = kind.map(|(vcpus, _)| vcpus);
        let quota = self.quota(&cfg);
        let headroom = plan::Headroom {
            live: plan::live_count(&instances),
            cap: cfg.max_workers,
            vcpus_per_box: per,
            quota,
        };
        let fit = headroom.fit(want);
        let facts = errors::Facts {
            quota_total: quota,
            vcpus_used: headroom.used(),
            vcpus_per_box: per,
            ..errors::Facts::of(&cfg)
        };
        Ok(Plan {
            want,
            fit,
            live: headroom.live,
            notes: headroom.notes(want, fit),
            shortfalls: headroom.shortfalls(want, fit, &facts),
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
        if let Err(s) = check_ram(&cfg, self.type_facts(&cfg)) {
            launch.skipped.push(s.covering(plan.fit));
            return launch;
        }
        let tag = match self.tag_value() {
            Ok(t) => t,
            Err(e) => {
                launch.skipped.push(e);
                return launch;
            }
        };
        let image = cfg.image().unwrap_or_default().to_string();
        let root_device = match crate::aws_cli_text(&self.root, &describe_image_args(&cfg, &image))
        {
            Ok(d) => d,
            Err(e) => {
                launch.skipped.push(errors::from_error(
                    &format!("{e:#}"),
                    &errors::Facts::of(&cfg),
                ));
                return launch;
            }
        };
        let per = self.vcpus(&cfg);
        let facts = errors::Facts {
            quota_total: self.quota(&cfg),
            vcpus_used: plan.live.checked_mul(per.unwrap_or(0)),
            vcpus_per_box: per,
            ..errors::Facts::of(&cfg)
        };
        let expires_at = self.deadline(&cfg);

        // One call per box on purpose: `--count N` is all-or-nothing, and a
        // refusal at box four must leave the first three standing.
        for i in 0..plan.fit {
            let argv = run_instances_args(&cfg, 1, root_device.trim(), &tag);
            let raw = match crate::aws_cli_raw(&self.root, &argv) {
                Ok(raw) => raw,
                Err(e) => {
                    let mut s = errors::from_error(&format!("{e:#}"), &facts);
                    s.want = plan.fit - i;
                    launch.skipped.push(s);
                    break;
                }
            };
            let Some(boxes) = parse_instances(&raw, &cfg.tag_key) else {
                launch.skipped.push(
                    Shortfall::new(
                        Reason::Unknown,
                        "unparsable_launch",
                        "the launch answered a shape this does not understand",
                    )
                    .covering(plan.fit - i),
                );
                break;
            };
            if boxes.is_empty() {
                // An accepted call with nothing in it: the account did create
                // something, or it did not, but this cannot tell which.
                launch.skipped.push(
                    Shortfall::new(
                        Reason::Unknown,
                        "empty_launch",
                        "the launch was accepted but named no instance — read the account before asking again",
                    )
                    .covering(plan.fit - i),
                );
                break;
            }
            for i in boxes {
                launch.created.push(box_of(&i, &cfg, expires_at));
            }
        }
        launch
    }

    fn list(&self, ids: &[String]) -> Result<Vec<ProviderView>, Shortfall> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let cfg = self.cfg()?;
        let raw = crate::aws_cli_raw(&self.root, &describe_instances_args(&cfg.region, ids))
            .map_err(|e| errors::from_error(&format!("{e:#}"), &errors::Facts::of(&cfg)))?;
        let boxes = parse_instances(&raw, &cfg.tag_key).ok_or_else(|| {
            Shortfall::new(
                Reason::Unknown,
                "unparsable_listing",
                "the answer to describe-instances is a shape this does not understand — not reading it as an empty account",
            )
        })?;
        Ok(boxes.iter().map(view_of).collect())
    }

    fn down(&self, ids: &[String]) -> Result<Vec<String>, Shortfall> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let cfg = AwsConfig::load_layered(&self.root);
        if cfg.region.trim().is_empty() {
            return Err(Shortfall::new(Reason::Stop, "no_region", "no region set")
                .with_fix("`bm-inductor aws init`, then fill it in"));
        }
        let raw = crate::aws_cli_raw(&self.root, &terminate_args(&cfg.region, ids))
            .map_err(|e| errors::from_error(&format!("{e:#}"), &errors::Facts::of(&cfg)))?;
        let mut out = Vec::new();
        for i in parse_instances(&raw, &cfg.tag_key).unwrap_or_default() {
            out.push(format!("terminating {} ({})", i.id, i.state));
        }
        Ok(out)
    }
}

/// A type below the sidecar's footprint is never launched, whatever the plan
/// costed: the box would be paid for and could not render one chapter.
fn check_ram(cfg: &AwsConfig, kind: Option<(u32, u32)>) -> Result<(), Shortfall> {
    let Some((_, mib)) = kind else {
        // Unread is not small: a role without DescribeInstanceTypes still runs.
        return Ok(());
    };
    if mib >= bm_core::cloud::MIN_BOX_MIB {
        return Ok(());
    }
    Err(bm_core::cloud::too_small(
        "instance_too_small",
        mib,
        format!(
            "`instance_type` in .bm/aws.json is {} — pick one with more RAM \
             (`aws ec2 describe-instance-types` lists them)",
            cfg.instance_type
        ),
    ))
}

/// One instance's state, in the shared vocabulary, with EC2's word kept.
fn view_of(i: &AwsInstance) -> ProviderView {
    let state = match i.state.as_str() {
        "pending" | "running" => Liveness::Running,
        "stopping" | "stopped" => Liveness::Stopped,
        "shutting-down" | "terminated" => Liveness::Dead,
        // A word AWS adds later is not read as dead: the instance is still
        // answering for itself, so the beat decides.
        _ => Liveness::Running,
    };
    let mut view = ProviderView::new(i.id.clone(), state, i.state.clone());
    if i.spot {
        view = view.saying(format!("spot instance ({})", i.state));
    }
    view
}

fn box_of(i: &AwsInstance, cfg: &AwsConfig, expires_at: Option<u64>) -> CloudBox {
    let m = machine_from_instance(i, cfg);
    CloudBox {
        provider: Provider::Aws,
        provider_id: i.id.clone(),
        name: m.name.clone(),
        addr: m.addr.clone(),
        note: m.note.clone(),
        state: m.state,
        ssh_user: m.ssh_user.clone(),
        ssh_port: m.ssh_port,
        key: m.ssh_key.clone(),
        expires_at,
        base_url: None,
    }
}

#[cfg(test)]
mod tests;
