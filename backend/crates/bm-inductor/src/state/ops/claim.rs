//! One voice, one speaker: the other users of it are moved off.

use super::*;
use std::collections::BTreeMap;

impl Inner {
    /// Give `character` `voice` to itself: every other speaker on it is moved
    /// to a free voice of its own, each through the same surgery a swap is.
    pub(crate) fn claim_voice_apply(
        &mut self,
        character: &str,
        voice: &str,
    ) -> anyhow::Result<String> {
        let engine = self.settings.engine.clone();
        let voice = bm_core::voices::resolve_voice_name(&engine, voice);
        self.validate_swap(character, &voice)?;
        let policy = bm_core::voices::effective_policy(&engine);
        let pool = bm_core::pool::load_pool(&self.layout.voice_pool());
        let bible = bm_core::digest::load_bible(&self.layout.bible());
        let cast = bm_core::cast::read_cast(&engine, &self.layout.cast(&engine));
        let mut others: Vec<String> = cast
            .iter()
            .filter(|(who, speaks)| speaks.as_str() == voice.as_str() && who.as_str() != character)
            .map(|(who, _)| who.clone())
            .collect();
        others.sort();
        // The owner first: the claim is a swap with an aftermath, and it also
        // assigns the voice to a speaker who never had it.
        let mut line = self.swap_apply(character, &voice)?;
        // Persist the claim: a new speaker must not roll onto this voice later.
        bm_core::pool::set_exclusive(&self.layout.voice_pool(), &voice, true)?;
        if others.is_empty() {
            line.push_str(&format!("; {character} is the only speaker on {voice}"));
            return Ok(line);
        }

        let mut used: BTreeMap<String, usize> = BTreeMap::new();
        for speaks in bm_core::cast::read_cast(&engine, &self.layout.cast(&engine)).values() {
            *used.entry(speaks.clone()).or_default() += 1;
        }
        let mut moved: Vec<String> = Vec::new();
        let mut left: Vec<String> = Vec::new();
        for who in others {
            // The voice a speaker is losing is the best statement of the gender
            // they should keep; a stated hint outranks it.
            let gender = hint_gender(&bible, &who)
                .or_else(|| bm_core::gender::voice_gender(&voice, &policy, &pool));
            let Some(pick) = free_voice(gender, &policy, &pool, &used, &voice) else {
                left.push(who);
                continue;
            };
            match self.swap_apply(&who, &pick) {
                Ok(_) => {
                    *used.entry(pick.clone()).or_default() += 1;
                    moved.push(format!("{who} → {pick}"));
                }
                // One refusal does not undo the moves already made: name it and
                // carry on with the rest.
                Err(e) => left.push(format!("{who} ({e:#})")),
            }
        }
        if !moved.is_empty() {
            line.push_str(&format!("; moved {}: {}", moved.len(), moved.join(", ")));
        }
        if !left.is_empty() {
            line.push_str(&format!(
                "; no free voice left for {} — swap by hand (`:swap`)",
                left.join(", ")
            ));
        }
        Ok(line)
    }
}

/// The gender the bible states for a name, if it states one.
fn hint_gender(bible: &serde_json::Value, character: &str) -> Option<&'static str> {
    let name = bm_core::digest::resolve_speaker(bible, character);
    let hint = bible
        .get("characters")
        .and_then(|c| c.as_array())
        .into_iter()
        .flatten()
        .find(|c| c.get("name").and_then(|n| n.as_str()) == Some(name.as_str()))
        .and_then(|c| c.get("voice_hint"))
        .and_then(|h| h.as_str())?;
    bm_core::gender::hint_gender(hint)
}

/// The least-used voice that no one speaks, preferring the given gender's
/// samples and presets. `keep` is the voice being claimed, never a candidate.
fn free_voice(
    gender: Option<&str>,
    policy: &bm_core::voices::VoicePolicy,
    pool: &bm_core::pool::Pool,
    used: &BTreeMap<String, usize>,
    keep: &str,
) -> Option<String> {
    let mut options: Vec<String> = Vec::new();
    if let Some(g) = gender {
        options.extend(bm_core::pool::candidates(pool, &[g.to_string()]));
        options.extend(policy.pool_for_hint(g).iter().cloned());
    }
    // Genderless, or nothing left in that gender: the whole catalogue, so a
    // collision is still broken rather than left standing.
    options.extend(policy.male.iter().cloned());
    options.extend(policy.female.iter().cloned());
    options.extend(policy.neutral.iter().cloned());
    let mut seen: Vec<String> = Vec::new();
    options.retain(|v| {
        if v == keep || seen.contains(v) || pool.get(v).is_some_and(|e| e.exclusive) {
            return false;
        }
        seen.push(v.clone());
        true
    });
    // Least used, then the candidate order the list was built in: a pooled
    // sample of the right gender outranks the whole catalogue.
    options
        .into_iter()
        .enumerate()
        .min_by_key(|(i, v)| (used.get(v).copied().unwrap_or(0), *i))
        .map(|(_, v)| v)
}
