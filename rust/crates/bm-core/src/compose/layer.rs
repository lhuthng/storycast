use super::patch::append_inside;
use super::patch::concat_key;
use super::patch::entries_text;
use super::patch::keyed_key;
use super::patch::layered;
use super::patch::list_record;
use super::patch::member_text;
use super::patch::parse_list_record;
use super::patch::piece;
use super::patch::remove_member;
use super::patch::replace_member;
use super::patch::text_hash;
use super::patch::unescape;
use super::patch::whole_key;
use super::patch::Layered;
use super::patch::Mode;
use super::*;
use anyhow::{bail, Result};

/// One layered file's live text, and what a resolve has put into it.
pub(crate) struct Layer {
    pub(crate) file: &'static str,
    pub(crate) text: String,
    /// Members whose value came from a dependency, so a later one may override
    pub(crate) filled: BTreeSet<String>,
    pub(crate) filled_keys: BTreeSet<(String, String)>,
    pub(crate) records: BTreeMap<String, String>,
    pub(crate) original: String,
}

impl Layer {
    /// Open `assets/<file>`, or start one empty.
    pub(crate) fn open(assets: &Path, file: &'static str) -> Result<Self> {
        let path = assets.join(file);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) if !t.trim().is_empty() => t,
            Ok(_) => bail!("{} is empty — refusing to merge into it", path.display()),
            Err(_) => "{}".to_string(),
        };
        if crate::audio_pool::scan_entries(&text).is_none() {
            bail!(
                "{} is not a JSON object this merge can read — refusing to rewrite it",
                path.display()
            );
        }
        Ok(Layer {
            file,
            original: text.clone(),
            text,
            filled: BTreeSet::new(),
            filled_keys: BTreeSet::new(),
            records: BTreeMap::new(),
        })
    }

    fn policy(&self) -> &'static Layered {
        layered(self.file).expect("only a layered file is ever opened as one")
    }

    fn member_value(&self, member: &str) -> Option<String> {
        let entries = crate::audio_pool::scan_entries(&self.text)?;
        let e = entries.iter().find(|e| e.key == member)?;
        Some(self.text[e.value_start..e.value_end].to_string())
    }

    fn has_member(&self, member: &str) -> bool {
        self.member_value(member).is_some()
    }

    /// Forget a member an earlier dependency contributed, so a later one wins
    fn override_member(&mut self, member: &str) {
        if !self.filled.remove(member) {
            return;
        }
        if let Some(next) = remove_member(&self.text, member) {
            self.text = next;
        }
        self.records
            .retain(|k, _| k != member && !k.starts_with(&format!("{member}/")));
    }

    fn fill_whole(&mut self, member: &str, value: &str) {
        if self.has_member(member) {
            // The file's own member wins. A member a *dependency* put there does
            if !self.filled.contains(member) {
                return;
            }
            self.override_member(member);
        }
        let run = piece("  ", "", &member_text(member, value));
        let Some(next) = append_inside(&self.text, &run) else {
            return;
        };
        self.text = next;
        self.filled.insert(member.to_string());
        self.records.insert(whole_key(member), text_hash(value));
    }

    /// A member whose value is an object merges key by key; anything else (a
    fn fill_keyed(&mut self, member: &str, value: &str) {
        let Some(parent_keys) = crate::audio_pool::scan_entries(value) else {
            self.override_member(member);
            self.fill_whole(member, value);
            return;
        };
        // No `override_member` here, deliberately. A keyed member *accumulates*:
        // the file's names win, every dependency's are added, and "a later
        // dependency overrides an earlier one" is per key — see `filled_keys`
        let Some(current) = self.member_value(member) else {
            let run = piece("  ", "", &member_text(member, value));
            let Some(next) = append_inside(&self.text, &run) else {
                return;
            };
            self.text = next;
            self.filled.insert(member.to_string());
            for pk in &parent_keys {
                // Marked as well as recorded: the member arriving created these
                self.filled_keys
                    .insert((member.to_string(), pk.key.clone()));
                self.records.insert(
                    keyed_key(member, &pk.key),
                    text_hash(&value[pk.value_start..pk.value_end]),
                );
            }
            return;
        };
        let Some(mine) = crate::audio_pool::scan_entries(&current) else {
            return; // the file's member is not an object; it wins as it stands
        };
        let mut merged = current.clone();
        let mut touched = false;
        for pk in &parent_keys {
            let own = mine.iter().any(|m| m.key == pk.key);
            let filled = self
                .filled_keys
                .contains(&(member.to_string(), pk.key.clone()));
            if own && !filled {
                continue; // the file's own name wins
            }
            let one = &value[pk.value_start..pk.value_end];
            // The names the file lacks are *added* to what is already here, never
            if filled {
                merged = remove_member(&merged, &pk.key).unwrap_or(merged);
            }
            let run = piece("    ", "  ", &member_text(&pk.key, one));
            match append_inside(&merged, &run) {
                Some(next) => merged = next,
                None => continue,
            }
            touched = true;
            self.filled_keys
                .insert((member.to_string(), pk.key.clone()));
            self.records
                .insert(keyed_key(member, &pk.key), text_hash(one));
        }
        if touched {
            if let Some(next) = replace_member(&self.text, member, &merged) {
                self.text = next;
            }
        }
    }

    /// An ordered list: the file's entries first, then this dependency's.
    fn fill_list(&mut self, member: &str, value: &str, dep: &str) {
        let Some(entries) = entries_text(value) else {
            return;
        };
        let count = crate::audio_pool::scan_array(value)
            .map(|v| v.len())
            .unwrap_or(0);
        if count == 0 {
            return;
        }
        // A list never overrides — it accumulates, and each dependency's
        match self.member_value(member) {
            None => {
                let run = piece("  ", "", &member_text(member, value));
                let Some(next) = append_inside(&self.text, &run) else {
                    return;
                };
                self.text = next;
                self.filled.insert(member.to_string());
            }
            Some(current) => {
                // The dependency's own inner text, verbatim: its entries, its
                let trimmed = value.trim_end();
                let inner = match trimmed.get(1..trimmed.len().saturating_sub(1)) {
                    Some(i) => i,
                    None => return,
                };
                let Some(merged) = append_inside(&current, inner) else {
                    return;
                };
                if let Some(next) = replace_member(&self.text, member, &merged) {
                    self.text = next;
                }
            }
        }
        self.records.insert(
            concat_key(member, dep),
            list_record(count, &text_hash(&entries)),
        );
    }

    /// Fold one dependency into this file.
    pub(crate) fn fill(&mut self, dir: &Path) {
        let path = dir.join(self.file);
        let Ok(parent) = std::fs::read_to_string(&path) else {
            return;
        };
        let Some(entries) = crate::audio_pool::scan_entries(&parent) else {
            return; // a dependency's file this cannot read contributes nothing
        };
        let policy = self.policy();
        for e in &entries {
            let value = &parent[e.value_start..e.value_end];
            match policy.mode(&e.key) {
                Mode::Whole => {
                    self.override_member(&e.key);
                    self.fill_whole(&e.key, value);
                }
                Mode::ByKey => self.fill_keyed(&e.key, value),
                // Folded in by `fill_lists`, after the scalars and strongest
                Mode::Concat => {}
            }
        }
    }

    /// Fold in the members that are *lists*, after the scalars and **strongest
    pub(crate) fn fill_lists(&mut self, dir: &Path, dep: &str) {
        let path = dir.join(self.file);
        let Ok(parent) = std::fs::read_to_string(&path) else {
            return;
        };
        let Some(entries) = crate::audio_pool::scan_entries(&parent) else {
            return;
        };
        let policy = self.policy();
        for e in &entries {
            if policy.mode(&e.key) == Mode::Concat {
                let value = &parent[e.value_start..e.value_end];
                self.fill_list(&e.key, value, dep);
            }
        }
    }

    /// Take back what a previous resolve put here, and only where nobody has
    pub(crate) fn withdraw(
        &mut self,
        records: &BTreeMap<String, String>,
        deps: &[DepRecord],
        adopted: &mut BTreeSet<(String, String)>,
    ) {
        // Lists first, in `deps` order — which is the reverse of the order they
        for dep in deps {
            for (member, _) in self
                .policy()
                .members
                .iter()
                .copied()
                .filter(|(_, m)| *m == Mode::Concat)
            {
                let key = concat_key(member, &dep.name);
                let Some(record) = records.get(&key) else {
                    continue;
                };
                let Some((count, hash)) = parse_list_record(record) else {
                    continue;
                };
                let Some(current) = self.member_value(member) else {
                    continue;
                };
                let Some(spans) = crate::audio_pool::scan_array(&current) else {
                    continue;
                };
                if count == 0 || spans.len() < count {
                    continue;
                }
                let at = spans.len() - count;
                let tail: String = spans[at..]
                    .iter()
                    .map(|(a, b)| &current[*a..*b])
                    .collect::<Vec<_>>()
                    .join(",");
                if text_hash(&tail) != hash {
                    // Edited here: the operator has adopted it, so it stays and
                    adopted.insert((self.file.to_string(), key));
                    continue;
                }
                // Everything the dependency appended was the tail, so what is
                let head = current[..spans[at].0]
                    .trim_end_matches([',', ' ', '\n'])
                    .to_string();
                let next = if at == 0 {
                    "[]".to_string()
                } else if head.contains('\n') {
                    format!("{head}\n  ]")
                } else {
                    format!("{head}]")
                };
                if let Some(text) = replace_member(&self.text, member, &next) {
                    self.text = text;
                }
                self.filled.remove(member);
            }
        }
        // Then whole members and keyed names, whose hash is of the value.
        for (key, hash) in records {
            if key.contains('+') {
                continue; // a list, above
            }
            match key.split_once('/') {
                Some((member, name)) => {
                    let (member, name) = (&unescape(member), &unescape(name));
                    let Some(current) = self.member_value(member) else {
                        continue;
                    };
                    let Some(mine) = crate::audio_pool::scan_entries(&current) else {
                        continue;
                    };
                    let Some(m) = mine.iter().find(|m| m.key == *name) else {
                        continue;
                    };
                    if text_hash(&current[m.value_start..m.value_end]) != *hash {
                        adopted.insert((self.file.to_string(), key.clone()));
                        continue;
                    }
                    let Some(merged) = remove_member(&current, name) else {
                        continue;
                    };
                    let next = if crate::audio_pool::scan_entries(&merged)
                        .map(|e| e.is_empty())
                        .unwrap_or(false)
                    {
                        // Nothing left of it: the member itself came from the
                        remove_member(&self.text, member).unwrap_or(merged)
                    } else {
                        replace_member(&self.text, member, &merged).unwrap_or(merged)
                    };
                    self.text = next;
                    self.filled_keys
                        .remove(&(member.to_string(), name.to_string()));
                }
                None => {
                    let member = unescape(key);
                    let Some(current) = self.member_value(&member) else {
                        continue;
                    };
                    if text_hash(&current) != *hash {
                        adopted.insert((self.file.to_string(), key.clone()));
                        continue;
                    }
                    if let Some(text) = remove_member(&self.text, &member) {
                        self.text = text;
                    }
                    self.filled.remove(&member);
                }
            }
        }
    }

    /// Write the merged file, and only when it differs.
    pub(crate) fn finish(&self, assets: &Path) -> Result<()> {
        if self.text == self.original {
            return Ok(());
        }
        crate::atomic_write(&assets.join(self.file), &self.text)?;
        Ok(())
    }
}
