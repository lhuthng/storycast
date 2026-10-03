use super::*;
use anyhow::{bail, Result};

/// `assets/pack.json`: the other assets this one is built on, weakest first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pack {
    #[serde(default)]
    pub deps: Vec<String>,
}

/// One dependency, as it stood when the last resolve folded it in.
/// The hash is the point: it is what lets a child say "I was built against
/// `common` at *this* content" and therefore be told, cheaply and exactly, that
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DepRecord {
    pub name: String,
    /// A content hash of the dependency's whole tree.
    pub hash: String,
}

/// `assets/_extends.json`: what a resolve put in the live tree, and from where.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inherited {
    /// The **direct** dependencies folded in, in `pack.json` order — what a
    #[serde(default)]
    pub deps: Vec<DepRecord>,
    /// The whole **closure**, weakest first: every pack the graph reaches, once,
    #[serde(default)]
    pub tree: Vec<PackNode>,
    /// Which **release** each unpacked dependency came from, by pack name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub versions: BTreeMap<String, String>,
    /// Per registry filename, `key -> hash of the value inserted`.
    #[serde(default)]
    pub keys: BTreeMap<String, BTreeMap<String, String>>,
    /// Per file copied in, relative to `assets/`, its content hash.
    #[serde(default)]
    pub files: BTreeMap<String, String>,
}

/// One pack in the composition's **closure**, weakest first, and how the graph
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackNode {
    pub name: String,
    /// A content hash of the pack's own tree — the number a release, a child's
    pub hash: String,
    /// Whether the live `pack.json` names it directly.
    #[serde(default)]
    pub direct: bool,
    /// The packs that name it, in the order they were visited. Empty when it is
    #[serde(default)]
    pub via: Vec<String>,
}

impl Inherited {
    pub fn is_empty(&self) -> bool {
        self.deps.is_empty() && self.keys.is_empty() && self.files.is_empty()
    }
}

/// The composition graph: every pack the live tree reaches, **weakest first**,
pub fn closure(assets: &Path) -> Result<Vec<PackNode>> {
    let mut order: Vec<String> = Vec::new();
    let mut nodes: BTreeMap<String, PackNode> = BTreeMap::new();
    let mut path: Vec<String> = Vec::new();
    for dep in read_pack(assets).deps {
        visit(assets, &dep, true, None, &mut order, &mut nodes, &mut path)?;
    }
    Ok(order
        .into_iter()
        .filter_map(|name| nodes.remove(&name))
        .collect())
}

/// One step of [`closure`]: a pack's parents first, then the pack, and a name
#[allow(clippy::too_many_arguments)]
fn visit(
    assets: &Path,
    name: &str,
    direct: bool,
    via: Option<&str>,
    order: &mut Vec<String>,
    nodes: &mut BTreeMap<String, PackNode>,
    path: &mut Vec<String>,
) -> Result<()> {
    if let Some(node) = nodes.get_mut(name) {
        node.direct |= direct;
        if let Some(via) = via {
            if !node.via.iter().any(|seen| seen == via) {
                node.via.push(via.to_string());
            }
        }
        return Ok(());
    }
    if path.iter().any(|seen| seen == name) {
        bail!(
            "pack '{name}' depends on itself: {} -> {name} — a composition is a graph, not a loop",
            path.join(" -> ")
        );
    }
    let dir = extends_dir(assets).join(name);
    if !dir.is_dir() {
        bail!(
            "asset '{name}' is not unpacked: {} is missing — unpack its release under {}/ before resolving",
            dir.display(),
            extends_dir(assets).display(),
        );
    }
    path.push(name.to_string());
    for parent in read_pack(&dir).deps {
        visit(assets, &parent, false, Some(name), order, nodes, path)?;
    }
    path.pop();
    nodes.insert(
        name.to_string(),
        PackNode {
            name: name.to_string(),
            hash: tree_hash(&dir)?,
            direct,
            via: via.map(|v| vec![v.to_string()]).unwrap_or_default(),
        },
    );
    order.push(name.to_string());
    Ok(())
}

/// Read `assets/pack.json`. Absent or unreadable is *no dependencies*, not an
pub fn read_pack(assets: &Path) -> Pack {
    std::fs::read_to_string(pack_path(assets))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Record which release each unpacked dependency came from.
pub fn set_release_versions(assets: &Path, versions: &BTreeMap<String, String>) -> Result<()> {
    let mut marker = read_marker(assets);
    for (name, version) in versions {
        if version.is_empty() {
            marker.versions.remove(name);
        } else {
            marker.versions.insert(name.clone(), version.clone());
        }
    }
    write_marker(assets, &marker)
}

/// Read `assets/_extends.json`. A missing or broken record degrades to "nothing
/// was inherited", which is the safe direction: no withdrawal happens and every
pub fn read_marker(assets: &Path) -> Inherited {
    std::fs::read_to_string(marker_path(assets))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn write_marker(assets: &Path, marker: &Inherited) -> Result<()> {
    let path = marker_path(assets);
    let text = serde_json::to_string_pretty(marker)?;
    crate::atomic_write(&path, &format!("{text}\n"))?;
    Ok(())
}
