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
            bucket: String::new(),
            spot: true,
            max_workers: 8,
            ttl_hours: 6,
            tag_key: DEFAULT_TAG.into(),
        }
    }
}

impl AwsConfig {
    /// Read the pool definition. A missing file is not an error — it means the
    /// pool has never been set up, which is a state [`Self::missing`] describes.
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
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
