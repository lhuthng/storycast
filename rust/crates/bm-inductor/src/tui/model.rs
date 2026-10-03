//! Pure selection: filters, cast rows, task queries. No widgets, no keys.
use crate::tui::{
    app::App,
    style::{gender_label, style_of, worker_alias, Level, LogLine},
};
use bm_proto::{Heartbeat, Machine, MachineState, Roster, Stage, Task, TaskState, VoiceInfo};
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;

/// Fold Vietnamese diacritics to ASCII so a filter of `thai son` matches
/// `Thái Sơn`. Without it, filtering a Vietnamese cast means typing exact
/// diacritics on every keystroke. The table lives in `bm_core::util` (shared
/// with the segment-voice matcher); this re-export keeps call sites unchanged.
pub(crate) use bm_core::util::fold;

pub(crate) fn matches(filter: &str, haystack: &str) -> bool {
    let f = fold(filter.trim());
    f.is_empty() || fold(haystack).contains(&f)
}

/// Registry as persisted on disk: connection config joined with runtime, no
/// liveness. The fallback behind `effective_machines` when the inductor is
/// unreachable. Reads both shapes: the current `machine_state` + machines.json
/// join, and the pre-migration `machines` array.
///
/// Takes the layout, not a root: the machine states live in the *workspace's*
/// ledger while `machines.json` stays at the root, so a root-only path read a
/// file that no longer exists the moment a workspace was selected.
pub(crate) fn registry_machines(layout: &bm_core::Layout) -> Vec<Machine> {
    if layout.root.as_os_str().is_empty() {
        return Vec::new();
    }
    let text = match std::fs::read_to_string(layout.ledger()) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let doc: serde_json::Value = match serde_json::from_str(&text) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    if let Some(a) = doc.get("machines").and_then(|m| m.as_array()) {
        let mut out: Vec<Machine> = a
            .iter()
            .filter_map(|v| serde_json::from_value(v.clone()).ok())
            .collect();
        out.sort_by(|a, b| a.addr.cmp(&b.addr));
        return out;
    }
    let boxes = bm_core::provision::load_boxes(&layout.machines());
    let empty = serde_json::Map::new();
    let rt = doc
        .get("machine_state")
        .and_then(|v| v.as_object())
        .unwrap_or(&empty);
    bm_core::provision::join_all(boxes, rt)
}

/// What to call the active workspace in the footer: its directory name, or
/// `default` for the implicit root workspace (no pointer, `work == root`).
pub(crate) fn workspace_label(layout: &bm_core::Layout) -> String {
    if layout.work == layout.root {
        return "default".into();
    }
    layout
        .work
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| layout.work.display().to_string())
}

/// What to call the loaded profile in the footer. `none` is not a placeholder
/// for "unknown": it is the state every runner refuses to start in, so the
/// footer says so plainly.
/// The pieces this workspace is bound to, and the pack's hash.
///
/// All three names, because they change independently and a mismatch is worth
/// seeing before a run rather than after one: `xianxia · vi-VN · vieneu
/// (6b8d5fc00761)`. The hash stays the pack's — it is the one the operator
/// loaded and the one `profile pack` writes back.
pub(crate) fn profile_label(profile: Option<&bm_core::profile::Binding>) -> String {
    match profile {
        Some(binding) if !binding.is_unset() => {
            let hash = &binding.pack.hash;
            if hash.is_empty() {
                bm_core::profile::label(binding)
            } else {
                format!(
                    "{} ({})",
                    bm_core::profile::label(binding),
                    &hash[..12.min(hash.len())]
                )
            }
        }
        _ => "none".into(),
    }
}

pub(crate) fn filtered_characters(app: &App, filter: &str) -> Vec<String> {
    match &app.roster {
        None => Vec::new(),
        Some(r) => r
            .characters
            .iter()
            .filter(|c| matches(filter, c))
            .cloned()
            .collect(),
    }
}

pub(crate) mod cast;
pub(crate) mod log;
pub(crate) mod machines;
pub(crate) mod tasks;
pub(crate) mod voices;

pub(crate) use cast::*;
pub(crate) use log::*;
pub(crate) use machines::*;
pub(crate) use tasks::*;
pub(crate) use voices::*;
