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
/// Credentials belong in neither file. They come from the standard AWS chain,
/// which is also what makes this shareable: two people with the same repo and
/// different accounts each set up their own `.bm/aws.json` and neither can
/// commit the other's account IDs by accident.
pub const DEFAULT_FILE: &str = "aws.default.json";

/// One AWS worker pool. Everything a launch needs, written once.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AwsConfig {
    /// `eu-central-1`, `us-east-1`, … Empty means "not set up yet".
    pub region: String,
    /// Instance type. The TTS path is a hand-written SIMD matvec on CPU and
    /// there is no GPU code, so this is a CPU choice: 2 vCPU is the floor
    /// (models 668 MB + a working set), 4 is where render stops queueing
    /// behind itself.
    pub instance_type: String,
    /// Root volume, GB. Models 668 MB, the profile 57 MB, plus the segments a
    /// chapter accumulates; 30 is comfortable and cheap.
    pub disk_gb: u32,
    /// Private subnet. The workers dial the inductor *out*, so nothing here
    /// needs a public address for the cluster to work.
    pub subnet_id: String,
    /// Security group. Its egress is what matters (S3, the inductor); ingress
    /// only has to allow ssh from wherever this machine is.
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
    /// Deliberately explicit rather than looked up: the AMI decides what
    /// actually runs on the account, so it is a decision to make and see, not
    /// one to resolve from a moving "latest" pointer. `aws init` prints the
    /// command that resolves the current Ubuntu LTS in a region. It must match
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
                "no AMI for region {:?} — add it to `images`; `aws init` prints the lookup for the current Ubuntu LTS",
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
    let reservations = doc.get("Reservations")?.as_array()?;
    let mut out = Vec::new();
    for res in reservations {
        let Some(instances) = res.get("Instances").and_then(|i| i.as_array()) else {
            continue;
        };
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
                profile: tag,
                launch_time: s("LaunchTime"),
            });
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Some(out)
}

/// One line per box, in the shape the Machines pane uses.
pub fn instance_line(i: &AwsInstance) -> String {
    format!(
        "{:<20} {:<14} {:<9} {:<16} {:<5} {}",
        i.id,
        i.instance_type,
        i.state,
        i.az,
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
             "Tags":[{"Key":"Name","Value":"other"},{"Key":"storycast-worker","Value":"b20f7789f510"}]},
            {"InstanceId":"i-0bbb","InstanceType":"c7i.large",
             "State":{"Name":"pending"},"Placement":{"AvailabilityZone":"eu-central-1b"},
             "LaunchTime":"2026-09-20T12:05:00+00:00",
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
        // Fields the API may omit must not panic or shift the row.
        assert_eq!(got[2].instance_type, "");
        assert_eq!(got[2].state, "stopped");
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
