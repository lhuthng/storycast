use super::tags::normalise_tags;
use crate::util::squeeze_ws;
use serde_json::{json, Value};

/// Surface forms that may NEVER join the bible: pronouns, generic nouns, verb phrases.
const ALIAS_STOP: [&str; 18] = [
    "hắn", "nàng", "ta", "ngươi", "y", "huynh", "đệ", "tỷ", "muội", "phàm nhân", "con", "người",
    "tên", "tiểu", "lão", "tiểu tử", "narrator", "người dẫn chuyện",
];

/// Vietnamese letters carrying a diacritic — the tell-tale of un-translated text.
pub(crate) const VI_DIACRITICS: &str = "àáạảãâầấậẩẫăằắặẳẵèéẹẻẽêềếệểễìíịỉĩòóọỏõôồốộổỗơờớợởỡùúụủũưừứựửữỳýỵỷỹđ";

/// Honorific/title suffixes that never denote a different person: "Huyền Vũ
/// lão tổ" is Huyền Vũ addressed with respect, not a debut. Stripped (after
/// lowercasing) when comparing names, so a suffixed form folds into the bare
/// name instead of forking a second bible entry with its own voice.
const TITLE_SUFFIXES: [&str; 13] = [
    "lão tổ",
    "tiền bối",
    "đại nhân",
    "công tử",
    "tiểu thư",
    "thiếu gia",
    "trưởng lão",
    "sư phụ",
    "sư huynh",
    "sư tỷ",
    "sư đệ",
    "sư muội",
    "đạo hữu",
];

/// Comparison key for character names: squeezed whitespace, no trailing
/// `(...)` description ("Mao Ý (thanh niên mặc hoa phục)" → "mao ý"), no
/// title suffix, lowercased ("Sở Cuồng Sư" == "Sở Cuồng sư"). Display forms
/// are never rewritten — only compared through this.
pub fn canon_key(name: &str) -> String {
    let mut s = squeeze_ws(name);
    if let Some(open) = s.rfind('(') {
        if s.ends_with(')') {
            s = s[..open].trim_end().to_string();
        }
    }
    let mut low = s.to_lowercase();
    loop {
        let mut stripped = false;
        for t in TITLE_SUFFIXES {
            if let Some(rest) = low.strip_suffix(t) {
                let rest = rest.trim_end();
                if !rest.is_empty() {
                    low = rest.to_string();
                    stripped = true;
                    break;
                }
            }
        }
        if !stripped {
            break;
        }
    }
    low.trim().to_string()
}

/// Canonical bible name for any surface form: exact name-or-alias match
/// first (today's behaviour, unchanged), then the canonical-key fallback.
/// Unknown forms come back untouched — never invent an owner.
pub fn resolve_speaker(bible: &Value, name: &str) -> String {
    let chars: &[Value] = bible
        .get("characters")
        .and_then(|c| c.as_array())
        .map(|v| v.as_slice())
        .unwrap_or(&[]);
    for c in chars {
        let cname = c.get("name").and_then(|n| n.as_str()).unwrap_or("");
        if cname == name {
            return cname.to_string();
        }
        if c
            .get("proper_aliases")
            .and_then(|a| a.as_array())
            .map(|a| a.iter().any(|x| x.as_str() == Some(name)))
            .unwrap_or(false)
        {
            return cname.to_string();
        }
    }
    let key = canon_key(name);
    for c in chars {
        let cname = c.get("name").and_then(|n| n.as_str()).unwrap_or("");
        if canon_key(cname) == key {
            return cname.to_string();
        }
        if c
            .get("proper_aliases")
            .and_then(|a| a.as_array())
            .map(|a| a.iter().any(|x| x.as_str().map(canon_key).as_deref() == Some(key.as_str())))
            .unwrap_or(false)
        {
            return cname.to_string();
        }
    }
    name.to_string()
}

/// Rewrite a digest's roster + segment speakers to canonical bible names, in
/// place. Returns how many speaker slots changed. Run wherever a script is
/// persisted (worker digest, inductor completion, reconcile) so everything
/// downstream — cast, render runs, merge — only ever sees one name per person.
pub fn canonicalize_script(data: &mut Value, bible: &Value) -> usize {
    let mut changed = 0;
    if let Some(roster) = data.get_mut("roster").and_then(|r| r.as_array_mut()) {
        for r in roster.iter_mut() {
            if let Some(n) = r.as_str() {
                let c = resolve_speaker(bible, n);
                if c != n {
                    *r = Value::String(c);
                    changed += 1;
                }
            }
        }
    }
    if let Some(segs) = data.get_mut("segments").and_then(|s| s.as_array_mut()) {
        for s in segs.iter_mut() {
            if let Some(sp) = s.get("speaker").and_then(|v| v.as_str()).map(String::from) {
                let c = resolve_speaker(bible, &sp);
                if c != sp {
                    s["speaker"] = Value::String(c);
                    changed += 1;
                }
            }
        }
    }
    changed
}

fn alias_owner(form: &str, owner: &str, bible: &Value) -> Option<String> {
    // Compared through the canonical key: "Sở Cuồng sư" is owned by whoever
    // holds "Sở Cuồng Sư", and a character owns its own variant spellings.
    let (fk, ok) = (canon_key(form), canon_key(owner));
    bible
        .get("characters")
        .and_then(|c| c.as_array())
        .and_then(|chars| {
            chars.iter().find_map(|c| {
                let name = c.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let owns = c
                    .get("proper_aliases")
                    .and_then(|a| a.as_array())
                    .map(|a| a.iter().any(|x| x.as_str().map(canon_key) == Some(fk.clone())))
                    .unwrap_or(false);
                if canon_key(name) != ok && owns {
                    Some(name.to_string())
                } else {
                    None
                }
            })
        })
}

/// Proper-name forms may join the bible; pronouns/generics stay chapter-local.
fn promotable(form: &str, owner: &str, bible: &Value, log: &mut Vec<String>) -> Option<String> {
    let f = form.trim();
    if f.is_empty() || ALIAS_STOP.contains(&f.to_lowercase().as_str()) || f.chars().count() < 2 {
        return None;
    }
    if let Some(conflict) = alias_owner(f, owner, bible) {
        log.push(format!(
            "   bible: reject alias {f:?} for {owner} (owned by {conflict})"
        ));
        return None;
    }
    Some(f.to_string())
}

/// Attach surface forms to an existing character's `proper_aliases`, through
/// the full promotable check. Shared by the new-character and new-alias
/// paths so both agree on what may join the bible.
fn attach_aliases(
    bible: &mut Value,
    owner: &str,
    forms: &[String],
    chapter: &str,
    log: &mut Vec<String>,
) {
    let mut ok_forms: Vec<String> = Vec::new();
    for f in forms {
        if let Some(ok) = promotable(f, owner, bible, log) {
            if !ok_forms.contains(&ok) {
                ok_forms.push(ok);
            }
        }
    }
    if ok_forms.is_empty() {
        return;
    }
    if let Some(arr) = bible.get_mut("characters").and_then(|c| c.as_array_mut()) {
        for c in arr.iter_mut() {
            if c.get("name").and_then(|n| n.as_str()) == Some(owner) {
                let aliases = c.get_mut("proper_aliases").and_then(|a| a.as_array_mut());
                if let Some(aliases) = aliases {
                    for ok in &ok_forms {
                        if !aliases.iter().any(|x| x.as_str() == Some(ok.as_str())) {
                            aliases.push(json!(ok));
                            log.push(format!("   bible alias {ok:?} -> {owner} [{chapter}]"));
                        }
                    }
                }
            }
        }
    }
}

/// Fold a digest's new-character/alias findings into the shared bible.
pub fn merge_bible(bible: &mut Value, data: &Value, chapter: &str) -> Vec<String> {
    let mut log = Vec::new();

    let new_chars = data
        .get("new_characters")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();

    for nc in &new_chars {
        let name = nc
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if name.is_empty() {
            continue;
        }
        let extra: Vec<String> = nc
            .get("proper_aliases")
            .and_then(|a| a.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        // A variant spelling of someone already in the bible folds into them —
        // "Huyền Vũ lão tổ" arriving as a new_character joins Huyền Vũ as an
        // alias instead of forking a second entry with its own voice.
        let key = canon_key(&name);
        let matched = bible
            .get("characters")
            .and_then(|c| c.as_array())
            .and_then(|a| {
                a.iter().find_map(|c| {
                    let n = c.get("name").and_then(|x| x.as_str()).unwrap_or("");
                    (canon_key(n) == key).then(|| n.to_string())
                })
            });
        if let Some(owner) = matched {
            let mut forms = vec![name.clone()];
            forms.extend(extra);
            attach_aliases(bible, &owner, &forms, chapter, &mut log);
            log.push(format!("   bible =fold {name:?} into {owner} [{chapter}]"));
            continue;
        }
        // The name itself always joins (ownership veto only): a character must be
        // findable by its own name even when the digest emits a stopword/pronoun.
        // Extra aliases go through the full promotable check as before.
        let mut aliases: Vec<String> = Vec::new();
        if let Some(conflict) = alias_owner(&name, &name, bible) {
            log.push(format!(
                "   bible: reject name {name:?} (owned by {conflict})"
            ));
        } else {
            aliases.push(name.clone());
        }
        if let Some(extra) = nc.get("proper_aliases").and_then(|a| a.as_array()) {
            for x in extra.iter().filter_map(|x| x.as_str()) {
                if let Some(ok) = promotable(x, &name, bible, &mut log) {
                    if !aliases.contains(&ok) {
                        aliases.push(ok);
                    }
                }
            }
        }
        let hint = nc
            .get("voice_hint")
            .and_then(|h| h.as_str())
            .unwrap_or("")
            .to_string();
        let entry = json!({
            "name": name,
            "personality": nc.get("personality").cloned().unwrap_or(json!("")),
            "voice_hint": hint,
            "tags": normalise_tags(nc.get("tags")),
            "proper_aliases": aliases,
            "first_seen": chapter,
            "chapters_seen": [],
        });
        log.push(format!("   bible +{name} ({hint}) [{chapter}]"));
        if let Some(arr) = bible.get_mut("characters").and_then(|c| c.as_array_mut()) {
            arr.push(entry);
        }
    }

    if let Some(map) = data.get("new_aliases").and_then(|a| a.as_object()) {
        for (raw_owner, forms) in map {
            // The owner key goes through the same resolution as speakers: a
            // variant spelling still lands on the canonical character.
            let owner = resolve_speaker(bible, raw_owner);
            let owns_character = bible
                .get("characters")
                .and_then(|c| c.as_array())
                .map(|a| {
                    a.iter()
                        .any(|c| c.get("name").and_then(|n| n.as_str()) == Some(owner.as_str()))
                })
                .unwrap_or(false);
            if !owns_character {
                continue;
            }
            let Some(list) = forms.as_array() else { continue };
            let strs: Vec<String> = list
                .iter()
                .filter_map(|f| f.as_str().map(String::from))
                .collect();
            attach_aliases(bible, &owner, &strs, chapter, &mut log);
        }
    }

    // Mark everyone who appeared in this chapter.
    let mut spoke: Vec<String> = data
        .get("roster")
        .and_then(|r| r.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if let Some(segs) = data.get("segments").and_then(|s| s.as_array()) {
        for s in segs {
            if let Some(sp) = s.get("speaker").and_then(|v| v.as_str()) {
                // Canonical comparison: a variant speaker still marks its
                // character as seen.
                if !spoke.iter().any(|x| canon_key(x) == canon_key(sp)) {
                    spoke.push(sp.to_string());
                }
            }
        }
    }
    if let Some(chars) = bible.get_mut("characters").and_then(|c| c.as_array_mut()) {
        for c in chars.iter_mut() {
            let name = c.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
            if spoke.iter().any(|x| canon_key(x) == canon_key(&name)) {
                let seen = c.get_mut("chapters_seen").and_then(|v| v.as_array_mut());
                if let Some(seen) = seen {
                    if !seen.iter().any(|x| x.as_str() == Some(chapter)) {
                        seen.push(json!(chapter));
                    }
                }
            }
        }
    }

    log
}

// ---------------------------------------------------------------------------
// bible reconciliation — unifying forked characters after the fact
// ---------------------------------------------------------------------------

/// One proposed fold: every name in `absorb` is the same person as
/// `canonical` and disappears into them.
pub type BibleMerge = (String, Vec<String>);

/// Fold absorbed entries into their canonical character: aliases and
/// chapters_seen union, the absorbed names themselves join `proper_aliases`
/// (so future digests resolve through them), personality/voice_hint fill in
/// only when the canonical side is empty. Returns the merges that actually
/// applied, plus the log.
pub fn apply_merges(
    bible: &mut Value,
    merges: &[BibleMerge],
) -> (Vec<BibleMerge>, Vec<String>) {
    let mut log = Vec::new();
    let mut applied: Vec<BibleMerge> = Vec::new();
    let Some(chars) = bible.get_mut("characters").and_then(|c| c.as_array_mut()) else {
        return (applied, log);
    };
    for (canonical, absorb) in merges {
        let canon_idx = chars
            .iter()
            .position(|c| c.get("name").and_then(|n| n.as_str()) == Some(canonical.as_str()));
        let Some(ci) = canon_idx else {
            log.push(format!("   reconcile: skip — {canonical:?} not in bible"));
            continue;
        };
        let mut done: Vec<String> = Vec::new();
        for name in absorb {
            if name == canonical {
                continue;
            }
            let Some(ai) = chars
                .iter()
                .position(|c| c.get("name").and_then(|n| n.as_str()) == Some(name.as_str()))
            else {
                continue;
            };
            if ai == ci {
                continue;
            }
            let victim = chars.remove(ai);
            // Removing shifts indices: re-locate the canonical entry.
            let ci = chars
                .iter()
                .position(|c| c.get("name").and_then(|n| n.as_str()) == Some(canonical.as_str()))
                .unwrap_or(usize::MAX);
            if ci == usize::MAX {
                chars.push(victim);
                continue;
            }
            let target = &mut chars[ci];
            let mut aliases: Vec<String> = target
                .get("proper_aliases")
                .and_then(|a| a.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            for a in victim
                .get("proper_aliases")
                .and_then(|x| x.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)))
                .into_iter()
                .flatten()
                .chain(std::iter::once(name.clone()))
            {
                if !aliases.iter().any(|x| x == &a) {
                    aliases.push(a);
                }
            }
            target["proper_aliases"] = json!(aliases);
            let mut seen: Vec<String> = target
                .get("chapters_seen")
                .and_then(|a| a.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            for ch in victim
                .get("chapters_seen")
                .and_then(|a| a.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)))
                .into_iter()
                .flatten()
            {
                if !seen.contains(&ch) {
                    seen.push(ch);
                }
            }
            seen.sort();
            target["chapters_seen"] = json!(seen);
            for key in ["personality", "voice_hint"] {
                let empty = target
                    .get(key)
                    .and_then(|v| v.as_str())
                    .map(|s| s.trim().is_empty())
                    .unwrap_or(true);
                if empty {
                    if let Some(v) = victim.get(key).cloned() {
                        target[key] = v;
                    }
                }
            }
            log.push(format!("   reconcile: {name:?} -> {canonical}"));
            done.push(name.clone());
        }
        if !done.is_empty() {
            applied.push((canonical.clone(), done));
        }
    }
    (applied, log)
}

#[cfg(test)]
mod tests {
    use super::*;

        fn bible_with(name: &str, aliases: &[&str]) -> Value {
            json!({"characters": [{
                "name": name,
                "personality": "x",
                "voice_hint": "adult male",
                "proper_aliases": aliases,
                "first_seen": "01",
                "chapters_seen": []
            }]})
        }

    #[test]
    fn merge_bible_keeps_normalised_tags() {
        let mut bible = json!({"characters": []});
        let data = json!({
            "new_characters": [{"name": "Lão Trần", "personality": "gruff",
                "voice_hint": "elderly male", "tags": ["Old", "MALE", "old"],
                "proper_aliases": []}],
            "roster": ["Lão Trần"],
            "segments": []
        });
        merge_bible(&mut bible, &data, "07");
        assert_eq!(bible["characters"][0]["tags"], json!(["old", "male"]));
    }

    #[test]
    fn merge_bible_adds_characters_and_refuses_duplicates() {
        let mut bible = json!({"characters": []});
        let data = json!({
            "new_characters": [{"name": "Lão Trần", "personality": "gruff", "voice_hint": "elderly male", "proper_aliases": ["Trần lão"]}],
            "roster": ["Lão Trần"],
            "segments": [{"speaker": "Lão Trần"}]
        });
        let log = merge_bible(&mut bible, &data, "07");
        assert_eq!(bible["characters"].as_array().unwrap().len(), 1);
        assert_eq!(bible["characters"][0]["first_seen"], "07");
        assert_eq!(bible["characters"][0]["chapters_seen"], json!(["07"]));
        assert!(log.iter().any(|l| l.contains("bible +Lão Trần")));

        // second time: no duplicate
        merge_bible(&mut bible, &data, "08");
        assert_eq!(bible["characters"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn merge_bible_never_promotes_pronouns() {
        let mut bible = json!({"characters": []});
        let data = json!({
            "new_characters": [{"name": "Hắn", "voice_hint": "adult male", "proper_aliases": ["y", "phàm nhân"]}],
            "roster": [],
            "segments": []
        });
        merge_bible(&mut bible, &data, "01");
        let aliases = bible["characters"][0]["proper_aliases"].as_array().unwrap();
        assert_eq!(aliases.len(), 1, "only the name itself: {aliases:?}");
        assert_eq!(aliases[0], "Hắn");
    }

    #[test]
    fn merge_bible_rejects_an_alias_owned_by_another_character() {
        let mut bible = bible_with("A", &["Tuyết"]);
        let data = json!({
            "new_characters": [{"name": "B", "voice_hint": "adult female", "proper_aliases": ["Tuyết"]}],
            "roster": [],
            "segments": []
        });
        let log = merge_bible(&mut bible, &data, "02");
        assert!(log.iter().any(|l| l.contains("reject alias")), "{log:?}");
        let b = bible["characters"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "B")
            .unwrap();
        assert_eq!(b["proper_aliases"], json!(["B"]));
    }

    #[test]
    fn canon_key_strips_titles_descriptions_and_case() {
        assert_eq!(canon_key("Huyền Vũ lão tổ"), "huyền vũ");
        assert_eq!(canon_key("Mao Ý (thanh niên mặc hoa phục)"), "mao ý");
        assert_eq!(canon_key("Sở Cuồng Sư"), canon_key("Sở Cuồng sư"));
        assert_eq!(canon_key("  Dịch   Phong  "), "dịch phong");
        assert_eq!(canon_key("Lão Tổ"), "lão tổ", "a bare title is a name, not stripped");
        assert_eq!(canon_key("Huyền Vũ tiền bối"), "huyền vũ");
    }

    #[test]
    fn merge_bible_folds_a_suffixed_new_character_into_its_owner() {
        let mut bible = bible_with("Huyền Vũ", &["Huyền Vũ"]);
        let data = json!({
            "new_characters": [{
                "name": "Huyền Vũ lão tổ", "voice_hint": "adult male",
                "personality": "x", "tags": ["male"],
                "proper_aliases": []
            }],
            "roster": ["Huyền Vũ lão tổ"],
            "segments": [{"speaker": "Huyền Vũ lão tổ", "text": "Ừ."}]
        });
        let log = merge_bible(&mut bible, &data, "25");
        let chars = bible["characters"].as_array().unwrap();
        assert_eq!(chars.len(), 1, "no fork: {chars:?}");
        let aliases = chars[0]["proper_aliases"].as_array().unwrap();
        assert!(aliases.iter().any(|a| a == "Huyền Vũ lão tổ"), "{aliases:?}");
        assert!(log.iter().any(|l| l.contains("=fold")), "{log:?}");
        assert_eq!(chars[0]["chapters_seen"], json!(["25"]), "variant speaker marks seen");
    }

    #[test]
    fn merge_bible_folds_a_case_variant_without_a_second_entry() {
        let mut bible = bible_with("Sở Cuồng Sư", &["Sở Cuồng Sư"]);
        let data = json!({
            "new_characters": [{
                "name": "Sở Cuồng sư", "voice_hint": "adult male",
                "personality": "x", "tags": ["male"], "proper_aliases": []
            }],
            "roster": [],
            "segments": []
        });
        merge_bible(&mut bible, &data, "03");
        assert_eq!(bible["characters"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn merge_bible_resolves_a_variant_owner_key() {
        let mut bible = bible_with("Mao Ý", &["Mao Ý"]);
        let data = json!({
            "new_characters": [],
            "new_aliases": {"Mao Ý (thanh niên mặc hoa phục)": ["Mao Ý hoa phục"]},
            "roster": [],
            "segments": []
        });
        merge_bible(&mut bible, &data, "04");
        let aliases = bible["characters"][0]["proper_aliases"].as_array().unwrap();
        assert!(aliases.iter().any(|a| a == "Mao Ý hoa phục"), "{aliases:?}");
    }

    #[test]
    fn resolve_speaker_prefers_exact_then_canon_then_passthrough() {
        let bible = bible_with("Sở Cuồng Sư", &["Sở Cuồng Sư"]);
        assert_eq!(resolve_speaker(&bible, "Sở Cuồng Sư"), "Sở Cuồng Sư");
        assert_eq!(resolve_speaker(&bible, "Sở Cuồng sư"), "Sở Cuồng Sư");
        assert_eq!(resolve_speaker(&bible, "Người Lạ"), "Người Lạ", "unknown passes through");
    }

    #[test]
    fn canonicalize_script_rewrites_roster_and_speakers() {
        let bible = bible_with("Mao Ý", &["Mao Ý", "Mao Ý (thanh niên mặc hoa phục)"]);
        let mut data = json!({
            "roster": ["Mao Ý (thanh niên mặc hoa phục)", "Narrator"],
            "segments": [
                {"speaker": "Mao Ý (thanh niên mặc hoa phục)", "text": "Hừ."},
                {"speaker": "Narrator", "text": "Gió thổi."}
            ]
        });
        assert_eq!(canonicalize_script(&mut data, &bible), 2);
        assert_eq!(data["roster"], json!(["Mao Ý", "Narrator"]));
        assert_eq!(data["segments"][0]["speaker"], json!("Mao Ý"));
        assert_eq!(data["segments"][1]["speaker"], json!("Narrator"));
    }

    #[test]
    fn apply_merges_unions_aliases_chapters_and_keeps_the_canonical_voice_hint() {
        let mut bible = json!({"characters": [
            {"name": "Huyền Vũ", "personality": "cold", "voice_hint": "adult male",
             "proper_aliases": ["Huyền Vũ"], "first_seen": "10", "chapters_seen": ["10"]},
            {"name": "Huyền Vũ lão tổ", "personality": "", "voice_hint": "",
             "proper_aliases": ["Huyền Vũ lão tổ"], "first_seen": "25", "chapters_seen": ["25", "26"]}
        ]});
        let (applied, _) = apply_merges(&mut bible, &[("Huyền Vũ".into(), vec!["Huyền Vũ lão tổ".into()])]);
        assert_eq!(applied.len(), 1);
        let chars = bible["characters"].as_array().unwrap();
        assert_eq!(chars.len(), 1);
        assert_eq!(chars[0]["name"], json!("Huyền Vũ"));
        let aliases = chars[0]["proper_aliases"].as_array().unwrap();
        assert!(aliases.iter().any(|a| a == "Huyền Vũ lão tổ"), "{aliases:?}");
        assert_eq!(chars[0]["chapters_seen"], json!(["10", "25", "26"]));
        assert_eq!(chars[0]["voice_hint"], json!("adult male"), "canonical hint survives");
    }

    #[test]
    fn apply_merges_skips_unknown_names_quietly() {
        let mut bible = bible_with("A", &["A"]);
        let (applied, _) = apply_merges(
            &mut bible,
            &[("A".into(), vec!["Ghost".into()]), ("Ghost".into(), vec!["A".into()])],
        );
        assert!(applied.is_empty());
        assert_eq!(bible["characters"].as_array().unwrap().len(), 1);
    }

}
