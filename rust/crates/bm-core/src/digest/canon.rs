use super::tags::normalise_tags;
use crate::util::squeeze_ws;
use serde_json::{json, Value};

/// Surface forms that may NEVER join the bible: pronouns, bare role nouns,
/// titles, self-references and indefinite descriptions. Compared lowercased,
/// so "Nữ tử" and "nữ tử" are the same trap.
///
/// The rule behind the list: an alias is only stored when it identifies its
/// owner better than chance. A bare generic ("nữ tử", "công tử", "tiền bối",
/// "sư phụ") matches half the cast, so storing it does not resolve future
/// chapters — it hijacks them, routing every unnamed woman to whoever owns
/// "nữ tử" (ch112 went to Lạc Lan Tuyết that way although the sword, the
/// frost-face and the sect all said Bạch Phiêu Phiêu). Forms carrying a
/// proper-name token ("Lý cô nương", "Dịch sư phụ", "lão Ngô", "nữ tử áo
/// trắng") stay: the name does the identifying.
/// Names themselves are never touched by this list — only `proper_aliases`.
const ALIAS_STOP: [&str; 75] = [
    "hắn",
    "nàng",
    "ta",
    "ngươi",
    "y",
    "huynh",
    "đệ",
    "tỷ",
    "muội",
    "phàm nhân",
    "con",
    "người",
    "tên",
    "tiểu",
    "lão",
    "tiểu tử",
    "narrator",
    "người dẫn chuyện",
    // Bare person nouns: any chapter's unnamed figure matches them.
    "nữ tử",
    "nam tử",
    "cô gái",
    "thiếu nữ",
    "thiếu niên",
    "nam hài",
    "cháu gái",
    "tiểu thư",
    "công tử",
    "tiểu nam hài",
    "nữ nhân này",
    // Titles and offices: every sect has one, and offices change hands.
    "tiền bối",
    "sư phụ",
    "sư thúc",
    "sư tôn",
    "tông chủ",
    "hội trưởng",
    "thánh nữ",
    "phu nhân",
    "lão gia",
    "các hạ",
    "đại vương",
    "vương",
    "tiên nữ",
    // Roles and relations: generic by definition.
    "thuộc hạ",
    "quản gia",
    "lão ca",
    "lão già",
    "lão giả",
    "lão đầu",
    // Kinship and address: who the word points at is decided by who is
    // speaking, so storing it hands every future speaker's master, brother or
    // disciple to whichever character claimed the word first. "sư phụ" was
    // here already; the rest of the family belongs with it.
    "đồ nhi",
    "đệ tử",
    "sư điệt",
    "ca ca",
    "đại ca",
    "hiền đệ",
    "hiền huynh",
    "lão hữu",
    // Self-references: first-person pronouns wearing a noun's clothes.
    "bản tôn",
    "bổn hoàng",
    // Anaphora and indefinites: "that one", "a mortal".
    "vị kia",
    "ông ta",
    "một thanh niên",
    "một phàm nhân",
    "phàm nhân này",
    "bọn họ",
    "thiếu niên này",
    "vị thiếu niên này",
    "hai vị cô nương",
    "hai cô gái",
    // Species nouns used as names: any second dog/crow/centipede hijacks them.
    "chó",
    "tiểu cẩu",
    "chú chó",
    "con chó",
    "cẩu nhi",
    "quạ đen",
    "con rết",
];

/// Vietnamese letters carrying a diacritic — the tell-tale of un-translated text.
pub(crate) const VI_DIACRITICS: &str =
    "àáạảãâầấậẩẫăằắặẳẵèéẹẻẽêềếệểễìíịỉĩòóọỏõôồốộổỗơờớợởỡùúụủũưừứựửữỳýỵỷỹđ";

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

/// Canonical bible name for any surface form: exact name, then exact alias,
/// then the canonical-key fallback over each. Unknown forms come back
/// untouched — never invent an owner.
///
/// The passes are ordered, and that order is load-bearing. A character's own
/// name is an identity, so it must beat *any* other character's alias for it —
/// the bible routinely contradicts itself on this, because a digest will list
/// an epithet as an alias of one character and later introduce it as a
/// character in its own right. `Vân bá` is exactly that: a character, and also
/// listed among `Lão giả`'s aliases. A single interleaved pass let whichever
/// character happened to sit earlier in the file win, so the cast was keyed
/// under `Lão giả` while every script still said `Vân bá` — and planning that
/// chapter failed with `cast has no voice for "Vân bá"`.
pub fn resolve_speaker(bible: &Value, name: &str) -> String {
    let chars: &[Value] = bible
        .get("characters")
        .and_then(|c| c.as_array())
        .map(|v| v.as_slice())
        .unwrap_or(&[]);
    fn name_of(c: &Value) -> &str {
        c.get("name").and_then(|n| n.as_str()).unwrap_or("")
    }
    let owns = |c: &Value, form: &str| -> bool {
        c.get("proper_aliases")
            .and_then(|a| a.as_array())
            .map(|a| a.iter().any(|x| x.as_str() == Some(form)))
            .unwrap_or(false)
    };
    let owns_key = |c: &Value, key: &str| -> bool {
        c.get("proper_aliases")
            .and_then(|a| a.as_array())
            .map(|a| {
                a.iter()
                    .any(|x| x.as_str().map(canon_key).as_deref() == Some(key))
            })
            .unwrap_or(false)
    };
    // 1. exact name — an identity, never another character's alias.
    for c in chars {
        if name_of(c) == name {
            return name.to_string();
        }
    }
    // 2. exact alias.
    for c in chars {
        if owns(c, name) {
            return name_of(c).to_string();
        }
    }
    let key = canon_key(name);
    // 3. the same name up to case, a title suffix or a trailing description.
    for c in chars {
        if canon_key(name_of(c)) == key {
            return name_of(c).to_string();
        }
    }
    // 4. an alias up to the same folding.
    for c in chars {
        if owns_key(c, &key) {
            return name_of(c).to_string();
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
                    .map(|a| {
                        a.iter()
                            .any(|x| x.as_str().map(canon_key) == Some(fk.clone()))
                    })
                    .unwrap_or(false);
                if canon_key(name) != ok && owns {
                    Some(name.to_string())
                } else {
                    None
                }
            })
        })
}

/// Whether a surface form's owner depends on the local scene rather than the
/// form itself. These are safe to interpret from nearby narration, but unsafe
/// to store in a chapter-wide map: `Đồ nhi` may address Chung Thanh in one
/// exchange and Lạc Lan Tuyết in another in the same chapter.
pub(crate) fn is_scenario_dependent(form: &str) -> bool {
    ALIAS_STOP.contains(&form.trim().to_lowercase().as_str())
}

/// Proper-name forms may join the bible; pronouns/generics stay chapter-local.
fn promotable(form: &str, owner: &str, bible: &Value, log: &mut Vec<String>) -> Option<String> {
    let f = form.trim();
    if f.is_empty() || is_scenario_dependent(f) || f.chars().count() < 2 {
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

/// Drop the stop-listed forms from one character's alias list, keeping the
/// character's own name even when the words are generic ("Quản gia" is
/// somebody's name — identity beats ambiguity).
///
/// **One implementation, two callers, and that is the point.** A scrub and a
/// reconcile fold have to agree on what may sit in `proper_aliases`, or the
/// fold re-imports exactly the generic the scrub just removed. That was the
/// hole: `apply_merges` copied the absorbed entry's aliases wholesale, so a
/// scrub could never win — bare "sư phụ" / "tiểu thư" kept coming back onto
/// the wrong character, and `validate_digest_identity` then read them as
/// exclusive ownership, failing a *correct* chapter-local resolution
/// ("sư phụ" is whoever is speaking's own master) as
/// `mention "sư phụ" is owned by {"Thanh Sơn lão tổ"}, not "Dịch Phong"`.
fn strip_stopped_aliases(name: &str, aliases: &mut Vec<String>) {
    let name_low = name.trim().to_lowercase();
    aliases.retain(|a| {
        let low = a.trim().to_lowercase();
        low == name_low || !ALIAS_STOP.contains(&low.as_str())
    });
}

/// Drop ambiguous surface forms from every character's `proper_aliases`.
///
/// The legacy this cleans: `merge_bible` used to attach bare generics before
/// `ALIAS_STOP` covered them, so entries like "nữ tử" sit on characters today
/// and hijack every future chapter about an unnamed woman. The check is the
/// same list `promotable` enforces for new aliases, so a scrubbed bible and a
/// bible built from scratch agree.
///
/// Names are sacred: an entry equal to its own character's name stays even
/// when the words are generic ("Quản gia" is somebody's name). Everything
/// else on the list goes, and the log names each removal. Idempotent.
pub fn scrub_ambiguous_aliases(bible: &mut Value) -> Vec<String> {
    let mut log = Vec::new();
    let Some(chars) = bible.get_mut("characters").and_then(|c| c.as_array_mut()) else {
        return log;
    };
    for c in chars.iter_mut() {
        let name = c
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string();
        let Some(aliases) = c.get_mut("proper_aliases").and_then(|a| a.as_array_mut()) else {
            continue;
        };
        let mut forms: Vec<String> = aliases
            .iter()
            .filter_map(|a| a.as_str().map(String::from))
            .collect();
        let before = forms.len();
        strip_stopped_aliases(&name, &mut forms);
        if forms.len() == before {
            continue;
        }
        let kept = forms.clone();
        aliases.clear();
        for f in forms {
            aliases.push(json!(f));
        }
        log.push(format!(
            "   bible scrub {name:?}: dropped {} ambiguous alias(es), kept {kept:?}",
            before - kept.len()
        ));
    }
    log
}

/// Fold a digest's new-character/alias findings into the shared bible.
pub fn merge_bible(bible: &mut Value, data: &Value, chapter: &str) -> Vec<String> {
    let mut log = Vec::new();

    // Heal before writing. A bible polluted by an older merge keeps a bare
    // generic on the wrong character for ever otherwise: the scrub was reachable
    // only through an operator's `:reconcile` press, and a fold then handed the
    // same words back, so the entry could never be cleaned and the ownership
    // check went on failing *correct* chapter-local resolutions. The bible has
    // exactly one writer, so cleaning it here is the same guarantee the merges
    // below already have — and it costs nothing on a clean bible, because the
    // scrub is idempotent.
    log.extend(scrub_ambiguous_aliases(bible));

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
            let Some(list) = forms.as_array() else {
                continue;
            };
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
            let name = c
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
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
pub fn apply_merges(bible: &mut Value, merges: &[BibleMerge]) -> (Vec<BibleMerge>, Vec<String>) {
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
            // A fold must not re-import a generic a scrub just removed.
            strip_stopped_aliases(canonical, &mut aliases);
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
        assert_eq!(
            canon_key("Lão Tổ"),
            "lão tổ",
            "a bare title is a name, not stripped"
        );
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
        assert!(
            aliases.iter().any(|a| a == "Huyền Vũ lão tổ"),
            "{aliases:?}"
        );
        assert!(log.iter().any(|l| l.contains("=fold")), "{log:?}");
        assert_eq!(
            chars[0]["chapters_seen"],
            json!(["25"]),
            "variant speaker marks seen"
        );
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
        assert_eq!(
            resolve_speaker(&bible, "Người Lạ"),
            "Người Lạ",
            "unknown passes through"
        );
    }

    #[test]
    fn a_characters_own_name_beats_another_characters_alias_for_it() {
        // The bible contradicts itself routinely: a digest lists an epithet as
        // one character's alias, then a later chapter introduces it as a
        // character in its own right. Real shape — `Vân bá` is a character and
        // also sits in `Lão giả`'s aliases, earlier in the file. With the
        // passes interleaved, position decided the answer, the cast was keyed
        // under `Lão giả`, and the chapter would not plan.
        let bible = json!({"characters": [
            {"name": "Lão giả", "proper_aliases": ["Lão giả", "Kim lão", "Vân bá"]},
            {"name": "Vân bá", "proper_aliases": ["Ngao Vân"]}
        ]});
        assert_eq!(
            resolve_speaker(&bible, "Vân bá"),
            "Vân bá",
            "an exact name is an identity, never an alias"
        );
        // The alias still works for a form that is nobody's own name.
        assert_eq!(resolve_speaker(&bible, "Kim lão"), "Lão giả");
        // And the owner is stable whichever way round the file lists them.
        let flipped = json!({"characters": [
            {"name": "Vân bá", "proper_aliases": ["Ngao Vân"]},
            {"name": "Lão giả", "proper_aliases": ["Lão giả", "Kim lão", "Vân bá"]}
        ]});
        assert_eq!(resolve_speaker(&flipped, "Vân bá"), "Vân bá");
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
        let (applied, _) = apply_merges(
            &mut bible,
            &[("Huyền Vũ".into(), vec!["Huyền Vũ lão tổ".into()])],
        );
        assert_eq!(applied.len(), 1);
        let chars = bible["characters"].as_array().unwrap();
        assert_eq!(chars.len(), 1);
        assert_eq!(chars[0]["name"], json!("Huyền Vũ"));
        let aliases = chars[0]["proper_aliases"].as_array().unwrap();
        assert!(
            aliases.iter().any(|a| a == "Huyền Vũ lão tổ"),
            "{aliases:?}"
        );
        assert_eq!(chars[0]["chapters_seen"], json!(["10", "25", "26"]));
        assert_eq!(
            chars[0]["voice_hint"],
            json!("adult male"),
            "canonical hint survives"
        );
    }

    #[test]
    fn apply_merges_skips_unknown_names_quietly() {
        let mut bible = bible_with("A", &["A"]);
        let (applied, _) = apply_merges(
            &mut bible,
            &[
                ("A".into(), vec!["Ghost".into()]),
                ("Ghost".into(), vec!["A".into()]),
            ],
        );
        assert!(applied.is_empty());
        assert_eq!(bible["characters"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn merge_bible_refuses_bare_generics_as_new_aliases() {
        // ch112's hijack, pinned at the gate: "nữ tử" must never attach to
        // anyone again, while a name-bearing form still does.
        let mut bible = bible_with("Lạc Lan Tuyết", &["Lạc Lan Tuyết"]);
        let data = json!({
            "new_characters": [],
            "new_aliases": {"Lạc Lan Tuyết": ["nữ tử", "Nữ tử", "cô gái", "tiền bối", "vị kia", "Lý cô nương"]},
            "roster": [],
            "segments": []
        });
        merge_bible(&mut bible, &data, "200");
        let aliases: Vec<&str> = bible["characters"][0]["proper_aliases"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|a| a.as_str())
            .collect();
        assert!(
            aliases.contains(&"Lý cô nương"),
            "a name-bearing form still attaches: {aliases:?}"
        );
        for generic in ["nữ tử", "Nữ tử", "cô gái", "tiền bối", "vị kia"] {
            assert!(
                !aliases.iter().any(|a| a.to_lowercase() == generic),
                "{generic:?} must be refused: {aliases:?}"
            );
        }
    }

    #[test]
    fn scrub_drops_legacy_generics_but_keeps_names_and_specifics() {
        let mut bible = json!({"characters": [
            {"name": "Lạc Lan Tuyết", "proper_aliases":
                ["Lạc Lan Tuyết", "Nữ tử", "nữ tử", "cô gái", "nữ tử áo trắng", "Lý cô nương"]},
            {"name": "Dịch Phong", "proper_aliases":
                ["Dịch Phong", "công tử", "tiền bối", "vị kia", "Dịch sư phụ"]},
            {"name": "Quản gia", "proper_aliases": ["Quản gia"]},
        ]});
        let log = scrub_ambiguous_aliases(&mut bible);
        assert_eq!(log.len(), 2, "{log:?}");
        let aliases = |n: &str| -> Vec<String> {
            bible["characters"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["name"] == n)
                .unwrap()["proper_aliases"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|a| a.as_str().map(String::from))
                .collect()
        };
        assert_eq!(
            aliases("Lạc Lan Tuyết"),
            vec!["Lạc Lan Tuyết", "nữ tử áo trắng", "Lý cô nương"],
            "bare generics go, name-bearing forms stay"
        );
        assert_eq!(aliases("Dịch Phong"), vec!["Dịch Phong", "Dịch sư phụ"]);
        assert_eq!(
            aliases("Quản gia"),
            vec!["Quản gia"],
            "a character keeps its own name even when generic"
        );
        // Second run is a no-op: the scrub is idempotent.
        assert!(scrub_ambiguous_aliases(&mut bible).is_empty());
    }

    #[test]
    fn a_fold_never_re_imports_a_generic_alias() {
        // The hole a scrub could never win through: `apply_merges` copied the
        // absorbed entry's aliases wholesale, so "Sư phụ" came back onto the
        // character every reconcile had just removed it from — and
        // `validate_digest_identity` then read it as exclusive ownership.
        let mut bible = json!({"characters": [
            {"name": "Thanh Sơn lão tổ", "proper_aliases":
                ["Thanh Sơn lão tổ", "Sư phụ", "sư tôn"]},
            {"name": "Thanh Sơn", "proper_aliases": ["lão đầu", "Lục Thanh Sơn"]},
        ]});
        let merges = vec![(
            "Thanh Sơn lão tổ".to_string(),
            vec!["Thanh Sơn".to_string()],
        )];
        let (applied, _) = apply_merges(&mut bible, &merges);
        assert_eq!(applied.len(), 1, "the fold applied");
        let aliases = alias_list(&bible, "Thanh Sơn lão tổ");
        for generic in ["Sư phụ", "sư tôn", "lão đầu"] {
            assert!(
                !aliases.iter().any(|a| a == generic),
                "{generic:?} must not survive the fold: {aliases:?}"
            );
        }
        assert!(
            aliases.contains(&"Lục Thanh Sơn".to_string()),
            "a proper name still joins: {aliases:?}"
        );
    }

    #[test]
    fn merge_bible_heals_an_already_polluted_alias_list() {
        // Nobody presses reconcile on a machine nobody drives, so the legacy
        // pollution has to be cleaned by the writer that already runs: the
        // merge on every digest completion.
        let mut bible = json!({"characters": [{
            "name": "Thanh Sơn lão tổ", "personality": "p", "voice_hint": "elderly male",
            "tags": [], "proper_aliases":
                ["Thanh Sơn lão tổ", "Sư phụ", "tiểu thư", "Lục Thanh Sơn"],
            "first_seen": "01", "chapters_seen": []
        }]});
        let data = json!({
            "new_characters": [], "new_aliases": {},
            "roster": ["Thanh Sơn lão tổ"], "segments": []
        });
        let log = merge_bible(&mut bible, &data, "01");
        assert_eq!(
            alias_list(&bible, "Thanh Sơn lão tổ"),
            vec!["Thanh Sơn lão tổ", "Lục Thanh Sơn"],
            "the generic goes, the name stays"
        );
        assert!(
            log.iter().any(|l| l.contains("scrub")),
            "the repair is said out loud: {log:?}"
        );
    }

    /// The alias list of the named character, as owned strings.
    fn alias_list(bible: &Value, name: &str) -> Vec<String> {
        bible["characters"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == name)
            .unwrap()["proper_aliases"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|a| a.as_str().map(String::from))
            .collect()
    }
}
