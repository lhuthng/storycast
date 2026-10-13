use super::*;

/// The Ubuntu LTS whose AMI [`ubuntu_ami_args`] resolves.
pub const UBUNTU_LTS: &str = "26.04";

/// The `aws ssm get-parameter` call that resolves the current Ubuntu LTS AMI.
pub fn ubuntu_ami_args(region: &str) -> Vec<String> {
    vec![
        "ssm".into(),
        "get-parameter".into(),
        "--region".into(),
        region.into(),
        "--name".into(),
        format!(
            "/aws/service/canonical/ubuntu/server/{UBUNTU_LTS}/stable/current/amd64/hvm/ebs-gp3/ami-id"
        ),
        "--query".into(),
        "Parameter.Value".into(),
        "--output".into(),
        "text".into(),
    ]
}

/// The default subnet, for a pool whose network nobody has chosen yet.
pub fn default_subnet_args(region: &str) -> Vec<String> {
    vec![
        "ec2".into(),
        "describe-subnets".into(),
        "--region".into(),
        region.into(),
        "--filters".into(),
        "Name=default-for-az,Values=true".into(),
        "--query".into(),
        "Subnets[0].SubnetId".into(),
        "--output".into(),
        "text".into(),
    ]
}

/// The default security group of the default VPC.
pub fn default_security_group_args(region: &str) -> Vec<String> {
    vec![
        "ec2".into(),
        "describe-security-groups".into(),
        "--region".into(),
        region.into(),
        "--filters".into(),
        "Name=group-name,Values=default".into(),
        "--query".into(),
        "SecurityGroups[0].GroupId".into(),
        "--output".into(),
        "text".into(),
    ]
}

/// The keypair names that exist in one region.
pub fn keypair_names_args(region: &str) -> Vec<String> {
    vec![
        "ec2".into(),
        "describe-key-pairs".into(),
        "--region".into(),
        region.into(),
        "--query".into(),
        "KeyPairs[].KeyName".into(),
        "--output".into(),
        "text".into(),
    ]
}

/// One security group, as JSON, for checking what it actually admits.
pub fn describe_security_group_args(region: &str, group_id: &str) -> Vec<String> {
    vec![
        "ec2".into(),
        "describe-security-groups".into(),
        "--region".into(),
        region.into(),
        "--group-ids".into(),
        group_id.into(),
        "--output".into(),
        "json".into(),
    ]
}

/// The ports a box must accept **from wherever the inductor runs**.
/// ssh, looks healthy, and is never driven**. It reads as "the cluster is
/// broken" rather than "a rule is missing".
pub const REQUIRED_INGRESS: &[u16] = &[22, bm_proto::DEFAULT_TASK_PORT];

/// Whether a security group admits **`port` from an address**.
pub fn admits_port(json: &str, port: u16) -> Option<bool> {
    let doc: serde_json::Value = serde_json::from_str(json).ok()?;
    let groups = doc.get("SecurityGroups")?.as_array()?;
    let want = i64::from(port);
    for group in groups {
        let Some(perms) = group.get("IpPermissions").and_then(|p| p.as_array()) else {
            continue;
        };
        for perm in perms {
            let proto = perm
                .get("IpProtocol")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let all_traffic = proto == "-1";
            let tcp = proto == "tcp" || proto == "6";
            let from = perm.get("FromPort").and_then(|v| v.as_i64()).unwrap_or(-1);
            let to = perm.get("ToPort").and_then(|v| v.as_i64()).unwrap_or(-1);
            if !(all_traffic || (tcp && from <= want && to >= want)) {
                continue;
            }
            let admits_an_address = ["IpRanges", "Ipv6Ranges", "PrefixListIds"].iter().any(|k| {
                perm.get(*k)
                    .and_then(|v| v.as_array())
                    .map(|a| !a.is_empty())
                    .unwrap_or(false)
            });
            if admits_an_address {
                return Some(true);
            }
        }
    }
    Some(false)
}

/// The instance profile names in the account. IAM is global, no `--region`.
pub fn instance_profile_names_args() -> Vec<String> {
    vec![
        "iam".into(),
        "list-instance-profiles".into(),
        "--query".into(),
        "InstanceProfiles[].InstanceProfileName".into(),
        "--output".into(),
        "text".into(),
    ]
}

/// Split one `--output text` list into names.
pub fn parse_name_list(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "None")
        .map(str::to_string)
        .collect()
}

/// The single name in `names`, when there is exactly one to choose from.
pub fn sole_name(names: &[String]) -> Option<&str> {
    match names {
        [only] => Some(only.as_str()),
        _ => None,
    }
}

/// The `aws ec2 run-instances` call for `count` boxes.
pub fn run_instances_args(
    cfg: &AwsConfig,
    count: u32,
    root_device: &str,
    tag_value: &str,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "ec2".into(),
        "run-instances".into(),
        "--region".into(),
        cfg.region.clone(),
        "--image-id".into(),
        cfg.image().unwrap_or_default().into(),
        "--instance-type".into(),
        cfg.instance_type.clone(),
        "--count".into(),
        count.to_string(),
        "--subnet-id".into(),
        cfg.subnet_id.clone(),
        "--security-group-ids".into(),
        cfg.security_group_id.clone(),
        "--key-name".into(),
        cfg.keypair().unwrap_or_default().into(),
        // DeleteOnTermination: a pool that leaks volumes on every launch is a
        "--block-device-mappings".into(),
        format!(
            "DeviceName={root_device},Ebs={{VolumeSize={},VolumeType=gp3,DeleteOnTermination=true}}",
            cfg.disk_gb
        ),
        // The marker is the safety mechanism, not decoration: `down` filters on
        "--tag-specifications".into(),
        format!(
            "ResourceType=instance,Tags=[{{Key={},Value={}}},{{Key=Name,Value={}}}]",
            cfg.tag_key, tag_value, cfg.tag_key
        ),
        // Ask for a public address explicitly instead of trusting the subnet's
        "--associate-public-ip-address".into(),
        "--output".into(),
        "json".into(),
    ];
    // The role the *box* assumes. Required by `missing()` since the pool shape
    if !cfg.iam_instance_profile.trim().is_empty() {
        args.push("--iam-instance-profile".into());
        args.push(format!("Name={}", cfg.iam_instance_profile));
    }
    if cfg.spot {
        // No max-price: the on-demand ceiling is the sane default, and a
        args.push("--instance-market-options".into());
        args.push("MarketType=spot".into());
    }
    args
}

/// The `aws ec2 terminate-instances` call for explicit instance ids.
pub fn terminate_args(region: &str, ids: &[String]) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "ec2".into(),
        "terminate-instances".into(),
        "--region".into(),
        region.into(),
    ];
    args.push("--instance-ids".into());
    args.extend(ids.iter().cloned());
    args.push("--output".into());
    args.push("json".into());
    args
}

/// One running box, as much of it as the dashboard needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwsInstance {
    pub id: String,
    pub instance_type: String,
    /// `pending` | `running` | `stopping` | `stopped`.
    pub state: String,
    pub az: String,
    /// `InstanceLifecycle == "spot"`, worth showing, because a reclaimed spot
    pub spot: bool,
    /// Public address, when the box has one. Empty until the box is running.
    pub public_ip: String,
    /// Private address. The fallback when there is no public one, but a box
    pub private_ip: String,
    /// The profile hash this box was launched for, from the marker tag.
    pub profile: String,
    pub launch_time: String,
}

/// Read `aws ec2 describe-instances --output json`.
pub fn parse_instances(json: &str, tag_key: &str) -> Option<Vec<AwsInstance>> {
    let doc: serde_json::Value = serde_json::from_str(json).ok()?;
    // Two shapes, and both are real. `describe-instances` nests instances inside
    let instances: Vec<&serde_json::Value> = match doc.get("Reservations") {
        Some(res) => res
            .as_array()?
            .iter()
            .filter_map(|r| r.get("Instances").and_then(|i| i.as_array()))
            .flatten()
            .collect(),
        None => doc.get("Instances")?.as_array()?.iter().collect(),
    };
    let mut out = Vec::new();
    for i in instances {
        let s = |k: &str| {
            i.get(k)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        };
        let tag = i
            .get("Tags")
            .and_then(|t| t.as_array())
            .and_then(|tags| {
                tags.iter()
                    .find(|t| t.get("Key").and_then(|k| k.as_str()) == Some(tag_key))
            })
            .and_then(|t| t.get("Value").and_then(|v| v.as_str()))
            .unwrap_or_default()
            .to_string();
        out.push(AwsInstance {
            id: s("InstanceId"),
            instance_type: s("InstanceType"),
            state: i
                .get("State")
                .and_then(|st| st.get("Name"))
                .and_then(|n| n.as_str())
                .unwrap_or_default()
                .to_string(),
            az: i
                .get("Placement")
                .and_then(|p| p.get("AvailabilityZone"))
                .and_then(|z| z.as_str())
                .unwrap_or_default()
                .to_string(),
            spot: s("InstanceLifecycle") == "spot",
            public_ip: s("PublicIpAddress"),
            private_ip: s("PrivateIpAddress"),
            profile: tag,
            launch_time: s("LaunchTime"),
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Some(out)
}

/// The EC2 instance id a machine's note carries, if any.
pub fn ec2_id_from_note(note: &str) -> Option<String> {
    let start = note.find("i-")?;
    let rest = &note[start..];
    let end = rest
        .char_indices()
        .find(|(_, c)| !(c.is_ascii_alphanumeric() || *c == '-'))
        .map(|(i, _)| i)
        .unwrap_or(rest.len());
    let id = &rest[..end];
    // EC2 ids are `i-` + 17 hex chars (newer) or 8 (older). Anything longer
    let hexlen = id[2..].len();
    if (8..=17).contains(&hexlen) && id[2..].chars().all(|c| c.is_ascii_hexdigit()) {
        Some(id.to_string())
    } else {
        None
    }
}

/// A new note for a machine, keeping any EC2 instance id the old note carried.
pub fn preserve_ec2_id(old_note: &str, new_note: &str) -> String {
    match ec2_id_from_note(old_note) {
        Some(id) if ec2_id_from_note(new_note).is_none() => format!("{new_note} · EC2 {id}"),
        _ => new_note.to_string(),
    }
}

/// The note marker for a launched box that has just become dialable and has
pub const AWAITING_ONBOARD: &str = "address assigned · not yet onboarded";

/// Is this note waiting to be onboarded? See [`AWAITING_ONBOARD`].
pub fn awaiting_onboard(note: &str) -> bool {
    note.contains(AWAITING_ONBOARD)
}

/// The machine a freshly launched instance *is*, made the moment the launch
pub fn machine_from_instance(i: &AwsInstance, cfg: &AwsConfig) -> Machine {
    // Two states, and the address decides which. `RunInstances` answers before
    let dialable = !i.public_ip.is_empty();
    let addr = if dialable {
        i.public_ip.clone()
    } else {
        i.id.clone()
    };
    let key = cfg.key_file().to_string_lossy().into_owned();
    let mut m = Machine::new(&addr, &cfg.ssh_user, 22, Some(key), "worker");
    // Born initializing, never `Unknown`: the account has just created this box
    m.set_state(if dialable {
        MachineState::Initializing
    } else {
        MachineState::AwaitingIp
    });
    m.note = instance_note(i, dialable);
    m
}

/// The note for a box the account just created: the instance id (which is how
pub fn instance_note(i: &AwsInstance, dialable: bool) -> String {
    let mut note = format!("EC2 {} ({})", i.id, i.state);
    if !dialable {
        note.push_str(" · no public address yet");
        if !i.private_ip.is_empty() {
            note.push_str(&format!(" · private {}", i.private_ip));
        }
    }
    note
}

/// One line per box, in the shape the Machines pane uses.
pub fn instance_line(i: &AwsInstance) -> String {
    let addr = if !i.public_ip.is_empty() {
        i.public_ip.as_str()
    } else if !i.private_ip.is_empty() {
        i.private_ip.as_str()
    } else {
        "-"
    };
    format!(
        "{:<20} {:<14} {:<9} {:<16} {:<15} {:<5} {}",
        i.id,
        i.instance_type,
        i.state,
        i.az,
        addr,
        if i.spot { "spot" } else { "od" },
        if i.profile.is_empty() {
            "(no profile tag)".to_string()
        } else {
            format!("profile {}", &i.profile[..12.min(i.profile.len())])
        },
    )
}

#[cfg(test)]
mod note_id_tests {
    use super::ec2_id_from_note;

    #[test]
    fn note_rewrites_keep_the_instance_id_sticky() {
        use super::preserve_ec2_id;
        assert_eq!(
            preserve_ec2_id("EC2 i-09def58f197d3092c (running)", "provisioning (p)"),
            "provisioning (p) · EC2 i-09def58f197d3092c"
        );
        // A new note that already names an id is left alone.
        assert_eq!(
            preserve_ec2_id(
                "EC2 i-09def58f197d3092c (running)",
                "EC2 i-0fffffff (stopped)"
            ),
            "EC2 i-0fffffff (stopped)"
        );
        // Never EC2-born: nothing to preserve.
        assert_eq!(
            preserve_ec2_id("added by hand", "provisioning (p)"),
            "provisioning (p)"
        );
    }

    #[test]
    fn the_instance_id_survives_wherever_it_sits_in_the_note() {
        assert_eq!(
            ec2_id_from_note("EC2 i-09def58f197d3092c (running)"),
            Some("i-09def58f197d3092c".into())
        );
        // State updates overwrite the note; the id still reads back.
        assert_eq!(
            ec2_id_from_note("provisioned · EC2 i-0abc1234abcdef567 alive"),
            Some("i-0abc1234abcdef567".into())
        );
        assert_eq!(ec2_id_from_note("i-0deadbeef"), Some("i-0deadbeef".into()));
        // Not EC2-born: no id, no relink.
        assert_eq!(ec2_id_from_note(""), None);
        assert_eq!(ec2_id_from_note("added by hand"), None);
        assert_eq!(ec2_id_from_note("hostname ip-172-31-21-86"), None);
    }
}
