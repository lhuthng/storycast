use super::*;

impl Layout {
    /// Bring a pre-adapter-home checkout into the `adapters/<name>/` shape.
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
        let mut binding = crate::profile::read_binding(&self.root)?;
        binding.adapter.name = name.clone();
        crate::profile::write_binding(&self.root, &binding)?;
        let _ = crate::profile::verify_binding(&self.root, None);
        Ok(Some(name))
    }

    /// Bring a pre-split cache into the `(adapter, engine)` shape.
    pub fn migrate_cache_keys(&self, engine: &str) -> Result<Vec<PathBuf>> {
        let mut moved = Vec::new();
        let data = self.data();

        // The adapter half first, when this checkout was migrated *out of*
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
        let _ = std::fs::remove_dir(bm.join("voices"));
        Ok(moved)
    }
}
