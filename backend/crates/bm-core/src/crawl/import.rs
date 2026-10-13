//! Manual import: supplying a chapter as a file instead of fetching it.
//! `chNN.txt` has to be dense (the range math, `--through` and every "what is
//! missing" check depend on it), so the import must establish `n` — and it does

use anyhow::{anyhow, Context, Result};
use std::path::Path;

use super::provider::MIN_CHAPTER_BYTES;
use crate::Layout;

/// One adopted chapter.
#[derive(Debug, Clone)]
pub struct Imported {
    pub n: u32,
    /// Bytes as written — the number the length guard measured.
    pub bytes: usize,
    pub chars: usize,
}

/// Adopt `text` as chapter `n`.
pub fn import_text(layout: &Layout, n: u32, text: &str) -> Result<Imported> {
    let clean = prepare(layout, n, text)?;
    crate::atomic_write(&layout.chapter_txt(n), &clean)?;
    Ok(Imported {
        n,
        bytes: clean.len(),
        chars: clean.chars().count(),
    })
}

/// Everything `import_text` checks, without writing.
pub fn prepare(layout: &Layout, n: u32, text: &str) -> Result<String> {
    if n == 0 {
        anyhow::bail!("chapter 0 is not a chapter — the pipeline's index starts at 1");
    }
    let clean = super::sanitize_chapter_text(text);
    if clean.len() < MIN_CHAPTER_BYTES {
        anyhow::bail!(
            "ch{n}: refusing to import {} bytes ({} chars) — too short to be a chapter, so this is a truncated file or the wrong one",
            clean.len(),
            clean.chars().count()
        );
    }
    // A chapter that already has a script would be left holding the *old*
    let script = layout.script(n);
    if script.is_file() {
        anyhow::bail!(
            "ch{n} is already digested — importing new text would leave {} describing the old text. Remove it first, then import.",
            script.display()
        );
    }
    Ok(clean)
}

/// What one entry of an import request turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Spec {
    /// A file on this machine.
    File { n: Option<u32>, path: String },
    /// Literal chapter text — what a paste looks like when it is not a path.
    Text { n: Option<u32>, text: String },
}

impl Spec {
    pub fn number(&self) -> Option<u32> {
        match self {
            Spec::File { n, .. } | Spec::Text { n, .. } => *n,
        }
    }
}

/// Interpret the strings an operator handed over.
pub fn import_specs(explicit: Option<u32>, inputs: &[String]) -> Result<Vec<Spec>> {
    if inputs.is_empty() {
        anyhow::bail!("nothing to import — give a file path, or paste the chapter text");
    }
    let multiple = inputs.len() > 1;
    let mut out = Vec::new();
    for (i, raw) in inputs.iter().enumerate() {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let explicit_here = (i == 0).then_some(explicit).flatten();
        if Path::new(raw).is_file() {
            out.push(Spec::File {
                n: explicit_here.or_else(|| number_from_name(raw)),
                path: raw.to_string(),
            });
            continue;
        }
        // Not a file. Two very different things look like this: a mistyped or
        if multiple || looks_like_path(raw) {
            anyhow::bail!("{raw}: no such file");
        }
        // A pasted chapter has no filename, so its number must come from the
        let n = explicit_here;
        if n.is_none() {
            anyhow::bail!(
                "cannot tell which chapter this is: give the text a number (`:import 34`, or a file named ch34.txt)"
            );
        }
        out.push(Spec::Text {
            n,
            text: raw.to_string(),
        });
    }
    if out.is_empty() {
        anyhow::bail!("nothing to import — give a file path, or paste the chapter text");
    }
    Ok(out)
}

/// Whether a string is a path someone meant to name rather than a chapter
fn looks_like_path(s: &str) -> bool {
    !s.contains('\n') && (s.contains('/') || s.ends_with(".txt") || s.ends_with(".md"))
}

/// Read one spec's text: the file's contents, or the literal text.
pub fn read_spec(spec: &Spec) -> Result<String> {
    match spec {
        Spec::Text { text, .. } => Ok(text.clone()),
        Spec::File { path, .. } => {
            std::fs::read_to_string(path).with_context(|| format!("reading {path}"))
        }
    }
}

/// The chapter number in a filename: `ch34.txt`, `34.txt`, `chapter-34.txt`,
pub fn number_from_name(name: &str) -> Option<u32> {
    let stem = Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name);
    // The **last** run of digits: that is where the number sits in `ch34`,
    let digits_start = stem.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    if digits_start == stem.len() {
        // No trailing digits — but `034 - Tên chương` has them in the middle,
        let end = stem.rfind(|c: char| c.is_ascii_digit()).map(|i| i + 1)?;
        let start = stem[..end]
            .trim_end_matches(|c: char| c.is_ascii_digit())
            .len();
        if start > 0 && stem[..start].ends_with('.') {
            return None;
        }
        let n: u32 = stem[start..end].parse().ok()?;
        return (n > 0).then_some(n);
    }
    // `chapter-7.5` ends in digits too, and reading that as chapter 5 would
    if stem[..digits_start].ends_with('.') {
        return None;
    }
    let n: u32 = stem[digits_start..].parse().ok()?;
    (n > 0).then_some(n)
}

/// Import every entry. All of it validates before any of it lands.
pub fn import_all(
    layout: &Layout,
    explicit: Option<u32>,
    inputs: &[String],
) -> Result<(Vec<Imported>, String)> {
    let specs = import_specs(explicit, inputs)?;
    // Two passes, deliberately: resolve and validate everything first, write
    let mut planned = Vec::new();
    for spec in &specs {
        let n = spec.number().ok_or_else(|| {
            anyhow!(
                "{}: no chapter number in the name — name it `ch34.txt`, or type the number (`:import 34 <path>`)",
                match spec {
                    Spec::File { path, .. } => path.clone(),
                    Spec::Text { .. } => "pasted text".into(),
                }
            )
        })?;
        let text = read_spec(spec)?;
        planned.push((n, prepare(layout, n, &text)?));
    }
    let mut done = Vec::new();
    let mut lines = Vec::new();
    for (n, clean) in planned {
        crate::atomic_write(&layout.chapter_txt(n), &clean)?;
        let got = Imported {
            n,
            bytes: clean.len(),
            chars: clean.chars().count(),
        };
        lines.push(format!(
            "ch{}: {} bytes ({} chars)",
            got.n, got.bytes, got.chars
        ));
        done.push(got);
    }
    Ok((done, lines.join("; ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(name: &str) -> Layout {
        let dir = std::env::temp_dir().join(format!("bm-import-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        let l = Layout::new(&dir);
        l.ensure().unwrap();
        l
    }

    fn body(tag: &str) -> String {
        format!("Chương 5: Thử nghiệm\n\n{tag}\n")
            + &"Nội dung chương này đủ dài để vượt qua ngưỡng kiểm tra. ".repeat(8)
    }

    #[test]
    fn numbers_come_from_the_filename_and_never_from_the_order() {
        for (name, want) in [
            ("ch34.txt", Some(34)),
            ("34.txt", Some(34)),
            ("chapter-34.txt", Some(34)),
            ("chapter-034.txt", Some(34)),
            ("034 - Tên chương.txt", Some(34)),
            ("/tmp/drop/ch217.txt", Some(217)),
            ("prologue.txt", None),
            ("ch0.txt", None),
            // A decimal bonus chapter has no honest place in a dense integer
            ("chapter-7.5.txt", None),
        ] {
            assert_eq!(number_from_name(name), want, "{name}");
        }
    }

    #[test]
    fn a_numberless_file_is_refused_rather_than_sorted_into_place() {
        let l = layout("nonumber");
        let dir = l.scratch();
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("prologue.txt");
        std::fs::write(&p, body("P")).unwrap();
        let err = import_all(&l, None, &[p.display().to_string()]).unwrap_err();
        assert!(err.to_string().contains("no chapter number"), "{err}");
        // Even alongside a file that does have one: the numberless entry is
        let q = dir.join("ch12.txt");
        std::fs::write(&q, body("Q")).unwrap();
        let err = import_all(
            &l,
            None,
            &[q.display().to_string(), p.display().to_string()],
        )
        .unwrap_err();
        assert!(err.to_string().contains("no chapter number"), "{err}");
        assert!(
            !l.chapter_txt(12).is_file(),
            "nothing lands when any entry of the batch is refused"
        );
    }

    #[test]
    fn a_path_that_does_not_exist_is_refused_instead_of_being_adopted_as_text() {
        // `:import 34 /tmp/typo.txt` must not write the *string* `/tmp/typo.txt`
        let l = layout("typo");
        let err = import_all(&l, Some(34), &["/tmp/typo-does-not-exist.txt".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("no such file"), "{err}");
        assert!(!l.chapter_txt(34).is_file());
        // A single line of prose is still text, not a path.
        let (done, _) = import_all(&l, Some(35), &[body("paste")]).unwrap();
        assert_eq!(done[0].n, 35);
        assert!(l.chapter_txt(35).is_file());
    }

    #[test]
    fn a_multi_entry_batch_lands_all_or_nothing() {
        // The three-file case: the second is too short to be a chapter. The
        let l = layout("atomic");
        let dir = l.scratch();
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("ch40.txt");
        let bad = dir.join("ch41.txt");
        let also = dir.join("ch42.txt");
        std::fs::write(&good, body("G")).unwrap();
        std::fs::write(&bad, "quá ngắn").unwrap();
        std::fs::write(&also, body("A")).unwrap();
        let err = import_all(
            &l,
            None,
            &[
                good.display().to_string(),
                bad.display().to_string(),
                also.display().to_string(),
            ],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("ch41"), "{err}");
        for n in [40, 41, 42] {
            assert!(!l.chapter_txt(n).is_file(), "ch{n} was written anyway");
        }
    }

    #[test]
    fn the_typed_number_applies_to_the_first_entry_only() {
        let l = layout("explicit");
        let dir = l.scratch();
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("drop-a.txt");
        let b = dir.join("drop-b.txt");
        std::fs::write(&a, body("A")).unwrap();
        std::fs::write(&b, body("B")).unwrap();
        // Two files, one number: the second has no number of its own, so the
        let err = import_all(
            &l,
            Some(34),
            &[a.display().to_string(), b.display().to_string()],
        )
        .unwrap_err();
        assert!(err.to_string().contains("no chapter number"), "{err}");
        // Named files import by their own numbers.
        let c = dir.join("ch36.txt");
        let d = dir.join("ch37.txt");
        std::fs::write(&c, body("C")).unwrap();
        std::fs::write(&d, body("D")).unwrap();
        let (done, line) = import_all(
            &l,
            None,
            &[c.display().to_string(), d.display().to_string()],
        )
        .unwrap();
        assert_eq!(done.iter().map(|i| i.n).collect::<Vec<_>>(), vec![36, 37]);
        assert!(line.contains("ch36"));
        assert!(l.chapter_txt(36).is_file() && l.chapter_txt(37).is_file());
    }

    #[test]
    fn pasted_text_imports_under_the_number_typed_and_goes_through_the_boundary() {
        let l = layout("paste");
        // The paste carries a raw entity and a stray entity-encoded quote: the
        let raw = format!(
            "Chương 9: Tên chương\n\n{}&#x27;két&#x27; một tiếng.\n",
            "Nội dung chương này đủ dài để vượt qua ngưỡng kiểm tra. ".repeat(8)
        );
        let (done, _) = import_all(&l, Some(9), &[raw]).unwrap();
        assert_eq!(done[0].n, 9);
        let written = std::fs::read_to_string(l.chapter_txt(9)).unwrap();
        assert!(written.contains("'két'"), "{written}");
        assert!(!written.contains("&#"), "entities are decoded: {written}");
        assert!(written.ends_with('\n'));
    }

    #[test]
    fn an_import_does_not_silently_edit_the_operators_words() {
        // The host no longer knows what any site's furniture looks like, so it
        let l = layout("verbatim");
        let raw = format!(
            "Chương 9: Tên chương\n\nCài đặt đọc\n\n{}xin chào một tiếng.\n",
            "Nội dung chương này đủ dài để vượt qua ngưỡng kiểm tra. ".repeat(8)
        );
        let (done, _) = import_all(&l, Some(9), &[raw]).unwrap();
        assert_eq!(done[0].n, 9);
        let written = std::fs::read_to_string(l.chapter_txt(9)).unwrap();
        assert!(
            written.contains("Cài đặt đọc"),
            "the operator's text is stored as given: {written}"
        );
    }

    #[test]
    fn a_short_paste_is_refused_by_name() {
        let l = layout("short");
        let err = import_text(&l, 4, "quá ngắn").unwrap_err().to_string();
        assert!(err.contains("ch4"), "{err}");
        assert!(err.contains("too short"), "{err}");
        assert!(!l.chapter_txt(4).is_file(), "nothing was written");
    }

    #[test]
    fn importing_over_a_digested_chapter_is_refused_not_silently_shadowed() {
        // The write would land under a script that still describes the old
        let l = layout("digested");
        crate::atomic_write(&l.script(3), r#"{"segments":[]}"#).unwrap();
        let err = import_text(&l, 3, &body("new")).unwrap_err().to_string();
        assert!(err.contains("already digested"), "{err}");
        assert!(err.contains("script/03.json"), "{err}");
        assert!(!l.chapter_txt(3).is_file());
    }

    #[test]
    fn a_missing_path_that_is_not_the_whole_input_is_an_error() {
        let err = import_specs(None, &["/nope/a.txt".into(), "/nope/b.txt".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("no such file"), "{err}");
        // And an empty request says what to do instead of doing nothing.
        let err = import_specs(None, &[]).unwrap_err().to_string();
        assert!(err.contains("nothing to import"), "{err}");
    }
}
