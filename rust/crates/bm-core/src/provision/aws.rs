//! AWS as a *source of worker addresses*.

use anyhow::{Context, Result};
use bm_proto::{Machine, MachineState};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// The tag every box this tool starts carries, so `ls`/`down` can find ours and
pub const DEFAULT_TAG: &str = "storycast-worker";

/// The tracked template at the repo root, beside `voices.default.json` and for
pub const DEFAULT_FILE: &str = "aws.default.json";

/// One AWS worker pool. Everything a launch needs, written once.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AwsConfig {
    /// `eu-central-1`, `us-east-1`, … Empty means "not set up yet".
    pub region: String,
    /// Instance type. The TTS path is a hand-written SIMD matvec on CPU and
    pub instance_type: String,
    /// Root volume, GB. Models 668 MB, the profile 57 MB, plus the segments a
    pub disk_gb: u32,
    /// The subnet the boxes land in, and it must be one **the inductor can
    pub subnet_id: String,
    /// Security group. **Ingress** is what matters, on two ports, 22 for
    pub security_group_id: String,
    /// IAM instance profile **name**. Required by [`AwsConfig::missing`], because
    pub iam_instance_profile: String,
    /// Login on the box. `ubuntu` on the stock Ubuntu AMIs, `ec2-user` on AL.
    pub ssh_user: String,
    /// EC2 keypair **name** per region. The private half never leaves this
    pub keypairs: BTreeMap<String, String>,
    /// AMI per region, because an image is one region's copy.
    pub images: BTreeMap<String, String>,
    /// Ask for spot capacity. Render and merge are both idempotent and the
    pub spot: bool,
    /// Hard cap on concurrently registered cloud boxes. Refuses a launch that
    pub max_workers: u32,
    /// Default lifetime in hours. The box drains and stops at the deadline; a
    pub ttl_hours: u32,
    /// The marker tag **key** every box we start carries, so `ls`/`down` can
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
            spot: true,
            max_workers: 8,
            ttl_hours: 6,
            tag_key: DEFAULT_TAG.into(),
        }
    }
}

impl AwsConfig {
    /// Read the pool definition from one file. A missing file is not an error
    pub fn load(path: &Path) -> Self {
        read_doc(path)
            .and_then(|d| serde_json::from_value(d).ok())
            .unwrap_or_default()
    }

    /// The effective pool: the local values over the tracked template over the
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
    pub fn keypair(&self) -> Option<&str> {
        self.keypairs.get(&self.region).map(String::as_str)
    }

    /// The AMI for the configured region, if one is set.
    pub fn image(&self) -> Option<&str> {
        self.images.get(&self.region).map(String::as_str)
    }

    /// Where this region's private half lives, as a path relative to the repo
    pub fn key_file(&self) -> std::path::PathBuf {
        std::path::PathBuf::from(".bm")
            .join("aws")
            .join(format!("{}.pem", self.region))
    }

    /// What still has to be filled in before a launch can run.
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
                "no keypair for region {:?}, add it to `keypairs` (an EC2 keypair belongs to one region)",
                self.region
            ));
        }
        if self.image().is_none() {
            out.push(format!(
                "no AMI for region {:?}, run `bm-inductor aws discover` to fill it in, or set `images` by hand",
                self.region
            ));
        }
        if self.max_workers == 0 {
            out.push("max_workers is 0, no launch can ever be within the cap".into());
        }
        if self.ttl_hours == 0 {
            out.push("ttl_hours is 0, boxes would outlive their work by design".into());
        }
        out
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
            "assets: rsync from here",
            self.spot,
            self.max_workers,
            self.ttl_hours,
        )
    }
}

/// One JSON file, or `None` when it is absent or unreadable.
fn read_doc(path: &Path) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// `over` wins, key by key; nested objects merge rather than replace.
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

mod ec2;
pub use ec2::*;

#[cfg(test)]
mod tests;
