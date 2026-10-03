use super::*;

pub fn chapter_of(path: &Path) -> Option<u32> {
    let stem = path.file_stem()?.to_str()?;
    if stem.is_empty() || !stem.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    stem.parse().ok()
}

/// The `NN.json` files in a chapter directory, in whatever order the
/// filesystem hands them over. Callers that order the answer ask
/// [`Layout::scripts`].
pub(crate) fn chapter_files(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok().map(|x| x.path()))
                .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
                .filter(|p| chapter_of(p).is_some())
                .collect()
        })
        .unwrap_or_default()
}

/// What a directory under `workspaces/` carries: the config that makes it a
/// book, or the reason it is not one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceConfig {
    /// `settings.json` is there and parses — the file `workspace new` stamps
    /// and the first thing every command reads, so this directory is a book.
    Valid,
    /// No `settings.json`: a directory somebody left here, not a workspace.
    Missing,
    /// The file is there and does not parse. Worse than missing, because
    /// `Settings::load` falls back to defaults rather than refusing — so the
    /// commands would run against settings nobody wrote.
    Broken,
}

/// One directory under `workspaces/`: what it is called, whether the pointer
/// names it, what config it carries, and how far the book has got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceEntry {
    /// The directory name — what `:ws <name>` and `workspace use <name>` take.
    pub name: String,
    /// The pointer names this one: the book every command runs on right now.
    pub active: bool,
    pub config: WorkspaceConfig,
    /// Chapters crawled and scripts written: the two numbers that say whether a
    /// switch is worth making.
    pub chapters: usize,
    pub scripts: usize,
}

/// Every directory under `workspaces/`, by name, with the pointer marked.
///
/// For a *list*, never for a switch — switching is a pointer write, and what a
/// list has to answer is which directories are books at all. An unusable one is
/// listed and marked rather than hidden: the usual way to find one is to have
/// made it by accident, and a row that quietly vanished is how a stale pointer
/// turns into a mystery.
pub fn workspaces(root: &Path) -> Vec<WorkspaceEntry> {
    let active = std::fs::read_to_string(Layout::active_workspace_file(root))
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let mut out: Vec<WorkspaceEntry> = std::fs::read_dir(root.join("workspaces"))
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    WorkspaceEntry {
                        active: name == active,
                        config: workspace_config(&e.path()),
                        chapters: files_with_extension(
                            &e.path().join("data").join("chapters"),
                            "txt",
                        ),
                        scripts: files_with_extension(
                            &e.path().join("data").join("script"),
                            "json",
                        ),
                        name,
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// A workspace's own `settings.json`, or the reason it is not a book. Read
/// rather than loaded: [`crate::config::Settings::load`] treats a missing file
/// as defaults, which is right for a command that may legitimately run at the
/// repo root and wrong for a question about whether a directory is a workspace.
fn workspace_config(dir: &Path) -> WorkspaceConfig {
    let path = dir.join("settings.json");
    if !path.is_file() {
        return WorkspaceConfig::Missing;
    }
    let parsed = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<crate::config::Settings>(&raw).ok());
    match parsed {
        Some(_) => WorkspaceConfig::Valid,
        None => WorkspaceConfig::Broken,
    }
}

/// How many `*.<ext>` files sit in `dir`. Zero for a directory that is not
/// there, which is an ordinary state: a book nobody has crawled yet.
fn files_with_extension(dir: &Path, ext: &str) -> usize {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some(ext))
                .count()
        })
        .unwrap_or_default()
}
