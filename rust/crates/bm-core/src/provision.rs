//! Machine onboarding: probe, distribute, verify.
//!
//! New in the cluster version — the legacy `swarm` hardcoded one box in three
//! separate places. Here any machine is reachable by address, and the
//! provisioner answers the question that matters before doing any work:
//! *is this box already configured, or do we have to push to it?*
//!
//! Everything shells out to `ssh` and `rsync` rather than linking an SSH
//! library. That keeps the build small, reuses the user's existing keys and
//! `~/.ssh/config`, and makes the exact command visible in the TUI log.

use anyhow::{Context, Result};
use bm_proto::Machine;
use serde::{Deserialize, Serialize};
use std::path::Path;

mod ssh;
mod stamp;
mod steps;

pub use ssh::Ssh;
pub use stamp::{ProvisionStamp, compute_provision_stamp};
pub use steps::{Probe, provision};

/// Directory under the remote `$HOME` that holds a worker's whole world.
pub const REMOTE_DIR: &str = "bm-worker";

/// Port the Python TTS sidecar listens on.
pub const TTS_PORT: u16 = 8818;

/// One linked machine: how to reach a box plus everything `provision` needs to
/// prepare it, stored in `.bm/machines.json` (see `Layout::machines`) so it is
/// local-only by construction. A name, not an address, is the handle —
/// addresses change, the box does not.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkedBox {
    pub name: String,
    pub addr: String,
    #[serde(default = "default_ssh_user")]
    pub user: String,
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default = "default_role")]
    pub role: String,
}

fn default_ssh_user() -> String {
    "thang".into()
}

fn default_ssh_port() -> u16 {
    22
}

fn default_role() -> String {
    "worker".into()
}

impl LinkedBox {
    /// The runtime machine `provision` and the scheduler speak.
    pub fn machine(&self) -> Machine {
        let mut m = Machine::new(&self.addr, &self.user, self.port, self.key.clone(), &self.role);
        m.tts_url = Some(format!("http://127.0.0.1:{TTS_PORT}"));
        m
    }
}

/// Read the linked boxes, or an empty list when nothing is linked yet. A
/// missing file is not an error — it just means `link` has never run.
pub fn load_boxes(path: &Path) -> Vec<LinkedBox> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Insert or replace one box by name. Writes are atomic; the file stays valid
/// if the process dies mid-save.
pub fn save_box(path: &Path, bxo: &LinkedBox) -> Result<()> {
    let mut boxes = load_boxes(path);
    if let Some(slot) = boxes.iter_mut().find(|b| b.name == bxo.name) {
        *slot = bxo.clone();
    } else {
        boxes.push(bxo.clone());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    crate::atomic_write(path, &serde_json::to_string_pretty(&boxes)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn linked_boxes_round_trip_and_upsert_by_name() {
        let dir = std::env::temp_dir().join("bm-provision-boxes");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("machines.json");
        assert!(super::load_boxes(&path).is_empty(), "missing file, not an error");

        let bxo = super::LinkedBox {
            name: "box-1".into(),
            addr: "192.168.2.2".into(),
            user: "thang".into(),
            port: 22,
            key: Some("/k/id".into()),
            role: "worker".into(),
        };
        super::save_box(&path, &bxo).unwrap();
        let again = super::LinkedBox { addr: "10.0.0.9".into(), ..bxo.clone() };
        super::save_box(&path, &again).unwrap();

        let boxes = super::load_boxes(&path);
        assert_eq!(boxes.len(), 1, "same name replaces, never duplicates");
        assert_eq!(boxes[0].addr, "10.0.0.9");

        let m = boxes[0].machine();
        assert_eq!(m.ssh_target(), "thang@10.0.0.9");
        assert_eq!(m.tts_url.as_deref(), Some("http://127.0.0.1:8818"));
    }
}
