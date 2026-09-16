use super::canon::{canon_key, BibleMerge};
use serde_json::{json, Value};

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
    ReconcilePlan {
        folds,
        candidates,
        prompt,
    }
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
    let mut canon_of: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    for c in chars {
        if let Some(n) = c.get("name").and_then(|n| n.as_str()) {
            canon_of
                .entry(canon_key(n))
                .or_insert_with(|| n.to_string());
        }
    }
    let in_bible = |n: &str| {
        chars
            .iter()
            .any(|c| c.get("name").and_then(|x| x.as_str()) == Some(n))
    };
    let mut out: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
            plan.candidates
                .iter()
                .any(|(a, b)| a == "Huyền Vũ" && b == "Huyền Vũ Môn"),
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
        let cast = [
            "Huyền Vũ lão tổ",
            "Sở Cuồng Sư",
            "Mao Ý (thanh niên mặc hoa phục)",
            "Ngao Khánh",
            "Huyền Vũ",
            "Narrator",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();
        let folds = cast_only_folds(&bible, &cast);
        assert_eq!(folds.len(), 3, "{folds:?}");
        // New people and existing entries never fold.
        assert!(!folds
            .iter()
            .any(|(_, a)| a.contains(&"Ngao Khánh".to_string())));
    }
}
