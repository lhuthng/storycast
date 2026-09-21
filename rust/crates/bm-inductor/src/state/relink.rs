//! Auto-relink: keep EC2-launched boxes pointed at the address they carry now.
//!
//! An EC2 public address changes on every stop/start and every spot relaunch,
//! so a registry keyed by address drifts out of date the moment a box cycles.
//! The instance id is stable for the box's whole life and is stamped in the
//! machine's note, so the account read is enough to repair the drift without an
//! operator selecting anything: a box whose public address moved is re-keyed to
//! the address it has now, and an agent-reported *private*-address ghost of the
//! same instance is folded into the real entry. Every repair is returned as a
//! log line so the events pane explains what changed and why.

use super::Inner;
use bm_core::provision::{ec2_id_from_note, remove_box, save_box, split_machine, AwsInstance};

impl Inner {
    /// Reconcile the registry with one account listing. Returns the log lines
    /// describing every repair (empty when nothing drifted).
    pub fn relink_drifted(&mut self, instances: &[AwsInstance]) -> Vec<String> {
        let mut log = Vec::new();
        for i in instances {
            // A box we cannot dial is not one we can relink to.
            if i.public_ip.is_empty() {
                continue;
            }
            // The real entry: whichever machine carries this instance's id.
            let canonical: Option<String> = self
                .machines
                .iter()
                .find(|(_, m)| ec2_id_from_note(&m.note).as_deref() == Some(i.id.as_str()))
                .map(|(a, _)| a.clone());
            // The ghost: a machine the agent registered at the instance's
            // *private* address — the same box seen through a second address.
            let ghost: Option<String> = (!i.private_ip.is_empty())
                .then(|| {
                    self.machines
                        .iter()
                        .find(|(a, _)| a.as_str() == i.private_ip.as_str())
                        .map(|(a, _)| a.clone())
                })
                .flatten();

            // Adopt a ghost as the real entry when the launch entry is gone
            // (a hand-edited registry, a dropped box that kept beating).
            let canonical = match canonical {
                Some(c) => Some(c),
                None => match &ghost {
                    Some(g) => {
                        if let Some(m) = self.machines.get_mut(g) {
                            m.note = bm_core::provision::preserve_ec2_id(
                                &m.note,
                                &format!("adopted from beat · EC2 {} ({})", i.id, i.state),
                            );
                        }
                        log.push(format!(
                            "relink: adopted {} (agent-reported address) as EC2 {}",
                            g, i.id
                        ));
                        Some(g.clone())
                    }
                    None => None,
                },
            };
            let Some(canon) = canonical else { continue };

            let current = self
                .machines
                .get(&canon)
                .map(|m| m.addr.clone())
                .unwrap_or_default();
            if current != i.public_ip {
                if self.machines.contains_key(&i.public_ip) {
                    log.push(format!(
                        "relink: {} already registered at {} — leaving {} as-is",
                        i.id, i.public_ip, canon
                    ));
                } else {
                    let mut m = self
                        .machines
                        .remove(&canon)
                        .expect("canon was found by key just above");
                    let name = if m.name.is_empty() {
                        canon.clone()
                    } else {
                        m.name.clone()
                    };
                    m.addr = i.public_ip.clone();
                    m.id = i.public_ip.clone();
                    for v in self.workers.values_mut() {
                        if *v == canon {
                            *v = i.public_ip.clone();
                        }
                    }
                    // Config file: the box moves with its name, key and policy.
                    let (bxo, _) = split_machine(&m, &name);
                    let _ = remove_box(&self.layout.machines(), &canon);
                    let _ = save_box(&self.layout.machines(), &bxo);
                    self.machines.insert(i.public_ip.clone(), m);
                    log.push(format!(
                        "relink: {name} {canon} → {} · EC2 {} (address rotated)",
                        i.public_ip, i.id
                    ));
                }
            }
            // Fold the ghost once the real entry is in place.
            if let Some(g) = ghost {
                if g != i.public_ip && g != canon {
                    self.machines.remove(&g);
                    for v in self.workers.values_mut() {
                        if *v == g {
                            *v = i.public_ip.clone();
                        }
                    }
                    let _ = remove_box(&self.layout.machines(), &g);
                    log.push(format!(
                        "relink: dropped stale ghost {g} — same box (EC2 {}) at its private address",
                        i.id
                    ));
                }
            }
        }
        if !log.is_empty() {
            self.save();
        }
        log
    }
}
