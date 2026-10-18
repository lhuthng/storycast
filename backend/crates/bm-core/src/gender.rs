//! Gender repair: the cast must not contradict the gender a source states.
//!
//! Only a *stated* gender is a source — a real bible hint, or the adapter's own
//! words. The old `adult male` stamp on an undescribed speaker states nothing,
//! so it is repaired, not trusted.

use crate::pool::{self, Pool};
use crate::voices::VoicePolicy;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

pub const MALE: &str = "male";
pub const FEMALE: &str = "female";

/// The gender a `voice_hint` head announces: "adult female, cold" -> female.
/// `unknown` and an absent head say nothing.
pub fn hint_gender(hint: &str) -> Option<&'static str> {
    let head = hint
        .split([',', ':', ';', '-', '–'])
        .next()
        .unwrap_or("")
        .to_lowercase();
    // Female first: "female" contains "male".
    if head.contains("female") || head.contains("girl") {
        Some(FEMALE)
    } else if head.contains("male") || head.contains("boy") {
        Some(MALE)
    } else {
        None
    }
}

/// The gender a voice announces: the policy's preset pools first, then the
/// sample pool's tags. A voice that declares neither is not a witness.
pub fn voice_gender(voice: &str, policy: &VoicePolicy, pool: &Pool) -> Option<&'static str> {
    if policy.female.iter().any(|v| v == voice) {
        return Some(FEMALE);
    }
    if policy.male.iter().any(|v| v == voice) {
        return Some(MALE);
    }
    let tags = pool
        .get(voice)
        .map(|e| e.tags.as_slice())
        .unwrap_or_default();
    if tags.iter().any(|t| t == FEMALE) {
        Some(FEMALE)
    } else if tags.iter().any(|t| t == MALE) {
        Some(MALE)
    } else {
        None
    }
}

/// Lowercased whole words. Diacritics are kept: folding would collapse `má`
/// into `ma` and `bà` into `ba`, matching a demon's `Ma` or a `Bá` uncle.
fn words(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// A language's gender words, from its adapter's optional `gender.json`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct GenderLexicon {
    pub female: Vec<String>,
    pub male: Vec<String>,
}

impl GenderLexicon {
    pub fn is_empty(&self) -> bool {
        self.female.is_empty() && self.male.is_empty()
    }

    /// The gender a name or alias announces. Every word of a term must appear,
    /// so `"cô nương"` matches `"Cô Nương X"` and `"mẫu"` matches `"Vân mẫu"`.
    pub fn gender_of(&self, name: &str, aliases: &[String]) -> Option<&'static str> {
        let hay: Vec<String> = std::iter::once(name)
            .chain(aliases.iter().map(String::as_str))
            .flat_map(words)
            .collect();
        let finds = |terms: &[String]| {
            terms
                .iter()
                .any(|t| words(t).iter().all(|w| hay.iter().any(|h| h == w)))
        };
        if finds(&self.female) {
            Some(FEMALE)
        } else if finds(&self.male) {
            Some(MALE)
        } else {
            None
        }
    }
}

/// The lexicon an adapter ships, if it ships one.
pub fn load_lexicon(home: Option<&Path>) -> GenderLexicon {
    home.map(|h| h.join("gender.json"))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// The old synthesis stamped an undescribed speaker `adult male`/`["male"]`
/// with no personality: that is silence about gender, not a male gender.
fn is_placeholder(entry: &Value) -> bool {
    entry.get("voice_hint").and_then(Value::as_str) == Some("adult male")
        && entry
            .get("personality")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .is_empty()
        && entry
            .get("tags")
            .and_then(Value::as_array)
            .is_some_and(|t| t.iter().filter_map(Value::as_str).eq(["male"]))
}

fn aliases_of(entry: &Value) -> Vec<String> {
    entry
        .get("proper_aliases")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// One character whose voice contradicts a stated gender.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenderFix {
    pub character: String,
    pub old_voice: String,
    /// The gender the sources agree on.
    pub gender: &'static str,
    /// The least-used free voice of that gender; `None` when none is offered.
    pub new_voice: Option<String>,
    /// A corrected `voice_hint` for a bible entry that stated the wrong gender.
    pub new_hint: Option<String>,
}

fn pick(
    gender: &str,
    policy: &VoicePolicy,
    pool: &Pool,
    used: &BTreeMap<String, usize>,
) -> Option<String> {
    let mut options = pool::candidates(pool, &[gender.to_string()]);
    if options.is_empty() {
        options = policy.pool_for_hint(gender).to_vec();
    }
    options
        .into_iter()
        .min_by_key(|v| (used.get(v).copied().unwrap_or(0), v.clone()))
}

/// Every cast entry that contradicts a stated gender, least-used replacement
/// included. Empty when the cast and the sources already agree.
pub fn plan(
    bible: &Value,
    cast: &BTreeMap<String, String>,
    policy: &VoicePolicy,
    pool: &Pool,
    lexicon: &GenderLexicon,
) -> Vec<GenderFix> {
    let characters: Vec<&Value> = bible
        .get("characters")
        .and_then(Value::as_array)
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    let entry_of = |name: &str| {
        characters
            .iter()
            .copied()
            .find(|c| c.get("name").and_then(Value::as_str) == Some(name))
    };
    // A real bible hint, or the adapter's own words. A placeholder states
    // nothing, so the lexicon gets to speak — but never over a real hint.
    let stated = |name: &str| -> Option<&'static str> {
        let entry = entry_of(name);
        entry
            .filter(|e| !is_placeholder(e))
            .and_then(|e| e.get("voice_hint").and_then(Value::as_str))
            .and_then(hint_gender)
            .or_else(|| {
                let aliases = entry.map(aliases_of).unwrap_or_default();
                lexicon.gender_of(name, &aliases)
            })
    };

    let mut used: BTreeMap<String, usize> = BTreeMap::new();
    for voice in cast.values() {
        *used.entry(voice.clone()).or_default() += 1;
    }

    let mut fixes = Vec::new();
    for (character, old_voice) in cast {
        let Some(gender) = stated(character) else {
            continue;
        };
        let Some(have) = voice_gender(old_voice, policy, pool) else {
            continue;
        };
        if have == gender {
            continue;
        }
        let new_voice = pick(gender, policy, pool, &used);
        if let Some(v) = new_voice.as_deref() {
            *used.entry(v.to_string()).or_default() += 1;
        }
        let new_hint = entry_of(character)
            .filter(|e| is_placeholder(e))
            .map(|_| format!("adult {gender}"));
        fixes.push(GenderFix {
            character: character.clone(),
            old_voice: old_voice.clone(),
            gender,
            new_voice,
            new_hint,
        });
    }
    fixes
}

#[cfg(test)]
mod tests;
