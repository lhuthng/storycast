use super::*;
use anyhow::{bail, Result};

/// `assets/pack.json`: the other assets this one is built on, weakest first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pack {
    #[serde(default)]
    pub deps: Vec<String>,
}

/// One dependency, as it stood when the last resolve folded it in.
///
/// The hash is the point: it is what lets a child say "I was built against
/// `common` at *this* content" and therefore be told, cheaply and exactly, that
/// it is stale. A dependency that never moved leaves the child alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DepRecord {
    pub name: String,
    /// A content hash of the dependency's whole tree.
    pub hash: String,
}

/// `assets/_extends.json`: what a resolve put in the live tree, and from where.
///
/// Generated. Read back by the next resolve so it can withdraw what it owns,
/// and by the packer so a release can name the dependencies it was built
/// against.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inherited {
    /// The **direct** dependencies folded in, in `pack.json` order — what a
    /// release names, and what a staleness check was written against before the
    /// closure was recorded at all.
    #[serde(default)]
    pub deps: Vec<DepRecord>,
    /// The whole **closure**, weakest first: every pack the graph reaches, once,
    /// with the hash it was folded in at. This is the map — see [`PackNode`].
    #[serde(default)]
    pub tree: Vec<PackNode>,
    /// Which **release** each unpacked dependency came from, by pack name.
    ///
    /// The fold cannot fill this in and is right not to try: it hashes the tree
    /// in front of it, and a hash does not name the tag that produced it. The
    /// update path knows, so [`set_release_versions`] writes it after the fold,
    /// and what it buys is the cheapest question in the whole release plane —
    /// "has this moved?" answered from the release *list*, without downloading
    /// a 60 MB bundle to compare it against content that is identical.
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
/// reached it.
///
/// `pack.json` is what an asset *declares*; this is what its tree contains once
/// the walk has followed every dependency's own `pack.json`. The difference is
/// the diamond: two dependencies that both name `E` are **one** `E`, folded once
/// at the weakest position that keeps every parent behind it, and [`via`] is the
/// only place that fact survives — a tree that folded per direct dependency
/// would hold `E` twice and name neither.
///
/// It is also the map a provisioning step flattens from: one directory per
/// `name` under `_extends/`, and `order` (this vector's order) is the order to
/// fold them in.
///
/// [`via`]: PackNode::via
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackNode {
    pub name: String,
    /// A content hash of the pack's own tree — the number a release, a child's
    /// `deps` and the staleness check all name.
    pub hash: String,
    /// Whether the live `pack.json` names it directly.
    #[serde(default)]
    pub direct: bool,
    /// The packs that name it, in the order they were visited. Empty when it is
    /// only a direct dependency.
    #[serde(default)]
    pub via: Vec<String>,
}

impl Inherited {
    pub fn is_empty(&self) -> bool {
        self.deps.is_empty() && self.keys.is_empty() && self.files.is_empty()
    }
}

/// The composition graph: every pack the live tree reaches, **weakest first**,
/// each exactly once.
///
/// Depth-first over each pack's own `pack.json`: a pack's parents are visited
/// before it, and the direct list's order is kept at every level. That is the
/// flat weakest-first rule (a later name wins a shared key) carried into a
/// graph, and it is what makes a diamond fold once — `A` naming `B` and `C`,
/// both of which name `E`, resolves to `[E, F, B, G, C]` for `B = [E, F]` and
/// `C = [E, G]`: `E` is folded at its weakest position, `F` still overrides it,
/// `B` still beats both, and `C` still beats `B`. Folding per *direct*
/// dependency instead is how one pack ends up inside two trees at once, with no
/// record of either copy.
///
/// A pack's own `pack.json` **is** the edge, which is what lets a release be a
/// node with edges instead of a copy of everything underneath it: a dependency
/// carrying no `deps` is a leaf.
///
/// The closure is the **map**: [`PackNode::via`] remembers a pack reached from
/// two parents (the only place that fact exists), and [`Inherited::tree`] stores
/// the whole of it, so whoever provisions a checkout can place one directory per
/// name under `_extends/` and fold them in this order.
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
/// already placed is only annotated — which is what keeps a diamond one node.
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
/// error: a plain pack is the shape every checkout has today, and a
/// bookkeeping file must never stop a render.
pub fn read_pack(assets: &Path) -> Pack {
    std::fs::read_to_string(pack_path(assets))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Record which release each unpacked dependency came from.
///
/// The update path's one write into the composition record, and it is a write
/// *after* the fold on purpose: what a fold can see is a tree, and the version
/// is a fact about where the tree was downloaded from. Emptying a version
/// removes the entry, which is how a dependency that stopped coming from a
/// release stops claiming one.
///
/// An update that swaps trees and dies before this call loses only the record,
/// and it loses it in the safe direction: the next update sees no version for
/// that pack and pulls its release again.
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
/// entry already in the tree reads as the asset's own, so nothing is ever
/// deleted or overwritten on the strength of a file that did not parse.
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
