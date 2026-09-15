use super::plan::seg_text;
use serde_json::Value;

/// Mood -> (temperature, silence_p). Calm reads steady; hot moods swing wider
/// and pause harder. Retune here — filenames do not depend on these values.
pub const MOOD_TAKE: [(&str, f64, f64); 18] = [
    ("neutral", 0.80, 0.15),
    ("calm", 0.72, 0.12),
    ("reflective", 0.75, 0.18),
    ("grand", 0.85, 0.20),
    ("sarcastic", 0.85, 0.12),
    ("ironic", 0.85, 0.12),
    ("amused", 0.90, 0.12),
    ("excited", 0.92, 0.10),
    ("smug", 0.88, 0.12),
    ("happy", 0.90, 0.10),
    ("sad", 0.85, 0.22),
    ("angry", 0.90, 0.18),
    ("cold", 0.70, 0.15),
    ("stern", 0.72, 0.18),
    ("urgent", 0.92, 0.08),
    ("surprised", 0.90, 0.10),
    ("shocked", 0.90, 0.20),
    ("gossipy", 0.88, 0.10),
];

/// Normalize free-form digest moods onto the small acting vocabulary.
pub fn mood_cluster(mood: &str) -> String {
    let first = mood
        .split_whitespace()
        .next()
        .unwrap_or("neutral")
        .to_lowercase();
    let first = first.trim_matches(|c| c == ',' || c == '.').to_string();
    let first = if first.is_empty() {
        "neutral".to_string()
    } else {
        first
    };
    match first.as_str() {
        "flat" | "monotone" | "expository" | "narrative" | "matter-of-fact" | "mildly"
        | "observant" | "descriptive" | "indifferent" | "awkward" | "pragmatic" | "casual" => {
            "neutral".into()
        }
        "serene" | "unconcerned" | "reflective" => "calm".into(),
        "ironic" | "sarcastic" => "amused".into(),
        "helpless" | "resigned" | "subdued" | "pouting" => "sad".into(),
        "amazed" | "earnest" | "playful" | "pleading" | "happy" | "warm" | "grand" | "admiring" => {
            "excited".into()
        }
        "satisfied" | "self-satisfied" => "smug".into(),
        "hasty" => "urgent".into(),
        "aloof" | "arrogant" | "stern" | "strict" => "cold".into(),
        "annoyed" => "angry".into(),
        other => other.to_string(),
    }
}

pub fn take_for_mood(cluster: &str) -> (f64, f64) {
    MOOD_TAKE
        .iter()
        .find(|(m, _, _)| *m == cluster)
        .map(|(_, t, s)| (*t, *s))
        .unwrap_or((0.80, 0.15))
}

/// Hottest mood in the run wins — one expressive line should lift the whole breath.
/// No neutral floor: a uniformly calm run reads calm, not neutral.
pub fn mood_take(segments: &[Value], idx: &[usize]) -> (f64, f64) {
    let mut best: Option<(f64, f64)> = None;
    for i in idx {
        let mood = segments[*i].get("mood").and_then(|m| m.as_str()).unwrap_or("neutral");
        let cand = take_for_mood(&mood_cluster(mood));
        if best.is_none_or(|(t, _)| cand.0 > t) {
            best = Some(cand);
        }
    }
    best.unwrap_or((0.80, 0.15))
}

/// Plain joined text for local engines (the acting style is baked into the
/// voice, not the prompt).
pub fn run_text(segments: &[Value], idx: &[usize]) -> String {
    idx.iter()
        .map(|i| seg_text(&segments[*i]))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mood_cluster_normalizes_and_the_hottest_line_wins() {
        assert_eq!(mood_cluster("lazy calm"), "lazy");
        assert_eq!(mood_cluster("sarcastic"), "amused");
        assert_eq!(mood_cluster("descriptive, flat"), "neutral");
        assert_eq!(mood_cluster(""), "neutral");

        let segs = vec![
            json!({"speaker": "A", "text": "x", "mood": "calm"}),
            json!({"speaker": "A", "text": "y", "mood": "urgent"}),
        ];
        let (t, _s) = mood_take(&segs, &[0, 1]);
        assert_eq!(t, take_for_mood("urgent").0, "hottest mood must win");
    }
}
