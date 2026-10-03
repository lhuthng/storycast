//! Updating a composition: pull the newest release of everything it depends on.

use crate::compose;
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

/// One dependency's newest release, as a release source answers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Available {
    pub name: String,
    pub version: String,
    /// The release tag, for the log line — `<name>-pack-v<version>`.
    pub tag: String,
    pub url: String,
}

/// Where an update asks what exists, and gets the bytes.
pub trait Releases {
    /// The newest release of `name`, or `None` when there is none.
    fn latest(&mut self, name: &str) -> Result<Option<Available>>;

    /// Download `release`, verify it, and answer with its **unpacked tree**.
    fn fetch(&mut self, release: &Available) -> Result<PathBuf>;
}

/// What an update found, per dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Not unpacked at all — the release is the only copy of it.
    New,
    /// Unpacked at an older release; the newest one is what it will become.
    Update,
    /// Already at the newest release. Nothing is downloaded.
    UpToDate,
    /// Edited here since the last fold. Only `--force` replaces it.
    Drift,
}

/// One dependency the closure reached, and what the update decided about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub name: String,
    /// Named in the live `pack.json` rather than reached through another pack.
    pub direct: bool,
    /// The packs that name it, in the order the walk found them. Empty for a
    pub via: Vec<String>,
    /// The version recorded as unpacked, when the record has one.
    pub was: Option<String>,
    /// The version being pulled.
    pub version: String,
    pub state: State,
}

impl Step {
    /// One line for the operator, in the order the questions get asked: what is
    pub fn line(&self) -> String {
        let was = match &self.was {
            Some(v) => format!("v{v}"),
            None => "—".to_string(),
        };
        let to = format!("v{}", self.version);
        let motion = if self.state == State::UpToDate {
            to
        } else {
            format!("{was} -> {to}")
        };
        let what = match self.state {
            State::New => "new",
            State::Update => "pull",
            State::UpToDate => "up to date",
            State::Drift => "pulled over local edits",
        };
        let via = if self.via.is_empty() {
            String::new()
        } else {
            format!("  (via {})", self.via.join(", "))
        };
        format!("{:<24} {:<24} {what}{via}", self.name, motion)
    }
}

/// What an update did — or, in a dry run, would do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    /// Every pack the closure reached, in the order the walk found it.
    pub steps: Vec<Step>,
    /// The packs actually replaced on disk. Empty on a dry run, and empty when
    pub replaced: Vec<String>,
    /// The fold, after the swap. `None` on a dry run, which computes the plan
    pub fold: Option<compose::Report>,
    pub dry_run: bool,
}

impl Update {
    /// Whether anything would be (or was) replaced.
    pub fn pending(&self) -> bool {
        self.steps.iter().any(|s| s.state != State::UpToDate)
    }

    /// One line for the operator.
    pub fn summary(&self) -> String {
        let count = |state: State| self.steps.iter().filter(|s| s.state == state).count();
        let mut parts = vec![format!("{} pack(s) in the closure", self.steps.len())];
        for (state, label) in [
            (State::Update, "to pull"),
            (State::New, "new"),
            (State::Drift, "edited here"),
            (State::UpToDate, "up to date"),
        ] {
            let n = count(state);
            if n > 0 {
                parts.push(format!("{n} {label}"));
            }
        }
        let verb = if self.dry_run {
            if self.pending() {
                "would change"
            } else {
                "would not change"
            }
        } else if self.replaced.is_empty() {
            "up to date"
        } else {
            "changed"
        };
        format!("{} — {verb}", parts.join("; "))
    }
}

/// Bring every dependency of the live composition up to its newest release.
pub fn update(
    assets: &Path,
    releases: &mut dyn Releases,
    dry_run: bool,
    force: bool,
) -> Result<Update> {
    let direct = compose::read_pack(assets).deps;
    if direct.is_empty() {
        bail!(
            "{} names no dependencies — an update follows a dependency list, and this \
             checkout has none (a plain pack has nothing to update)",
            compose::pack_path(assets).display()
        );
    }
    let marker = compose::read_marker(assets);
    let recorded: BTreeMap<&str, &str> = marker
        .tree
        .iter()
        .map(|n| (n.name.as_str(), n.hash.as_str()))
        .collect();
    let extends = compose::extends_dir(assets);

    let mut steps: Vec<Step> = Vec::new();
    let mut staged: BTreeMap<String, PathBuf> = BTreeMap::new();
    // name -> its own dependencies, and name -> who names it. The first is what
    let mut graph: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut parents: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut queue: VecDeque<String> = direct.iter().cloned().collect();

    while let Some(name) = queue.pop_front() {
        if !seen.insert(name.clone()) {
            continue; // a diamond is one pack, walked once
        }
        safe_name(&name, &extends)?;
        let release = releases.latest(&name)?.ok_or_else(|| {
            anyhow::anyhow!(
                "no release for dependency '{name}' — publish it (`tools/profile.sh pack \
                 {name} --dep --version V` then `gh release create`), or drop it from {}",
                compose::pack_path(assets).display()
            )
        })?;
        let dir = extends.join(&name);
        let unpacked = dir.is_dir();
        // Edited here since the fold, or never folded in at all — either way the
        let drifted = unpacked
            && match recorded.get(name.as_str()) {
                Some(hash) => crate::compose::tree_hash(&dir)? != *hash,
                None => true,
            };
        let state = if !unpacked {
            State::New
        } else if drifted {
            if !force {
                bail!(
                    "dependency '{name}' has been edited here — {} no longer matches the \
                     composition record. Re-resolve to fold the edit in (`asset resolve`), \
                     or pass --force to replace it with release v{version}",
                    dir.display(),
                    version = release.version,
                );
            }
            State::Drift
        } else if marker.versions.get(&name).map(String::as_str) == Some(release.version.as_str()) {
            State::UpToDate
        } else {
            State::Update
        };

        // Only a pack that is going to be replaced is downloaded. This is where
        let mut tree = dir.clone();
        if state != State::UpToDate && !dry_run {
            let fetched = releases
                .fetch(&release)
                .with_context(|| format!("fetching {} ({})", release.name, release.tag))?;
            if !fetched.is_dir() {
                bail!(
                    "the release source answered {} for '{name}', which is not a tree",
                    fetched.display()
                );
            }
            tree = fetched.clone();
            staged.insert(name.clone(), fetched);
        }

        // Whichever tree the update knows about: the staged release when one was
        let deps = compose::read_pack(&tree).deps;
        for parent in &deps {
            let who = parents.entry(parent.clone()).or_default();
            if !who.iter().any(|p| p == &name) {
                who.push(name.clone());
            }
        }
        graph.insert(name.clone(), deps.clone());
        queue.extend(deps);
        steps.push(Step {
            name: name.clone(),
            direct: direct.contains(&name),
            via: Vec::new(),
            was: marker.versions.get(&name).cloned(),
            version: release.version,
            state,
        });
    }

    // Before anything moves. The walk terminates on a cycle — `seen` is what
    check_acyclic(&graph)?;

    for step in steps.iter_mut() {
        step.via = parents.get(&step.name).cloned().unwrap_or_default();
    }

    let mut replaced: Vec<String> = Vec::new();
    let mut fold = None;
    if !dry_run {
        for (name, tree) in &staged {
            let dir = extends.join(name);
            crate::artifact::swap(tree, &dir)
                .with_context(|| format!("installing '{name}' at {}", dir.display()))?;
            replaced.push(name.clone());
        }
        // The fold, then the record. A pack that was replaced has content the
        fold = Some(compose::resolve(assets, false)?);
        let versions: BTreeMap<String, String> = steps
            .iter()
            .map(|s| (s.name.clone(), s.version.clone()))
            .collect();
        compose::set_release_versions(assets, &versions)?;
    }

    Ok(Update {
        steps,
        replaced,
        fold,
        dry_run,
    })
}

/// A dependency name becomes a path under `_extends/`, so it must be one.
fn safe_name(name: &str, extends: &Path) -> Result<()> {
    let ok = !name.is_empty()
        && !name.contains('/')
        && !name.contains('\\')
        && name != "."
        && name != ".."
        && !name.contains('\0');
    if !ok {
        bail!(
            "'{name}' is not a dependency name — a pack unpacks to one directory under {}",
            extends.display()
        );
    }
    Ok(())
}

/// Refuse a graph a fold cannot order.
fn check_acyclic(nodes: &BTreeMap<String, Vec<String>>) -> Result<()> {
    fn walk(
        name: &str,
        nodes: &BTreeMap<String, Vec<String>>,
        path: &mut Vec<String>,
        done: &mut BTreeSet<String>,
    ) -> Result<()> {
        if done.contains(name) {
            return Ok(());
        }
        if let Some(at) = path.iter().position(|n| n == name) {
            let mut loop_: Vec<String> = path[at..].to_vec();
            loop_.push(name.to_string());
            bail!(
                "dependency loop: {} — a composition is a graph, not a loop",
                loop_.join(" -> ")
            );
        }
        path.push(name.to_string());
        if let Some(parents) = nodes.get(name) {
            for parent in parents {
                walk(parent, nodes, path, done)?;
            }
        }
        path.pop();
        done.insert(name.to_string());
        Ok(())
    }
    let mut done = BTreeSet::new();
    for name in nodes.keys() {
        walk(name, nodes, &mut Vec::new(), &mut done)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// One registry value per version, so a replaced tree is visibly the release
    fn pool_text(version: &str) -> String {
        format!(
            "{{\n  \"wind\": {{\"tags\":[\"wind\"],\"files\":[\"effects/wind-{version}.mp3\"]}}\n}}\n"
        )
    }

    fn release_map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect()
    }

    /// The live checkout: `assets/pack.json` plus whatever is unpacked under
    struct Live {
        root: PathBuf,
        assets: PathBuf,
    }

    impl Live {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir().join(format!("bm-pack-update-{tag}"));
            let _ = std::fs::remove_dir_all(&root);
            let assets = root.join("assets");
            std::fs::create_dir_all(&assets).unwrap();
            Live { root, assets }
        }

        fn pack(&self, deps: &[&str]) {
            std::fs::write(
                compose::pack_path(&self.assets),
                serde_json::json!({ "deps": deps }).to_string(),
            )
            .unwrap();
        }

        /// Unpack a dependency, the way `profile.sh unpack` leaves one.
        fn unpack(&self, name: &str, version: &str, deps: &[&str]) {
            let dir = compose::extends_dir(&self.assets).join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("effect-pool.json"), pool_text(version)).unwrap();
            std::fs::write(
                dir.join("pack.json"),
                serde_json::json!({ "deps": deps }).to_string(),
            )
            .unwrap();
        }

        /// Fold, then record — the state a checkout is in before an update.
        fn settled(&self, versions: &[(&str, &str)]) {
            compose::resolve(&self.assets, false).unwrap();
            compose::set_release_versions(&self.assets, &release_map(versions)).unwrap();
        }

        fn text(&self, name: &str) -> String {
            std::fs::read_to_string(
                compose::extends_dir(&self.assets)
                    .join(name)
                    .join("effect-pool.json"),
            )
            .unwrap()
        }

        fn recorded(&self) -> BTreeMap<String, String> {
            compose::read_marker(&self.assets).versions.clone()
        }
    }

    /// A release source answering from a table.
    struct Fake {
        root: PathBuf,
        released: BTreeMap<String, String>,
        deps: BTreeMap<String, Vec<String>>,
        broken: BTreeSet<String>,
        fetched: Vec<String>,
        listed: Vec<String>,
    }

    impl Fake {
        fn new(root: &Path) -> Self {
            Fake {
                root: root.to_path_buf(),
                released: BTreeMap::new(),
                deps: BTreeMap::new(),
                broken: BTreeSet::new(),
                fetched: Vec::new(),
                listed: Vec::new(),
            }
        }

        fn releasing(mut self, name: &str, version: &str, deps: &[&str]) -> Self {
            self.released.insert(name.to_string(), version.to_string());
            self.deps.insert(
                name.to_string(),
                deps.iter().map(|d| d.to_string()).collect(),
            );
            self
        }

        fn broken(mut self, name: &str) -> Self {
            self.broken.insert(name.to_string());
            self
        }
    }

    impl Releases for Fake {
        fn latest(&mut self, name: &str) -> Result<Option<Available>> {
            self.listed.push(name.to_string());
            Ok(self.released.get(name).map(|version| Available {
                name: name.to_string(),
                version: version.clone(),
                tag: format!("{name}-pack-v{version}"),
                url: format!("https://example.invalid/{name}-{version}.tar.zst"),
            }))
        }

        fn fetch(&mut self, release: &Available) -> Result<PathBuf> {
            self.fetched.push(release.name.clone());
            if self.broken.contains(&release.name) {
                anyhow::bail!("{} sha256 does not match the manifest", release.tag);
            }
            let tree = self
                .root
                .join(format!("stage-{}-{}", release.name, release.version));
            let _ = std::fs::remove_dir_all(&tree);
            std::fs::create_dir_all(&tree)?;
            std::fs::write(tree.join("effect-pool.json"), pool_text(&release.version))?;
            let deps = self.deps.get(&release.name).cloned().unwrap_or_default();
            std::fs::write(
                tree.join("pack.json"),
                serde_json::json!({ "deps": deps }).to_string(),
            )?;
            Ok(tree)
        }
    }

    /// **The loop's whole reason for existing.** A dependency the live
    #[test]
    fn the_walk_follows_a_dependency_s_own_pack_json() {
        let l = Live::new("closure");
        l.pack(&["B"]);
        l.unpack("B", "0.1.0", &["E"]);
        l.unpack("E", "0.1.0", &[]);
        l.settled(&[("B", "0.1.0"), ("E", "0.1.0")]);

        let mut fake = Fake::new(&l.root)
            .releasing("B", "0.2.0", &["E"])
            .releasing("E", "0.2.0", &[]);
        let u = update(&l.assets, &mut fake, false, false).unwrap();

        let names: Vec<&str> = u.steps.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["B", "E"], "the live list first, then its parent");
        assert!(
            u.steps[0].direct && !u.steps[1].direct,
            "E is reached, not named"
        );
        assert!(
            l.text("E").contains("0.2.0"),
            "E moved too: {}",
            l.text("E")
        );
        assert!(l.text("B").contains("0.2.0"));
        assert_eq!(u.fold.unwrap().tree, 2, "and the fold knows the new graph");
    }

    /// A diamond is one pack. `B` and `C` both name `E`, and it is fetched once
    #[test]
    fn a_shared_parent_is_fetched_once_and_says_who_asked_for_it() {
        let l = Live::new("diamond");
        l.pack(&["B", "C"]);
        l.unpack("B", "0.1.0", &["E"]);
        l.unpack("C", "0.1.0", &["E"]);
        l.unpack("E", "0.1.0", &[]);
        l.settled(&[("B", "0.1.0"), ("C", "0.1.0"), ("E", "0.1.0")]);

        let mut fake = Fake::new(&l.root)
            .releasing("B", "0.2.0", &["E"])
            .releasing("C", "0.2.0", &["E"])
            .releasing("E", "0.2.0", &[]);
        let u = update(&l.assets, &mut fake, false, false).unwrap();

        assert_eq!(u.steps.len(), 3, "E is one step, not one per parent");
        assert_eq!(
            fake.fetched.iter().filter(|n| *n == "E").count(),
            1,
            "fetched once: {:?}",
            fake.fetched
        );
        let shared = u.steps.iter().find(|s| s.name == "E").unwrap();
        assert_eq!(shared.via, ["B", "C"], "and both paths are written down");
    }

    /// The version record is what makes a no-op update cost no bytes: three
    #[test]
    fn a_dependency_already_at_the_newest_release_is_not_downloaded() {
        let l = Live::new("current");
        l.pack(&["B"]);
        l.unpack("B", "0.1.0", &[]);
        l.settled(&[("B", "0.1.0")]);

        let mut fake = Fake::new(&l.root).releasing("B", "0.1.0", &[]);
        let u = update(&l.assets, &mut fake, false, false).unwrap();

        assert!(fake.fetched.is_empty(), "nothing was downloaded");
        assert!(u.replaced.is_empty());
        assert!(!u.pending(), "{}", u.summary());
        assert_eq!(u.steps[0].state, State::UpToDate);
        assert!(u.summary().ends_with("up to date"), "{}", u.summary());
        assert_eq!(
            u.fold.unwrap().tree,
            1,
            "the fold still ran, and found nothing"
        );
    }

    /// **Fetch everything, then swap.** A release that does not verify is
    #[test]
    fn a_release_that_does_not_verify_leaves_every_tree_untouched() {
        let l = Live::new("corrupt");
        l.pack(&["B", "C"]);
        l.unpack("B", "0.1.0", &[]);
        l.unpack("C", "0.1.0", &[]);
        l.settled(&[("B", "0.1.0"), ("C", "0.1.0")]);

        let mut fake = Fake::new(&l.root)
            .releasing("B", "0.2.0", &[])
            .releasing("C", "0.2.0", &[])
            .broken("C");
        let err = update(&l.assets, &mut fake, false, false).unwrap_err();

        assert!(err.to_string().contains("C"), "{err}");
        assert!(l.text("B").contains("0.1.0"), "B was staged, not installed");
        assert!(l.text("C").contains("0.1.0"), "C is exactly as it was");
        assert_eq!(
            l.recorded(),
            release_map(&[("B", "0.1.0"), ("C", "0.1.0")]),
            "and nothing claims a release that did not land"
        );
    }

    /// An edit under `_extends/` is work, and an update is not allowed to be the
    #[test]
    fn a_dependency_edited_here_is_refused_unless_forced() {
        let l = Live::new("drift");
        l.pack(&["B"]);
        l.unpack("B", "0.1.0", &[]);
        l.settled(&[("B", "0.1.0")]);
        // Retune the tree by hand, the way `:sound` and an editor both do.
        std::fs::write(
            compose::extends_dir(&l.assets)
                .join("B")
                .join("effect-pool.json"),
            pool_text("0.1.0-edited"),
        )
        .unwrap();

        let mut fake = Fake::new(&l.root).releasing("B", "0.2.0", &[]);
        let err = update(&l.assets, &mut fake, false, false).unwrap_err();
        assert!(err.to_string().contains("--force"), "{err}");
        assert!(fake.fetched.is_empty(), "and it did not even download");

        let mut fake = Fake::new(&l.root).releasing("B", "0.2.0", &[]);
        let u = update(&l.assets, &mut fake, false, true).unwrap();
        assert_eq!(u.replaced, ["B"]);
        assert_eq!(
            u.steps[0].state,
            State::Drift,
            "and it says what it replaced"
        );
        assert!(l.text("B").contains("0.2.0"));
    }

    /// A loop terminates the walk — `seen` is what makes it terminate — so the
    #[test]
    fn a_loop_is_refused_before_anything_is_swapped() {
        let l = Live::new("loop");
        l.pack(&["B"]);
        l.unpack("B", "0.1.0", &[]);
        l.settled(&[("B", "0.1.0")]);

        let mut fake = Fake::new(&l.root)
            .releasing("B", "0.2.0", &["A"])
            .releasing("A", "0.2.0", &["B"]);
        let err = update(&l.assets, &mut fake, false, false).unwrap_err();

        assert!(err.to_string().contains("loop"), "{err}");
        assert!(err.to_string().contains("A -> B -> A"), "{err}");
        assert!(l.text("B").contains("0.1.0"), "nothing was installed");
    }

    /// A dry run asks the release list and nothing else: the plan can be read
    #[test]
    fn a_dry_run_reports_the_plan_and_downloads_nothing() {
        let l = Live::new("plan");
        l.pack(&["B"]);
        l.unpack("B", "0.1.0", &[]);
        l.settled(&[("B", "0.1.0")]);

        let mut fake = Fake::new(&l.root).releasing("B", "0.2.0", &[]);
        let u = update(&l.assets, &mut fake, true, false).unwrap();

        assert!(fake.fetched.is_empty(), "a plan is not a download");
        assert_eq!(fake.listed, ["B"], "but it does ask what exists");
        assert!(u.replaced.is_empty() && u.fold.is_none());
        assert!(u.pending(), "{}", u.summary());
        assert_eq!(u.steps[0].was.as_deref(), Some("0.1.0"));
        assert_eq!(u.steps[0].version, "0.2.0");
        assert!(
            u.steps[0].line().contains("v0.1.0 -> v0.2.0"),
            "{}",
            u.steps[0].line()
        );
        assert!(l.text("B").contains("0.1.0"), "and the tree is as it was");
    }

    /// The record the next update reads: without it, every update would have to
    #[test]
    fn an_update_records_the_releases_it_unpacked() {
        let l = Live::new("record");
        l.pack(&["B", "C"]);
        l.unpack("B", "0.1.0", &[]);
        l.unpack("C", "0.1.0", &[]);
        l.settled(&[("B", "0.1.0"), ("C", "0.1.0")]);

        let mut fake = Fake::new(&l.root)
            .releasing("B", "0.2.0", &[])
            .releasing("C", "0.2.0", &[]);
        let u = update(&l.assets, &mut fake, false, false).unwrap();

        assert_eq!(u.replaced, ["B", "C"]);
        assert_eq!(
            l.recorded(),
            release_map(&[("B", "0.2.0"), ("C", "0.2.0")]),
            "the record names the release each tree came from"
        );
        // And the fold that ran between the swap and the record saw the new
        let marker = compose::read_marker(&l.assets);
        assert_eq!(marker.tree.len(), 2);
        for node in &marker.tree {
            let dir = compose::extends_dir(&l.assets).join(&node.name);
            assert_eq!(
                compose::tree_hash(&dir).unwrap(),
                node.hash,
                "{} is folded at the hash of what is on disk",
                node.name
            );
        }
    }

    /// A dependency added to `pack.json` and never unpacked is the one state a
    #[test]
    fn a_dependency_the_record_never_folded_is_pulled_as_new() {
        let l = Live::new("new");
        l.pack(&["B"]);
        l.unpack("B", "0.1.0", &[]);
        l.settled(&[("B", "0.1.0")]);
        // Now the operator adds a dependency and has not fetched it yet.
        l.pack(&["B", "C"]);

        let mut fake = Fake::new(&l.root)
            .releasing("B", "0.1.0", &[])
            .releasing("C", "0.2.0", &[]);
        let u = update(&l.assets, &mut fake, false, false).unwrap();

        let added = u.steps.iter().find(|s| s.name == "C").unwrap();
        assert_eq!(added.state, State::New);
        assert_eq!(added.was, None);
        assert_eq!(fake.fetched, ["C"], "B was already current");
        assert!(l.text("C").contains("0.2.0"));
        assert_eq!(l.recorded(), release_map(&[("B", "0.1.0"), ("C", "0.2.0")]));
        assert_eq!(u.fold.unwrap().tree, 2, "and the tree folds again");
    }

    /// A checkout that is not composed is not an error to fix, it is a question
    #[test]
    fn a_checkout_with_no_dependencies_has_nothing_to_update() {
        let l = Live::new("plain");
        let mut fake = Fake::new(&l.root);
        let err = update(&l.assets, &mut fake, false, false).unwrap_err();
        assert!(err.to_string().contains("pack.json"), "{err}");
        assert!(fake.listed.is_empty(), "and it asked nothing");
    }
}
