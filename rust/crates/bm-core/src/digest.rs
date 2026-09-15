//! Stage 2 — digest a chapter into `script-NN.json`.
//!
//! Ported from `analyze.py`. Two behavioural changes are forced by running
//! across a cluster:
//!
//! 1. The worker never writes the authoritative bible. It receives a snapshot
//!    in its task offer, uses it to build the prompt, and returns a *delta*
//!    (new characters, new aliases, who spoke) which the inductor merges as the
//!    single writer. Concurrent digests therefore cannot clobber each other.
//! 2. The snapshot is mirrored to the worker's local `data/bible.json` so the
//!    cast assigner can still read voice hints.

use crate::config::Settings;
use crate::paths::Layout;
use crate::util::{atomic_write, head_chars, squeeze_ws};
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;

/// Surface forms that may NEVER join the bible: pronouns, generic nouns, verb phrases.
const ALIAS_STOP: [&str; 18] = [
    "hắn", "nàng", "ta", "ngươi", "y", "huynh", "đệ", "tỷ", "muội", "phàm nhân", "con", "người",
    "tên", "tiểu", "lão", "tiểu tử", "narrator", "người dẫn chuyện",
];

/// Vietnamese letters carrying a diacritic — the tell-tale of un-translated text.
const VI_DIACRITICS: &str = "àáạảãâầấậẩẫăằắặẳẵèéẹẻẽêềếệểễìíịỉĩòóọỏõôồốộổỗơờớợởỡùúụủũưừứựửữỳýỵỷỹđ";

/// The gender/age prefixes a `voice_hint` is allowed to start with.
const VOICE_HEADS: [&str; 6] = [
    "adult male",
    "adult female",
    "boy",
    "girl",
    "elderly male",
    "elderly female",
];

/// Why a generation attempt failed.
#[derive(Debug)]
pub enum GenError {
    /// The provider asked us to slow down. Retry after a delay.
    RateLimited(String),
    /// Anything else — do not retry, the prompt or the credentials are wrong.
    Fatal(anyhow::Error),
}

impl std::fmt::Display for GenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GenError::RateLimited(m) => write!(f, "rate limited: {m}"),
            GenError::Fatal(e) => write!(f, "{e}"),
        }
    }
}

/// What a digest produces: the per-chapter script plus the bible delta.
#[derive(Debug, Clone)]
pub struct DigestOutcome {
    pub script: Value,
    /// `{new_characters, new_aliases, roster, speakers}` — merged by the inductor.
    pub delta: Value,
    pub segments: usize,
    pub log: Vec<String>,
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// bible
// ---------------------------------------------------------------------------

pub fn load_bible(path: &Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .filter(|v| v.get("characters").is_some())
        .unwrap_or_else(|| json!({"characters": []}))
}

pub fn save_bible(bible: &Value, path: &Path) -> Result<()> {
    atomic_write(path, &serde_json::to_string_pretty(bible)?)
}

/// Lean context for the prompt: identity only, no chapter baggage.
pub fn bible_context(bible: &Value) -> String {
    let lean: Vec<Value> = bible
        .get("characters")
        .and_then(|c| c.as_array())
        .map(|chars| {
            chars
                .iter()
                .map(|c| {
                    json!({
                        "name": c.get("name"),
                        "personality": c.get("personality"),
                        "voice_hint": c.get("voice_hint"),
                        "tags": c.get("tags").cloned().unwrap_or(json!([])),
                        "proper_aliases": c.get("proper_aliases"),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    serde_json::to_string(&lean).unwrap_or_else(|_| "[]".to_string())
}

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

/// Parse the reconciler LLM's answer: `{"merges":[{"canonical":..,"absorb":[..]}]}`
/// (a bare array of the same objects also parses). Unknown names are kept —
/// the applier skips what is not in the bible. Unparseable input means no
/// LLM merges, never an error: the deterministic folds still apply.
pub fn parse_reconcile_merges(text: &str) -> Vec<BibleMerge> {
    let s = text.trim();
    let s = s.strip_prefix("```json").unwrap_or(s);
    let s = s.strip_prefix("```").unwrap_or(s);
    let s = s.strip_suffix("```").unwrap_or(s).trim();
    let v: Value = match serde_json::from_str(s) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let arr = v
        .get("merges")
        .and_then(|m| m.as_array())
        .cloned()
        .or_else(|| v.as_array().cloned())
        .unwrap_or_default();
    let mut out = Vec::new();
    for m in &arr {
        let canonical = m
            .get("canonical")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let absorb: Vec<String> = m
            .get("absorb")
            .and_then(|a| a.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|x| x.trim().to_string()))
                    .filter(|x| !x.is_empty() && x != &canonical)
                    .collect()
            })
            .unwrap_or_default();
        if !canonical.is_empty() && !absorb.is_empty() {
            out.push((canonical, absorb));
        }
    }
    out
}

/// What the reconciler found without asking anyone: certain folds plus the
/// ambiguous pairs worth one LLM call, with the prompt for it.
pub struct ReconcilePlan {
    /// Canon-key collisions — same bare name, safe to fold blind.
    pub folds: Vec<BibleMerge>,
    /// Same-token pairs with different keys — the LLM's only question.
    pub candidates: Vec<(String, String)>,
    /// Empty when there is nothing to ask.
    pub prompt: String,
}

fn first_seen_of(c: &Value) -> String {
    c.get("first_seen")
        .and_then(|v| v.as_str())
        .unwrap_or("zz")
        .to_string()
}

/// Plan a bible reconciliation: deterministic folds first, LLM candidates
/// second. Pure over the bible value — no I/O, so the inductor never holds
/// its lock while the LLM thinks.
pub fn reconcile_plan(bible: &Value) -> ReconcilePlan {
    let chars: &[Value] = bible
        .get("characters")
        .and_then(|c| c.as_array())
        .map(|v| v.as_slice())
        .unwrap_or(&[]);
    let names: Vec<String> = chars
        .iter()
        .filter_map(|c| c.get("name").and_then(|n| n.as_str()).map(String::from))
        .collect();

    // Group by canonical key: "Sở Cuồng Sư"/"Sở Cuồng sư", titled and
    // parenthetical variants all land in one bucket. Canonical is the
    // earliest-seen entry (the original, not the fork), ties go shortest.
    let mut groups: std::collections::BTreeMap<String, Vec<usize>> =
        std::collections::BTreeMap::new();
    for (i, n) in names.iter().enumerate() {
        groups.entry(canon_key(n)).or_default().push(i);
    }
    let mut folds: Vec<BibleMerge> = Vec::new();
    for idxs in groups.values().filter(|v| v.len() > 1) {
        let mut idxs = idxs.clone();
        idxs.sort_by_key(|&i| (first_seen_of(&chars[i]), names[i].len()));
        let canonical = names[idxs[0]].clone();
        let absorb: Vec<String> = idxs[1..].iter().map(|&i| names[i].clone()).collect();
        folds.push((canonical, absorb));
    }

    // Ambiguous pairs: share a word but differ canonically ("Huyền Vũ" vs
    // "Huyền Vũ Môn"?). Capped — the prompt stays small either way.
    let folded: std::collections::HashSet<String> = folds
        .iter()
        .flat_map(|(c, a)| std::iter::once(c.clone()).chain(a.iter().cloned()))
        .collect();
    let tokens = |n: &str| -> Vec<String> {
        n.to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| w.chars().count() > 1)
            .map(String::from)
            .collect()
    };
    let mut candidates: Vec<(String, String)> = Vec::new();
    for (i, a) in names.iter().enumerate() {
        if folded.contains(a) || a == "Narrator" {
            continue;
        }
        let ta = tokens(a);
        for b in names.iter().skip(i + 1) {
            if folded.contains(b) || b == "Narrator" {
                continue;
            }
            let tb = tokens(b);
            if ta.iter().any(|w| tb.contains(w)) {
                candidates.push((a.clone(), b.clone()));
                if candidates.len() >= 20 {
                    break;
                }
            }
        }
        if candidates.len() >= 20 {
            break;
        }
    }

    let prompt = if candidates.is_empty() {
        String::new()
    } else {
        let roster: Vec<Value> = chars
            .iter()
            .map(|c| {
                json!({
                    "name": c.get("name"),
                    "aliases": c.get("proper_aliases"),
                    "personality": c.get("personality"),
                })
            })
            .collect();
        format!(
            "You are merging duplicate characters in a Vietnamese web-novel cast list.\n\
             These pairs share a word but may be different people. Reply STRICT JSON only, no commentary:\n\
             {{\"merges\":[{{\"canonical\":\"<exact existing name>\",\"absorb\":[\"<exact existing name>\",...]}}]}}\n\
             Merge ONLY when certain both names are the same person (titles like lão tổ/tiền bối/đại nhân, \
             descriptions in parentheses, or casing/spelling variants of one name). When unsure, omit the pair — \
             an empty merges list is a valid answer.\n\
             Candidate pairs: {}\nBible: {}",
            serde_json::to_string(&candidates).unwrap_or_default(),
            serde_json::to_string(&roster).unwrap_or_default(),
        )
    };
    ReconcilePlan { folds, candidates, prompt }
}

/// Deterministic folds for cast keys that never made it into the bible:
/// same canon-key as a bible entry (titles, parentheticals, casing).
/// The bible name is canonical. Pure — the caller applies them through
/// `apply_reconcile`, which records the alias and rewrites cast + scripts.
pub fn cast_only_folds(bible: &Value, cast_keys: &[String]) -> Vec<BibleMerge> {
    let chars: &[Value] = bible
        .get("characters")
        .and_then(|c| c.as_array())
        .map(|v| v.as_slice())
        .unwrap_or(&[]);
    let mut canon_of: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for c in chars {
        if let Some(n) = c.get("name").and_then(|n| n.as_str()) {
            canon_of.entry(canon_key(n)).or_insert_with(|| n.to_string());
        }
    }
    let in_bible =
        |n: &str| chars.iter().any(|c| c.get("name").and_then(|x| x.as_str()) == Some(n));
    let mut out: std::collections::BTreeMap<String, Vec<String>> = std::collections::BTreeMap::new();
    for k in cast_keys {
        if k == "Narrator" || in_bible(k) {
            continue;
        }
        if let Some(canonical) = canon_of.get(&canon_key(k)) {
            out.entry(canonical.clone()).or_default().push(k.clone());
        }
    }
    out.into_iter().collect()
}

/// Inline non-verbal cues the VieNeu v3 Turbo emotion checkpoint renders as
/// sound instead of speech — researched from the installed engine
/// (`vieneu_utils/phonemize_text.py`, `_EMOTION_TAG_TO_K`): exactly these
/// three, in English, Vietnamese and unaccented forms. Any other bracketed
/// span is phonemized as ORDINARY TEXT (read aloud!), so the digest may only
/// emit these, and validation below rejects the rest.
const ALLOWED_INLINE_TAGS: [&str; 9] = [
    "cười", "chuckle", "cuoi",
    "thở dài", "sigh", "tho dai",
    "hắng giọng", "clear throat", "hang giong",
];

/// Bracketed spans in segment text, without the brackets.
fn inline_tags(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        let after = &rest[open + 1..];
        let Some(close) = after.find(']') else { break };
        out.push(after[..close].trim().to_string());
        rest = &after[close + 1..];
    }
    out
}

fn split_voice_head(hint: &str) -> String {
    hint.split([',', ':', '-', '–'])
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase()
}

/// Tags for one bible character entry: its `tags` field, or the voice_hint for
/// entries written before tags existed — so the pool works without re-digesting
/// the whole book.
pub fn tags_of(entry: &Value) -> Vec<String> {
    let tags = normalise_tags(entry.get("tags"));
    if tags.is_empty() {
        let hint = entry.get("voice_hint").and_then(|h| h.as_str()).unwrap_or("");
        crate::pool::tags_from_hint(hint)
    } else {
        tags
    }
}

/// Lowercase, deduped tags for a bible entry. Anything goes — the pool matches
/// by equality — but each tag must be a non-empty token, not a sentence.
fn normalise_tags(v: Option<&Value>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in v.and_then(|x| x.as_array()).cloned().unwrap_or_default() {
        let t = t.as_str().unwrap_or("").trim().to_lowercase();
        if !t.is_empty() && !t.contains(char::is_whitespace) && !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

pub fn validate(data: &Value, bible: &Value) -> Result<()> {
    if !data.is_object() {
        anyhow::bail!("top-level must be a JSON object");
    }
    let segments = data
        .get("segments")
        .and_then(|s| s.as_array())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("no segments"))?;

    let mut names: Vec<String> = data
        .get("roster")
        .and_then(|r| r.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    names.push("Narrator".to_string());
    let mut known = names.clone();
    if let Some(chars) = bible.get("characters").and_then(|c| c.as_array()) {
        for c in chars {
            if let Some(n) = c.get("name").and_then(|n| n.as_str()) {
                known.push(n.to_string());
            }
        }
    }

    for (i, s) in segments.iter().enumerate() {
        let speaker = s.get("speaker").and_then(|v| v.as_str()).unwrap_or("");
        // A variant spelling that resolves to a known character is fine — the
        // inductor canonicalizes the script on completion.
        if !names.iter().any(|n| n == speaker)
            && !known.iter().any(|k| k == &resolve_speaker(bible, speaker))
        {
            anyhow::bail!("segment {i}: unknown speaker {speaker:?}");
        }
        let text = s.get("text").and_then(|t| t.as_str()).unwrap_or("");
        if text.is_empty() {
            anyhow::bail!("segment {i}: empty text");
        }
        // Only the engine's three emotion cues may stand in brackets —
        // anything else is spoken aloud literally downstream.
        for tag in inline_tags(text) {
            if !ALLOWED_INLINE_TAGS.contains(&tag.to_lowercase().as_str()) {
                anyhow::bail!(
                    "segment {i}: [{tag}] is not a voice tag ([cười]/[thở dài]/[hắng giọng] only)"
                );
            }
        }
    }

    if let Some(mentions) = data.get("mentions").and_then(|m| m.as_object()) {
        for (form, owner) in mentions {
            let owner = owner.as_str().unwrap_or("");
            if !known.iter().any(|k| k == owner)
                && !known.iter().any(|k| k == &resolve_speaker(bible, owner))
            {
                anyhow::bail!("mention {form:?} -> unknown {owner:?}");
            }
        }
    }

    if let Some(ncs) = data.get("new_characters").and_then(|c| c.as_array()) {
        for nc in ncs {
            if nc.get("name").and_then(|n| n.as_str()).unwrap_or("").is_empty() {
                anyhow::bail!("new_character without name");
            }
            let hint = nc.get("voice_hint").and_then(|h| h.as_str()).unwrap_or("");
            let head = split_voice_head(hint);
            if !VOICE_HEADS.contains(&head.as_str()) {
                anyhow::bail!(
                    "new_character {}: voice_hint must start with gender/age",
                    nc.get("name").and_then(|n| n.as_str()).unwrap_or("?")
                );
            }
            // Tags are what the sample pool rolls on; without them a character
            // can only ever draw preset voices. `[]` is valid (the ageless),
            // a missing key or a sentence is not.
            let Some(tags) = nc.get("tags").and_then(|t| t.as_array()) else {
                anyhow::bail!(
                    "new_character {}: missing tags array",
                    nc.get("name").and_then(|n| n.as_str()).unwrap_or("?")
                );
            };
            for t in tags {
                let s = t.as_str().unwrap_or("");
                if s.trim().is_empty() || s.contains(char::is_whitespace) {
                    anyhow::bail!(
                        "new_character {}: tags must be single tokens, got {t:?}",
                        nc.get("name").and_then(|n| n.as_str()).unwrap_or("?")
                    );
                }
            }
        }
    }
    Ok(())
}

fn has_diacritic(word: &str) -> bool {
    word.chars().any(|c| VI_DIACRITICS.contains(c))
}

/// EN policy is trust-based; flag obvious violations for the review gate.
pub fn warn_vietnamese(data: &Value, bible: &Value) -> Vec<String> {
    let mut skip: Vec<String> = ["dich", "lac", "doan", "thanh", "nguyen", "tran", "ngo", "phong", "tuyet", "ly"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    if let Some(chars) = bible.get("characters").and_then(|c| c.as_array()) {
        for c in chars {
            if let Some(name) = c.get("name").and_then(|n| n.as_str()) {
                skip.extend(name.to_lowercase().split_whitespace().map(String::from));
            }
            if let Some(aliases) = c.get("proper_aliases").and_then(|a| a.as_array()) {
                skip.extend(
                    aliases
                        .iter()
                        .filter_map(|a| a.as_str())
                        .map(|a| a.to_lowercase()),
                );
            }
        }
    }

    let looks_vi = |s: &str| -> bool {
        s.split(|c: char| !c.is_alphabetic())
            .filter(|w| !w.is_empty())
            .any(|w| has_diacritic(w) && !skip.iter().any(|s| s == &w.to_lowercase()))
    };

    let mut warns = Vec::new();
    if let Some(ncs) = data.get("new_characters").and_then(|c| c.as_array()) {
        for nc in ncs {
            let name = nc.get("name").and_then(|n| n.as_str()).unwrap_or("?");
            for key in ["personality", "voice_hint"] {
                let v = nc.get(key).and_then(|x| x.as_str()).unwrap_or("");
                if looks_vi(v) {
                    warns.push(format!(
                        "   WARN: {name}.{key} looks Vietnamese, expected English"
                    ));
                }
            }
        }
    }
    let atmosphere = data.get("atmosphere").and_then(|a| a.as_str()).unwrap_or("");
    if looks_vi(atmosphere) {
        warns.push("   WARN: atmosphere looks Vietnamese, expected English".to_string());
    }
    warns
}

// ---------------------------------------------------------------------------
// generation backends
// ---------------------------------------------------------------------------

/// Pull a delay out of a provider error body: `retry in 53.2s` or `"retryDelay": "53s"`.
pub fn parse_retry_delay(s: &str) -> Option<f64> {
    if let Some(idx) = s.find("retry in ") {
        let tail = &s[idx + "retry in ".len()..];
        let num: String = tail
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        if let Ok(v) = num.parse::<f64>() {
            return Some(v);
        }
    }
    if let Some(idx) = s.find("retryDelay") {
        let tail = &s[idx..];
        let num: String = tail
            .chars()
            .skip_while(|c| !c.is_ascii_digit())
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        if let Ok(v) = num.parse::<f64>() {
            return Some(v);
        }
    }
    None
}

fn extract_json_object(text: &str) -> Result<String> {
    let start = text
        .find('{')
        .ok_or_else(|| anyhow!("no JSON object in output: {:?}", head_chars(text, 200)))?;
    let end = text
        .rfind('}')
        .ok_or_else(|| anyhow!("no closing brace in output: {:?}", head_chars(text, 200)))?;
    if end <= start {
        anyhow::bail!("malformed JSON span in output: {:?}", head_chars(text, 200));
    }
    Ok(text[start..=end].to_string())
}

async fn generate_opencode(prompt: &str, settings: &Settings) -> Result<String, GenError> {
    let full = format!(
        "Do not use any tools. Answer with the requested output and nothing else.\n\n{prompt}"
    );
    let out = tokio::process::Command::new("opencode")
        .args(["run", "-m", &settings.opencode_model, &full])
        .output()
        .await
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                GenError::Fatal(anyhow!("opencode CLI not found — install it first"))
            } else {
                GenError::Fatal(anyhow!(e).context("running opencode"))
            }
        })?;
    if !out.status.success() {
        return Err(GenError::Fatal(anyhow!(
            "opencode run failed: {}",
            head_chars(&String::from_utf8_lossy(&out.stderr), 500)
        )));
    }
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    extract_json_object(&stdout).map_err(GenError::Fatal)
}

async fn generate_ollama(prompt: &str, settings: &Settings) -> Result<String, GenError> {
    let body = json!({
        "model": settings.local_model,
        "messages": [{"role": "user", "content": prompt}],
        "stream": false,
        "format": "json",
        "options": {"temperature": 0, "num_ctx": 16384},
    });
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(1800))
        .build()
        .map_err(|e| GenError::Fatal(anyhow!(e)))?;
    let resp = client
        .post(format!("{}/api/chat", settings.ollama_url))
        .json(&body)
        .send()
        .await
        .map_err(|e| {
            GenError::Fatal(anyhow!(
                "cannot reach Ollama at {} ({e}); run: ollama serve",
                settings.ollama_url
            ))
        })?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(GenError::Fatal(anyhow!(
            "ollama error {status}: {}",
            head_chars(&text, 300)
        )));
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| GenError::Fatal(anyhow!(e)))?;
    v.pointer("/message/content")
        .and_then(|c| c.as_str())
        .map(String::from)
        .ok_or_else(|| GenError::Fatal(anyhow!("ollama response had no message.content")))
}

async fn generate_openrouter(prompt: &str, settings: &Settings) -> Result<String, GenError> {
    let key = std::env::var("OPENROUTER_API_KEY")
        .map_err(|_| GenError::Fatal(anyhow!("OPENROUTER_API_KEY missing — add it to .env")))?;
    let body = json!({
        "model": settings.openrouter_model,
        "messages": [{"role": "user", "content": prompt}],
        "temperature": 0,
        "max_tokens": 16384,
        "response_format": {"type": "json_object"},
    });
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(|e| GenError::Fatal(anyhow!(e)))?;
    let resp = client
        .post("https://openrouter.ai/api/v1/chat/completions")
        .header("Authorization", format!("Bearer {key}"))
        .header("HTTP-Referer", "https://github.com/lhuthng/storycast")
        .header("X-Title", "storycast")
        .json(&body)
        .send()
        .await
        .map_err(|e| GenError::Fatal(anyhow!("cannot reach OpenRouter ({e})")))?;
    let status = resp.status();
    let retry_after = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<f64>().ok());
    let text = resp.text().await.unwrap_or_default();
    if status.as_u16() == 429 {
        let delay = retry_after.map(|d| d + 2.0).unwrap_or(60.0);
        return Err(GenError::RateLimited(format!(
            "retry in {delay}s: {}",
            head_chars(&text, 200)
        )));
    }
    if !status.is_success() {
        return Err(GenError::Fatal(anyhow!(
            "OpenRouter error {status}: {}",
            head_chars(&text, 200)
        )));
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| GenError::Fatal(anyhow!(e)))?;
    v.pointer("/choices/0/message/content")
        .and_then(|c| c.as_str())
        .map(String::from)
        .ok_or_else(|| GenError::Fatal(anyhow!("OpenRouter response had no content")))
}

/// Gemini model chain over REST, ending in opencode as the last resort.
///
/// `analyze_models` (when set) IS the chain; otherwise the legacy single
/// `analyze_model` stands alone, which is today's behavior. Each model gets a
/// few attempts, then the next one, then opencode.
///
/// Skipped fast, never retried: 401/403 (the key is wrong for every model)
/// and 400 (the request itself is bad) — retrying those anywhere is burning
/// quota for nothing. Everything else walks on: 429s (after sleeping the
/// provider's own delay), 5xx, transport errors, unknown-model 404s and spent
/// day-quotas.
async fn generate_gemini(prompt: &str, settings: &Settings) -> Result<String, GenError> {
    let key = std::env::var("GEMINI_API_KEY")
        .map_err(|_| GenError::Fatal(anyhow!("GEMINI_API_KEY missing — copy .env.example to .env")))?;
    let mut last = String::from("no models configured");
    for model in analyze_chain(settings) {
        match try_gemini_model(prompt, &key, &model).await {
            ModelNext::Text(t) => return Ok(t),
            ModelNext::Abort(e) => return Err(GenError::Fatal(e)),
            ModelNext::Skip(reason) => {
                eprintln!("gemini {model} exhausted ({reason}) — next model");
                last = format!("{model}: {reason}");
            }
        }
    }
    eprintln!("gemini chain exhausted ({last}) — falling back to opencode");
    match generate_opencode(prompt, settings).await {
        Ok(t) => Ok(t),
        Err(GenError::RateLimited(m)) => Err(GenError::RateLimited(m)),
        Err(GenError::Fatal(e)) => Err(GenError::Fatal(anyhow!(
            "gemini chain exhausted ({last}); opencode fallback failed: {e:#}"
        ))),
    }
}

/// What one model attempt resolved to: text, the next model, or give up now.
enum ModelNext {
    Text(String),
    Skip(String),
    Abort(anyhow::Error),
}

/// Up to three attempts against one Gemini model.
async fn try_gemini_model(prompt: &str, key: &str, model: &str) -> ModelNext {
    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent?key={key}"
    );
    let body = json!({
        "contents": [{"parts": [{"text": prompt}]}],
        "generationConfig": {"responseMimeType": "application/json", "maxOutputTokens": 16384},
    });
    let client = reqwest::Client::new();
    let mut last = String::from("no attempts ran");
    for attempt in 0..3 {
        let resp = client.post(&url).json(&body).send().await;
        let (status, text) = match resp {
            Ok(r) => {
                let status = r.status();
                (status, r.text().await.unwrap_or_default())
            }
            Err(e) => {
                last = format!("transport error: {e}");
                continue;
            }
        };
        if status.is_success() {
            let v: Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(e) => return ModelNext::Abort(anyhow!(e).context("gemini response not JSON")),
            };
            return match v
                .pointer("/candidates/0/content/parts/0/text")
                .and_then(|t| t.as_str())
                .map(String::from)
            {
                Some(t) => ModelNext::Text(t),
                None => ModelNext::Abort(anyhow!(
                    "gemini response had no text part: {}",
                    head_chars(&text, 300)
                )),
            };
        }
        match status.as_u16() {
            // Wrong key or bad request: identical for every model, stop now.
            401 | 403 => {
                return ModelNext::Abort(anyhow!(
                    "gemini error {status} on {model}: key or project rejected — {}",
                    head_chars(text.trim(), 200)
                ))
            }
            400 => {
                return ModelNext::Abort(anyhow!(
                    "gemini error 400 on {model}: {}",
                    head_chars(text.trim(), 200)
                ))
            }
            // Unknown model name or its day quota spent: the next model is
            // exactly what the chain is for.
            404 => return ModelNext::Skip(format!("{status} ({})", head_chars(text.trim(), 120))),
            _ if text.contains("PerDay") => {
                return ModelNext::Skip("day quota spent".to_string())
            }
            429 => {
                let wait = parse_retry_delay(&text).map(|d| d + 2.0).unwrap_or(30.0);
                last = format!("429, retry in {wait:.0}s");
                eprintln!(
                    "gemini {model} attempt {}/3 rate-limited, sleeping {:.0}s",
                    attempt + 1,
                    wait
                );
                tokio::time::sleep(Duration::from_secs_f64(wait)).await;
            }
            _ => {
                last = format!("{status}: {}", head_chars(text.trim(), 200));
            }
        }
    }
    ModelNext::Skip(last)
}

/// Models to try, in order: `analyze_models` when set, else the legacy single.
fn analyze_chain(settings: &Settings) -> Vec<String> {
    let chain: Vec<String> = settings
        .analyze_models
        .iter()
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
        .collect();
    if chain.is_empty() {
        vec![settings.analyze_model.clone()]
    } else {
        chain
    }
}

/// One generation attempt against the configured backend.
pub async fn generate(prompt: &str, analyzer: &str, settings: &Settings) -> Result<String, GenError> {
    match analyzer {
        "local" => generate_ollama(prompt, settings).await,
        "openrouter" => generate_openrouter(prompt, settings).await,
        "opencode" => generate_opencode(prompt, settings).await,
        "gemini" => generate_gemini(prompt, settings).await,
        other => Err(GenError::Fatal(anyhow!(
            "unknown analyzer {other:?} (expected opencode | openrouter | local | gemini)"
        ))),
    }
}

fn strip_fences(raw: &str) -> &str {
    let s = raw.trim();
    let s = s.strip_prefix("```json").unwrap_or(s);
    let s = s.strip_suffix("```").unwrap_or(s);
    s.trim()
}

// ---------------------------------------------------------------------------
// the stage entry point
// ---------------------------------------------------------------------------

pub fn build_prompt(layout: &Layout, bible: &Value, chapter_text: &str) -> Result<String> {
    let template = std::fs::read_to_string(layout.prompt())
        .with_context(|| format!("reading prompt template {}", layout.prompt().display()))?;
    Ok(template
        .replace("{bible_json}", &bible_context(bible))
        .replace("{chapter_text}", chapter_text))
}

/// Digest one chapter. `bible` is the inductor's snapshot; the returned delta is
/// merged by the inductor, never here.
pub async fn digest_chapter(
    layout: &Layout,
    n: u32,
    bible: &Value,
    settings: &Settings,
    analyzer: &str,
    progress: &mut (dyn FnMut(f32, String) + Send),
) -> Result<DigestOutcome> {
    let chapter_path = layout.chapter_txt(n);
    let text = std::fs::read_to_string(&chapter_path)
        .with_context(|| format!("reading {}", chapter_path.display()))?;
    let prompt = build_prompt(layout, bible, &text)?;

    progress(0.10, format!("digest ch{n} via {analyzer}"));
    let mut raw: Option<String> = None;
    let mut last_rl = String::new();
    for attempt in 0..6 {
        match generate(&prompt, analyzer, settings).await {
            Ok(t) => {
                raw = Some(t);
                break;
            }
            Err(GenError::RateLimited(msg)) => {
                let wait = parse_retry_delay(&msg)
                    .unwrap_or_else(|| (30.0 * 2f64.powi(attempt)).min(300.0));
                progress(
                    (0.10 + 0.05 * attempt as f32).min(0.30),
                    format!("rate-limited, sleeping {wait:.0}s"),
                );
                tokio::time::sleep(Duration::from_secs_f64(wait)).await;
                last_rl = msg;
            }
            Err(GenError::Fatal(e)) => return Err(e),
        }
    }
    let mut raw = raw.ok_or_else(|| {
        anyhow!("analyzer {analyzer} still rate-limited after retries: {last_rl}")
    })?;

    progress(0.60, "validating digest".to_string());
    let parsed = parse_and_validate(&raw, bible);
    let data = match parsed {
        Ok(d) => d,
        Err(e) => {
            progress(0.65, "invalid JSON, asking for one repair".to_string());
            let repair = format!(
                "{prompt}\n\nYour last output was invalid: {e}. Return ONLY the corrected JSON object."
            );
            let second = match generate(&repair, analyzer, settings).await {
                Ok(t) => t,
                Err(GenError::RateLimited(m)) => {
                    anyhow::bail!("repair attempt rate-limited: {m}")
                }
                Err(GenError::Fatal(e2)) => return Err(e2),
            };
            raw = second;
            match parse_and_validate(&raw, bible) {
                Ok(d) => d,
                Err(e2) => {
                    let dump = layout.data().join(".last-analyze-raw.json");
                    let _ = atomic_write(&dump, &raw);
                    anyhow::bail!(
                        "digest invalid ({e2}); raw saved to {}",
                        dump.display()
                    );
                }
            }
        }
    };

    let mut log = Vec::new();
    let warnings = warn_vietnamese(&data, bible);

    // Grammar fixes must reference text that is actually in the chapter.
    let fixes = data
        .get("fixes")
        .and_then(|f| f.as_array())
        .cloned()
        .unwrap_or_default();
    for fx in &fixes {
        let before = fx.get("before").and_then(|b| b.as_str()).unwrap_or("");
        let after = fx.get("after").and_then(|a| a.as_str()).unwrap_or("");
        if before.is_empty() || after.is_empty() {
            anyhow::bail!("fix needs before+after: {fx}");
        }
        if !text.contains(before) {
            log.push(format!(
                "   WARN: fix source not found in chapter: {:?}",
                head_chars(before, 60)
            ));
        }
    }
    if !fixes.is_empty() {
        log.push(format!("   grammar fixes: {}", fixes.len()));
    }

    let segments = data
        .get("segments")
        .and_then(|s| s.as_array())
        .cloned()
        .unwrap_or_default();

    let script = json!({
        "atmosphere": data.get("atmosphere").cloned().unwrap_or(json!("")),
        "roster": data.get("roster").cloned().unwrap_or(json!([])),
        "mentions": data.get("mentions").cloned().unwrap_or(json!({})),
        "segments": segments,
        "fixes": fixes,
    });

    let script_path = layout.script(n);
    atomic_write(&script_path, &serde_json::to_string_pretty(&script)?)?;

    let delta = json!({
        "new_characters": data.get("new_characters").cloned().unwrap_or(json!([])),
        "new_aliases": data.get("new_aliases").cloned().unwrap_or(json!({})),
        "roster": data.get("roster").cloned().unwrap_or(json!([])),
        "segments": script.get("segments").cloned().unwrap_or(json!([])),
    });

    progress(1.0, format!("digest ch{n} done"));
    log.push(format!(
        "segments={} roster={} -> {}",
        script
            .get("segments")
            .and_then(|s| s.as_array())
            .map(|s| s.len())
            .unwrap_or(0),
        squeeze_ws(
            &script
                .get("roster")
                .map(|r| r.to_string())
                .unwrap_or_else(|| "[]".into())
        ),
        script_path.display()
    ));

    Ok(DigestOutcome {
        segments: script
            .get("segments")
            .and_then(|s| s.as_array())
            .map(|s| s.len())
            .unwrap_or(0),
        script,
        delta,
        log,
        warnings,
    })
}

fn parse_and_validate(raw: &str, bible: &Value) -> Result<Value> {
    let cleaned = strip_fences(raw);
    let data: Value = serde_json::from_str(cleaned).context("not valid JSON")?;
    validate(&data, bible)?;
    Ok(data)
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
    fn analyze_chain_defaults_to_the_single_model() {
        let plain = Settings::default();
        assert!(plain.analyze_models.is_empty());
        assert_eq!(analyze_chain(&plain), vec![plain.analyze_model.clone()]);

        let chained = Settings {
            analyze_models: vec![" gemini-3.8-flash ".into(), " ".into(), "gemini-3.5-flash".into()],
            ..Settings::default()
        };
        assert_eq!(analyze_chain(&chained), vec!["gemini-3.8-flash", "gemini-3.5-flash"]);
    }

    #[test]
    fn gemini_without_a_key_fails_before_touching_the_network() {
        let saved = std::env::var("GEMINI_API_KEY").ok();
        std::env::remove_var("GEMINI_API_KEY");
        let err = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(generate_gemini("{}", &Settings::default()))
            .unwrap_err();
        assert!(err.to_string().contains("GEMINI_API_KEY missing"), "{err}");
        if let Some(k) = saved {
            std::env::set_var("GEMINI_API_KEY", k);
        }
    }

    #[test]
    fn inline_tags_accept_the_engine_three_and_nothing_else() {
        assert_eq!(inline_tags("Hắn [cười] lớn."), vec!["cười"]);
        assert_eq!(inline_tags("[thở dài] Rồi đi."), vec!["thở dài"]);
        assert!(inline_tags("Không có gì.").is_empty());
        assert_eq!(inline_tags("a [b] c [d]"), vec!["b", "d"]);

        let tagged = |text: &str| {
            json!({
                "segments": [{"speaker": "Narrator", "text": text, "direction": "Say calm in Vietnamese: x"}],
                "roster": ["Narrator"]
            })
        };
        validate(&tagged("Hắn [cười]."), &json!({"characters": []})).unwrap();
        validate(&tagged("Nàng [CƯỜI]."), &json!({"characters": []})).unwrap();
        validate(&tagged("Hắn [sigh]."), &json!({"characters": []})).unwrap();
        let err = validate(&tagged("Dừng [pause] lại."), &json!({"characters": []})).unwrap_err();
        assert!(err.to_string().contains("[pause]"), "{err}");
        // Invented tags are read aloud downstream — that is why they fail here.
        let err = validate(&tagged("Hắn [khóc]."), &json!({"characters": []})).unwrap_err();
        assert!(err.to_string().contains("voice tag"), "{err}");
    }

    #[test]
    fn retry_delay_parses_both_provider_shapes() {
        assert_eq!(parse_retry_delay("... retry in 53.262507263s"), Some(53.262507263));
        assert_eq!(parse_retry_delay(r#"{"retryDelay": "53s"}"#), Some(53.0));
        assert_eq!(parse_retry_delay("no hint here"), None);
    }

    #[test]
    fn fences_are_stripped() {
        assert_eq!(strip_fences("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_fences("{\"a\":1}"), "{\"a\":1}");
    }

    #[test]
    fn json_object_is_extracted_from_surrounding_prose() {
        let t = "Sure! Here you go:\n{\"a\": 1}\nHope that helps.";
        assert_eq!(extract_json_object(t).unwrap(), "{\"a\": 1}");
        assert!(extract_json_object("no braces").is_err());
    }

    #[test]
    fn validate_rejects_a_speaker_outside_the_roster() {
        let data = json!({
            "segments": [{"speaker": "Ghost", "text": "hi", "direction": "Say calm in Vietnamese: hi"}],
            "roster": ["Narrator"]
        });
        let err = validate(&data, &json!({"characters": []})).unwrap_err();
        assert!(err.to_string().contains("unknown speaker"), "{err}");
    }

    #[test]
    fn validate_ignores_direction_and_rejects_a_bad_voice_hint() {
        // `direction` used to be required ("Say ..."); nothing consumes it, so
        // it is neither required nor checked now — old scripts keep passing.
        let no_dir = json!({
            "segments": [{"speaker": "Narrator", "text": "hi"}],
            "roster": ["Narrator"]
        });
        validate(&no_dir, &json!({"characters": []})).unwrap();

        let bad_hint = json!({
            "segments": [{"speaker": "Narrator", "text": "hi"}],
            "roster": ["Narrator"],
            "new_characters": [{"name": "X", "voice_hint": "mysterious"}]
        });
        let err = validate(&bad_hint, &json!({"characters": []})).unwrap_err();
        assert!(err.to_string().contains("gender/age"), "{err}");
    }

    #[test]
    fn validate_accepts_a_well_formed_digest() {
        let data = json!({
            "atmosphere": "A market at dawn.",
            "roster": ["Narrator", "Dịch Phong"],
            "mentions": {"hắn": "Dịch Phong"},
            "new_characters": [{"name": "Lão Trần", "voice_hint": "elderly male, gruff", "tags": ["old", "male"]}],
            "segments": [{"speaker": "Narrator", "text": "Trời sáng.", "direction": "Say calm in Vietnamese: Trời sáng."}]
        });
        validate(&data, &json!({"characters": []})).unwrap();
    }

    #[test]
    fn validate_rejects_a_missing_or_sloppy_tags_array() {
        let base = || {
            json!({
                "segments": [{"speaker": "Narrator", "text": "hi", "direction": "Say calm in Vietnamese: hi"}],
                "roster": ["Narrator"],
            })
        };
        // Missing key entirely.
        let mut no_tags = base();
        no_tags["new_characters"] =
            json!([{"name": "X", "voice_hint": "adult male, gruff"}]);
        assert!(validate(&no_tags, &json!({"characters": []})).is_err());

        // A sentence is not a tag.
        let mut sloppy = base();
        sloppy["new_characters"] =
            json!([{"name": "X", "voice_hint": "adult male, gruff", "tags": ["old man"]}]);
        let err = validate(&sloppy, &json!({"characters": []})).unwrap_err();
        assert!(err.to_string().contains("single tokens"), "{err}");

        // `[]` is the honest answer for the ageless — and it validates.
        let mut ageless = base();
        ageless["new_characters"] =
            json!([{"name": "X", "voice_hint": "elderly male, flat", "tags": []}]);
        validate(&ageless, &json!({"characters": []})).unwrap();
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

    #[test]
    fn parse_reconcile_merges_tolerates_shapes_and_garbage() {
        let got = parse_reconcile_merges(
            "```json\n{\"merges\":[{\"canonical\":\"A\",\"absorb\":[\"B\",\"A\"]}]}\n```",
        );
        assert_eq!(got, vec![("A".to_string(), vec!["B".to_string()])]);
        let bare = parse_reconcile_merges("[{\"canonical\":\"A\",\"absorb\":[\"B\"]}]");
        assert_eq!(bare.len(), 1);
        assert!(parse_reconcile_merges("not json at all").is_empty());
        assert!(parse_reconcile_merges("{\"merges\":[]}").is_empty());
    }

    #[test]
    fn reconcile_plan_folds_canon_groups_and_asks_about_the_rest() {
        let bible = json!({"characters": [
            {"name": "Sở Cuồng Sư", "personality": "x", "voice_hint": "adult male",
             "proper_aliases": ["Sở Cuồng Sư"], "first_seen": "02", "chapters_seen": []},
            {"name": "Sở Cuồng sư", "personality": "y", "voice_hint": "",
             "proper_aliases": ["Sở Cuồng sư"], "first_seen": "09", "chapters_seen": []},
            {"name": "Huyền Vũ", "personality": "cold", "voice_hint": "adult male",
             "proper_aliases": ["Huyền Vũ"], "first_seen": "01", "chapters_seen": []},
            {"name": "Huyền Vũ Môn", "personality": "a sect", "voice_hint": "",
             "proper_aliases": ["Huyền Vũ Môn"], "first_seen": "05", "chapters_seen": []}
        ]});
        let plan = reconcile_plan(&bible);
        assert_eq!(plan.folds.len(), 1, "only the case pair folds blind");
        assert_eq!(plan.folds[0].0, "Sở Cuồng Sư", "earliest-seen wins");
        assert_eq!(plan.folds[0].1, vec!["Sở Cuồng sư".to_string()]);
        assert!(
            plan.candidates.iter().any(|(a, b)| a == "Huyền Vũ" && b == "Huyền Vũ Môn"),
            "shared-token pair goes to the LLM: {:?}",
            plan.candidates
        );
        assert!(!plan.prompt.is_empty());
    }

    #[test]
    fn reconcile_plan_is_empty_on_a_clean_bible() {
        let bible = bible_with("A", &["A"]);
        let plan = reconcile_plan(&bible);
        assert!(plan.folds.is_empty() && plan.candidates.is_empty() && plan.prompt.is_empty());
    }

    #[test]
    fn cast_only_folds_catches_title_case_and_parenthetical_variants() {
        let bible = json!({"characters": [
            {"name": "Huyền Vũ", "proper_aliases": [], "first_seen": "01", "chapters_seen": []},
            {"name": "Sở Cuồng sư", "proper_aliases": [], "first_seen": "02", "chapters_seen": []},
            {"name": "Mao Ý", "proper_aliases": [], "first_seen": "03", "chapters_seen": []}
        ]});
        let cast = ["Huyền Vũ lão tổ", "Sở Cuồng Sư", "Mao Ý (thanh niên mặc hoa phục)",
            "Ngao Khánh", "Huyền Vũ", "Narrator"]
            .iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let folds = cast_only_folds(&bible, &cast);
        assert_eq!(folds.len(), 3, "{folds:?}");
        // New people and existing entries never fold.
        assert!(!folds.iter().any(|(_, a)| a.contains(&"Ngao Khánh".to_string())));
    }

    #[test]
    fn vietnamese_leak_detection_ignores_known_names() {
        let bible = bible_with("Lạc Lan Tuyết", &["Tuyết"]);
        let data = json!({
            "atmosphere": "A cold morning in the courtyard.",
            "new_characters": [{"name": "Lạc Lan Tuyết", "personality": "lạnh lùng", "voice_hint": "adult female"}]
        });
        let warns = warn_vietnamese(&data, &bible);
        assert!(
            warns.iter().any(|w| w.contains("personality")),
            "expected a personality warning: {warns:?}"
        );
        // the name itself must not trip the detector
        assert!(
            !warns.iter().any(|w| w.contains("atmosphere")),
            "English atmosphere flagged: {warns:?}"
        );
    }

    #[test]
    fn bible_context_is_identity_only() {
        let bible = json!({"characters": [{
            "name": "A", "personality": "p", "voice_hint": "adult male",
            "proper_aliases": ["B"], "first_seen": "01", "chapters_seen": ["01"]
        }]});
        let ctx = bible_context(&bible);
        assert!(ctx.contains("\"name\":\"A\""));
        assert!(!ctx.contains("chapters_seen"), "context leaked chapter baggage: {ctx}");
    }

    #[test]
    fn load_bible_defaults_when_missing_or_corrupt() {
        let missing = load_bible(Path::new("/nonexistent/bible.json"));
        assert_eq!(missing, json!({"characters": []}));
    }
}
