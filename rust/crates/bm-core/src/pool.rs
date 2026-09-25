//! Sample voice pool: tag-matched clone voices from `refs/`.
//!
//! Preset voices cover gender; they know nothing about age or build. The pool
//! covers that: each sample clip is tagged (`young-female-1.mp3` → young,
//! female) and each bible character carries tags too. A new character rolls a
//! voice from the compatible samples — sharing at least one tag and clashing
//! on none — and falls back to the preset pools when nothing fits.
//!
//! The registry is `voice-pool.json` at the repo root (`name -> {file,
//! tags}`), gitignored like `voices.json`. Filenames are only the *suggestion*:
//! `add_sample` parses the tags out of the name, the registry is the truth.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Tag pairs that can never share a voice. Everything else is free-form: a tag
/// the table does not mention only ever matches by equality.
const CONFLICTS: [(&str, &str); 3] = [("male", "female"), ("young", "old"), ("strong", "weak")];

/// One pooled sample: the clip enrolled as a clone voice plus its tags.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct PoolEntry {
    /// `refs/<file>`, the clip `voices.json` enrolls on workers.
    pub file: String,
    pub tags: Vec<String>,
}

/// `name -> entry`. Ordered so the file on disk diffs cleanly.
pub type Pool = BTreeMap<String, PoolEntry>;

/// Tags suggested by a sample filename: lowercase tokens of the stem on
/// `-`/`_` splits, minus the trailing take number.
///
/// `young-female-1.mp3` → `[young, female]`; `old_male_2.wav` → `[old, male]`.
/// A bare proper name (`bao-cong.mp3`) parses to its words — harmless, because
/// only registry members are ever candidates.
pub fn parse_sample_tags(stem: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for tok in stem
        .split(['-', '_', ' '])
        .map(|t| t.trim().to_lowercase())
        .filter(|t| !t.is_empty())
    {
        // `young-female-1`: the take number is an identity, not a tag.
        if tok.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        if !out.contains(&tok) {
            out.push(tok);
        }
    }
    out
}

/// Whether a sample may voice a character: at least one shared tag, and no
/// conflict pair split across the two sides.
///
/// `young-female-1` voices `young`, `female` and `young+female` — but never
/// `young+male`, where `male` clashes with the sample's `female`.
pub fn compatible(sample: &[String], character: &[String]) -> bool {
    if !sample.iter().any(|t| character.contains(t)) {
        return false;
    }
    !CONFLICTS.iter().any(|(a, b)| {
        (sample.contains(&a.to_string()) && character.contains(&b.to_string()))
            || (sample.contains(&b.to_string()) && character.contains(&a.to_string()))
    })
}

/// Tags for a bible character that predates `tags`: derived from its
/// `voice_hint`, female-first (`female` contains `male`).
///
/// Whole words only: substring matching would read the `old` in `cold`.
pub fn tags_from_hint(hint: &str) -> Vec<String> {
    let words: Vec<String> = hint
        .to_lowercase()
        .split(|c: char| !c.is_alphabetic())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_string())
        .collect();
    let has = |w: &str| words.iter().any(|x| x == w);
    let mut out: Vec<String> = Vec::new();
    let mut push = |t: &str| {
        if !out.contains(&t.to_string()) {
            out.push(t.to_string());
        }
    };
    if has("girl") {
        push("young");
        push("female");
    } else if has("boy") {
        push("young");
        push("male");
    } else {
        if has("elderly") || has("old") {
            push("old");
        } else if has("young") {
            push("young");
        }
        if has("female") || has("nữ") || has("cô") || has("gái") || has("bà") || has("lady") {
            push("female");
        } else if has("male")
            || has("nam")
            || has("ông")
            || has("lão")
            || has("anh")
            || has("trai")
            || has("man")
        {
            push("male");
        }
    }
    if has("strong") {
        push("strong");
    } else if has("weak") {
        push("weak");
    }
    out
}

/// Pool members compatible with a character, by name, sorted.
pub fn candidates(pool: &Pool, character_tags: &[String]) -> Vec<String> {
    let mut out: Vec<String> = pool
        .iter()
        .filter(|(_, e)| compatible(&e.tags, character_tags))
        .map(|(name, _)| name.clone())
        .collect();
    out.sort();
    out
}

/// Read the registry. A missing file is "no pool", not an error — and a broken
/// one reads as empty rather than failing a render for a bookkeeping file.
pub fn load_pool(path: &Path) -> Pool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Pool::new();
    };
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Pool::new();
    };
    let Some(obj) = doc.as_object() else {
        return Pool::new();
    };
    obj.iter()
        .filter(|(k, _)| !k.starts_with('_'))
        .filter_map(|(k, v)| {
            serde_json::from_value::<PoolEntry>(v.clone())
                .ok()
                .map(|e| (k.clone(), e))
        })
        .collect()
}

/// The clone manifest: enrolled voice name -> clip, from `voices.json` at the
/// repo root. Missing or broken reads as empty, like the pool.
pub fn load_manifest(root: &Path) -> std::collections::BTreeMap<String, String> {
    std::fs::read_to_string(root.join("voices.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.as_object().cloned())
        .map(|obj| {
            obj.into_iter()
                .filter(|(k, _)| !k.starts_with('_'))
                .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

fn write_pool(path: &Path, pool: &Pool) -> anyhow::Result<()> {
    let mut doc = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| {
            serde_json::json!({"_note": "Sample voice pool: name -> {file, tags}. Tags suggested from the filename (young-female-1.mp3 -> young, female); the registry is the truth. Clips live in refs/ and are enrolled via voices.json."})
        });
    for (name, entry) in pool {
        doc[name] = serde_json::to_value(entry)?;
    }
    crate::util::atomic_write(path, &serde_json::to_string_pretty(&doc)?)?;
    Ok(())
}

/// Resolve a user-typed clip path: `~` grows to `$HOME`, and a relative path
/// is tried against the working directory first, then the repo root. The TUI
/// prompt is not a shell, so neither happens by itself.
fn resolve_clip(root: &Path, src: &Path) -> PathBuf {
    let expanded = crate::util::expand_tilde(&src.to_string_lossy());
    if expanded.is_file() || expanded.is_absolute() {
        return expanded;
    }
    let under_root = root.join(&expanded);
    if under_root.is_file() {
        under_root
    } else {
        // Return the CWD-relative form so the "no such file" error names what
        // was typed, not a guess.
        expanded
    }
}

/// Enroll a clip into the pool: copy it under `refs/`, tag it from its
/// filename (or `tags_override`), register it in `voice-pool.json` under
/// `name_override` (or the file stem), and map it in `voices.json` so the next
/// provision enrolls it on workers.
///
/// Returns the human-readable lines the caller should log.
pub fn add_sample(
    root: &Path,
    src: &Path,
    tags_override: Option<Vec<String>>,
    name_override: Option<String>,
) -> anyhow::Result<Vec<String>> {
    use anyhow::Context;
    let mut log = Vec::new();
    let src = resolve_clip(root, src);
    let src = src.as_path();
    if !src.is_file() {
        anyhow::bail!("no such file: {}", src.display());
    }
    let filename = src
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let stem = src
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    if stem.is_empty() {
        anyhow::bail!("cannot take a sample name from {}", src.display());
    }
    // An explicit name answers to exactly that; filename tags only apply to
    // the classic pooled shape (no rename, no override).
    let name = name_override
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or(stem.clone());
    let ext = src
        .extension()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    if !matches!(
        ext.to_lowercase().as_str(),
        "mp3" | "wav" | "m4a" | "ogg" | "flac"
    ) {
        anyhow::bail!("{filename:?} is not audio (mp3/wav/m4a/ogg/flac)");
    }
    let tags = match tags_override {
        // Explicit — including empty, which is a named voice: assignable by
        // hand, never auto-rolled (`compatible` needs a shared tag).
        Some(t) => t,
        // Renamed without tags is also a named voice, not a pool sample.
        None if name != stem => Vec::new(),
        // Classic pooled sample: tags come from the filename or not at all.
        None => {
            let t = parse_sample_tags(&stem);
            if t.is_empty() {
                anyhow::bail!("no tags in {stem:?} — rename to tag-tag-N.ext or pass --tags");
            }
            t
        }
    };

    let refs = root.join("refs");
    std::fs::create_dir_all(&refs)?;
    let dest = refs.join(&filename);
    if dest.exists() {
        // Covers "already added" and "pointed straight at refs/x.mp3": the
        // bytes on disk are compared, so a name collision with different
        // audio can never silently win.
        let identical = std::fs::read(src).ok() == std::fs::read(&dest).ok();
        if !identical {
            anyhow::bail!("refs/{filename} already exists with different bytes — rename first");
        }
        log.push(format!("refs/{filename} already in place"));
    } else {
        std::fs::copy(src, &dest)
            .with_context(|| format!("copying {} -> {}", src.display(), dest.display()))?;
        log.push(format!("copied {} -> refs/{filename}", src.display()));
    }

    let pool_path = root.join("voice-pool.json");
    let mut pool = load_pool(&pool_path);
    pool.insert(
        name.clone(),
        PoolEntry {
            file: format!("refs/{filename}"),
            tags: tags.clone(),
        },
    );
    write_pool(&pool_path, &pool)?;
    if tags.is_empty() {
        log.push(format!(
            "named voice: {name} (manual assignment only, never auto-rolled)"
        ));
    } else {
        log.push(format!("pool: {name} [{tags}]", tags = tags.join(", ")));
    }

    // The pool decides *who* a sample may voice; `voices.json` gets it onto
    // workers. One entry, same name, so the two files cannot drift apart.
    let manifest_path = root.join("voices.json");
    let mut manifest: serde_json::Value = std::fs::read_to_string(&manifest_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| {
            serde_json::json!({"_note": "Clone voices enrolled on every worker during provisioning. Re-enrolled automatically when missing (e.g. after a venv rebuild wipes the voice store). Reference clips live in refs/."})
        });
    manifest[name.clone()] = serde_json::Value::String(format!("refs/{filename}"));
    crate::util::atomic_write(&manifest_path, &serde_json::to_string_pretty(&manifest)?)?;
    log.push(format!(
        "voices.json: {name} -> refs/{filename} (enrolled on next provision)"
    ));

    // Usable now, not just after provisioning: enroll into this machine's own
    // store when it has one. A failure here never fails the add — the registry
    // above is the durable state; the enroll is a convenience for this box.
    // Blocking (loads the voice model); callers run it off the UI thread.
    match enroll_local(root, &name, &format!("refs/{filename}")) {
        Ok(lines) => log.extend(lines),
        Err(e) => log.push(format!(
            "local enroll failed (provision still covers it): {e:#}"
        )),
    }
    Ok(log)
}

/// Bake clone voices the manifest declares but the pushed store lacks.
///
/// The disconnect this closes: `add_sample` enrolls into the venv package's
/// preset file, while provision pushes (and warns against) the repo's
/// `models/voices.json` — a different file. Without this merge, an enrolled
/// voice warned forever ("declared in voices.json but missing from
/// models/voices.json — fix it and run :prov again") and every render naming
/// it failed on workers, no matter how often `:prov` ran.
///
/// Copies missing presets from the local enrollment's own files into the
/// bake. Called at the top of `provision`, before the stamp is computed, so
/// the `tts_hash` drift pushes the updated store to workers in the same run.
/// Returns the names baked (empty = nothing to do). Voices enrolled nowhere
/// stay missing — the provision warning still names exactly those.
pub fn bake_missing_voices(root: &Path) -> Vec<String> {
    let manifest: BTreeMap<String, String> = std::fs::read_to_string(root.join("voices.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let bake_path = root.join("models/voices.json");
    let mut bake: serde_json::Value = std::fs::read_to_string(&bake_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(serde_json::Value::Null);
    let Some(presets) = bake.get_mut("presets").and_then(|p| p.as_object_mut()) else {
        return Vec::new();
    };
    let missing: Vec<String> = manifest
        .keys()
        .filter(|n| !n.starts_with('_') && !presets.contains_key(*n))
        .cloned()
        .collect();
    if missing.is_empty() {
        return Vec::new();
    }
    let stores = enrollment_stores(root);
    if stores.is_empty() {
        return Vec::new();
    }
    let mut baked = Vec::new();
    for name in &missing {
        for store in &stores {
            let entry: Option<serde_json::Value> = std::fs::read_to_string(store)
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok())
                .and_then(|v: serde_json::Value| v.get("presets")?.get(name).cloned());
            if let Some(entry) = entry {
                presets.insert(name.clone(), entry);
                baked.push(name.clone());
                break;
            }
        }
    }
    if baked.is_empty() {
        return Vec::new();
    }
    // Compact like the file already is (one line): nothing reorders, only
    // the missing presets are added.
    let text = serde_json::to_string(&bake).unwrap_or_default();
    if crate::util::atomic_write(&bake_path, &text).is_err() {
        return Vec::new();
    }
    baked.sort();
    baked
}

/// Preset files the local enrollment writes to, turbo first: the venv
/// package's own assets, found through the venv this checkout carries.
/// Empty where there is no venv — then nothing can be baked.
fn enrollment_stores(root: &Path) -> Vec<PathBuf> {
    let layout = crate::Layout::new(root);
    let Some(py) = layout.venv_python() else {
        return Vec::new();
    };
    // `.venv/bin/python` -> `.venv`.
    let Some(venv) = py.parent().and_then(|b| b.parent()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Ok(lib) = std::fs::read_dir(venv.join("lib")) {
        let mut pydirs: Vec<PathBuf> = lib.filter_map(|e| e.ok().map(|x| x.path())).collect();
        pydirs.sort();
        for dir in pydirs {
            for name in ["voices_v3_turbo.json", "voices_v3_nano.json"] {
                let p = dir.join("site-packages/vieneu/assets").join(name);
                if p.is_file() {
                    out.push(p);
                }
            }
        }
    }
    out
}

/// Enroll one sample into THIS machine's voice store, so renders use it
/// immediately. Skips cleanly with no local venv — provision enrolls from
/// `voices.json` then.
pub fn enroll_local(root: &Path, name: &str, file: &str) -> anyhow::Result<Vec<String>> {
    let layout = crate::Layout::new(root);
    let Some(py) = layout.venv_python() else {
        return Ok(vec![
            "no local voice store — enrolled on next provision".to_string()
        ]);
    };
    if !root.join("python/tts_vieneu.py").is_file() {
        return Ok(vec![
            "no local voice store — enrolled on next provision".to_string()
        ]);
    }
    // Name and clip travel as argv, never interpolated: diacritics and spaces
    // survive intact, and there is nothing to quote.
    let script = [
        "import sys, tts_vieneu as vn",
        "name, ref = sys.argv[1], sys.argv[2]",
        "tts = vn.engine()",
        "have = {vid for label, vid in tts.list_preset_voices() if label == vid}",
        "print('already enrolled' if name in have else 'enrolling ' + name, flush=True)",
        "if name not in have:",
        "    tts.add_voice(name, ref)",
        "    tts.save_voices()",
        "    print('enrolled ' + name, flush=True)",
    ]
    .join("\n");
    let out = std::process::Command::new(&py)
        .arg("-c")
        .arg(&script)
        .arg(name)
        .arg(file)
        .current_dir(root)
        .env("PYTHONPATH", root.join("python"))
        .output()?;
    let mut lines: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!(
            "{} ({})",
            lines.pop().unwrap_or_else(|| "enroll failed".into()),
            crate::util::head_chars(err.trim(), 160)
        );
    }
    Ok(lines)
}

/// Synthesize `text` with `voice` on THIS machine, for an offline audition:
/// no inductor, no worker, no sidecar. Same engine call the sidecar makes
/// (`temperature`/`silence_p` are its defaults), so the sample sounds like
/// the render would. Needs the model weights (downloaded on first use) and
/// the voice enrolled in the local store (`:A` does that) — otherwise the
/// error names what is missing instead of 500ing through HTTP.
pub fn synth_preview(
    root: &Path,
    voice: &str,
    text: &str,
    dest: &Path,
) -> anyhow::Result<Vec<String>> {
    let layout = crate::Layout::new(root);
    let Some(py) = layout.venv_python() else {
        anyhow::bail!("no local voice store — :B to connect, or provision this box first");
    };
    let script = [
        "import sys, unicodedata, tts_vieneu as vn",
        "voice, text, dest = sys.argv[1], sys.argv[2], sys.argv[3]",
        "tts = vn.engine()",
        "fold = lambda s: ''.join(c for c in unicodedata.normalize('NFD', s or '').replace('đ','d').replace('Đ','d').lower() if unicodedata.category(c) != 'Mn' and c not in '-_ ')",
        "known = {fold(x) for pair in tts.list_preset_voices() for x in (pair[0], pair[1], pair[0].split('—')[0].split('–')[0])}",
        "assert not voice or fold(voice) in known, 'unknown voice %r — not enrolled here; add it (:A/:N) or provision' % voice",
        "tts.save(tts.infer(text, voice=voice, temperature=0.8, silence_p=0.15), dest)",
        "print('previewed ' + voice, flush=True)",
    ]
    .join("\n");
    let out = std::process::Command::new(&py)
        .arg("-c")
        .arg(&script)
        .arg(voice)
        .arg(text)
        .arg(dest)
        .current_dir(root)
        .env("PYTHONPATH", root.join("python"))
        .output()?;
    let mut lines: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!(
            "{} ({})",
            lines.pop().unwrap_or_else(|| "preview failed".into()),
            crate::util::head_chars(err.trim(), 160)
        );
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_clip_expands_tilde_and_falls_back_to_the_root() {
        // `~` without a shell.
        let _env = crate::ENV_LOCK.lock().unwrap();
        let home = std::env::temp_dir().join("bm-clip-home");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("young-male-8.mp3"), b"fake").unwrap();
        let saved = std::env::var("HOME").ok();
        std::env::set_var("HOME", &home);
        let got = resolve_clip(
            Path::new("/nonexistent-root"),
            Path::new("~/young-male-8.mp3"),
        );
        assert_eq!(got, home.join("young-male-8.mp3"));
        if let Some(h) = saved {
            std::env::set_var("HOME", h);
        } else {
            std::env::remove_var("HOME");
        }

        // Relative, missing from the working directory: found under the root.
        // (Cargo runs tests with CWD at the crate dir, which has no `in/`.)
        let root = std::env::temp_dir().join("bm-clip-root");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("in")).unwrap();
        std::fs::write(root.join("in/young-male-9.mp3"), b"fake").unwrap();
        let got = resolve_clip(&root, Path::new("in/young-male-9.mp3"));
        assert_eq!(got, root.join("in/young-male-9.mp3"));

        // End to end through add_sample with a root-relative path.
        let log = add_sample(&root, Path::new("in/young-male-9.mp3"), None, None).unwrap();
        assert!(log.iter().any(|l| l.contains("young-male-9")), "{log:?}");
        assert!(root.join("refs/young-male-9.mp3").is_file());

        // Missing everywhere: the error names what was typed.
        let err = add_sample(&root, Path::new("nope/young-male-9.mp3"), None, None).unwrap_err();
        assert!(err.to_string().contains("nope/young-male-9.mp3"), "{err}");
    }

    #[test]
    fn filename_tags_drop_the_take_number() {
        assert_eq!(parse_sample_tags("young-female-1"), vec!["young", "female"]);
        assert_eq!(parse_sample_tags("old_male_12"), vec!["old", "male"]);
        assert_eq!(parse_sample_tags("shizuka"), vec!["shizuka"]);
        assert!(
            parse_sample_tags("007").is_empty(),
            "a bare number is no tag"
        );
    }

    #[test]
    fn young_male_cannot_take_a_young_female_sample() {
        let sample = vec!["young".to_string(), "female".to_string()];
        let has = |tags: &[&str]| tags.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(
            compatible(&sample, &has(&["young"])),
            "shares young, clashes on nothing"
        );
        assert!(compatible(&sample, &has(&["female"])), "shares female");
        assert!(
            compatible(&sample, &has(&["young", "female"])),
            "shares both"
        );
        assert!(
            !compatible(&sample, &has(&["young", "male"])),
            "male clashes with female"
        );
        assert!(
            !compatible(&sample, &has(&["old", "female"])),
            "old clashes with young"
        );
        assert!(
            !compatible(&sample, &has(&["male"])),
            "no shared tag at all"
        );
        assert!(
            !compatible(&sample, &has(&[])),
            "untagged characters fall back to presets"
        );
    }

    #[test]
    fn free_form_tags_match_by_equality_only() {
        let sample = vec!["sly".to_string()];
        let has = |tags: &[&str]| tags.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(compatible(&sample, &has(&["sly", "young"])));
        assert!(
            !compatible(&sample, &has(&["young"])),
            "unknown tags give no antonyms, only overlap"
        );
    }

    #[test]
    fn hints_backfill_tags_for_untagged_bible_entries() {
        assert_eq!(tags_from_hint("elderly male, stern"), vec!["old", "male"]);
        assert_eq!(tags_from_hint("girl, lively"), vec!["young", "female"]);
        assert_eq!(tags_from_hint("boy, polite"), vec!["young", "male"]);
        // Female-first: "female" contains "male".
        assert_eq!(tags_from_hint("adult female, cold"), vec!["female"]);
        assert_eq!(tags_from_hint("adult male, warm"), vec!["male"]);
        assert!(tags_from_hint("ageless genderless system").is_empty());
    }

    #[test]
    fn candidates_are_compatible_names_sorted() {
        let mut pool = Pool::new();
        pool.insert(
            "young-female-1".into(),
            PoolEntry {
                file: "refs/young-female-1.mp3".into(),
                tags: vec!["young".into(), "female".into()],
            },
        );
        pool.insert(
            "young-female-2".into(),
            PoolEntry {
                file: "refs/young-female-2.mp3".into(),
                tags: vec!["young".into(), "female".into()],
            },
        );
        pool.insert(
            "old-male-1".into(),
            PoolEntry {
                file: "refs/old-male-1.mp3".into(),
                tags: vec!["old".into(), "male".into()],
            },
        );
        let got = candidates(&pool, &["young".to_string(), "female".to_string()]);
        assert_eq!(got, vec!["young-female-1", "young-female-2"]);
        assert!(candidates(&pool, &[]).is_empty());
    }

    #[test]
    fn a_missing_or_broken_registry_is_an_empty_pool() {
        assert!(load_pool(Path::new("/nonexistent/voice-pool.json")).is_empty());
        let d = std::env::temp_dir().join("bm-pool-broken");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("voice-pool.json"), "{ nope").unwrap();
        assert!(load_pool(&d.join("voice-pool.json")).is_empty());
    }

    #[test]
    fn add_sample_registers_pool_and_enrollment_together() {
        let d = std::env::temp_dir().join("bm-pool-add");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let src = d.join("young-female-9.mp3");
        std::fs::write(&src, b"fake-audio").unwrap();

        let log = add_sample(&d, &src, None, None).unwrap();
        assert!(log.iter().any(|l| l.contains("young-female-9")), "{log:?}");
        assert!(d.join("refs/young-female-9.mp3").is_file());
        // No venv here: enrollment defers to provisioning, loudly, not silently.
        assert!(log.iter().any(|l| l.contains("next provision")), "{log:?}");

        let pool = load_pool(&d.join("voice-pool.json"));
        assert_eq!(pool["young-female-9"].tags, vec!["young", "female"]);
        let manifest: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(d.join("voices.json")).unwrap()).unwrap();
        assert_eq!(manifest["young-female-9"], "refs/young-female-9.mp3");

        // Adding the same file twice is idempotent, not an error.
        let again = add_sample(&d, &src, None, None).unwrap();
        assert!(
            again.iter().any(|l| l.contains("already in place")),
            "{again:?}"
        );
    }

    #[test]
    fn venv_order_prefers_the_managed_store() {
        // Enrollment and serving must resolve the same interpreter, or a
        // fresh voice enrolls into a store nobody reads and previews 500.
        let d = std::env::temp_dir().join("bm-pool-venvs");
        let _ = std::fs::remove_dir_all(&d);
        let managed = d.join("python/.venv/bin/python");
        let legacy = d.join(".venv/bin/python");
        std::fs::create_dir_all(managed.parent().unwrap()).unwrap();
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&managed, b"x").unwrap();
        std::fs::write(&legacy, b"x").unwrap();
        let layout = crate::Layout::new(&d);
        assert_eq!(layout.venv_python().as_deref(), Some(managed.as_path()));
        std::fs::remove_file(&managed).unwrap();
        assert_eq!(layout.venv_python().as_deref(), Some(legacy.as_path()));
        std::fs::remove_file(&legacy).unwrap();
        assert_eq!(layout.venv_python(), None);
    }

    #[test]
    fn preview_without_a_venv_says_so_instead_of_500ing() {
        let d = std::env::temp_dir().join("bm-pool-no-venv-preview");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let err = synth_preview(&d, "Đức Trí", "xin chào", &d.join("p.wav")).unwrap_err();
        assert!(err.to_string().contains("no local voice store"), "{err}");
    }

    #[test]
    fn enroll_without_a_venv_defers_to_provisioning() {
        let d = std::env::temp_dir().join("bm-pool-no-venv");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        // Positive path needs model weights and minutes; the skip contract —
        // Ok, and said out loud — is what pins the fresh-clone behavior.
        let lines = enroll_local(&d, "young-female-1", "refs/young-female-1.mp3").unwrap();
        assert!(
            lines.iter().any(|l| l.contains("next provision")),
            "{lines:?}"
        );
    }

    #[test]
    fn a_renamed_voice_is_named_not_pooled() {
        // `refs/narrator.mp3 as Narrator`: the registry and the enrollment
        // answer to the given name — and with no tags it never auto-rolls,
        // however tag-compatible a character looks.
        let d = std::env::temp_dir().join("bm-pool-rename");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let src = d.join("narrator.mp3");
        std::fs::write(&src, b"fake-audio").unwrap();

        let log = add_sample(&d, &src, None, Some("Narrator".into())).unwrap();
        assert!(log.iter().any(|l| l.contains("named voice")), "{log:?}");
        let pool = load_pool(&d.join("voice-pool.json"));
        assert!(pool["Narrator"].tags.is_empty());
        assert!(
            !pool.contains_key("narrator"),
            "the stem must not leak in as a second voice"
        );
        assert!(
            candidates(&pool, &["young".to_string(), "female".to_string()]).is_empty(),
            "a named voice rolls for nobody"
        );
        let manifest: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(d.join("voices.json")).unwrap()).unwrap();
        assert_eq!(manifest["Narrator"], "refs/narrator.mp3");
    }

    #[test]
    fn explicit_tags_still_pool_a_renamed_voice() {
        // The power-user shape: custom name AND rotation tags.
        let d = std::env::temp_dir().join("bm-pool-rename-tags");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let src = d.join("clip.mp3");
        std::fs::write(&src, b"fake-audio").unwrap();

        add_sample(
            &d,
            &src,
            Some(vec!["old".into(), "male".into()]),
            Some("Lão".into()),
        )
        .unwrap();
        let pool = load_pool(&d.join("voice-pool.json"));
        assert_eq!(pool["Lão"].tags, vec!["old", "male"]);
        assert_eq!(
            candidates(&pool, &["old".to_string(), "male".to_string()]),
            vec!["Lão".to_string()]
        );
    }

    #[test]
    fn bake_merges_only_manifest_voices_missing_from_the_store() {
        // Wolf's box: enrolled in the venv store, absent from the pushed
        // bake — the warning that never cleared. The bake copies exactly
        // those, and a second run is a silent no-op.
        let d = std::env::temp_dir().join("bm-pool-bake");
        let _ = std::fs::remove_dir_all(&d);
        let assets = d.join(".venv/lib/python3.12/site-packages/vieneu/assets");
        std::fs::create_dir_all(&assets).unwrap();
        std::fs::create_dir_all(d.join("models")).unwrap();
        std::fs::create_dir_all(d.join(".venv/bin")).unwrap();
        std::fs::write(d.join(".venv/bin/python"), b"x").unwrap();
        std::fs::write(
            d.join("voices.json"),
            r#"{"Have":"refs/h.mp3","Want":"refs/w.mp3","Ghost":"refs/g.mp3","_note":"x"}"#,
        )
        .unwrap();
        std::fs::write(
            d.join("models/voices.json"),
            r#"{"meta":{},"default_voice":"Have","presets":{"Have":{"emb":[1]}}}"#,
        )
        .unwrap();
        std::fs::write(
            assets.join("voices_v3_turbo.json"),
            r#"{"presets":{"Want":{"emb":[2]},"Else":{"emb":[3]}}}"#,
        )
        .unwrap();

        assert_eq!(bake_missing_voices(&d), vec!["Want".to_string()]);
        let bake: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(d.join("models/voices.json")).unwrap())
                .unwrap();
        assert_eq!(bake["presets"]["Have"]["emb"], serde_json::json!([1]));
        assert_eq!(bake["presets"]["Want"]["emb"], serde_json::json!([2]));
        assert!(
            bake["presets"].get("Else").is_none(),
            "unmentioned presets never ride along"
        );
        assert!(
            bake["presets"].get("Ghost").is_none(),
            "enrolled nowhere stays missing for the warning"
        );
        // Idempotent: nothing missing, nothing written.
        assert!(bake_missing_voices(&d).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn bake_without_a_venv_or_bake_is_a_quiet_noop() {
        let d = std::env::temp_dir().join("bm-pool-bake-none");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("models")).unwrap();
        std::fs::write(d.join("voices.json"), r#"{"Want":"refs/w.mp3"}"#).unwrap();
        std::fs::write(d.join("models/voices.json"), r#"{"presets":{"Have":{}}}"#).unwrap();
        // No venv here, so nothing can be baked — and nothing breaks.
        assert!(bake_missing_voices(&d).is_empty());
        // No bake file at all: also nothing, not an error.
        std::fs::remove_file(d.join("models/voices.json")).unwrap();
        assert!(bake_missing_voices(&d).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }
}
