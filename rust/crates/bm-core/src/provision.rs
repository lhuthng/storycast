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
use bm_proto::{Machine, MachineState};
use serde::{Deserialize, Serialize};
use std::path::Path;

mod ssh;
mod stamp;
mod steps;

pub use ssh::{KeySource, Ssh, resolve_key};
pub use stamp::{ProvisionStamp, compute_provision_stamp};
pub use steps::{Probe, provision};

/// Directory under the remote `$HOME` that holds a worker's whole world.
pub const REMOTE_DIR: &str = "bm-worker";

/// Port the Python TTS sidecar listens on.
pub const TTS_PORT: u16 = 8818;

/// One linked machine: how to reach a box plus everything `provision` needs to
/// prepare it, stored in `.bm/machines.json` (see `Layout::machines`) so it is
/// local-only by construction. Keyed by address — addresses are the identity
/// the ledger, the scheduler and the TUI already join on; the name is a human
/// handle for `provision --box` and may change.
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

/// Insert or replace one box by address. Writes are atomic; the file stays valid
/// if the process dies mid-save.
pub fn save_box(path: &Path, bxo: &LinkedBox) -> Result<()> {
    let mut boxes = load_boxes(path);
    if let Some(slot) = boxes.iter_mut().find(|b| b.addr == bxo.addr) {
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

/// Drop one box by address. Missing entries are not an error.
pub fn remove_box(path: &Path, addr: &str) -> Result<()> {
    let mut boxes = load_boxes(path);
    boxes.retain(|b| b.addr != addr);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    crate::atomic_write(path, &serde_json::to_string_pretty(&boxes)?)?;
    Ok(())
}

/// What the cluster thinks of a box right now: liveness, probe output,
/// worker-reported facts. Lives in `ledger.json` under `machine_state`,
/// keyed by address; everything about *reaching* the box lives in
/// `machines.json` instead.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MachineRuntime {
    pub state: MachineState,
    #[serde(default)]
    pub last_seen: u64,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub tts_url: Option<String>,
}

/// Split a joined `Machine` into config (`machines.json`) and runtime
/// (`ledger.json`). `name` is the box's handle; existing boxes keep theirs,
/// new ones take the caller's fallback (usually the address or hostname).
pub fn split_machine(m: &Machine, name: &str) -> (LinkedBox, MachineRuntime) {
    let bxo = LinkedBox {
        name: name.to_string(),
        addr: m.addr.clone(),
        user: m.ssh_user.clone(),
        port: m.ssh_port,
        key: m.ssh_key.clone(),
        role: m.role.clone(),
    };
    let rt = MachineRuntime {
        state: m.state,
        last_seen: m.last_seen,
        note: m.note.clone(),
        capabilities: m.capabilities.clone(),
        tts_url: m.tts_url.clone(),
    };
    (bxo, rt)
}

/// Rebuild the `Machine` shape the API and the TUI speak: config fields from
/// the box, runtime fields from the ledger. A missing runtime means
/// never-seen (a linked box that has not provisioned yet) — it still joins,
/// so bound boxes are visible before their first beat.
pub fn join_machine(bxo: &LinkedBox, rt: Option<&MachineRuntime>) -> Machine {
    let rt = rt.cloned().unwrap_or_default();
    let mut m = Machine::new(&bxo.addr, &bxo.user, bxo.port, bxo.key.clone(), &bxo.role);
    m.state = rt.state;
    m.last_seen = rt.last_seen;
    m.note = rt.note;
    m.capabilities = rt.capabilities;
    m.tts_url = rt.tts_url;
    m
}

/// Join every known box with its runtime, sorted by address. Runtime without
/// a box (a hand-edited file, an older drop) synthesizes config from the
/// field defaults rather than silently dropping a machine's liveness.
pub fn join_all(
    boxes: Vec<LinkedBox>,
    rt: &serde_json::Map<String, serde_json::Value>,
) -> Vec<Machine> {
    let mut runtimes: std::collections::HashMap<String, MachineRuntime> = std::collections::HashMap::new();
    for (addr, v) in rt {
        if let Ok(r) = serde_json::from_value::<MachineRuntime>(v.clone()) {
            runtimes.insert(addr.clone(), r);
        }
    }
    let mut out = Vec::new();
    for bxo in &boxes {
        out.push(join_machine(bxo, runtimes.get(&bxo.addr)));
    }
    for (addr, r) in &runtimes {
        if !boxes.iter().any(|b| &b.addr == addr) {
            let bxo: LinkedBox = serde_json::from_value(serde_json::json!({
                "name": addr, "addr": addr,
            }))
            .expect("name+addr with serde defaults always parses");
            out.push(join_machine(&bxo, Some(r)));
        }
    }
    out.sort_by(|a, b| a.addr.cmp(&b.addr));
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn linked_boxes_round_trip_and_upsert_by_addr() {
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
        // Same address re-binds in place (the name may change); a new
        // address adds a second box.
        let again = super::LinkedBox { name: "renamed".into(), ..bxo.clone() };
        super::save_box(&path, &again).unwrap();
        let other = super::LinkedBox { name: "box-2".into(), addr: "10.0.0.9".into(), ..bxo.clone() };
        super::save_box(&path, &other).unwrap();

        let boxes = super::load_boxes(&path);
        assert_eq!(boxes.len(), 2, "same addr replaces, new addr adds");
        let first = boxes.iter().find(|b| b.addr == "192.168.2.2").unwrap();
        assert_eq!(first.name, "renamed");

        super::remove_box(&path, "192.168.2.2").unwrap();
        let boxes = super::load_boxes(&path);
        assert_eq!(boxes.len(), 1);
        assert_eq!(boxes[0].addr, "10.0.0.9");
        // Missing entries are not an error.
        super::remove_box(&path, "192.168.2.2").unwrap();

        let m = super::LinkedBox {
            name: "box-1".into(),
            addr: "10.0.0.9".into(),
            user: "thang".into(),
            port: 22,
            key: Some("/k/id".into()),
            role: "worker".into(),
        }
        .machine();
        assert_eq!(m.ssh_target(), "thang@10.0.0.9");
        assert_eq!(m.tts_url.as_deref(), Some("http://127.0.0.1:8818"));
    }

    #[test]
    fn split_join_roundtrips_config_and_runtime() {
        let m = bm_proto::Machine::new("192.168.2.2", "thang", 2222, Some("~/.ssh/k".into()), "worker");
        let (bxo, rt) = super::split_machine(&m, "box-1");
        assert_eq!((bxo.name.as_str(), bxo.addr.as_str(), bxo.port), ("box-1", "192.168.2.2", 2222));
        assert_eq!(bxo.key.as_deref(), Some("~/.ssh/k"));
        // No runtime yet: a bound-but-never-seen box still joins, as Unknown.
        let joined = super::join_machine(&bxo, None);
        assert_eq!(joined.addr, "192.168.2.2");
        assert_eq!(joined.ssh_key.as_deref(), Some("~/.ssh/k"));
        assert_eq!(joined.state, bm_proto::MachineState::Unknown);
        let joined = super::join_machine(&bxo, Some(&rt));
        assert_eq!(joined.ssh_target(), "thang@192.168.2.2");
    }
}
