use super::*;

impl Layout {
    /// Bring a pre-adapter-home checkout into the `adapters/<name>/` shape.
    ///
    /// Before the adapter was a home, a language was two directory names the
    /// checkout happened to have: `prompts/` at the root and, for its crawlers,
    /// the pack's `assets/crawl/`. Neither said which language it was — which is
    /// why a second one could not exist, and why the crawlers ended up in the
    /// pack, where nothing about them is a genre fact.
    ///
    /// So the prompts are **moved**, never rebuilt, into one directory that
    /// names the language: `adapters/<name>/prompts/`. (The crawlers are the
    /// global `crawlers/` tree now, tracked in the repo, so a checkout's old
    /// `assets/crawl/` is left where it is rather than moved into a home the
    /// resolver no longer reads crawlers from.) The name is the checkout's own
    /// when it already names one, and [`LEGACY_ADAPTER`] when it does not.
    /// Rename-only, never overwriting, idempotent — and it does nothing at all
    /// until a pointer exists, because stamping a name is a claim about a
    /// checkout that has loaded something.
    pub fn migrate_adapter_tree(&self) -> Result<Option<String>> {
        if self.adapter_home().is_some() {
            return Ok(None); // already has one; nothing to move again
        }
        if crate::profile::read_binding(&self.root).is_err() {
            return Ok(None); // a fresh clone: no pointer, so nothing to name
        }
        let name = if self.adapter == DEFAULT_ADAPTER {
            LEGACY_ADAPTER.to_string()
        } else {
            self.adapter.clone()
        };
        let home = self.root.join(ADAPTERS_DIR).join(&name);
        let mut moved = false;
        // Only the prompts. The crawlers that a pre-split checkout kept in
        // `assets/crawl/` are global now (the tracked `crawlers/` tree), so
        // moving one checkout's old copy into a per-language home would put a
        // second, stale tree where nothing resolves it.
        for (from, to) in [(self.root.join("prompts"), home.join("prompts"))] {
            if !from.exists() || to.exists() {
                continue;
            }
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            std::fs::rename(&from, &to)
                .with_context(|| format!("renaming {} to {}", from.display(), to.display()))?;
            moved = true;
        }
        if !moved {
            return Ok(None);
        }
        // The name is what every path below resolves through, so the pointer
        // carries it. `assets/` lost its crawlers in the same breath, so the
        // pack's hash has moved — re-stamping here keeps the next start from
        // warning about a drift this migration caused.
        let mut binding = crate::profile::read_binding(&self.root)?;
        binding.adapter.name = name.clone();
        crate::profile::write_binding(&self.root, &binding)?;
        let _ = crate::profile::verify_binding(&self.root, None);
        Ok(Some(name))
    }

    /// Bring a pre-split cache into the `(adapter, engine)` shape.
    ///
    /// Before the adapter reached the path, the cast was `cast-<engine>.json`
    /// (and plain `cast.json` for anything but VieNeu) and the segment
    /// directories were `segments-<engine>-NN`. Those bytes are already
    /// correct — they were produced by this adapter and this engine; only the
    /// *name* was missing a component — so this renames rather than rebuilds.
    /// Leaving the old names behind would re-render every chapter already
    /// spoken, which is hours of synthesis for a path string.
    ///
    /// Idempotent, and it never overwrites: a target that already exists wins.
    /// Returns what it moved, so a caller can say so once instead of silently
    /// rewriting the operator's data directory.
    pub fn migrate_cache_keys(&self, engine: &str) -> Result<Vec<PathBuf>> {
        let mut moved = Vec::new();
        let data = self.data();

        // The adapter half first, when this checkout was migrated *out of*
        // `default`: those bytes are in the language that now has a name, and
        // leaving them keyed by the name-less default would re-render every
        // chapter already spoken — the same waste, and the same fix, as the
        // engine half below.
        if self.adapter != DEFAULT_ADAPTER {
            moved.extend(self.rename_default_caches(&data)?);
        }

        let legacy_cast = match engine {
            "vieneu" => Some(data.join("cast-vieneu.json")),
            _ => Some(data.join("cast.json")),
        };
        if let Some(legacy) = legacy_cast {
            let target = self.cast(engine);
            if legacy.is_file() && !target.exists() {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::rename(&legacy, &target).with_context(|| {
                    format!("renaming {} to {}", legacy.display(), target.display())
                })?;
                moved.push(target);
            }
        }

        let Some(legacy_engine) = legacy_engine_key(engine) else {
            return Ok(moved);
        };
        let prefix = format!("segments-{legacy_engine}-");
        let audio = data.join("audio");
        let Ok(entries) = std::fs::read_dir(&audio) else {
            return Ok(moved);
        };
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|name| name.starts_with(&prefix))
            .collect();
        names.sort();
        for name in names {
            let Ok(n) = name[prefix.len()..].parse::<u32>() else {
                continue;
            };
            let (from, to) = (audio.join(&name), self.seg_dir(engine, n));
            if to.exists() {
                continue;
            }
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(&from, &to)
                .with_context(|| format!("renaming {} to {}", from.display(), to.display()))?;
            moved.push(to);
        }
        Ok(moved)
    }

    /// Rename `default`-keyed caches to this adapter's name.
    ///
    /// `cast-default-<engine>.json` and `segments-default-<engine>-NN` were
    /// correct content under a name that said nothing, written before the
    /// language had one. The engine half is taken from each filename rather
    /// than assumed, so `gemini-v2` (which carries its own `-`) renames as
    /// faithfully as `vieneu` does.
    fn rename_default_caches(&self, data: &Path) -> Result<Vec<PathBuf>> {
        let mut moved = Vec::new();
        let cast_prefix = format!("cast-{DEFAULT_ADAPTER}-");
        if let Ok(entries) = std::fs::read_dir(data) {
            let mut names: Vec<String> = entries
                .filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.starts_with(&cast_prefix) && n.ends_with(".json"))
                .collect();
            names.sort();
            for name in names {
                let engine = &name[cast_prefix.len()..name.len() - ".json".len()];
                if engine.is_empty() {
                    continue;
                }
                let (from, to) = (data.join(&name), self.cast(engine));
                if to.exists() {
                    continue;
                }
                if let Some(parent) = to.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::rename(&from, &to)
                    .with_context(|| format!("renaming {} to {}", from.display(), to.display()))?;
                moved.push(to);
            }
        }

        let seg_prefix = format!("segments-{DEFAULT_ADAPTER}-");
        let audio = data.join("audio");
        let Ok(entries) = std::fs::read_dir(&audio) else {
            return Ok(moved);
        };
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|name| name.starts_with(&seg_prefix))
            .collect();
        names.sort();
        for name in names {
            // `segments-default-<engine>-NN`: the chapter is the last field, so
            // an engine that carries a `-` of its own still splits right.
            let Some((rest, chapter)) = name[seg_prefix.len()..].rsplit_once('-') else {
                continue;
            };
            let (Ok(n), false) = (chapter.parse::<u32>(), rest.is_empty()) else {
                continue;
            };
            let (from, to) = (audio.join(&name), self.seg_dir(rest, n));
            if to.exists() {
                continue;
            }
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(&from, &to)
                .with_context(|| format!("renaming {} to {}", from.display(), to.display()))?;
            moved.push(to);
        }
        Ok(moved)
    }

    /// Bring a pre-engine-tree checkout into the `engines/<name>/` shape.
    ///
    /// Before engines had trees, `models/`, `bm-tts`, `libonnxruntime.so.1`
    /// and the `.bm/voices/` pair sat at the root — and they were **VieNeu's**,
    /// because it was the only local engine. So the files are *moved*, never
    /// rebuilt, into [`LEGACY_ENGINE`]'s tree whatever this checkout now runs:
    /// the bytes have a fixed owner even though `settings.engine` is a setting
    /// somebody can switch.
    ///
    /// Rename-only, never overwriting, and idempotent. Everything it moves sits
    /// on one filesystem, so this is a handful of renames rather than a
    /// gigabyte of copying — and the alternative, leaving the old tree where no
    /// new path points, would make the sidecar unspawnable and the weights
    /// unreachable rather than merely misnamed.
    pub fn migrate_engine_tree(&self) -> Result<Vec<PathBuf>> {
        let target = self.root.join(ENGINES_DIR).join(LEGACY_ENGINE);
        let bm = self.bm_state();
        let mut moved = Vec::new();
        for (from, to) in [
            (self.root.join("models"), target.join("models")),
            (self.root.join("bm-tts"), target.join("bm-tts")),
            (
                self.root.join("libonnxruntime.so"),
                target.join("libonnxruntime.so"),
            ),
            (
                self.root.join("libonnxruntime.so.1"),
                target.join("libonnxruntime.so.1"),
            ),
            (bm.join("voices/refs"), target.join("refs")),
            (bm.join("voices/samples"), target.join("samples")),
        ] {
            if !from.exists() || to.exists() {
                continue;
            }
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            std::fs::rename(&from, &to)
                .with_context(|| format!("renaming {} to {}", from.display(), to.display()))?;
            moved.push(to);
        }
        // `.bm/voices/` only ever held the pair above, so an empty one is
        // leftover scaffolding rather than state.
        let _ = std::fs::remove_dir(bm.join("voices"));
        Ok(moved)
    }
}
