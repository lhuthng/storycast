use super::*;
fn configured() -> AwsConfig {
    AwsConfig {
        region: "eu-central-1".into(),
        subnet_id: "subnet-0abc".into(),
        security_group_id: "sg-0abc".into(),
        iam_instance_profile: "storycast-worker".into(),
        keypairs: BTreeMap::from([("eu-central-1".to_string(), "storycast".to_string())]),
        images: BTreeMap::from([("eu-central-1".to_string(), "ami-0abc".to_string())]),
        ..Default::default()
    }
}

#[test]
fn a_launched_box_becomes_a_machine_with_its_address_and_the_pool_key() {
    // The join made at birth: the same reply that names the box carries the
    let cfg = configured();
    let reply = r#"{"Groups":[],"Instances":[
        {"InstanceId":"i-09def58f197d3092c","InstanceType":"t3.micro",
         "State":{"Name":"pending"},"Placement":{"AvailabilityZone":"eu-central-1a"},
         "PrivateIpAddress":"172.31.19.210","PublicIpAddress":"3.76.103.21",
         "LaunchTime":"2026-09-20T18:38:56+00:00",
         "Tags":[{"Key":"storycast-worker","Value":"b20f7789f510"}]}],
        "OwnerId":"790139457078","ReservationId":"r-0abc"}"#;
    let instances = parse_instances(reply, DEFAULT_TAG).unwrap();
    let m = machine_from_instance(&instances[0], &cfg);
    assert_eq!(m.addr, "3.76.103.21", "public wins over private");
    assert_eq!(m.id, m.addr, "the registry keys by address, as `:add` does");
    assert_eq!(m.ssh_user, "ubuntu", "the pool's login");
    assert_eq!(m.ssh_port, 22);
    assert_eq!(
        m.ssh_key.as_deref(),
        Some(".bm/aws/eu-central-1.pem"),
        "the private half `discover` imported, finally wired up"
    );
    assert_eq!(m.role, "worker");
    // Born *initializing*, never `Unknown`. The account has just created
    assert_eq!(
        m.state,
        MachineState::Initializing,
        "a launched box is born booting"
    );
    assert!(
        m.state_since > 0,
        "and the boot deadline needs a clock to read: {}",
        m.state_since
    );
    assert!(
        m.note.contains("i-09def58f197d3092c"),
        "the EC2 id survives as a note, never as a field: {}",
        m.note
    );
}

/// The reply that arrives with no address, which is most of them, because
#[test]
fn a_box_with_no_address_yet_is_tracked_by_its_instance_id() {
    let cfg = configured();
    let reply = r#"{"Groups":[],"Instances":[
        {"InstanceId":"i-09def58f197d3092c","InstanceType":"t3.micro",
         "State":{"Name":"pending"},"Placement":{"AvailabilityZone":"eu-central-1a"},
         "PrivateIpAddress":"172.31.19.210",
         "LaunchTime":"2026-09-20T18:38:56+00:00",
         "Tags":[{"Key":"storycast-worker","Value":"b20f7789f510"}]}],
        "OwnerId":"790139457078","ReservationId":"r-0abc"}"#;
    let instances = parse_instances(reply, DEFAULT_TAG).unwrap();
    let no_address = instances[0].clone();
    let m = machine_from_instance(&no_address, &cfg);
    assert_eq!(
        m.addr, "i-09def58f197d3092c",
        "the handle is the instance id, stable for the box's whole life"
    );
    assert_eq!(m.id, m.addr, "id stays the key, exactly as for `:add`");
    assert_eq!(m.state, MachineState::AwaitingIp);
    assert!(!m.state.dialable(), "and there is nothing to poll");
    assert!(m.state.coming_up(), "but it is on its way, not gone");
    // The private address is kept, as information: an operator whose
    assert!(m.note.contains("private 172.31.19.210"), "{}", m.note);
    assert!(m.note.contains("i-09def58f197d3092c"), "{}", m.note);
    assert!(!m.note.contains("172.31.19.210 ("));
    // The one property relink depends on: the id is readable out of the note
    assert_eq!(
        ec2_id_from_note(&m.note).as_deref(),
        Some("i-09def58f197d3092c")
    );
}

#[test]
fn a_launch_asks_for_a_public_address_rather_than_trusting_the_subnet() {
    // Reachability must not rest on a per-subnet checkbox nobody re-reads:
    let args = run_instances_args(&configured(), 3, "/dev/sda1", "profilehash");
    assert!(
        args.iter().any(|a| a == "--associate-public-ip-address"),
        "a launch must ask: {args:?}"
    );
    // Still one flag, not a value: nothing here invents an address.
    assert!(!args
        .iter()
        .any(|a| a.starts_with("--associate-public-ip-address=")));
}

#[test]
fn the_onboard_marker_is_readable_out_of_a_rewritten_note() {
    // It is a note marker rather than a field because the provision job
    let note = format!("EC2 i-09def58f197d3092c (running) · {}", AWAITING_ONBOARD);
    assert!(awaiting_onboard(&note));
    assert!(!awaiting_onboard("provisioning (p)"));
    assert!(!awaiting_onboard(""));
    // And it survives a rewrite the way the id does.
    assert!(awaiting_onboard(&preserve_ec2_id(&note, AWAITING_ONBOARD)));
}

#[test]
fn a_fresh_pool_says_exactly_what_is_missing() {
    // The first thing anyone sees. It must name the fields, not fail at
    let fresh = AwsConfig::default();
    let missing = fresh.missing();
    for field in [
        "region",
        "subnet_id",
        "security_group_id",
        "iam_instance_profile",
    ] {
        assert!(
            missing.iter().any(|m| m.contains(field)),
            "{field} must be named: {missing:?}"
        );
    }
    // The type, disk, ssh user, tag and caps all have usable defaults, so a
    assert!(
        !missing.iter().any(|m| m.contains("instance_type")),
        "{missing:?}"
    );
    assert!(
        !missing.iter().any(|m| m.contains("max_workers")),
        "{missing:?}"
    );
    assert!(
        configured().missing().is_empty(),
        "{:?}",
        configured().missing()
    );
}

#[test]
fn a_keypair_belongs_to_one_region() {
    // The trap this prevents: a name that exists in one region is not a
    let mut c = configured();
    assert_eq!(c.keypair(), Some("storycast"));
    c.region = "us-east-1".into();
    assert_eq!(c.keypair(), None);
    assert!(
        c.missing().iter().any(|m| m.contains("us-east-1")),
        "the region must be named, not just 'no keypair': {:?}",
        c.missing()
    );
    // And the private half follows the region, inside the ignored `.bm/`.
    assert_eq!(c.key_file(), std::path::Path::new(".bm/aws/us-east-1.pem"));
}

#[test]
fn the_summary_names_the_asset_plane_as_an_rsync_from_here() {
    // There is one asset plane now, and it is this machine's disk. The
    let c = configured();
    assert!(c.summary().contains("rsync from here"), "{}", c.summary());
}

#[test]
fn the_launch_argv_is_reviewable_and_carries_the_marker() {
    // `aws up --dry-run` prints this argv, so it is the thing an operator
    let mut c = configured();
    let argv = run_instances_args(&c, 3, "/dev/sda1", "b20f7789f510");
    let joined = argv.join(" ");
    for want in [
        "run-instances",
        "--region eu-central-1",
        "--image-id ami-0abc",
        "--instance-type c7i.xlarge",
        "--count 3",
        "--subnet-id subnet-0abc",
        "--security-group-ids sg-0abc",
        "--key-name storycast",
        "--iam-instance-profile Name=storycast-worker",
    ] {
        assert!(joined.contains(want), "missing {want:?} in {joined}");
    }
    // The volume follows the image's own root device, and it is deleted
    assert!(joined.contains("DeviceName=/dev/sda1"), "{joined}");
    assert!(joined.contains("VolumeSize=30"), "{joined}");
    assert!(joined.contains("VolumeType=gp3"), "{joined}");
    assert!(joined.contains("DeleteOnTermination=true"), "{joined}");
    // The marker, with the profile hash as its value.
    assert!(
        joined.contains("ResourceType=instance,Tags=[{Key=storycast-worker,Value=b20f7789f510}"),
        "{joined}"
    );
    assert!(
        joined.contains("MarketType=spot"),
        "spot is on by default: {joined}"
    );
    // On-demand drops the market option entirely rather than asking for
    c.spot = false;
    assert!(!run_instances_args(&c, 1, "/dev/xvda", "h")
        .join(" ")
        .contains("MarketType"));
}

#[test]
fn the_image_is_looked_up_rather_than_assumed() {
    // `/dev/sda1` on Ubuntu, `/dev/xvda` on Amazon Linux: assuming one
    let c = configured();
    assert_eq!(
        describe_image_args(&c, "ami-0abc"),
        vec![
            "ec2",
            "describe-images",
            "--region",
            "eu-central-1",
            "--image-ids",
            "ami-0abc",
            "--query",
            "Images[0].RootDeviceName",
            "--output",
            "text"
        ]
    );
}

#[test]
fn discovery_looks_up_only_what_it_cannot_read_off_a_page() {
    // Every one of these is a read-only call whose answer `discover` writes
    assert_eq!(
        ubuntu_ami_args("eu-central-1"),
        vec![
            "ssm",
            "get-parameter",
            "--region",
            "eu-central-1",
            "--name",
            "/aws/service/canonical/ubuntu/server/26.04/stable/current/amd64/hvm/ebs-gp3/ami-id",
            "--query",
            "Parameter.Value",
            "--output",
            "text"
        ]
    );
    assert!(ubuntu_ami_args("eu-central-1")
        .join(" ")
        .contains(UBUNTU_LTS));
    // The default network: one per AZ, so the caller takes the first and
    assert_eq!(
        default_subnet_args("eu-central-1"),
        vec![
            "ec2",
            "describe-subnets",
            "--region",
            "eu-central-1",
            "--filters",
            "Name=default-for-az,Values=true",
            "--query",
            "Subnets[0].SubnetId",
            "--output",
            "text"
        ]
    );
    assert_eq!(
        default_security_group_args("eu-central-1"),
        vec![
            "ec2",
            "describe-security-groups",
            "--region",
            "eu-central-1",
            "--filters",
            "Name=group-name,Values=default",
            "--query",
            "SecurityGroups[0].GroupId",
            "--output",
            "text"
        ]
    );
    // IAM is global: a `--region` here would be a lie about the API.
    let iam = instance_profile_names_args();
    assert_eq!(iam[0], "iam");
    assert!(!iam.contains(&"--region".to_string()), "{iam:?}");
}

#[test]
fn ingress_is_read_off_the_group_rather_than_assumed() {
    // The real shape of the default group in a live account, a
    let default_group = r#"{"SecurityGroups":[{"IpPermissions":[{"IpProtocol":"-1",
         "UserIdGroupPairs":[{"UserId":"790139457078","GroupId":"sg-0fe0cc48ab8ca3bc0"}],
         "IpRanges":[],"Ipv6Ranges":[],"PrefixListIds":[]}]}]}"#;
    for port in REQUIRED_INGRESS {
        assert_eq!(
            admits_port(default_group, *port),
            Some(false),
            "group-to-group traffic is not the operator getting in (port {port})"
        );
    }

    // A rule that admits an address, on the port it names, and *not* on the
    let ssh_only = r#"{"SecurityGroups":[{"IpPermissions":[{"IpProtocol":"tcp","FromPort":22,"ToPort":22,
         "IpRanges":[{"CidrIp":"203.0.113.4/32"}]}]}]}"#;
    assert_eq!(admits_port(ssh_only, 22), Some(true));
    assert_eq!(
        admits_port(ssh_only, bm_proto::DEFAULT_TASK_PORT),
        Some(false)
    );

    // All traffic from anywhere covers both.
    let wide = r#"{"SecurityGroups":[{"IpPermissions":[{"IpProtocol":"-1",
         "IpRanges":[{"CidrIp":"0.0.0.0/0"}]}]}]}"#;
    for port in REQUIRED_INGRESS {
        assert_eq!(admits_port(wide, *port), Some(true));
    }

    // A range that happens to span a port.
    assert_eq!(
        admits_port(
            r#"{"SecurityGroups":[{"IpPermissions":[{"IpProtocol":"tcp","FromPort":20,"ToPort":30,
                 "Ipv6Ranges":[{"CidrIpv6":"::/0"}]}]}]}"#,
            22
        ),
        Some(true)
    );
    // The wrong port is the wrong port.
    assert_eq!(
        admits_port(
            r#"{"SecurityGroups":[{"IpPermissions":[{"IpProtocol":"tcp","FromPort":443,"ToPort":443,
                 "IpRanges":[{"CidrIp":"0.0.0.0/0"}]}]}]}"#,
            22
        ),
        Some(false)
    );
    assert_eq!(admits_port(r#"{"SecurityGroups":[]}"#, 22), Some(false));
    // Anything we cannot read is `None`, so the caller stays quiet rather
    assert_eq!(admits_port("not json", 22), None);
    assert_eq!(admits_port("{}", 22), None);
}

#[test]
fn a_name_list_is_split_and_a_sole_candidate_is_not_a_guess() {
    // `--output text` separates a list with tabs and newlines; empty is a
    assert_eq!(
        parse_name_list("storycast\tbox-key\nother\n"),
        vec!["storycast", "box-key", "other"]
    );
    assert_eq!(parse_name_list("   \n\t "), Vec::<String>::new());
    assert_eq!(parse_name_list("None\n"), Vec::<String>::new());

    // One candidate is the only answer, so `discover` may fill the field.
    let one = vec!["storycast".to_string()];
    assert_eq!(sole_name(&one), Some("storycast"));
    let two = vec!["a".to_string(), "b".to_string()];
    assert_eq!(sole_name(&two), None);
    assert_eq!(sole_name(&[]), None);
}

#[test]
fn terminating_names_ids_and_never_a_filter() {
    // The destructive step is always over a list someone could have read.
    let argv = terminate_args("eu-central-1", &["i-0aaa".into(), "i-0bbb".into()]);
    assert_eq!(
        argv,
        vec![
            "ec2",
            "terminate-instances",
            "--region",
            "eu-central-1",
            "--instance-ids",
            "i-0aaa",
            "i-0bbb",
            "--output",
            "json"
        ]
    );
    assert!(!argv.join(" ").contains("--filters"), "no pattern matching");
}

#[test]
fn a_pool_without_an_image_cannot_launch() {
    // The image decides what runs on the account, so a missing one is a
    let mut c = configured();
    c.images.clear();
    assert!(
        c.missing().iter().any(|m| m.contains("AMI")),
        "{:?}",
        c.missing()
    );
    c.images = BTreeMap::from([("eu-central-1".into(), "ami-0abc".into())]);
    assert!(c.missing().is_empty(), "{:?}", c.missing());
    assert_eq!(c.image(), Some("ami-0abc"));
}

#[test]
fn a_run_instances_reply_is_not_an_empty_account() {
    // `run-instances` answers with a top-level `Instances` array and **no**
    let reply = r#"{"Groups":[],"Instances":[
        {"InstanceId":"i-09def58f197d3092c","InstanceType":"t3.micro",
         "State":{"Name":"pending"},"Placement":{"AvailabilityZone":"eu-central-1a"},
         "PrivateIpAddress":"172.31.19.210","PublicIpAddress":"3.76.103.21",
         "LaunchTime":"2026-09-20T18:38:56+00:00",
         "Tags":[{"Key":"storycast-worker","Value":"b20f7789f510"}]}],
        "OwnerId":"790139457078","ReservationId":"r-0abc"}"#;
    let got = parse_instances(reply, DEFAULT_TAG).unwrap();
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].id, "i-09def58f197d3092c");
    assert_eq!(got[0].public_ip, "3.76.103.21");
    assert_eq!(got[0].private_ip, "172.31.19.210");
    assert_eq!(got[0].profile, "b20f7789f510");
    // Neither shape is still `None`, not an empty account.
    assert_eq!(
        parse_instances(r#"{"Groups":[],"OwnerId":"1"}"#, DEFAULT_TAG),
        None
    );
}

#[test]
fn a_cli_payload_we_do_not_understand_is_not_an_empty_account() {
    // The one wrong answer that costs money: reporting "nothing running"
    assert_eq!(parse_instances("not json", DEFAULT_TAG), None);
    assert_eq!(parse_instances("{}", DEFAULT_TAG), None);
    assert_eq!(
        parse_instances(r#"{"Reservations":null}"#, DEFAULT_TAG),
        None
    );
    // A well-formed empty account *is* an empty list, the distinction is
    assert_eq!(
        parse_instances(r#"{"Reservations":[]}"#, DEFAULT_TAG),
        Some(vec![])
    );
}

#[test]
fn instances_carry_the_profile_they_were_launched_for() {
    // Two boxes, one spot, one with a foreign tag that must not be read as
    let json = r#"{"Reservations":[
      {"Instances":[
        {"InstanceId":"i-0aaa","InstanceType":"c7i.xlarge",
         "State":{"Name":"running"},"Placement":{"AvailabilityZone":"eu-central-1a"},
         "InstanceLifecycle":"spot","LaunchTime":"2026-09-20T12:00:00+00:00",
         "PublicIpAddress":"18.198.4.7","PrivateIpAddress":"10.0.1.5",
         "Tags":[{"Key":"Name","Value":"other"},{"Key":"storycast-worker","Value":"b20f7789f510"}]},
        {"InstanceId":"i-0bbb","InstanceType":"c7i.large",
         "State":{"Name":"pending"},"Placement":{"AvailabilityZone":"eu-central-1b"},
         "LaunchTime":"2026-09-20T12:05:00+00:00",
         "PrivateIpAddress":"10.0.2.9",
         "Tags":[]}
      ]},
      {"Instances":[
        {"InstanceId":"i-0ccc","State":{"Name":"stopped"}}
      ]}
    ]}"#;
    let got = parse_instances(json, DEFAULT_TAG).unwrap();
    assert_eq!(got.len(), 3, "every reservation is walked: {got:?}");
    assert_eq!(got[0].id, "i-0aaa");
    assert!(got[0].spot, "InstanceLifecycle=spot is what the flag means");
    assert_eq!(got[0].profile, "b20f7789f510");
    assert_eq!(got[1].profile, "", "no marker tag is not an error");
    assert!(!got[1].spot, "absent lifecycle is on-demand");
    assert_eq!(got[1].state, "pending");
    // The address, because `aws up` ends by telling you to
    assert_eq!(got[0].public_ip, "18.198.4.7");
    assert_eq!(got[1].public_ip, "", "a private-subnet box has none");
    assert_eq!(got[1].private_ip, "10.0.2.9");
    // Fields the API may omit must not panic or shift the row.
    assert_eq!(got[2].instance_type, "");
    assert_eq!(got[2].state, "stopped");
    // And the line carries it, with `-` when the box has no address yet.
    assert!(
        instance_line(&got[0]).contains("18.198.4.7"),
        "{}",
        instance_line(&got[0])
    );
    assert!(
        instance_line(&got[1]).contains("10.0.2.9"),
        "{}",
        instance_line(&got[1])
    );
    assert!(
        instance_line(&got[2]).contains(" - "),
        "{}",
        instance_line(&got[2])
    );
}

#[test]
fn the_local_values_layer_over_the_tracked_template() {
    // The split that makes this shareable: the template travels with the
    let dir = std::env::temp_dir().join(format!("bm-aws-layer-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".bm")).unwrap();
    std::fs::write(
        dir.join(DEFAULT_FILE),
        r#"{"_note":"shipped","instance_type":"c7i.xlarge","ttl_hours":6,
            "tag_key":"storycast-worker","keypairs":{"eu-central-1":"team"}}"#,
    )
    .unwrap();

    // Template alone: a clone that has not set anything up yet still gets
    let from_template = AwsConfig::load_layered(&dir);
    assert_eq!(from_template.instance_type, "c7i.xlarge");
    assert_eq!(from_template.keypair(), None, "no region chosen yet");
    assert_eq!(from_template.tag_key, "storycast-worker");

    // Local values win, field by field, and an omitted field keeps the
    std::fs::write(
        dir.join(".bm/aws.json"),
        r#"{"region":"eu-central-1","ttl_hours":2,"subnet_id":"subnet-1"}"#,
    )
    .unwrap();
    let merged = AwsConfig::load_layered(&dir);
    assert_eq!(merged.region, "eu-central-1", "local wins");
    assert_eq!(merged.ttl_hours, 2, "local wins");
    assert_eq!(merged.subnet_id, "subnet-1");
    assert_eq!(
        merged.instance_type, "c7i.xlarge",
        "omitted locally, so the template's value survives"
    );
    assert_eq!(merged.tag_key, "storycast-worker");
    // Objects merge rather than replace, so a per-region keypair in the
    assert_eq!(merged.keypair(), Some("team"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_shipped_template_parses_and_only_needs_the_account_fields() {
    // The tracked `aws.default.json` is a real parse target, not just
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let cfg = AwsConfig::load_layered(&root);
    let missing = cfg.missing();
    // What the template cannot know: which account, which subnet, which
    for field in [
        "region",
        "subnet_id",
        "security_group_id",
        "iam_instance_profile",
    ] {
        assert!(
            missing.iter().any(|m| m.contains(field)),
            "the template must still be missing {field}: {missing:?}"
        );
    }
    for supplied in [
        "instance_type",
        "disk_gb",
        "ssh_user",
        "tag_key",
        "max_workers",
    ] {
        assert!(
            !missing.iter().any(|m| m.contains(supplied)),
            "the template supplies {supplied}: {missing:?}"
        );
    }
    assert_eq!(cfg.instance_type, "c7i.xlarge");
    assert!(cfg.spot, "spot is the shipped default");
}

#[test]
fn the_config_round_trips_and_a_missing_file_is_the_default() {
    let dir = std::env::temp_dir().join(format!("bm-aws-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join(".bm/aws.json");
    // Nothing written yet: defaults, not an error.
    assert_eq!(AwsConfig::load(&path).region, "");
    let c = configured();
    c.save(&path).unwrap();
    let back = AwsConfig::load(&path);
    assert_eq!(back.region, "eu-central-1");
    assert_eq!(back.keypair(), Some("storycast"));
    assert!(back.spot);
    // A config written before a field existed still reads: every field is
    std::fs::write(&path, r#"{"region":"eu-west-1"}"#).unwrap();
    let partial = AwsConfig::load(&path);
    assert_eq!(partial.region, "eu-west-1");
    assert_eq!(partial.instance_type, "c7i.xlarge");
    assert_eq!(partial.max_workers, 8);
    let _ = std::fs::remove_dir_all(&dir);
}
