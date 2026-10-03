//! Machine onboarding: probe, distribute, verify.

use anyhow::{Context, Result};
use bm_proto::{Machine, MachineState};
use serde::{Deserialize, Serialize};
use std::path::Path;

mod aws;
/// Public rather than re-exported: the callers that need it are the CLI's
pub mod aws_credentials;
/// What a worker is handed, selected by its work policy. Public because the
pub mod sources;
mod ssh;
mod stamp;
mod steps;

pub use aws::{
    admits_port, awaiting_onboard, default_security_group_args, default_subnet_args,
    describe_image_args, describe_security_group_args, ec2_id_from_note, instance_line,
    instance_note, instance_profile_names_args, keypair_names_args, machine_from_instance,
    parse_instances, parse_name_list, preserve_ec2_id, run_instances_args, sole_name,
    terminate_args, ubuntu_ami_args, AwsConfig, AwsInstance, AWAITING_ONBOARD, DEFAULT_FILE,
    DEFAULT_TAG, REQUIRED_INGRESS, UBUNTU_LTS,
};
pub use ssh::{resolve_key, KeySource, RsyncProgress, Ssh};
pub use stamp::{compute_provision_stamp, ProvisionStamp};
pub use steps::{provision, LiveLog, Probe};

/// Directory under the remote `$HOME` that holds a worker's whole world.
pub const REMOTE_DIR: &str = "bm-worker";

/// Port the Python TTS sidecar listens on.
pub const TTS_PORT: u16 = 8818;

/// One linked machine: how to reach a box plus everything `provision` needs to
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
    /// Per-machine work policy: which stages this box may run and in what
    #[serde(default)]
    pub task_policy: Option<Vec<bm_proto::TaskPref>>,
    /// Parked by the operator: takes no new work until switched back on.
    #[serde(default = "default_true")]
    pub accepting_work: bool,
    /// ONNX intra-op threads this box's sidecar should open with. Config, like
    #[serde(default)]
    pub tts_threads: Option<u16>,
}

fn default_true() -> bool {
    true
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
        let mut m = Machine::new(
            &self.addr,
            &self.user,
            self.port,
            self.key.clone(),
            &self.role,
        );
        m.tts_url = Some(format!("http://127.0.0.1:{TTS_PORT}"));
        m.task_policy = self.task_policy.clone();
        m.accepting_work = self.accepting_work;
        m.tts_threads = self.tts_threads;
        m
    }
}

/// Read the linked boxes, or an empty list when nothing is linked yet. A
pub fn load_boxes(path: &Path) -> Vec<LinkedBox> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Insert or replace one box by address. Writes are atomic; the file stays valid
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
pub fn split_machine(m: &Machine, name: &str) -> (LinkedBox, MachineRuntime) {
    let bxo = LinkedBox {
        name: name.to_string(),
        addr: m.addr.clone(),
        user: m.ssh_user.clone(),
        port: m.ssh_port,
        key: m.ssh_key.clone(),
        role: m.role.clone(),
        task_policy: m.task_policy.clone(),
        accepting_work: m.accepting_work,
        tts_threads: m.tts_threads,
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
pub fn join_machine(bxo: &LinkedBox, rt: Option<&MachineRuntime>) -> Machine {
    let rt = rt.cloned().unwrap_or_default();
    let mut m = Machine::new(&bxo.addr, &bxo.user, bxo.port, bxo.key.clone(), &bxo.role);
    // The one human-chosen handle for this box. `id` stays the address
    m.name = bxo.name.clone();
    m.state = rt.state;
    m.last_seen = rt.last_seen;
    m.note = rt.note;
    m.capabilities = rt.capabilities;
    m.tts_url = rt.tts_url;
    m.task_policy = bxo.task_policy.clone();
    m.accepting_work = bxo.accepting_work;
    m.tts_threads = bxo.tts_threads;
    m
}

/// Join every known box with its runtime, sorted by address. Runtime without
pub fn join_all(
    boxes: Vec<LinkedBox>,
    rt: &serde_json::Map<String, serde_json::Value>,
) -> Vec<Machine> {
    let mut runtimes: std::collections::HashMap<String, MachineRuntime> =
        std::collections::HashMap::new();
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
            let mut m = join_machine(&bxo, Some(r));
            // **A box with no config record takes nothing.** The runtime
            m.task_policy = Some(bm_proto::TaskPref::nothing());
            out.push(m);
        }
    }
    out.sort_by(|a, b| a.addr.cmp(&b.addr));
    out
}

#[cfg(test)]
mod tests {
    /// A `machines.json` written before parking existed is a set of boxes that
    #[test]
    fn a_box_from_before_parking_existed_still_takes_work() {
        let old = r#"{"name":"box-1","addr":"192.168.2.2","user":"thang","port":22}"#;
        let bxo: super::LinkedBox = serde_json::from_str(old).unwrap();
        assert!(bxo.accepting_work, "absent must read as awake");
        assert!(bxo.task_policy.is_none(), "and so must an absent policy");
        // And the flag survives the config/runtime split, in both directions —
        let mut m = bxo.machine();
        assert!(!m.relaxed());
        m.accepting_work = false;
        let (written, _rt) = super::split_machine(&m, "box-1");
        assert!(!written.accepting_work);
        assert!(
            super::join_machine(&written, None).relaxed(),
            "a park that did not survive the round trip would be lost on the next save"
        );
    }

    /// A box whose config record is gone but whose runtime row stayed must not
    #[test]
    fn a_box_with_no_config_record_is_a_box_that_works_on_nothing() {
        let rt = serde_json::json!({
            "192.168.2.2": {"state": "online", "last_seen": 123, "note": ""},
        });
        let joined = super::join_all(Vec::new(), rt.as_object().unwrap());
        assert_eq!(joined.len(), 1, "liveness is still reported");
        let m = &joined[0];
        assert_eq!(m.addr, "192.168.2.2");
        assert_eq!(m.state, bm_proto::MachineState::Online);
        assert!(
            m.effective_task_policy().iter().all(|p| !p.enabled),
            "no config record is no stage it may run: {:?}",
            m.effective_task_policy()
        );
        assert!(
            super::sources::stages_of(&m.effective_task_policy()).is_empty(),
            "so nothing is selected for it either"
        );

        // A *linked* box with no policy is a different thing entirely: the
        let bxo = super::LinkedBox {
            name: "box-1".into(),
            addr: "10.0.0.5".into(),
            user: "thang".into(),
            port: 22,
            key: None,
            role: "worker".into(),
            task_policy: None,
            accepting_work: true,
            tts_threads: None,
        };
        let one = super::join_all(vec![bxo], &serde_json::Map::new());
        assert!(
            one[0].effective_task_policy().iter().all(|p| p.enabled),
            "a configured box with no policy still runs the default"
        );
    }

    #[test]
    fn linked_boxes_round_trip_and_upsert_by_addr() {
        let dir = std::env::temp_dir().join("bm-provision-boxes");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("machines.json");
        assert!(
            super::load_boxes(&path).is_empty(),
            "missing file, not an error"
        );

        let bxo = super::LinkedBox {
            name: "box-1".into(),
            addr: "192.168.2.2".into(),
            user: "thang".into(),
            port: 22,
            key: Some("/k/id".into()),
            role: "worker".into(),
            task_policy: None,
            accepting_work: true,
            tts_threads: None,
        };
        super::save_box(&path, &bxo).unwrap();
        // Same address re-binds in place (the name may change); a new
        let again = super::LinkedBox {
            name: "renamed".into(),
            ..bxo.clone()
        };
        super::save_box(&path, &again).unwrap();
        let other = super::LinkedBox {
            name: "box-2".into(),
            addr: "10.0.0.9".into(),
            ..bxo.clone()
        };
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
            task_policy: None,
            accepting_work: true,
            tts_threads: None,
        }
        .machine();
        assert_eq!(m.ssh_target(), "thang@10.0.0.9");
        assert_eq!(m.tts_url.as_deref(), Some("http://127.0.0.1:8818"));
    }

    #[test]
    fn split_join_roundtrips_config_and_runtime() {
        let m = bm_proto::Machine::new(
            "192.168.2.2",
            "thang",
            2222,
            Some("~/.ssh/k".into()),
            "worker",
        );
        let (bxo, rt) = super::split_machine(&m, "box-1");
        assert_eq!(
            (bxo.name.as_str(), bxo.addr.as_str(), bxo.port),
            ("box-1", "192.168.2.2", 2222)
        );
        assert_eq!(bxo.key.as_deref(), Some("~/.ssh/k"));
        // No runtime yet: a bound-but-never-seen box still joins, as Unknown.
        let joined = super::join_machine(&bxo, None);
        assert_eq!(joined.addr, "192.168.2.2");
        assert_eq!(joined.ssh_key.as_deref(), Some("~/.ssh/k"));
        assert_eq!(joined.state, bm_proto::MachineState::Unknown);
        let joined = super::join_machine(&bxo, Some(&rt));
        assert_eq!(joined.ssh_target(), "thang@192.168.2.2");
    }

    #[test]
    fn join_carries_the_registry_handle_id_stays_the_address() {
        // The panes can only agree with the provision log if the handle
        let m = bm_proto::Machine::new("192.168.2.2", "thang", 22, None, "worker");
        let (bxo, rt) = super::split_machine(&m, "hawk");
        let joined = super::join_machine(&bxo, Some(&rt));
        assert_eq!(joined.id, "192.168.2.2");
        assert_eq!(joined.name, "hawk");
    }
}
