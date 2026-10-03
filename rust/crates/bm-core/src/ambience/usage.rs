use super::*;
/// One reason a pooled sound cannot be removed: a thing that names it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct UseOf {
    /// The scene map's rule or palette value, or `ch09` for a script.
    pub by: String,
    /// The tags that reach the sound. Empty for a script, which names the sound
    pub tags: Vec<String>,
}

impl UseOf {
    /// `mountain rule [mountain, wind]`, what a screen shows beside the entry.
    pub fn label(&self) -> String {
        if self.tags.is_empty() {
            self.by.clone()
        } else {
            format!("{} [{}]", self.by, self.tags.join(", "))
        }
    }
}

/// Sounds at least one tag set can reach, and the tag sets that reach them.
/// A scene names *tags*; the pool answers with a *sound*. So "is this sound in
/// use" is a question about the tag sets the map names, not about the sound's
fn reachable(pool: &ClipPool, sets: impl Iterator<Item = (String, Vec<String>)>) -> Usage {
    let mut out: Usage = BTreeMap::new();
    for (by, tags) in sets {
        if tags.is_empty() {
            continue;
        }
        for (name, sound) in pool {
            if sound.tags.iter().any(|t| tags.contains(t)) {
                out.entry(name.clone()).or_default().push(UseOf {
                    by: by.clone(),
                    tags: tags.clone(),
                });
            }
        }
    }
    for uses in out.values_mut() {
        uses.sort();
        uses.dedup();
    }
    out
}

/// Sound -> what the scene map's rules still reach. See [`reachable`].
pub fn effect_usage(map: &SceneMap, pool: &ClipPool) -> Usage {
    let named = map.rules.iter().map(|r| {
        (
            format!("scene rule {:?}", r.matches.join(", ")),
            r.effect.clone(),
        )
    });
    let dflt = std::iter::once(("scene default".to_string(), map.default.effect.clone()));
    reachable(pool, named.chain(dflt))
}

/// Sound -> what the palette still reaches. See [`reachable`].
pub fn music_usage(map: &SceneMap, pool: &ClipPool) -> Usage {
    reachable(
        pool,
        map.music_palette
            .iter()
            .map(|(mood, e)| (format!("palette {mood:?}"), e.tags.clone())),
    )
}

/// Sound -> the chapters whose script places it.
pub fn inject_usage(scripts: &[(u32, Value)]) -> BTreeMap<String, Vec<u32>> {
    let mut out: BTreeMap<String, Vec<u32>> = BTreeMap::new();
    for (chapter, doc) in scripts {
        let Some(items) = doc.get("segments").and_then(|s| s.as_array()) else {
            continue;
        };
        for item in items {
            if !crate::util::is_sound_item(item) {
                continue;
            }
            for key in ["sound", "stop"] {
                let Some(name) = item.get(key).and_then(|v| v.as_str()).map(str::trim) else {
                    continue;
                };
                if name.is_empty() {
                    continue;
                }
                let chapters = out.entry(name.to_string()).or_default();
                if !chapters.contains(chapter) {
                    chapters.push(*chapter);
                }
            }
        }
    }
    for chapters in out.values_mut() {
        chapters.sort_unstable();
    }
    out
}

/// Sound -> every reason it is still in use, per layer.
pub type Usage = BTreeMap<String, Vec<UseOf>>;
