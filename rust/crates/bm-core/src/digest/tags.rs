use super::canon::{resolve_speaker, VI_DIACRITICS};
use anyhow::{anyhow, Result};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Prompt-side synonyms for the closed sound vocabularies.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
pub struct TagAliases {
    pub music: BTreeMap<String, String>,
    pub effect: BTreeMap<String, String>,
    pub sound: BTreeMap<String, String>,
}

impl TagAliases {
    pub fn load(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default())
            }
            Err(error) => {
                return Err(anyhow!(
                    "reading tag aliases from {}: {error}",
                    path.display()
                ));
            }
        };
        serde_json::from_str(&text)
            .map_err(|error| anyhow!("parsing tag aliases from {}: {error}", path.display()))
    }

    pub fn validate(
        &self,
        palette: &[String],
        effect_tags: &[String],
        sounds: impl IntoIterator<Item = String>,
    ) -> Result<()> {
        fn check(
            kind: &str,
            aliases: &BTreeMap<String, String>,
            canonical: &BTreeSet<String>,
        ) -> Result<()> {
            for (alias, target) in aliases {
                if alias.trim().is_empty() {
                    anyhow::bail!("{kind} alias has an empty name");
                }
                if target.trim().is_empty() {
                    anyhow::bail!("{kind} alias {alias:?} has an empty target");
                }
                if canonical.contains(alias) {
                    anyhow::bail!("{kind} alias {alias:?} shadows a canonical name");
                }
                if !canonical.contains(target) {
                    anyhow::bail!("{kind} alias {alias:?} points to unknown target {target:?}");
                }
            }
            Ok(())
        }

        let palette = palette.iter().cloned().collect::<BTreeSet<_>>();
        let effect_tags = effect_tags.iter().cloned().collect::<BTreeSet<_>>();
        let sounds = sounds.into_iter().collect::<BTreeSet<_>>();
        check("music", &self.music, &palette)?;
        check("effect", &self.effect, &effect_tags)?;
        check("sound", &self.sound, &sounds)?;
        Ok(())
    }
}

fn canonical_tag<'a>(aliases: &'a BTreeMap<String, String>, value: &str) -> Option<&'a str> {
    aliases.get(value.trim()).map(String::as_str)
}

/// Replace known prompt-side synonyms in-place, before any closed-vocabulary
pub fn apply_tag_aliases(data: &mut Value, aliases: &TagAliases) {
    let Some(segments) = data.get_mut("segments").and_then(Value::as_array_mut) else {
        return;
    };
    for segment in segments {
        let Some(segment) = segment.as_object_mut() else {
            continue;
        };
        if let Some(canonical) = segment
            .get("music")
            .and_then(Value::as_str)
            .and_then(|value| canonical_tag(&aliases.music, value))
        {
            segment.insert("music".into(), Value::String(canonical.into()));
        }
        if let Some(effects) = segment.get_mut("effect").and_then(Value::as_array_mut) {
            for effect in effects {
                if let Some(canonical) = effect
                    .as_str()
                    .and_then(|value| canonical_tag(&aliases.effect, value))
                {
                    *effect = Value::String(canonical.into());
                }
            }
        }
        // A stop names the same inject vocabulary as its start, so both follow
        for key in ["sound", "stop"] {
            if let Some(canonical) = segment
                .get(key)
                .and_then(Value::as_str)
                .and_then(|value| canonical_tag(&aliases.sound, value))
            {
                segment.insert(key.into(), Value::String(canonical.into()));
            }
        }
    }
}

/// Effect tags are optional scoring hints. Once aliases have run, a tag with
pub fn discard_unknown_effect_tags(data: &mut Value, valid_tags: &[String]) {
    let Some(segments) = data.get_mut("segments").and_then(Value::as_array_mut) else {
        return;
    };
    for segment in segments {
        let Some(effects) = segment
            .as_object_mut()
            .and_then(|segment| segment.get_mut("effect"))
            .and_then(Value::as_array_mut)
        else {
            continue;
        };
        effects.retain(|effect| {
            effect
                .as_str()
                .is_some_and(|tag| valid_tags.iter().any(|valid| valid == tag))
        });
    }
}

mod retag;
pub use retag::{retag_text, warn_vietnamese};
pub(crate) use validate::normalise_tags;
pub use validate::{
    tags_of, validate_context, validate_effect_tags, validate_injects, validate_script,
    validate_title,
};
mod validate;

#[cfg(test)]
mod tests;
