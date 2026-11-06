//! How many of the asked-for boxes the account can actually take.
use super::*;
use bm_core::cloud::{Reason, Shortfall};

/// The on-demand vCPU quota code every EC2 region carries.
pub(crate) const VCPU_QUOTA: &str = "L-1216C47A";

pub(crate) struct Headroom {
    pub(crate) live: u32,
    pub(crate) cap: u32,
    /// `None` when the type lookup failed: the cap then stands alone.
    pub(crate) vcpus_per_box: Option<u32>,
    /// The region's total vCPU quota, when the caller may read it.
    pub(crate) quota: Option<u32>,
}

impl Headroom {
    pub(crate) fn used(&self) -> Option<u32> {
        Some(
            self.live
                .saturating_mul(self.vcpus_per_box.filter(|n| *n > 0)?),
        )
    }

    /// Boxes the quota still has room for. Counts only boxes wearing our tag:
    /// anything else in the account is AWS's to refuse at the call.
    fn by_quota(&self) -> Option<u32> {
        let per = self.vcpus_per_box.filter(|n| *n > 0)?;
        Some(self.quota?.saturating_sub(self.used()?) / per)
    }

    pub(crate) fn fit(&self, want: u32) -> u32 {
        want.min(self.cap.saturating_sub(self.live))
            .min(self.by_quota().unwrap_or(u32::MAX))
    }

    fn by_cap(&self) -> u32 {
        self.cap.saturating_sub(self.live)
    }

    pub(crate) fn notes(&self, want: u32, fit: u32) -> Vec<String> {
        let mut out = vec![format!("{} live · max_workers {}", self.live, self.cap)];
        if let (Some(per), Some(used), Some(total)) = (self.vcpus_per_box, self.used(), self.quota)
        {
            out.push(format!(
                "quota {total} vCPU · {used} in use · {per} per box"
            ));
        }
        out.push(format!("{want} asked for · {fit} fit"));
        out
    }

    /// Why the rest were not created, in the order that decides the count:
    /// our own cap first, then the region's quota.
    pub(crate) fn shortfalls(
        &self,
        want: u32,
        fit: u32,
        facts: &super::errors::Facts,
    ) -> Vec<Shortfall> {
        if fit >= want {
            return Vec::new();
        }
        let mut left = want - fit;
        let mut out = Vec::new();
        let cap_room = self.by_cap();
        if want > cap_room && left > 0 {
            let n = left.min(want - cap_room);
            out.push(
                Shortfall::new(
                    Reason::Clamp,
                    "max_workers",
                    format!(
                        "{want} asked for, {} already live, max_workers is {}",
                        self.live, self.cap
                    ),
                )
                .covering(n)
                .with_fix("raise max_workers in .bm/aws.json, or ask for fewer"),
            );
            left -= n;
        }
        if let (Some(room), true) = (self.by_quota(), left > 0) {
            if want > room {
                let n = left.min(want - room);
                out.push(
                    Shortfall::new(
                        Reason::Clamp,
                        "vcpu_quota",
                        format!(
                            "quota {} vCPU, {} in use, {} per box",
                            facts.quota_total.unwrap_or_default(),
                            facts.vcpus_used.unwrap_or_default(),
                            facts.vcpus_per_box.unwrap_or_default()
                        ),
                    )
                    .covering(n)
                    .with_fix(quota_fix(facts)),
                );
            }
        }
        out
    }
}

fn quota_fix(facts: &super::errors::Facts) -> String {
    let used = facts.vcpus_used.unwrap_or_default();
    let per = facts.vcpus_per_box.unwrap_or_default();
    format!(
        "aws service-quotas request-service-quota-increase --service-code ec2 \
         --quota-code {VCPU_QUOTA} --desired-value {} --region {}",
        used.saturating_add(per),
        facts.region
    )
}

/// The boxes that count against the cap: the ones with a process, or about to.
pub(crate) fn live_count(instances: &[AwsInstance]) -> u32 {
    instances
        .iter()
        .filter(|i| matches!(i.state.as_str(), "pending" | "running" | "stopping"))
        .count() as u32
}

/// `aws ec2 describe-instance-types`, reduced to one type's vCPUs and RAM —
/// one call, because the plan needs both and they never disagree.
pub(crate) fn type_args(region: &str, instance_type: &str) -> Vec<String> {
    vec![
        "ec2".into(),
        "describe-instance-types".into(),
        "--region".into(),
        region.into(),
        "--instance-types".into(),
        instance_type.into(),
        "--query".into(),
        "InstanceTypes[0].[VCpuInfo.DefaultVCpus,MemoryInfo.SizeInMiB]".into(),
        "--output".into(),
        "text".into(),
    ]
}

/// `4\t8192` → the pair. A count without its RAM is no answer at all: half of
/// it would let a box through on a guessed size.
pub(crate) fn parse_type(text: &str) -> Option<(u32, u32)> {
    let mut fields = text.split_whitespace();
    let vcpus: u32 = fields.next()?.parse().ok()?;
    let mib: u32 = fields.next()?.parse().ok()?;
    (vcpus > 0 && mib > 0).then_some((vcpus, mib))
}

/// `aws service-quotas get-service-quota`, reduced to the quota's value.
pub(crate) fn quota_args(region: &str, code: &str) -> Vec<String> {
    vec![
        "service-quotas".into(),
        "get-service-quota".into(),
        "--region".into(),
        region.into(),
        "--service-code".into(),
        "ec2".into(),
        "--quota-code".into(),
        code.into(),
        "--query".into(),
        "Quota.Value".into(),
        "--output".into(),
        "text".into(),
    ]
}
