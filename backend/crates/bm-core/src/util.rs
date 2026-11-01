//! Small filesystem helpers shared by every stage.

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use std::path::Path;

/// Crash-safe write: write a sibling temp file, then rename over the target.
pub fn atomic_write(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "out".to_string());
    let tmp = path.with_file_name(format!(".{name}.tmp"));
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

/// Recursively copy a directory tree, creating `to`. Files are **copied**, not
pub fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    let entries = match std::fs::read_dir(from) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    for entry in entries.filter_map(|e| e.ok()) {
        let src = entry.path();
        let dst = to.join(entry.file_name());
        let kind = entry
            .file_type()
            .with_context(|| format!("reading {}", src.display()))?;
        if kind.is_dir() {
            copy_tree(&src, &dst)?;
        } else if kind.is_file() {
            std::fs::copy(&src, &dst)
                .with_context(|| format!("copying {} -> {}", src.display(), dst.display()))?;
        }
    }
    Ok(())
}

/// Move one file or directory to `to`, which must not exist. A rename when
/// both sit on one device; across devices the entry is copied under an
/// `.incoming` name and renamed only once whole, so a synced folder never
/// sees a partial tree.
pub fn move_entry(from: &Path, to: &Path) -> Result<()> {
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => copy_across(from, to),
        Err(e) => Err(e).with_context(|| format!("moving {} to {}", from.display(), to.display())),
    }
}

fn copy_across(from: &Path, to: &Path) -> Result<()> {
    let mut name = to
        .file_name()
        .map(std::ffi::OsString::from)
        .unwrap_or_default();
    name.push(".incoming");
    let tmp = to.with_file_name(name);
    discard(&tmp);
    let copied = copy_entry(from, &tmp).and_then(|_| {
        std::fs::rename(&tmp, to).with_context(|| format!("renaming into {}", to.display()))
    });
    if let Err(e) = copied {
        discard(&tmp);
        return Err(e).with_context(|| format!("copying {} to {}", from.display(), to.display()));
    }
    if from.is_dir() {
        std::fs::remove_dir_all(from)
    } else {
        std::fs::remove_file(from)
    }
    .with_context(|| format!("removing {}", from.display()))
}

/// Copy one entry whole, syncing every file, so the caller may rename it into
/// place: unlike [`copy_tree`], a source that will not read is an error, not
/// an empty tree, because the source is about to be deleted.
fn copy_entry(from: &Path, to: &Path) -> Result<()> {
    if std::fs::metadata(from)?.is_dir() {
        std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry.with_context(|| format!("reading {}", from.display()))?;
            copy_entry(&entry.path(), &to.join(entry.file_name()))?;
        }
    } else {
        std::fs::copy(from, to)
            .with_context(|| format!("copying {} -> {}", from.display(), to.display()))?;
        std::fs::File::open(to)?.sync_all()?;
    }
    Ok(())
}

fn discard(path: &Path) {
    if path.is_dir() {
        let _ = std::fs::remove_dir_all(path);
    } else {
        let _ = std::fs::remove_file(path);
    }
}

/// Run `f` over `items` on a few threads, preserving order. Capped so a merge
/// runs a handful of ffmpeg/sox processes at once instead of one per core.
pub(crate) fn parallel_map<T, R, F>(items: &[T], f: F) -> Result<Vec<R>>
where
    T: Sync,
    R: Send,
    F: Fn(usize, &T) -> Result<R> + Sync,
{
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex;

    if items.is_empty() {
        return Ok(Vec::new());
    }
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(8)
        .min(items.len());
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let out: Mutex<Vec<Option<Result<R>>>> = Mutex::new((0..items.len()).map(|_| None).collect());
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= items.len() {
                    break;
                }
                let r = f(i, &items[i]);
                if r.is_err() {
                    stop.store(true, Ordering::Relaxed);
                }
                out.lock().unwrap()[i] = Some(r);
            });
        }
    });
    let slots = out.into_inner().unwrap();
    let mut values = Vec::with_capacity(slots.len());
    let mut failed: Option<anyhow::Error> = None;
    for slot in slots {
        match slot {
            Some(Ok(v)) => values.push(v),
            Some(Err(e)) => failed = failed.or(Some(e)),
            None => {}
        }
    }
    match failed {
        Some(e) => Err(e),
        None if values.len() == items.len() => Ok(values),
        None => Err(anyhow::anyhow!("parallel map stopped without an error")),
    }
}

/// Whether a `segments` item is a sound rather than a line of speech.
pub fn restrict(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restricting {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// The keys that make an item a sound rather than a line: a name or a tag
/// query, to start one or to stop one.
pub const SOUND_KEYS: [&str; 4] = ["sound", "stop", "sound_tags", "stop_tags"];

pub fn is_sound_item(item: &Value) -> bool {
    let names_a_sound = SOUND_KEYS.iter().any(|k| item.get(k).is_some());
    if names_a_sound && item.get("text").is_some() {
        eprintln!(
            "inject: an item carries both `text` and a sound -> speaking the text, dropping the sound"
        );
        return false;
    }
    names_a_sound
}

/// Pretty-print JSON without escaping non-ASCII, matching Python's
pub fn to_pretty_json<T: Serialize>(value: &T) -> Result<String> {
    let mut s = serde_json::to_string_pretty(value)?;
    s.push('\n');
    Ok(s)
}

pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    atomic_write(path, &to_pretty_json(value)?)
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let v = serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(v)
}

/// Character count, not byte count — the Python code sized everything in
pub fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// Collapse every whitespace run to a single space and trim the ends.
pub fn squeeze_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether a string has content the speech pipeline can pronounce.
pub fn has_speakable_content(s: &str) -> bool {
    s.chars().any(|c| c.is_alphanumeric())
}

/// First `n` characters, for log lines. Slicing a `&str` by bytes would panic
pub fn head_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Fold Vietnamese diacritics to ASCII lowercase, so `thai son` matches
pub fn fold_char(c: char) -> char {
    match c {
        'à' | 'á' | 'ạ' | 'ả' | 'ã' | 'â' | 'ầ' | 'ấ' | 'ậ' | 'ẩ' | 'ẫ' | 'ă' | 'ằ' | 'ắ' | 'ặ'
        | 'ẳ' | 'ẵ' => 'a',
        'À' | 'Á' | 'Ạ' | 'Ả' | 'Ã' | 'Â' | 'Ầ' | 'Ấ' | 'Ậ' | 'Ẩ' | 'Ẫ' | 'Ă' | 'Ằ' | 'Ắ' | 'Ặ'
        | 'Ẳ' | 'Ẵ' => 'a',
        'è' | 'é' | 'ẹ' | 'ẻ' | 'ẽ' | 'ê' | 'ề' | 'ế' | 'ệ' | 'ể' | 'ễ' => 'e',
        'È' | 'É' | 'Ẹ' | 'Ẻ' | 'Ẽ' | 'Ê' | 'Ề' | 'Ế' | 'Ệ' | 'Ể' | 'Ễ' => 'e',
        'ì' | 'í' | 'ị' | 'ỉ' | 'ĩ' => 'i',
        'Ì' | 'Í' | 'Ị' | 'Ỉ' | 'Ĩ' => 'i',
        'ò' | 'ó' | 'ọ' | 'ỏ' | 'õ' | 'ô' | 'ồ' | 'ố' | 'ộ' | 'ổ' | 'ỗ' | 'ơ' | 'ờ' | 'ớ' | 'ợ'
        | 'ở' | 'ỡ' => 'o',
        'Ò' | 'Ó' | 'Ọ' | 'Ỏ' | 'Õ' | 'Ô' | 'Ồ' | 'Ố' | 'Ộ' | 'Ổ' | 'Ỗ' | 'Ơ' | 'Ờ' | 'Ớ' | 'Ợ'
        | 'Ở' | 'Ỡ' => 'o',
        'ù' | 'ú' | 'ụ' | 'ủ' | 'ũ' | 'ư' | 'ừ' | 'ứ' | 'ự' | 'ử' | 'ữ' => 'u',
        'Ù' | 'Ú' | 'Ụ' | 'Ủ' | 'Ũ' | 'Ư' | 'Ừ' | 'Ứ' | 'Ự' | 'Ử' | 'Ữ' => 'u',
        'ỳ' | 'ý' | 'ỵ' | 'ỷ' | 'ỹ' => 'y',
        'Ỳ' | 'Ý' | 'Ỵ' | 'Ỷ' | 'Ỹ' => 'y',
        'đ' => 'd',
        'Đ' => 'd',
        c => c.to_ascii_lowercase(),
    }
}

pub fn fold(s: &str) -> String {
    s.chars().map(fold_char).collect()
}

/// Expand a leading `~` to `$HOME`. Prompts and `-i` flags are not a shell,
pub fn expand_tilde(s: &str) -> std::path::PathBuf {
    if s == "~" || s.starts_with("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return std::path::PathBuf::from(home + &s[1..]);
        }
    }
    std::path::PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_in_place() {
        let dir = std::env::temp_dir().join("bm-util-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.json");
        atomic_write(&p, "one").unwrap();
        atomic_write(&p, "two").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "two");
        // no temp file left behind
        assert!(!dir.join(".a.json.tmp").exists());
    }

    #[test]
    fn json_roundtrip_keeps_unicode_readable() {
        let dir = std::env::temp_dir().join("bm-util-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("u.json");
        let v = serde_json::json!({"name": "Thục Đoan"});
        write_json(&p, &v).unwrap();
        let raw = std::fs::read_to_string(&p).unwrap();
        assert!(raw.contains("Thục Đoan"), "expected raw UTF-8, got {raw}");
        let back: serde_json::Value = read_json(&p).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn char_helpers_are_not_byte_sliced() {
        let s = "Chương một";
        assert_eq!(char_len(s), 10);
        assert_eq!(head_chars(s, 6), "Chương");
        assert_eq!(squeeze_ws("  a\n\t b  "), "a b");
    }

    #[test]
    fn speakable_content_distinguishes_words_and_cues_from_punctuation() {
        for text in [",", "...", "！？", "  "] {
            assert!(!has_speakable_content(text), "{text:?} is not speakable");
        }
        for text in ["Ừm!", "123", "[cười]"] {
            assert!(has_speakable_content(text), "{text:?} is speakable");
        }
    }

    #[test]
    fn fold_strips_vietnamese_diacritics_and_case() {
        assert_eq!(fold("Phạm Tuyên"), "pham tuyen");
        assert_eq!(fold("Đức Trí"), "duc tri");
        assert_eq!(fold("Adam"), "adam");
        assert_eq!(fold("neutral"), "neutral");
    }

    #[test]
    fn parallel_map_preserves_order_and_surfaces_the_first_error() {
        let items: Vec<usize> = (0..64).collect();
        let got = parallel_map(&items, |_, v| Ok(*v * 2)).unwrap();
        assert_eq!(got, (0..64).map(|v| v * 2).collect::<Vec<_>>());

        let err = parallel_map(&items, |_, v| {
            if *v == 7 {
                anyhow::bail!("boom {v}")
            } else {
                Ok(*v)
            }
        })
        .unwrap_err();
        assert!(err.to_string().contains("boom 7"), "{err}");
    }

    #[test]
    fn expand_tilde_grows_only_a_leading_bare_tilde() {
        let _env = crate::ENV_LOCK.lock().unwrap();
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand_tilde("~"), std::path::PathBuf::from(&home));
        assert_eq!(
            expand_tilde("~/x"),
            std::path::PathBuf::from(format!("{home}/x"))
        );
        // `~user`, absolute, relative and empty pass through untouched.
        assert_eq!(
            expand_tilde("~other/x"),
            std::path::PathBuf::from("~other/x")
        );
        assert_eq!(expand_tilde("/abs/x"), std::path::PathBuf::from("/abs/x"));
        assert_eq!(expand_tilde("rel/x"), std::path::PathBuf::from("rel/x"));
        assert_eq!(expand_tilde(""), std::path::PathBuf::from(""));
    }
}
