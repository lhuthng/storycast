//! Voice roster maintenance.
//!
//! Everything here is **local**: it reads and writes files under the repo root
//! and touches no worker. The worker-facing half of the roster plan — sync,
//! enrollment, `--purge` — is deliberately absent, because deleting a voice from
//! a worker means rewriting a voice store a live sidecar is reading, and that
//! may only happen once the workspace is secured
//! (`.docs/VOICE_CONFIG_PROPOSAL.md` §3.4, §8.3).

use anyhow::Result;
use bm_core::cast::{cast_on_disk, read_cast, write_cast};
use bm_core::voices::key_for_name;
use bm_core::Layout;
use std::path::{Path, PathBuf};

/// What one `migrate-cast` run did, or would do.
pub struct CastMigration {
    pub path: PathBuf,
    pub engine: String,
    pub entries: usize,
    /// Entries that carry a catalogue key.
    pub keyed: usize,
    /// Entries with no key — an enrolled clone, or a voice the catalogue has
    /// dropped. They stay display names, which still resolve.
    pub unmigratable: Vec<String>,
    /// Only the entries whose stored form actually changes, for the report.
    pub changed: Vec<(String, String, String)>,
    pub written: bool,
}

impl CastMigration {
    /// The `.bak` path a write leaves behind.
    pub fn backup(&self) -> PathBuf {
        self.path.with_extension("json.bak")
    }
}

/// Rewrite one cast file so its values are catalogue keys.
///
/// The cast reader already accepts names and the writer keys the file on its
/// next save, so this is **not** a prerequisite for anything — a migrated, half
/// migrated and untouched cast all render. It exists for the operator who wants
/// the file keyed now, and who wants to see what changed before it changes.
/// Hence the report and the `.bak`: the file it rewrites is the live cast of a
/// book that may already be rendered.
pub fn migrate_cast_file(engine: &str, path: &Path, dry_run: bool) -> Result<CastMigration> {
    // What the file holds right now, verbatim: keys, names, or a mix.
    let raw: bm_core::cast::Cast = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    // Resolved to display names, and then to the form we would write.
    let resolved = read_cast(engine, path);
    let keyed = cast_on_disk(engine, &resolved);

    let mut changed = Vec::new();
    let mut unmigratable = Vec::new();
    for (character, name) in &resolved {
        if key_for_name(engine, name).is_none() {
            unmigratable.push(format!("{character} -> {name}"));
        }
        // Compare what is stored against what would be stored. Comparing against
        // the *resolved* name instead would report every already-keyed entry as
        // a change, and the command would never settle.
        let old = raw.get(character).cloned().unwrap_or_default();
        let new = keyed.get(character).cloned().unwrap_or_default();
        if old != new {
            changed.push((character.clone(), old, new));
        }
    }

    let mut written = false;
    if !dry_run && !changed.is_empty() {
        // Copy before write. A bad rewrite is not something to discover from a
        // re-render, and `data/` is regenerable but not free.
        std::fs::copy(path, backup_for(path))?;
        write_cast(engine, path, &resolved)?;
        written = true;
    }

    Ok(CastMigration {
        path: path.to_path_buf(),
        engine: engine.to_string(),
        entries: resolved.len(),
        keyed: resolved.len() - unmigratable.len(),
        unmigratable,
        changed,
        written,
    })
}

fn backup_for(path: &Path) -> PathBuf {
    path.with_extension("json.bak")
}

/// Migrate every cast file that exists.
///
/// Both engines are visited because both files can be present; each is resolved
/// against its own engine, so a VieNeu cast can never pick up a Gemini key.
pub fn migrate_cast(layout: &Layout, dry_run: bool) -> Result<Vec<CastMigration>> {
    let mut out = Vec::new();
    for engine in ["vieneu", "gemini"] {
        let path = layout.cast(engine);
        if path.is_file() {
            out.push(migrate_cast_file(engine, &path, dry_run)?);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Layout {
        let root = std::env::temp_dir().join(format!("bm-roster-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        let l = Layout::new(&root);
        l.ensure().unwrap();
        l
    }

    #[test]
    fn migrates_names_to_keys_and_keeps_a_backup() {
        let l = fixture("migrate");
        let path = l.cast("vieneu");
        std::fs::write(&path, r#"{"Narrator":"Đức Trí","Dịch Phong":"Thái Sơn"}"#).unwrap();

        let r = migrate_cast_file("vieneu", &path, false).unwrap();
        assert!(r.written);
        assert_eq!(r.entries, 2);
        assert_eq!(r.keyed, 2);
        assert!(r.unmigratable.is_empty());
        assert_eq!(r.changed.len(), 2);

        let disk: std::collections::BTreeMap<String, String> =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(disk["Narrator"], "duc-tri");
        assert_eq!(disk["Dịch Phong"], "thai-son");
        // The backup holds the *previous* form, which is the whole point.
        let bak = std::fs::read_to_string(r.backup()).unwrap();
        assert!(bak.contains("Đức Trí"), "backup kept the name form: {bak}");
    }

    #[test]
    fn a_dry_run_reports_without_writing() {
        let l = fixture("dry");
        let path = l.cast("vieneu");
        let original = r#"{"Narrator":"Đức Trí"}"#;
        std::fs::write(&path, original).unwrap();

        let r = migrate_cast_file("vieneu", &path, true).unwrap();
        assert!(!r.written);
        assert_eq!(r.changed.len(), 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert!(!r.backup().exists(), "a dry run writes nothing at all");
    }

    #[test]
    fn an_already_migrated_file_is_left_alone() {
        let l = fixture("idempotent");
        let path = l.cast("vieneu");
        std::fs::write(&path, r#"{"Narrator":"duc-tri"}"#).unwrap();

        let r = migrate_cast_file("vieneu", &path, false).unwrap();
        assert!(!r.written, "nothing changes, so nothing is written");
        assert!(r.changed.is_empty());
        assert!(!r.backup().exists());
    }

    #[test]
    fn a_clone_is_reported_but_not_rewritten() {
        let l = fixture("clone");
        let path = l.cast("vieneu");
        std::fs::write(&path, r#"{"Narrator":"duc-tri","Suneo":"Suneo"}"#).unwrap();

        let r = migrate_cast_file("vieneu", &path, false).unwrap();
        assert!(
            !r.written,
            "only the clone is unmigratable, so nothing changes"
        );
        assert_eq!(r.entries, 2);
        assert_eq!(r.keyed, 1);
        assert_eq!(r.unmigratable.len(), 1);
        assert!(r.unmigratable[0].contains("Suneo"));
    }

    #[test]
    fn both_cast_files_are_visited_when_present() {
        let l = fixture("both");
        std::fs::write(l.cast("vieneu"), r#"{"Narrator":"Đức Trí"}"#).unwrap();
        std::fs::write(l.cast("gemini"), r#"{"Narrator":"Charon"}"#).unwrap();

        let runs = migrate_cast(&l, false).unwrap();
        assert_eq!(runs.len(), 2);
        let gem = runs.iter().find(|r| r.engine == "gemini").unwrap();
        assert_eq!(
            gem.changed[0].2, "charon",
            "resolved against its own engine"
        );
    }

    #[test]
    fn a_missing_cast_file_is_not_an_error() {
        let l = fixture("absent");
        assert!(migrate_cast(&l, false).unwrap().is_empty());
    }
}
