//! The AWS error catalog: AWS's own codes and words into the shared reason.
use super::plan::VCPU_QUOTA;
use bm_core::cloud::{Reason, Shortfall};
use bm_core::provision::AwsConfig;

/// The live facts a fix line is built from. Nothing here is a literal in a
/// message: every number comes from the account at the moment it is asked.
pub(crate) struct Facts {
    pub(crate) region: String,
    pub(crate) instance_type: String,
    pub(crate) subnet_id: String,
    pub(crate) keypair: String,
    pub(crate) quota_total: Option<u32>,
    pub(crate) vcpus_used: Option<u32>,
    pub(crate) vcpus_per_box: Option<u32>,
}

impl Facts {
    pub(crate) fn of(cfg: &AwsConfig) -> Self {
        Facts {
            region: cfg.region.clone(),
            instance_type: cfg.instance_type.clone(),
            subnet_id: cfg.subnet_id.clone(),
            keypair: cfg.keypair().unwrap_or_default().to_string(),
            quota_total: None,
            vcpus_used: None,
            vcpus_per_box: None,
        }
    }
}

/// `An error occurred (VcpuLimitExceeded) when calling the RunInstances
/// operation: <message>` → `("VcpuLimitExceeded", "<message>")`.
pub(crate) fn split(raw: &str) -> (String, String) {
    let text = raw.trim();
    if let Some(open) = text.find('(') {
        if let Some(len) = text[open + 1..].find(')') {
            let code = &text[open + 1..open + 1 + len];
            if !code.is_empty() && !code.contains(' ') {
                let rest = &text[open + 1 + len + 1..];
                let message = rest
                    .split_once("operation: ")
                    .map(|(_, m)| m)
                    .unwrap_or(rest)
                    .trim_start_matches(':')
                    .trim();
                return (code.to_string(), message.to_string());
            }
        }
    }
    let message = text.lines().next().unwrap_or_default().trim();
    ("unknown".to_string(), message.to_string())
}

/// One failed call → the shared reason, with the provider's own words kept.
pub(crate) fn from_error(raw: &str, facts: &Facts) -> Shortfall {
    let (code, message) = split(raw);
    let lower = raw.to_ascii_lowercase();

    if lower.contains("unable to locate credentials")
        || lower.contains("invalidclienttokenid")
        || lower.contains("expiredtoken")
        || lower.contains("invalidaccesskeyid")
        || lower.contains("signaturedoesnotmatch")
    {
        return Shortfall::new(Reason::Stop, "credentials", message).with_fix(
            "store the app's IAM user again: `bm-inductor aws login --csv <accessKeys.csv>`",
        );
    }
    if lower.contains("aws cli is not on path") {
        return Shortfall::new(Reason::Stop, "aws_cli_missing", message)
            .with_fix("install the AWS CLI v2, then run the command again");
    }
    if lower.contains("not authorized") || lower.contains("unauthorizedoperation") {
        return Shortfall::new(Reason::Stop, "not_authorized", message)
            .with_fix("`bm-inductor aws policy` prints the policy this IAM user needs");
    }

    match code.as_str() {
        "VcpuLimitExceeded" => {
            Shortfall::new(Reason::Clamp, code, message).with_fix(quota_fix(facts).unwrap_or_else(
                || "request a vCPU quota increase for this region (`aws service-quotas`)".into(),
            ))
        }
        "InsufficientInstanceCapacity"
        | "InsufficientHostCapacity"
        | "InsufficientInstanceCapacityOnHost"
        | "UnfulfillableCapacity" => {
            Shortfall::new(Reason::Capacity, code, message).with_fix(format!(
                "no spare {} capacity in {} (subnet {}) right now — another instance_type \
                 in .bm/aws.json, another subnet_id, or retry shortly",
                facts.instance_type, facts.region, facts.subnet_id
            ))
        }
        "MaxSpotInstanceCountExceeded" | "SpotMaxPriceTooLow" => {
            Shortfall::new(Reason::Capacity, code, message).with_fix(format!(
                "{} is at its spot limit — set `spot: false` in .bm/aws.json for on-demand, \
                 or ask for fewer",
                facts.region
            ))
        }
        // AWS gives no retry-after; the caller backs off on `retryable` alone.
        "RequestLimitExceeded" | "Throttling" | "ThrottlingException" => {
            Shortfall::new(Reason::Capacity, code, message).retry_in(None)
        }
        "InvalidAMIID.NotFound" | "InvalidAMIID.Malformed" => {
            Shortfall::new(Reason::Stop, code, message).with_fix(format!(
                "`bm-inductor aws discover --region {}` re-resolves the AMI",
                facts.region
            ))
        }
        "InvalidSubnetID.NotFound" => {
            Shortfall::new(Reason::Stop, code, message).with_fix(format!(
                "`subnet_id` in .bm/aws.json does not exist in {}",
                facts.region
            ))
        }
        "InvalidGroup.NotFound" => Shortfall::new(Reason::Stop, code, message).with_fix(format!(
            "`security_group_id` in .bm/aws.json does not exist in {}",
            facts.region
        )),
        "InvalidKeyPair.NotFound" => Shortfall::new(Reason::Stop, code, message).with_fix(format!(
            "`keypairs.{}` names {:?}, which is not a keypair in that region",
            facts.region, facts.keypair
        )),
        "InvalidIAMInstanceProfile.NotFound" => Shortfall::new(Reason::Stop, code, message)
            .with_fix("`iam_instance_profile` in .bm/aws.json names no profile in this account"),
        "OptInRequired" | "PendingVerification" | "Blocked" => {
            Shortfall::new(Reason::Stop, code, message)
                .with_docs("the AWS console shows what this account still has to enable")
        }
        // Unmapped codes are shown as AWS wrote them: a new limit must never be
        // flattened into advice we invented.
        _ => Shortfall::new(Reason::Unknown, code, message),
    }
}

fn quota_fix(facts: &Facts) -> Option<String> {
    let used = facts.vcpus_used?;
    let per = facts.vcpus_per_box?;
    Some(format!(
        "aws service-quotas request-service-quota-increase --service-code ec2 \
         --quota-code {VCPU_QUOTA} --desired-value {} --region {}",
        used.saturating_add(per),
        facts.region
    ))
}
