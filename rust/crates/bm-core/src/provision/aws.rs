//! AWS as a *source of worker addresses*.
//!
//! Nothing here is a new kind of worker. `provision` already models one as
//! "an address + ssh credentials + a mirror to fill", so a cloud box is that
//! same thing whose address came from `RunInstances` instead of a static IP.
//! What AWS adds is a place to get addresses from, and the reason this module
//! exists is that a launch needs a dozen decisions — region, type, subnet,
//! security group, keypair, instance profile, disk, lifetime — that nobody
//! should have to retype per call.
//!
//! So: **one file, written once**, `.bm/aws.json`. Machine-global and ignored,
//! like `machines.json` beside it, because it describes *this machine's access
//! to AWS*, not a book. After that a launch is `aws up --count 3`.
//!
//! The asset plane is not re-sent per call either, and the reason is the design
//! that was already here: `.bm/profile` holds `{name, hash}`, every worker
//! verifies that hash of `assets/` + `prompts/` at startup, and
//! `.provision_stamp.json` decides whether a given box needs anything pushed.
//! [`profile_object`] names the S3 object **by that same hash**, so "which
//! files" and "do I have the right files" are one value used twice — nothing to
//! pass around and nothing to compare separately.
//!
//! Two region-scoped things, and they are maps from the start: an EC2 keypair
//! belongs to one region, and an AMI is one region's copy of an image. A single
//! `keypair: String` would have to become a map the first time a pool spans two
//! regions, which is a config migration for a field that costs nothing to get
//! right now.

use anyhow::{Context, Result};
use bm_proto::{Machine, MachineState};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// The tag every box this tool starts carries, so `ls`/`down` can find ours and
/// never touch anything else in the account. Overridable, but never empty: an
/// untagged pool cannot be told from a stranger's instances.
pub const DEFAULT_TAG: &str = "storycast-worker";

/// The tracked template at the repo root, beside `voices.default.json` and for
/// the same reason: the *shape* of the pool has to travel with the repo, so
/// anyone who clones knows what to fill in. Only the values are personal, and
/// they live in the ignored `.bm/aws.json`.
///
/// Credentials belong in neither file. They are the key of the **IAM user
/// created for this app**, stored in the ignored `.bm/aws/credentials`
/// (see [`super::aws_credentials`]), and that is what makes this shareable: two
/// people with the same repo and different accounts each log in as their own
/// user, and neither can commit the other's account IDs by accident.
pub const DEFAULT_FILE: &str = "aws.default.json";

/// One AWS worker pool. Everything a launch needs, written once.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AwsConfig {
    /// `eu-central-1`, `us-east-1`, … Empty means "not set up yet".
    pub region: String,
    /// Instance type. The TTS path is a hand-written SIMD matvec on CPU and
    /// there is no GPU code, so this is a CPU choice: 2 vCPU is the floor, 4 is
    /// where render stops queueing behind itself.
    ///
    /// **RAM is what actually decides it, measured on the target platform**
    /// (`m7i-flex.large`, linux/x86_64, release build, 2026-09-20): the sidecar
    /// is **~2.85 GB resident as soon as the weights are loaded** (VmHWM
    /// 2,918,708 kB) and **~2.88 GB after a dozen renders** — the load dominates
    /// and renders add ~30 MB. So 1–2 GiB cannot run it, **4 GiB does not fit**
    /// (~2.9 GB of sidecar plus the agent and the OS in ~3.9 GB usable), and
    /// **8 GiB is the size to use**.
    ///
    /// Two traps: a macOS/arm64 build of the same binary idles at ~1.0 GB, so
    /// measuring on the wrong platform understates this by ~2.8×; and `models/`
    /// is 668 MB on disk, so sizing from `du` understates it by ~4×.
    ///
    /// The type must also be eligible for the account's plan — a Free-plan
    /// account is refused the default outright, with `m7i-flex.large` (2 vCPU /
    /// 8 GiB) the right pick off that list. Nothing this code can call can read
    /// a plan.
    pub instance_type: String,
    /// Root volume, GB. Models 668 MB, the profile 57 MB, plus the segments a
    /// chapter accumulates; 30 is comfortable and cheap.
    pub disk_gb: u32,
    /// The subnet the boxes land in — and it must be one **the inductor can
    /// reach**.
    ///
    /// The transport is inverted: the inductor dials the box on its task port,
    /// and nothing ever dials the inductor. So a box in a genuinely private
    /// subnet, with no public address and no tunnel, is one that can never be
    /// driven — it will sit at `Offline` for ever while looking perfectly
    /// healthy from the console. A default-VPC subnet maps a public address on
    /// launch, which is why the default works and why this is easy to get
    /// wrong.
    pub subnet_id: String,
    /// Security group. **Ingress** is what matters, on two ports — 22 for
    /// provisioning and the task port for the work (see [`REQUIRED_INGRESS`]) —
    /// and both must come from wherever the inductor runs. Egress stays open,
    /// but not to reach the inductor: it is for chapter URLs, S3 and the Gemini
    /// API.
    pub security_group_id: String,
    /// IAM instance profile **name**, which is what grants the S3 read. Without
    /// it a box cannot pull its own asset plane and every launch is a 668 MB
    /// upload from this machine instead.
    pub iam_instance_profile: String,
    /// Login on the box. `ubuntu` on the stock Ubuntu AMIs, `ec2-user` on AL.
    pub ssh_user: String,
    /// EC2 keypair **name** per region. The private half never leaves this
    /// machine — see [`AwsConfig::key_file`].
    pub keypairs: BTreeMap<String, String>,
    /// AMI per region, because an image is one region's copy.
    ///
    /// Deliberately explicit rather than looked up on every launch: the AMI
    /// decides what actually runs on the account, so it is a value you can read
    /// and change rather than one that moves under you. `bm-inductor aws
    /// discover` resolves the current Ubuntu LTS once and writes it here, so
    /// nobody has to hunt the AMI catalogue — and a value already present is
    /// left alone, with `--ami` the only thing that moves it. It must match
    /// `ssh_user` — `ubuntu` on Ubuntu, `ec2-user` on Amazon Linux — and the
    /// two are checked together.
    pub images: BTreeMap<String, String>,
    /// Where the profile bundles are published, content-addressed. Empty turns
    /// the S3 path off and falls back to rsyncing from here.
    pub bucket: String,
    /// Ask for spot capacity. Render and merge are both idempotent and the
    /// lease reaper requeues an interrupted task, so a reclaimed box costs a
    /// retry, not a lost chapter.
    pub spot: bool,
    /// Hard cap on concurrently registered cloud boxes. Refuses a launch that
    /// would exceed it rather than trusting the caller to do arithmetic.
    pub max_workers: u32,
    /// Default lifetime in hours. The box drains and stops at the deadline; a
    /// box that outlives its work is the whole cost of a cloud pool.
    pub ttl_hours: u32,
    /// The marker tag **key** every box we start carries, so `ls`/`down` can
    /// never touch anything else in the account. Its *value* is the profile
    /// hash the box was launched for, which is what makes `ls` able to say
    /// which build each box is running without asking it.
    pub tag_key: String,
}

impl Default for AwsConfig {
    fn default() -> Self {
        AwsConfig {
            region: String::new(),
            instance_type: "c7i.xlarge".into(),
            disk_gb: 30,
            subnet_id: String::new(),
            security_group_id: String::new(),
            iam_instance_profile: String::new(),
            ssh_user: "ubuntu".into(),
            keypairs: BTreeMap::new(),
            images: BTreeMap::new(),
            bucket: String::new(),
            spot: true,
            max_workers: 8,
            ttl_hours: 6,
            tag_key: DEFAULT_TAG.into(),
        }
    }
}

impl AwsConfig {
    /// Read the pool definition from one file. A missing file is not an error —
    /// it means the pool has never been set up, which is a state
    /// [`Self::missing`] describes.
    pub fn load(path: &Path) -> Self {
        read_doc(path)
            .and_then(|d| serde_json::from_value(d).ok())
            .unwrap_or_default()
    }

    /// The effective pool: the local values over the tracked template over the
    /// compiled defaults.
    ///
    /// Layered rather than either/or, because the two halves answer different
    /// questions. The template ships the *shape* — which fields exist, what a
    /// sensible instance type is, why spot is a good fit — and travels with the
    /// repo. The local file holds the account-specific values, which are
    /// nobody else's business and which git ignores. A field the local file
    /// omits keeps the template's value, so a config written before a field
    /// existed still picks it up instead of silently reverting to a compiled
    /// default that the template had deliberately changed.
    ///
    /// Objects merge key by key (a local `keypairs` entry adds to the
    /// template's rather than replacing it); everything else replaces.
    pub fn load_layered(root: &Path) -> Self {
        let mut doc = read_doc(&root.join(DEFAULT_FILE)).unwrap_or(serde_json::Value::Null);
        if let Some(local) = read_doc(&root.join(".bm/aws.json")) {
            merge(&mut doc, local);
        }
        serde_json::from_value(doc).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        crate::atomic_write(path, &serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    /// The EC2 keypair name for the configured region, if it has one.
    ///
    /// Per region because EC2 keypairs are: a name that exists in
    /// `eu-central-1` is not a keypair in `us-east-1`, and a launch that
    /// assumed otherwise fails at `RunInstances` with a message about a key
    /// that "does not exist" while it plainly does.
    pub fn keypair(&self) -> Option<&str> {
        self.keypairs.get(&self.region).map(String::as_str)
    }

    /// The AMI for the configured region, if one is set.
    ///
    /// Per region for the same reason as the keypair, and separate from it
    /// because the two are chosen for different reasons: the keypair is about
    /// how you get in, the image is about what is already installed when you do.
    pub fn image(&self) -> Option<&str> {
        self.images.get(&self.region).map(String::as_str)
    }

    /// Where this region's private half lives, as a path relative to the repo
    /// root. `.bm/` is already ignored, so no `.gitignore` change is needed and
    /// the material is out of the tree by construction.
    pub fn key_file(&self) -> std::path::PathBuf {
        std::path::PathBuf::from(".bm")
            .join("aws")
            .join(format!("{}.pem", self.region))
    }

    /// What still has to be filled in before a launch can run.
    ///
    /// Returned as a list rather than as a first-error, because the answer to
    /// "why can't it launch" is usually three fields, and fixing them one
    /// message at a time is three round trips.
    pub fn missing(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (value, what) in [
            (&self.region, "region"),
            (&self.instance_type, "instance_type"),
            (&self.subnet_id, "subnet_id"),
            (&self.security_group_id, "security_group_id"),
            (&self.iam_instance_profile, "iam_instance_profile"),
            (&self.ssh_user, "ssh_user"),
            (&self.tag_key, "tag_key"),
        ] {
            if value.trim().is_empty() {
                out.push(format!("{what} is empty"));
            }
        }
        if self.keypair().is_none() {
            out.push(format!(
                "no keypair for region {:?} — add it to `keypairs` (an EC2 keypair belongs to one region)",
                self.region
            ));
        }
        if self.image().is_none() {
            out.push(format!(
                "no AMI for region {:?} — run `bm-inductor aws discover` to fill it in, or set `images` by hand",
                self.region
            ));
        }
        if self.max_workers == 0 {
            out.push("max_workers is 0 — no launch can ever be within the cap".into());
        }
        if self.ttl_hours == 0 {
            out.push("ttl_hours is 0 — boxes would outlive their work by design".into());
        }
        out
    }

    /// Whether this pool can hand a box its asset plane without uploading it
    /// from here.
    ///
    /// Empty bucket is not an error: the rsync path still works and is what a
    /// LAN pool uses. It is a *degradation*, and a launch should say so rather
    /// than let 668 MB of egress be discovered on the bill.
    pub fn publishes_assets(&self) -> bool {
        !self.bucket.trim().is_empty()
    }

    /// One line for the Machines pane or a `show`, with the secret-free fields.
    pub fn summary(&self) -> String {
        let keypair = self.keypair().unwrap_or("-");
        format!(
            "{} · {} · {} GB · keypair={} · {} · spot={} · max {} · {}h",
            if self.region.is_empty() {
                "(no region)"
            } else {
                &self.region
            },
            self.instance_type,
            self.disk_gb,
            keypair,
            if self.publishes_assets() {
                format!("s3://{}", self.bucket)
            } else {
                "assets: rsync from here".into()
            },
            self.spot,
            self.max_workers,
            self.ttl_hours,
        )
    }
}

/// Where a profile's asset plane lives: `s3://<bucket>/profiles/<hash>.tar.zst`.
///
/// **Named by the manifest hash**, which is the value already sitting in
/// `.bm/profile` and the one every worker recomputes and checks at startup. So
/// the object key is derived from a hash the box already holds — there is no
/// mapping to keep in sync, and a box cannot be handed a bundle that does not
/// match the pointer it verifies against.
///
/// Re-packing identical content under a different compression level overwrites
/// the same key with the same tree, which is the correct outcome: the manifest
/// inside the bundle is what is checked, file by file, before anything moves.
///
/// `None` when there is no bucket or no hash — both are "the S3 path is off",
/// not an error.
pub fn profile_object(bucket: &str, manifest_hash: &str) -> Option<String> {
    let bucket = bucket.trim();
    let hash = manifest_hash.trim();
    if bucket.is_empty() || hash.is_empty() {
        return None;
    }
    Some(format!("s3://{bucket}/profiles/{hash}.tar.zst"))
}

/// One JSON file, or `None` when it is absent or unreadable.
fn read_doc(path: &Path) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// `over` wins, key by key; nested objects merge rather than replace.
///
/// The `_note` keys the template carries for the human are just keys here —
/// serde drops what the struct does not name, so the explanation survives in
/// the file the operator edits without ever reaching the code.
fn merge(base: &mut serde_json::Value, over: serde_json::Value) {
    match (base, over) {
        (serde_json::Value::Object(b), serde_json::Value::Object(o)) => {
            for (k, v) in o {
                merge(b.entry(k).or_insert(serde_json::Value::Null), v);
            }
        }
        (slot, v) => *slot = v,
    }
}

/// The `aws ec2 describe-images` call that finds the AMI's root device name.
///
/// Needed because the block-device mapping has to name it, and the name differs
/// by image (`/dev/sda1` on Ubuntu, `/dev/xvda` on Amazon Linux). Resolving it
/// instead of assuming one means `disk_gb` is honoured on any image rather than
/// silently ignored on half of them.
pub fn describe_image_args(cfg: &AwsConfig, image_id: &str) -> Vec<String> {
    vec![
        "ec2".into(),
        "describe-images".into(),
        "--region".into(),
        cfg.region.clone(),
        "--image-ids".into(),
        image_id.into(),
        "--query".into(),
        "Images[0].RootDeviceName".into(),
        "--output".into(),
        "text".into(),
    ]
}

/// The Ubuntu LTS whose AMI [`ubuntu_ami_args`] resolves.
///
/// A constant rather than a "latest" query because the *result* is what gets
/// stored: `aws discover` writes the resolved `ami-…` into `.bm/aws.json`, so
/// nothing downstream depends on this staying current — only the one command
/// that has to look something up does. Bump it when Canonical ships the next
/// LTS; the AWS parameter path carries the version.
pub const UBUNTU_LTS: &str = "26.04";

/// The `aws ssm get-parameter` call that resolves the current Ubuntu LTS AMI.
///
/// This is the one pool field that cannot be read off a console page without
/// hunting through the AMI catalogue, and it is a field whose absence is a
/// refusal — so it is the one worth a lookup. Read-only, one value.
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
///
/// `default-for-az=true` returns one per availability zone; the caller takes
/// the first and *prints* it, because "which subnet" is a real decision that
/// `discover` is only allowed to make visibly.
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
///
/// **It has no inbound rules**, so a box launched with it is unreachable until
/// SSH is allowed — which is why the caller must say so rather than presenting
/// the value as a working answer.
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
///
/// Used to *offer* a name rather than invent one: a keypair is created in the
/// console, so the name is whatever was typed there.
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

/// One security group, as JSON — for checking what it actually admits.
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
///
/// Two, and both are load-bearing:
///
/// - **22** — provisioning pushes the mirror over ssh + rsync.
/// - **the task port** — the work itself. The transport is inverted, so the
///   inductor dials the box: `GET /status`, `POST /task`, `GET /unit`,
///   `POST /shutdown`.
///
/// The second is the one that gets missed, and its failure mode is the quiet
/// one: a box whose 22 is open and whose task port is not **launches, accepts
/// ssh, looks healthy, and is never driven**. It reads as "the cluster is
/// broken" rather than "a rule is missing".
pub const REQUIRED_INGRESS: &[u16] = &[22, bm_proto::DEFAULT_TASK_PORT];

/// Whether a security group admits **`port` from an address**.
///
/// The check exists because the failure it predicts is invisible until you are
/// staring at a hung `ssh`, or at a machine that has never once been `Online`: a
/// closed port does not refuse a connection, it swallows it. So `discover` says
/// so while the group is being chosen.
///
/// Deliberately narrow about what counts as admitting you:
///
/// - `IpProtocol == "-1"` (all traffic) counts, and so does a `tcp` rule whose
///   port range spans `port`.
/// - A source must be an **`IpRanges` / `Ipv6Ranges` / `PrefixListIds`** entry.
///   A rule that names only `UserIdGroupPairs` does **not** count — that is
///   group-to-group traffic, which is exactly what the default group has and
///   exactly why the default group does not let you in.
///
/// `None` when the payload is not the shape we expect, so the caller can stay
/// quiet instead of warning on a guess.
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

/// The instance profile names in the account. IAM is global — no `--region`.
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
///
/// The CLI separates a list with tabs and newlines, so splitting on any
/// whitespace is the right shape; empty means the account holds none, which is
/// a normal answer rather than an error.
pub fn parse_name_list(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "None")
        .map(str::to_string)
        .collect()
}

/// The single name in `names`, when there is exactly one to choose from.
///
/// The whole reason `discover` is allowed to fill a field: one candidate is not
/// a guess, it is the only answer. Two is ambiguous and stays the operator's.
pub fn sole_name(names: &[String]) -> Option<&str> {
    match names {
        [only] => Some(only.as_str()),
        _ => None,
    }
}

/// The `aws ec2 run-instances` call for `count` boxes.
///
/// Pure, and the whole point of that: `aws up --dry-run` prints this exact argv,
/// so what will run on the account is reviewable before it costs anything. Every
/// value comes from the config — nothing is invented here.
///
/// `tag_value` is written to the marker tag and is what `ls`/`down` read back;
/// the caller passes the profile hash, so a box always says which build it was
/// launched for.
///
/// `root_device` comes from [`describe_image_args`] because the mapping must
/// name the image's own root device.
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
        // bill nobody looks at until it is large.
        "--block-device-mappings".into(),
        format!(
            "DeviceName={root_device},Ebs={{VolumeSize={},VolumeType=gp3,DeleteOnTermination=true}}",
            cfg.disk_gb
        ),
        // The marker is the safety mechanism, not decoration: `down` filters on
        // it, so an untagged box is one this tool can never terminate by
        // accident — including a stranger's.
        "--tag-specifications".into(),
        format!(
            "ResourceType=instance,Tags=[{{Key={},Value={}}},{{Key=Name,Value={}}}]",
            cfg.tag_key, tag_value, cfg.tag_key
        ),
        "--output".into(),
        "json".into(),
    ];
    // The role the *box* assumes — how it reads its own asset plane without a
    // key on disk. Required by `missing()` since the pool shape existed, and
    // never actually sent until now: a launch quietly produced boxes with no
    // role at all, which is the kind of gap that only shows up as "why can't
    // this box reach S3" a long way downstream.
    if !cfg.iam_instance_profile.trim().is_empty() {
        args.push("--iam-instance-profile".into());
        args.push(format!("Name={}", cfg.iam_instance_profile));
    }
    if cfg.spot {
        // No max-price: the on-demand ceiling is the sane default, and a
        // hard-coded bid is how a pool silently stops launching.
        args.push("--instance-market-options".into());
        args.push("MarketType=spot".into());
    }
    args
}

/// The `aws ec2 terminate-instances` call for explicit instance ids.
///
/// Takes ids rather than a filter on purpose. `down` resolves the ids from the
/// marker tag first and shows them, so the destructive step is always over a
/// list someone could read — never a pattern that might match something else.
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
    /// `InstanceLifecycle == "spot"` — worth showing, because a reclaimed spot
    /// box is a requeued task, not a lost one, and the operator should be able
    /// to tell which kind they are paying for.
    pub spot: bool,
    /// Public address, when the box has one. Empty until the box is running.
    pub public_ip: String,
    /// Private address. The fallback when there is no public one — but a box
    /// with **neither** is one the inductor cannot reach at all, and therefore
    /// one nothing can drive: it is the inductor that dials, so the address has
    /// to be reachable *from it*. See [`AwsConfig::subnet_id`].
    pub private_ip: String,
    /// The profile hash this box was launched for, from the marker tag.
    pub profile: String,
    pub launch_time: String,
}

/// Read `aws ec2 describe-instances --output json`.
///
/// Pure, and defensive on every field: this parses another program's JSON, so a
/// missing key is a normal event rather than a panic. `Reservations` is a list
/// of lists — the nesting is the API's shape, not a mistake here.
///
/// Returns `None` when the payload is not the shape we expect, so the caller can
/// say "the CLI answered something else" instead of reporting an empty account,
/// which would read as "nothing is running" and is the one wrong answer that
/// costs money.
pub fn parse_instances(json: &str, tag_key: &str) -> Option<Vec<AwsInstance>> {
    let doc: serde_json::Value = serde_json::from_str(json).ok()?;
    // Two shapes, and both are real. `describe-instances` nests instances inside
    // `Reservations`; `run-instances` answers with a single top-level `Instances`
    // array and no reservations at all. Reading only the first is why `aws up`
    // reported "the launch answered without any instances" while a box was in
    // fact running — the information was in the reply and the parser could not
    // see it.
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
///
/// An EC2 **public** address changes on every stop/start and every spot
/// relaunch, so a registry keyed by address drifts out of date — but the
/// instance id is stable for the box's whole life. `machine_from_instance`
/// stamps it into `Machine::note` (`"EC2 i-… (running)"`) and TUI state
/// updates may overwrite the note later, so this reads it back from wherever
/// it survives in the string. `None` for a box EC2 never launched (`:add`).
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
    // is a false match like an ip- hostname fragment.
    let hexlen = id[2..].len();
    if (8..=17).contains(&hexlen) && id[2..].chars().all(|c| c.is_ascii_hexdigit()) {
        Some(id.to_string())
    } else {
        None
    }
}

/// A new note for a machine, keeping any EC2 instance id the old note carried.
///
/// State updates rewrite the note freely ("provisioning (p)", the stale-verdict
/// refutation…), but the id is the box's one stable identity — lose it and a
/// relaunched box can never be re-found by [`ec2_id_from_note`]. Carried at the
/// tail, where the parser looks regardless of position.
pub fn preserve_ec2_id(old_note: &str, new_note: &str) -> String {
    match ec2_id_from_note(old_note) {
        Some(id) if ec2_id_from_note(new_note).is_none() => format!("{new_note} · EC2 {id}"),
        _ => new_note.to_string(),
    }
}

/// The machine a freshly launched instance *is*, made the moment the launch
/// reply names it.
///
/// This is the join the whole cloud path turns on: `RunInstances` returns
/// addresses, and the same job that reads that reply registers the box, so there
/// is no correlation problem later — no instance id on [`Machine`], no address
/// matching, no "which box is which". Nothing here remembers the EC2 id past
/// the note; the registry keys machines by address, exactly as `:add` does.
///
/// Public address when the box has one (the default VPC maps one on launch),
/// private otherwise — a genuinely private box is one the inductor cannot dial
/// anyway, so the private fallback is a best effort, not a promise. The key is
/// the `.pem` [`discover`] imported, which is the file nothing wired up before
/// this: the pool already knew where it lived, and every launch was one
/// forgotten `--key` from an unreachable afternoon.
///
/// Pure, so it is testable without a terminal or an account.
///
/// [`discover`]: AwsConfig::key_file
pub fn machine_from_instance(i: &AwsInstance, cfg: &AwsConfig) -> Machine {
    let addr = if !i.public_ip.is_empty() {
        i.public_ip.clone()
    } else {
        i.private_ip.clone()
    };
    let key = cfg.key_file().to_string_lossy().into_owned();
    let mut m = Machine::new(&addr, &cfg.ssh_user, 22, Some(key), "worker");
    // Born initializing, never `Unknown`: the account has just created this box
    // and nobody has spoken to it, so "we know it is booting" is the honest
    // state — and the one that carries a deadline. `Unknown` would read as
    // "never contacted", which is also true but says nothing about *why*, and
    // would let a box that never comes up sit there for ever.
    //
    // Both `pending` and `running` land here on purpose. EC2 calls an instance
    // `running` before sshd is listening, so a freshly launched box that says
    // `running` is still not reachable — reachability is proven by the probe,
    // not by the launch reply.
    m.set_state(MachineState::Initializing);
    m.note = format!("EC2 {} ({})", i.id, i.state);
    m
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

/// One line per box, in the shape the Machines pane uses.
///
/// The address is in here because `aws up` ends by telling you to
/// `provision --addr <ip>` — an instruction that is not actionable from the
/// output that gave it to you. Public if there is one, else private, else `-`
/// (a box that has not reached `running` yet has no address at all).
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
mod tests {
    use super::*;

    fn configured() -> AwsConfig {
        AwsConfig {
            region: "eu-central-1".into(),
            subnet_id: "subnet-0abc".into(),
            security_group_id: "sg-0abc".into(),
            iam_instance_profile: "storycast-worker".into(),
            keypairs: BTreeMap::from([("eu-central-1".to_string(), "storycast".to_string())]),
            images: BTreeMap::from([("eu-central-1".to_string(), "ami-0abc".to_string())]),
            bucket: "storycast-assets".into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_launched_box_becomes_a_machine_with_its_address_and_the_pool_key() {
        // The join made at birth: the same reply that names the box carries the
        // address the registry will key it by, and the key is the `.pem`
        // `discover` imported — not something the operator retypes.
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
        // this box and nobody has spoken to it, so "we know it is booting" is
        // the honest state — and the only one carrying a deadline. Note the
        // reply says `pending` here, but the assertion is about the state we
        // assign, which is the same for `running`: EC2 calls a box running
        // before sshd is listening, so reachability is proven by the probe.
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
        // A private-subnet box falls back to the private address — best
        // effort, because the public one is simply absent.
        let private = AwsInstance {
            public_ip: String::new(),
            ..instances[0].clone()
        };
        assert_eq!(machine_from_instance(&private, &cfg).addr, "172.31.19.210");
    }

    #[test]
    fn a_fresh_pool_says_exactly_what_is_missing() {
        // The first thing anyone sees. It must name the fields, not fail at
        // `RunInstances` with a message about one of them.
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
        // pool that has only been pointed at a region should not complain about
        // them.
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
        // keypair in another, so a single `keypair: String` works until the day
        // someone changes region — then `RunInstances` says the key does not
        // exist while it plainly does.
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
    fn the_object_key_is_the_hash_the_worker_already_verifies() {
        // One identity used twice: the pointer's hash names the object, and the
        // worker checks the same hash of the unpacked tree. Nothing to keep in
        // sync, and no way to hand a box a bundle its own pointer disagrees with.
        assert_eq!(
            profile_object("storycast-assets", "b20f7789f510").as_deref(),
            Some("s3://storycast-assets/profiles/b20f7789f510.tar.zst")
        );
        // Both halves absent is "the S3 path is off", not an error.
        assert_eq!(profile_object("", "b20f7789"), None);
        assert_eq!(profile_object("storycast-assets", "  "), None);
    }

    #[test]
    fn a_pool_without_a_bucket_says_so_instead_of_hiding_the_egress() {
        // An empty bucket is a working configuration — the rsync path — but it
        // is 668 MB of upload from this machine per box, and the summary is
        // where that has to be visible before the bill is.
        let mut c = configured();
        assert!(c.publishes_assets());
        assert!(
            c.summary().contains("s3://storycast-assets"),
            "{}",
            c.summary()
        );
        c.bucket = String::new();
        assert!(!c.publishes_assets());
        assert!(c.summary().contains("rsync from here"), "{}", c.summary());
    }

    #[test]
    fn the_launch_argv_is_reviewable_and_carries_the_marker() {
        // `aws up --dry-run` prints this argv, so it is the thing an operator
        // reads before anything costs money. Every value must come from the
        // config, and the marker tag must be present: `down` filters on it, so
        // a launch without it is a box this tool can never clean up.
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
        // with the box.
        assert!(joined.contains("DeviceName=/dev/sda1"), "{joined}");
        assert!(joined.contains("VolumeSize=30"), "{joined}");
        assert!(joined.contains("VolumeType=gp3"), "{joined}");
        assert!(joined.contains("DeleteOnTermination=true"), "{joined}");
        // The marker, with the profile hash as its value.
        assert!(
            joined
                .contains("ResourceType=instance,Tags=[{Key=storycast-worker,Value=b20f7789f510}"),
            "{joined}"
        );
        assert!(
            joined.contains("MarketType=spot"),
            "spot is on by default: {joined}"
        );
        // On-demand drops the market option entirely rather than asking for
        // on-demand explicitly — the two are not the same request.
        c.spot = false;
        assert!(!run_instances_args(&c, 1, "/dev/xvda", "h")
            .join(" ")
            .contains("MarketType"));
    }

    #[test]
    fn the_image_is_looked_up_rather_than_assumed() {
        // `/dev/sda1` on Ubuntu, `/dev/xvda` on Amazon Linux: assuming one
        // means `disk_gb` is silently ignored on half the images.
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
        // into `.bm/aws.json`, so what it chose stays visible and reviewable
        // instead of being re-resolved on every launch.
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
        // says which.
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
        // The real shape of the default group in a live account — a
        // self-referencing all-traffic rule and nothing else. It does **not**
        // let anyone in from outside, which is why a box launched into it hangs
        // on ssh and is never driven. The regression test for that afternoon.
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

        // A rule that admits an address, on the port it names — and *not* on the
        // task port, which is the mistake this check exists to catch: the box
        // would launch, accept ssh, and never be driven.
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
        // than warning on a guess.
        assert_eq!(admits_port("not json", 22), None);
        assert_eq!(admits_port("{}", 22), None);
    }

    #[test]
    fn a_name_list_is_split_and_a_sole_candidate_is_not_a_guess() {
        // `--output text` separates a list with tabs and newlines; empty is a
        // normal answer (an account with no keypairs), not an error.
        assert_eq!(
            parse_name_list("storycast\tbox-key\nother\n"),
            vec!["storycast", "box-key", "other"]
        );
        assert_eq!(parse_name_list("   \n\t "), Vec::<String>::new());
        assert_eq!(parse_name_list("None\n"), Vec::<String>::new());

        // One candidate is the only answer, so `discover` may fill the field.
        // Two is ambiguous and stays the operator's decision.
        let one = vec!["storycast".to_string()];
        assert_eq!(sole_name(&one), Some("storycast"));
        let two = vec!["a".to_string(), "b".to_string()];
        assert_eq!(sole_name(&two), None);
        assert_eq!(sole_name(&[]), None);
    }

    #[test]
    fn terminating_names_ids_and_never_a_filter() {
        // The destructive step is always over a list someone could have read.
        // A filter here would mean "terminate whatever matches", which is how
        // an autoscaler deletes the wrong account's boxes.
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
        // refusal naming the fix — not a launch against some default that
        // nobody chose.
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
        // `Reservations` — unlike `describe-instances`. Reading only the latter
        // made `aws up` print "the launch answered without any instances" while
        // the box was running, which is how this was found on a live account.
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
        // Neither shape is still `None` — not an empty account.
        assert_eq!(
            parse_instances(r#"{"Groups":[],"OwnerId":"1"}"#, DEFAULT_TAG),
            None
        );
    }

    #[test]
    fn a_cli_payload_we_do_not_understand_is_not_an_empty_account() {
        // The one wrong answer that costs money: reporting "nothing running"
        // when the truth is "the CLI said something else". An unparsable
        // payload must be `None`, never an empty list.
        assert_eq!(parse_instances("not json", DEFAULT_TAG), None);
        assert_eq!(parse_instances("{}", DEFAULT_TAG), None);
        assert_eq!(
            parse_instances(r#"{"Reservations":null}"#, DEFAULT_TAG),
            None
        );
        // A well-formed empty account *is* an empty list — the distinction is
        // the whole point.
        assert_eq!(
            parse_instances(r#"{"Reservations":[]}"#, DEFAULT_TAG),
            Some(vec![])
        );
    }

    #[test]
    fn instances_carry_the_profile_they_were_launched_for() {
        // Two boxes, one spot, one with a foreign tag that must not be read as
        // ours (it is filtered out upstream by the CLI; here it proves the tag
        // lookup is keyed, not positional).
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
        // `provision --addr <ip>`: public wins, private is the fallback.
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
        // repo so a clone knows what to fill in, and the values stay personal.
        // A field the local file omits keeps the template's value — otherwise a
        // local file that predates a field would silently revert to a compiled
        // default the template had deliberately changed.
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
        // the shipped shape, not the compiled defaults.
        let from_template = AwsConfig::load_layered(&dir);
        assert_eq!(from_template.instance_type, "c7i.xlarge");
        assert_eq!(from_template.keypair(), None, "no region chosen yet");
        assert_eq!(from_template.tag_key, "storycast-worker");

        // Local values win, field by field, and an omitted field keeps the
        // template's.
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
        // template is not lost the moment the local file names a region.
        assert_eq!(merged.keypair(), Some("team"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_shipped_template_parses_and_only_needs_the_account_fields() {
        // The tracked `aws.default.json` is a real parse target, not just
        // documentation: `aws init` seeds the operator's file from it. If it
        // ever stops matching the struct, every setup starts from a broken file.
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let cfg = AwsConfig::load_layered(&root);
        let missing = cfg.missing();
        // What the template cannot know: which account, which subnet, which
        // group, which profile, which keypair. Everything else it supplies.
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
        // `#[serde(default)]`, the same trick `Settings` relies on.
        std::fs::write(&path, r#"{"region":"eu-west-1"}"#).unwrap();
        let partial = AwsConfig::load(&path);
        assert_eq!(partial.region, "eu-west-1");
        assert_eq!(partial.instance_type, "c7i.xlarge");
        assert_eq!(partial.max_workers, 8);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
