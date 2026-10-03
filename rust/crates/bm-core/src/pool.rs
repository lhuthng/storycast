//! Sample voice pool: tag-matched clone voices from `refs/`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Tag pairs that can never share a voice. Everything else is free-form: a tag
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

/// The clone manifest: enrolled voice name -> clip, read from the given
pub fn load_manifest(path: &Path) -> std::collections::BTreeMap<String, String> {
    std::fs::read_to_string(path)
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
        expanded
    }
}

/// Enroll a clip into the pool: copy it under `refs/`, tag it from its
pub fn add_sample(
    layout: &crate::Layout,
    src: &Path,
    tags_override: Option<Vec<String>>,
    name_override: Option<String>,
) -> anyhow::Result<Vec<String>> {
    use anyhow::Context;
    let mut log = Vec::new();
    let root = layout.root.as_path();
    // The clip is resolved against the checkout (where the operator typed the
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

    let refs = layout.work.join("refs");
    std::fs::create_dir_all(&refs)?;
    let dest = refs.join(&filename);
    if dest.exists() {
        // Covers "already added" and "pointed straight at refs/x.mp3": the
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

    let pool_path = layout.work.join("voice-pool.json");
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
    let manifest_path = layout.work.join("voices.json");
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
    match crate::voices::store_kind(&layout.engine) {
        crate::voices::VoiceStore::Presets => {
            match enroll_local(root, &name, &dest.to_string_lossy()) {
                Ok(lines) => log.extend(lines),
                Err(e) => log.push(format!(
                    "local enroll failed (provision still covers it): {e:#}"
                )),
            }
        }
        crate::voices::VoiceStore::Clips => match enroll_clip_voice(layout, &name, &dest) {
            Ok(Some(file)) => log.push(format!(
                "engine store: {name} -> {file} (the sidecar clones it at load)"
            )),
            Ok(None) => log.push(format!(
                "no {} store on this machine — enroll it once the engine tree is fetched",
                layout.engine
            )),
            Err(e) => log.push(format!(
                "local enroll failed (provision still covers it): {e:#}"
            )),
        },
        crate::voices::VoiceStore::None => log.push(format!(
            "engine {} takes no local clone — {name} is registered, but no render can speak it here",
            layout.engine
        )),
    }
    Ok(log)
}

/// The voices this engine's **own store** holds, from
pub fn installed_voices(layout: &crate::Layout) -> Option<std::collections::BTreeSet<String>> {
    let text = std::fs::read_to_string(layout.tts_voices()).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let presets = value.get("presets")?.as_object()?;
    Some(
        presets
            .keys()
            .filter(|k| !k.starts_with('_'))
            .cloned()
            .collect(),
    )
}

/// Bake clone voices the manifest declares but the pushed store lacks.
/// voice warned forever ("declared in voices.json but missing from
/// models/voices.json — fix it and run :prov again") and every render naming
pub fn bake_missing_voices(layout: &crate::Layout) -> Vec<String> {
    match crate::voices::store_kind(&layout.engine) {
        crate::voices::VoiceStore::Presets => bake_preset_voices(layout),
        crate::voices::VoiceStore::Clips => bake_clip_voices(layout),
        crate::voices::VoiceStore::None => Vec::new(),
    }
}

/// The preset-store half of [`bake_missing_voices`]: copy each manifest voice
fn bake_preset_voices(layout: &crate::Layout) -> Vec<String> {
    let root = layout.root.as_path();
    let manifest: BTreeMap<String, String> = std::fs::read_to_string(layout.voices_manifest())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let bake_path = layout.tts_voices();
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
    let text = serde_json::to_string(&bake).unwrap_or_default();
    if crate::util::atomic_write(&bake_path, &text).is_err() {
        return Vec::new();
    }
    baked.sort();
    baked
}

/// The clip-store half of [`bake_missing_voices`]: copy each voice the store
fn bake_clip_voices(layout: &crate::Layout) -> Vec<String> {
    let manifest = load_manifest(&layout.voices_manifest());
    if manifest.is_empty() {
        return Vec::new();
    }
    // What the store already holds, read once: `enroll_clip_voice` is
    let before = load_store(&layout.tts_voices()).unwrap_or(serde_json::Value::Null);
    let mut baked = Vec::new();
    for (name, clip) in &manifest {
        if name.starts_with('_') || before["presets"].get(name).is_some() {
            continue;
        }
        let clip = if Path::new(clip).is_absolute() {
            PathBuf::from(clip)
        } else {
            layout.work.join(clip)
        };
        if enroll_clip_voice(layout, name, &clip).is_ok_and(|file| file.is_some()) {
            baked.push(name.clone());
        }
    }
    baked.sort();
    baked
}

/// Enroll `name` from `clip` into a **clip store** — see
pub fn enroll_clip_voice(
    layout: &crate::Layout,
    name: &str,
    clip: &Path,
) -> anyhow::Result<Option<String>> {
    use anyhow::Context;
    // The declaration is the guard as well as the dispatch: a clip entry
    if crate::voices::store_kind(&layout.engine) != crate::voices::VoiceStore::Clips {
        return Ok(None);
    }
    let store_path = layout.tts_voices();
    let Some(mut doc) = load_store(&store_path) else {
        return Ok(None);
    };
    if let Some(file) = doc["presets"]
        .get(name)
        .and_then(|entry| entry.get("file"))
        .and_then(|f| f.as_str())
    {
        return Ok(Some(file.to_string()));
    }
    if !clip.is_file() {
        anyhow::bail!("no such reference clip: {}", clip.display());
    }
    let rel = store_file_for(&doc, name);
    let dest = layout.models_dir().join(&rel);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    write_clone_source(clip, &dest)?;
    let description = match crate::assemble::wav_info(&dest) {
        Ok(info) if info.seconds() > 0.0 => {
            format!("Clone of {name} (24 kHz mono, {:.1} s)", info.seconds())
        }
        _ => format!("Clone of {name} (24 kHz mono)"),
    };
    doc["presets"][name] = serde_json::json!({ "description": description, "file": rel });
    crate::util::atomic_write(&store_path, &serde_json::to_string(&doc)?)?;
    Ok(Some(rel))
}

/// Read an engine store in place — its `presets` object is the only shape this
/// missing, unreadable or not a store, which every caller treats as "nothing
/// to enroll into" rather than as an error: an engine tree that is not fetched
fn load_store(path: &Path) -> Option<serde_json::Value> {
    let doc: serde_json::Value = std::fs::read_to_string(path).ok()?.parse().ok()?;
    doc.get("presets")?.as_object()?;
    Some(doc)
}

/// The store-relative file a new clip entry should name: `refs/<slug>.wav`,
fn store_file_for(doc: &serde_json::Value, name: &str) -> String {
    let stem = slug_for_file(name);
    let rel = format!("refs/{stem}.wav");
    let claimed = doc["presets"].as_object().is_some_and(|presets| {
        presets.iter().any(|(other, entry)| {
            other != name && entry.get("file").and_then(|f| f.as_str()) == Some(rel.as_str())
        })
    });
    if claimed {
        format!("refs/{}-{}.wav", stem, short_hash(name))
    } else {
        rel
    }
}

/// The stem a voice name contributes to a store file: lowercase ASCII
fn slug_for_file(name: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in crate::util::fold(name).chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            dash = false;
        } else if !out.is_empty() && !dash {
            out.push('-');
            dash = true;
        }
    }
    let out = out.trim_end_matches('-');
    if out.is_empty() {
        format!("voice-{}", short_hash(name))
    } else {
        out.to_string()
    }
}

/// The first four bytes of a name's sha256, hex — enough to separate two names
fn short_hash(name: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(name.as_bytes())
        .iter()
        .take(4)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Write the sidecar's clone source for `clip` at `dest`.
fn write_clone_source(clip: &Path, dest: &Path) -> anyhow::Result<()> {
    use anyhow::Context;
    if house_wav(clip) {
        std::fs::copy(clip, dest)
            .with_context(|| format!("copying {} -> {}", clip.display(), dest.display()))?;
        return Ok(());
    }
    let src = clip.to_string_lossy().into_owned();
    let dst = dest.to_string_lossy().into_owned();
    let out = std::process::Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-i",
            &src,
            "-ac",
            "1",
            "-ar",
            "24000",
            "-c:a",
            "pcm_s16le",
            &dst,
        ])
        .output()
        .context("spawning ffmpeg to decode the clone source")?;
    if !out.status.success() {
        anyhow::bail!(
            "ffmpeg could not decode {}: {}",
            clip.display(),
            crate::util::head_chars(String::from_utf8_lossy(&out.stderr).trim(), 200)
        );
    }
    Ok(())
}

/// Whether `path` is already the store's house shape — read from the header
fn house_wav(path: &Path) -> bool {
    matches!(
        crate::assemble::wav_info(path),
        Ok(info) if info.channels == 1 && info.sample_rate == 24_000 && info.bits == 16
    )
}

/// Preset files the local enrollment writes to, turbo first: the venv
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

/// Enroll one sample into THIS machine's **preset store**, so renders use it
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
mod tests;
