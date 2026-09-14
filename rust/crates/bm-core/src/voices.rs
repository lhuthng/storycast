//! Voice rosters and the accent policy.
//!
//! These mirror the VieNeu preset store and the Gemini pools from the legacy
//! `synthesize.py`. The TTS sidecar is the authority at runtime — the inductor
//! asks it for `/policy` — but the workers need a usable fallback so a scheduled
//! render never depends on the sidecar being up just to decide who speaks.
//!
//! **The shipped roster states no preference.** It used to carry only the
//! Central/South presets, which made one operator's regional taste look like a
//! fact about the engine: the 13 Northern presets were excluded by *omission*,
//! which is harder to notice than an allow-list and harder to argue with. A
//! restriction is the operator's and lives in `.bm/voices.json`; see
//! `OperatorRoster`.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use bm_proto::VoiceInfo;

/// Every male VieNeu preset, in the store's declaration order.
///
/// Complete on purpose — see the module comment. Accents are per-voice and
/// exact (`Northern` / `Central` / `South`), taken from the SDK's own
/// `"<Giới tính> · <Vùng> · <Phong cách>"` labels rather than from a policy
/// guarantee, so an operator can exclude precisely one region.
pub const VIENEU_MALE: [&str; 12] = [
    "Minh Đức",
    "Phạm Tuyên",
    "Thái Sơn",
    "Xuân Vĩnh",
    "Thanh Bình",
    "Minh Triết",
    "Quang Sơn",
    "Đức Trí",
    "Adam",
    "Mạnh Dũng",
    "Minh Quân",
    "Anh Khôi",
];

/// Every female VieNeu preset, in the store's declaration order.
pub const VIENEU_FEMALE: [&str; 11] = [
    "Trúc Ly",
    "Ngọc Linh",
    "Đoan Trang",
    "Mai Anh",
    "Thục Đoan",
    "Thùy Dung",
    "Ngọc Trân",
    "Mỹ Duyên",
    "Quỳnh Anh",
    "Kim Thanh",
    "Ngọc Huyền",
];

/// Gemini prebuilt voices, gendered by ear — Google's labels ("firm",
/// "breezy") do not encode gender.
pub const GEMINI_MALE: [&str; 6] = ["Orus", "Charon", "Fenrir", "Algenib", "Gacrux", "Alnilam"];
pub const GEMINI_FEMALE: [&str; 7] = [
    "Vindemiatrix",
    "Leda",
    "Aoede",
    "Callirrhoe",
    "Despina",
    "Sulafat",
    "Achernar",
];
pub const GEMINI_NEUTRAL: [&str; 4] = ["Schedar", "Puck", "Erinome", "Rasalgethi"];

/// Female markers are checked *first*: "female" contains "male".
const FEMALE_HINTS: [&str; 12] = [
    "female", "nữ", "cô", "chị", "tỷ", "muội", "gái", "girl", "woman", "lady", "bà", "muội tử",
];
const MALE_HINTS: [&str; 10] = [
    "male", "nam", "ông", "anh", "trai", "đàn ông", "boy", "man", "lão", "adult male",
];

/// Everything the cast assigner needs to pick a voice for a new character.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoicePolicy {
    pub engine: String,
    pub male: Vec<String>,
    pub female: Vec<String>,
    pub neutral: Vec<String>,
    /// Seed assignments, as `(character, voice)` pairs.
    pub default_cast: Vec<(String, String)>,
    /// Hard allow-list. Empty means "no accent restriction" (Gemini).
    pub allowed: Vec<String>,
}

impl VoicePolicy {
    /// Infer gender from a bible `voice_hint`, female-first.
    pub fn pool_for_hint(&self, hint: &str) -> &[String] {
        let h = hint.to_lowercase();
        if FEMALE_HINTS.iter().any(|k| h.contains(k)) {
            &self.female
        } else if MALE_HINTS.iter().any(|k| h.contains(k)) {
            &self.male
        } else {
            &self.neutral
        }
    }

    /// Voices that a cast is permitted to use. `customs` are user-enrolled
    /// clones, which bypass the preset allow-list but never the accent policy
    /// (a clone is assumed vetted when it was enrolled).
    pub fn violations(&self, cast: &[(String, String)], customs: &[String]) -> Vec<(String, String)> {
        if self.allowed.is_empty() {
            return Vec::new();
        }
        cast.iter()
            .filter(|(_, v)| !self.allowed.iter().any(|a| a == v) && !customs.iter().any(|c| c == v))
            .cloned()
            .collect()
    }
}

/// The shipped VieNeu policy: every declared preset, no preference.
///
/// This file is public, so it encodes nobody's regional taste. An operator who
/// wants a restriction states it in `.bm/voices.json` (`excluded_accents`),
/// which is ignored — see `OperatorRoster`. An empty `allowed` means "no
/// restriction" at every site that reads it, so a clone with no local roster
/// offers the whole roster rather than silently inheriting a stranger's policy.
pub fn vieneu_policy() -> VoicePolicy {
    let male: Vec<String> = VIENEU_MALE.iter().map(|s| s.to_string()).collect();
    let female: Vec<String> = VIENEU_FEMALE.iter().map(|s| s.to_string()).collect();
    VoicePolicy {
        engine: "vieneu".into(),
        neutral: male.clone(), // legacy behaviour: unknown gender -> male pool
        male,
        female,
        allowed: Vec::new(), // no restriction — see the doc comment
        // The cast is the operator's, not the catalogue's: character names are
        // specific to one novel, and shipping them would put someone's book in
        // everyone's clone.
        default_cast: Vec::new(),
    }
}

/// The shipped Gemini policy: cloud prebuilt voices, no restriction, no cast.
pub fn gemini_policy() -> VoicePolicy {
    VoicePolicy {
        engine: "gemini".into(),
        male: GEMINI_MALE.iter().map(|s| s.to_string()).collect(),
        female: GEMINI_FEMALE.iter().map(|s| s.to_string()).collect(),
        neutral: GEMINI_NEUTRAL.iter().map(|s| s.to_string()).collect(),
        allowed: Vec::new(),
        default_cast: Vec::new(),
    }
}

pub fn policy_for(engine: &str) -> VoicePolicy {
    if engine == "vieneu" {
        vieneu_policy()
    } else {
        gemini_policy()
    }
}

// --- voice metadata --------------------------------------------------------
//
// The operator picks voices by ear and by description, so the picker needs
// more than a bare name. The SDK already labels every preset
// `"<Name> — <Giới tính> · <Vùng> · <Phong cách>"`; the sidecar's `/roster`
// parses that. This module carries the same shape for when no sidecar answers,
// because a scheduled render must not depend on the sidecar being up.

/// Per-voice accent/style, transcribed from the preset store's own labels.
///
/// `vieneu/assets/voices_v3_turbo.json` carries
/// `"<Giới tính> · <Vùng> · <Phong cách>"` for every preset, and this table is
/// that text, not a guess. It used to default the accent column to the *policy
/// guarantee* (`Central/South`) — which was true only because every preset in
/// the list was Central/South, and therefore said nothing about any one voice.
/// With the full roster declared, each entry states its real region.
///
/// `Xuân Vĩnh` is the one entry worth a caveat: the store labels it `Bắc` while
/// the model card and several forks call it Southern. It is recorded as the
/// store has it, and an operator who disagrees can say so in their own roster.
const PRESET_META: [(&str, &str, &str); 23] = [
    ("Minh Đức", "Northern", "tin tức"),
    ("Phạm Tuyên", "Northern", "tự nhiên"),
    ("Thái Sơn", "South", "kể chuyện"),
    ("Xuân Vĩnh", "Northern", "tự nhiên"),
    ("Thanh Bình", "Northern", "kể chuyện"),
    ("Trúc Ly", "Northern", "tự nhiên"),
    ("Ngọc Linh", "Northern", "kể chuyện"),
    ("Đoan Trang", "Northern", "tự nhiên"),
    ("Mai Anh", "Northern", "tin tức"),
    ("Thục Đoan", "South", "kể chuyện"),
    ("Minh Triết", "South", "tin tức"),
    ("Thùy Dung", "South", "tin tức"),
    ("Quang Sơn", "Central", "tự nhiên"),
    ("Ngọc Trân", "Central", "tự nhiên"),
    ("Mỹ Duyên", "South", "đọc truyện"),
    ("Quỳnh Anh", "Northern", "đọc truyện"),
    ("Đức Trí", "South", "đọc truyện"),
    ("Kim Thanh", "South", "đọc truyện"),
    ("Ngọc Huyền", "Northern", "tự nhiên"),
    ("Adam", "South", "tự nhiên"),
    ("Mạnh Dũng", "Northern", "tự nhiên"),
    ("Minh Quân", "Northern", "tự nhiên"),
    ("Anh Khôi", "Northern", "kể chuyện"),
];

/// The language the pipeline *speaks*. Every engine here is fed Vietnamese
/// novel text, so this is the content language, not the voice's full
/// multilingual range.
const CONTENT_LANGUAGE: &str = "vi-VN";

fn preset_meta(name: &str) -> Option<&'static (&'static str, &'static str, &'static str)> {
    PRESET_META.iter().find(|(n, _, _)| *n == name)
}

/// Gender from an SDK label field. Female is tested first: `"female"`
/// contains `"male"`.
///
/// The Vietnamese check is the diacritic form `nữ` only — matching bare `nu`
/// would make `"neutral"` report as female.
fn gender_of(field: &str) -> &'static str {
    let f = field.to_lowercase();
    if f.contains("female") || f.contains("nữ") {
        "female"
    } else if f.contains("male") || f.contains("nam") {
        "male"
    } else if f.contains("neutral") || f.contains("trung tính") {
        "neutral"
    } else {
        "unknown"
    }
}

/// Accent from an SDK label field.
///
/// Positional parsing is what makes this work at all: `"Nam"` means *male* in
/// the gender slot and *South* in the accent slot. This function only ever
/// sees the accent slot.
fn accent_of(field: &str) -> &'static str {
    let f = field.to_lowercase();
    if f.contains("bắc") || f.contains("bac") {
        "Northern"
    } else if f.contains("trung") {
        "Central"
    } else if f.contains("nam") {
        "South"
    } else {
        "unknown"
    }
}

/// Split `"Thái Sơn — Nam · Trung · Kể chuyện"` into its name and fields.
///
/// Enrolled clones carry a bare label with no separator, which is exactly how
/// they are distinguished from presets.
fn split_label(label: &str) -> (String, Vec<String>) {
    for sep in ['—', '–'] {
        if let Some((name, rest)) = label.split_once(sep) {
            let fields = rest
                .split('·')
                .map(|f| f.trim().to_string())
                .filter(|f| !f.is_empty())
                .collect();
            return (name.trim().to_string(), fields);
        }
    }
    // ASCII fallback for labels that use a plain hyphen surrounded by spaces.
    if let Some((name, rest)) = label.split_once(" - ") {
        let fields = rest
            .split('·')
            .map(|f| f.trim().to_string())
            .filter(|f| !f.is_empty())
            .collect();
        return (name.trim().to_string(), fields);
    }
    (label.trim().to_string(), Vec::new())
}

/// One voice from an SDK `(label, id)` pair.
///
/// A bare label (`label == id`) is an enrolled clone: no gender or accent is
/// claimed, and it is flagged so the picker can distinguish cast members the
/// operator added by hand from the shipped presets.
fn voice_from_label(label: &str, id: &str, allowed: &[String], engine: &str) -> VoiceInfo {
    let (name, fields) = split_label(label);
    let name = if name.is_empty() { id.to_string() } else { name };
    let enrolled = label == id;
    let gender = fields.first().map(|f| gender_of(f)).unwrap_or("unknown");
    let accent = fields
        .get(1)
        .map(|f| accent_of(f))
        .unwrap_or("unknown");
    let style = if fields.len() > 2 { fields[2..].join(" · ") } else { String::new() };
    // A preset's key comes from the catalogue. An enrolled clone has none until
    // `roster add` gives it one (stage 3), so the field stays empty rather than
    // inventing a slug that the next rename would silently invalidate.
    let key = key_for_name(engine, &name).unwrap_or_default();
    VoiceInfo {
        key,
        // `allowed.is_empty()` is "no restriction", the same reading
        // `VoicePolicy::violations` and `offline_voices` use. Omitting it here
        // would make the sidecar-backed roster reject every preset the moment
        // the policy became permissive, while the offline roster accepted them.
        allowed: enrolled || allowed.is_empty() || allowed.contains(&name),
        enrolled,
        name,
        gender: gender.to_string(),
        accent: accent.to_string(),
        language: CONTENT_LANGUAGE.to_string(),
        style,
    }
}

/// Turn the sidecar's `(label, id)` roster into voice infos.
///
/// `allowed` is passed in rather than resolved here, because the effective
/// policy depends on the operator's roster and this function has no path to it.
pub fn voices_from_labels(
    engine: &str,
    labels: &[(String, String)],
    allowed: &[String],
) -> Vec<VoiceInfo> {
    labels
        .iter()
        .map(|(label, id)| voice_from_label(label, id, allowed, engine))
        .collect()
}

/// The bundled roster: policy pools plus the metadata table above.
///
/// Used when the TTS sidecar is unreachable, so the picker still shows the
/// whole cast with whatever is known about each voice.
pub fn offline_voices(engine: &str) -> Vec<VoiceInfo> {
    let policy = policy_for(engine);
    let mut out: Vec<VoiceInfo> = Vec::new();
    for (pool, gender) in [
        (&policy.male, "male"),
        (&policy.female, "female"),
        (&policy.neutral, "neutral"),
    ] {
        for name in pool {
            if out.iter().any(|v| v.name == *name) {
                continue; // the neutral pool aliases the male pool for VieNeu
            }
            let (accent, style) = match preset_meta(name) {
                Some((_, accent, style)) => (*accent, *style),
                // Undeclared: say so rather than appealing to a policy
                // guarantee that no longer exists.
                None => ("unknown", ""),
            };
            out.push(VoiceInfo {
                key: key_for_name(engine, name).unwrap_or_default(),
                name: name.clone(),
                gender: gender.to_string(),
                accent: accent.to_string(),
                language: CONTENT_LANGUAGE.to_string(),
                style: style.to_string(),
                enrolled: false,
                allowed: policy.allowed.is_empty() || policy.allowed.iter().any(|a| a == name),
            });
        }
    }
    out
}

/// One line describing the active accent policy, for the picker header.
///
/// Derived from the roster rather than restated. This used to hardcode
/// "Central/South presets only (Northern excluded)" — the same regional
/// preference in a fourth place, phrased as though it were a property of the
/// engine rather than somebody's choice.
pub fn policy_note(roster: &EngineRoster) -> String {
    let pol = &roster.policy;
    if pol.allowed_accents.is_empty() && pol.excluded_accents.is_empty() {
        return format!(
            "accent policy: none — all {} declared presets are assignable",
            roster.voices.len()
        );
    }
    let allowed = roster
        .voices
        .iter()
        .filter(|v| roster.accent_allowed(&v.accent))
        .count();
    let mut parts: Vec<String> = Vec::new();
    if !pol.allowed_accents.is_empty() {
        parts.push(format!("only {}", pol.allowed_accents.join("/")));
    }
    if !pol.excluded_accents.is_empty() {
        parts.push(format!("excluding {}", pol.excluded_accents.join("/")));
    }
    format!(
        "accent policy: {} — {allowed} of {} declared presets assignable; enrolled clones always pass",
        parts.join(", "),
        roster.voices.len()
    )
}

/// Operator-enrolled clones, read from `voices.json` (`name -> refs/clip.wav`).
///
/// These are part of the cast whether or not the sidecar lists them, so the
/// picker shows them even when the sidecar is down. The `_note` key is
/// documentation, not a voice.
pub fn enrolled_voices(path: &std::path::Path) -> Vec<VoiceInfo> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    let Some(obj) = doc.as_object() else {
        return Vec::new();
    };
    obj.keys()
        .filter(|k| !k.starts_with('_'))
        .map(|name| VoiceInfo {
            // No key: `voices.json` names clones but does not key them. Stage 3
            // moves this file to `.bm/voices.json` with a declared key per clone.
            key: String::new(),
            name: name.clone(),
            gender: "unknown".into(),
            accent: "unknown".into(),
            language: CONTENT_LANGUAGE.into(),
            style: "enrolled clone".into(),
            enrolled: true,
            allowed: true, // vetted when it was enrolled
        })
        .collect()
}

// --- the committed catalogue ------------------------------------------------
//
// `voices.default.json` is the shipped roster: both engines, their metadata,
// their accent policy and their default cast, in one place.
//
// Stage 1 of `.docs/VOICE_CONFIG_PROPOSAL.md` adds it *alongside* the `const`
// tables above. Nothing in the render path reads it yet — the tests at the
// bottom of this file assert the two agree exactly. Stage 5 deletes the consts
// and makes this file load-bearing, at which point the duplication is gone;
// until then it is deliberate and checked rather than accidental and drifting.
//
// The machine-local delta over this catalogue (enabled flags, enrolled clones,
// policy overrides) lives in `.bm/voices.json`, which is ignored. The catalogue
// itself is committed because the offline guarantee above is load-bearing: a
// fresh clone with no local config must still be able to render.

/// The committed catalogue, embedded at compile time.
///
/// Embedded rather than read at runtime on purpose — the file cannot go
/// missing, move, or be half-edited at the moment a render needs it. Three hops
/// up from the manifest directory: `bm-core` -> `crates` -> `rust` -> root.
pub const CATALOGUE_JSON: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../voices.default.json"
));

/// `voices.default.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RosterFile {
    /// Schema version. Bumped when a field's *meaning* changes, not when a
    /// voice is added.
    pub version: u32,
    pub engines: BTreeMap<String, EngineRoster>,
}

/// One engine's slice of the catalogue.
///
/// `Default` is the "engine not declared" answer: no pools, no policy, no cast.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EngineRoster {
    /// Human-readable description, for the picker header.
    #[serde(default)]
    pub label: String,
    /// What this engine's audio comes back as, before any resampling.
    #[serde(default)]
    pub sample_rate: u32,
    #[serde(default)]
    pub policy: EnginePolicy,
    /// Declaration order is meaningful: the offline roster groups by gender in
    /// this order, so male voices come first, then female, then neutral.
    #[serde(default)]
    pub voices: Vec<RosterVoice>,
    /// character -> voice `key`. Keys, not display names, so renaming a voice
    /// is a presentation change that touches nothing else.
    #[serde(default)]
    pub default_cast: BTreeMap<String, String>,
}

/// The declared accent policy for one engine.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EnginePolicy {
    /// Accents a voice may carry and still be assignable. Empty means "no
    /// restriction", matching the `VoicePolicy::allowed` semantics above.
    #[serde(default)]
    pub allowed_accents: Vec<String>,
    /// Accents that veto assignment even when `allowed_accents` admits them.
    ///
    /// This is the operator's half of the policy. The shipped catalogue leaves
    /// it empty on purpose: a regional preference is one person's, and the
    /// catalogue is public. A preset whose accent is `"Central/South"` matches
    /// either segment, so excluding `"Northern"` cannot accidentally catch it.
    #[serde(default)]
    pub excluded_accents: Vec<String>,
    /// One line for the picker header.
    #[serde(default)]
    pub note: String,
    /// Presets deliberately left out, recorded so the omission reads as a
    /// decision rather than an oversight. Not a filter: exclusion happens by
    /// absence from `voices` plus the accent rule, and a test asserts no
    /// excluded name is declared.
    #[serde(default)]
    pub excluded: Vec<String>,
}

/// One voice in the catalogue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RosterVoice {
    /// ASCII slug, `^[a-z0-9][a-z0-9-]*$`. This is the identity; `name` is
    /// display only. A slug also cannot contain `/` or `..`, which is what lets
    /// the audition cache be a path derived from it.
    pub key: String,
    pub name: String,
    /// `male` | `female` | `neutral`.
    #[serde(default)]
    pub gender: String,
    /// `Northern` | `Central` | `South` | `Central/South` | `unknown`.
    #[serde(default)]
    pub accent: String,
    /// Free text from the SDK label, e.g. `kể chuyện`.
    #[serde(default)]
    pub style: String,
}

impl RosterFile {
    /// Parse a catalogue from JSON. The primitive the embedded copy uses, and
    /// what tooling calls on the file at `Layout::roster_default()`.
    pub fn parse(json: &str) -> serde_json::Result<Self> {
        serde_json::from_str(json)
    }

    pub fn engine(&self, engine: &str) -> Option<&EngineRoster> {
        self.engines.get(engine)
    }

    /// The embedded catalogue.
    ///
    /// Panics only if the committed file is malformed, which the catalogue
    /// tests in this module make impossible to merge.
    pub fn catalogue() -> &'static RosterFile {
        static CATALOGUE: OnceLock<RosterFile> = OnceLock::new();
        CATALOGUE.get_or_init(|| {
            RosterFile::parse(CATALOGUE_JSON)
                .expect("voices.default.json is embedded and must parse")
        })
    }
}

impl EngineRoster {
    /// Voices of one gender, in catalogue order.
    fn pool(&self, gender: &str) -> Vec<String> {
        self.voices
            .iter()
            .filter(|v| v.gender == gender)
            .map(|v| v.name.clone())
            .collect()
    }

    /// Whether a voice's accent satisfies the declared policy.
    ///
    /// Two rules, both derived from the accents rather than hand-listed, so
    /// adding a preset to `voices` cannot widen the policy by accident:
    ///
    /// * `allowed_accents` empty means "anything"; otherwise the voice must
    ///   match one of them. The SDK labels a preset with one combined region, so
    ///   `"Central/South"` is split on `/` and any matching segment admits it.
    /// * `excluded_accents` then vetoes, and the veto wins. The shipped
    ///   catalogue leaves this empty; it is the operator's lever.
    fn accent_allowed(&self, accent: &str) -> bool {
        let parts: Vec<&str> = accent.split('/').map(|p| p.trim()).collect();
        let matches = |list: &[String]| {
            parts
                .iter()
                .any(|part| list.iter().any(|a| a.eq_ignore_ascii_case(part)))
        };
        if matches(&self.policy.excluded_accents) {
            return false;
        }
        self.policy.allowed_accents.is_empty() || matches(&self.policy.allowed_accents)
    }

    /// The runtime policy, in the shape the rest of the crate already speaks.
    pub fn to_policy(&self, engine: &str) -> VoicePolicy {
        let male = self.pool("male");
        let female = self.pool("female");
        let mut neutral = self.pool("neutral");
        if neutral.is_empty() {
            // Kept deliberately: VieNeu declares no neutral preset, and an
            // unknown-gender character has always fallen back to the male pool.
            neutral = male.clone();
        }
        // Both lists empty is the shipped default: an empty `allowed` is "no
        // restriction", which also lets undeclared names (enrolled clones)
        // through. Listing every declared name instead would silently narrow it.
        let unrestricted =
            self.policy.allowed_accents.is_empty() && self.policy.excluded_accents.is_empty();
        let allowed = if unrestricted {
            Vec::new()
        } else {
            self.voices
                .iter()
                .filter(|v| self.accent_allowed(&v.accent))
                .map(|v| v.name.clone())
                .collect()
        };
        VoicePolicy {
            engine: engine.to_string(),
            male,
            female,
            neutral,
            allowed,
            default_cast: self.resolve_cast(),
        }
    }

    /// `character -> name`, resolving each cast key through the voice list.
    ///
    /// The catalogue stores keys; `VoicePolicy` still speaks display names until
    /// stage 2 lands. A key that resolves to nothing is dropped here, and the
    /// catalogue tests fail on it rather than letting a character reach a render
    /// with no voice.
    fn resolve_cast(&self) -> Vec<(String, String)> {
        self.default_cast
            .iter()
            .filter_map(|(character, key)| {
                self.voices
                    .iter()
                    .find(|v| &v.key == key)
                    .map(|v| (character.clone(), v.name.clone()))
            })
            .collect()
    }

    /// The offline roster, built from the catalogue instead of the consts.
    ///
    /// Same shape and same ordering as `offline_voices()`: male pool, female
    /// pool, then neutral, deduplicated — because the neutral pool aliases the
    /// male pool for VieNeu.
    pub fn to_offline_voices(&self, engine: &str) -> Vec<VoiceInfo> {
        let policy = self.to_policy(engine);
        let mut out: Vec<VoiceInfo> = Vec::new();
        for (pool, gender) in [
            (&policy.male, "male"),
            (&policy.female, "female"),
            (&policy.neutral, "neutral"),
        ] {
            for name in pool {
                if out.iter().any(|v| v.name == *name) {
                    continue; // the neutral pool aliases the male pool for VieNeu
                }
                let declared = self.voices.iter().find(|v| &v.name == name);
                let (accent, style) = match declared {
                    Some(v) => (v.accent.clone(), v.style.clone()),
                    // Undeclared: "unknown", not a policy guarantee — there is
                    // no longer a policy to guarantee anything.
                    None => ("unknown".to_string(), String::new()),
                };
                out.push(VoiceInfo {
                    key: declared.map(|v| v.key.clone()).unwrap_or_default(),
                    name: name.clone(),
                    gender: gender.to_string(),
                    accent,
                    language: CONTENT_LANGUAGE.to_string(),
                    style,
                    enrolled: false,
                    allowed: policy.allowed.is_empty() || policy.allowed.iter().any(|a| a == name),
                });
            }
        }
        out
    }
}

// --- the operator's roster --------------------------------------------------
//
// `.bm/voices.json` is the machine-local half of the roster: the operator's own
// accent policy and their cast. Both are personal, so both are gitignored — the
// shipped catalogue is public and states no preference at all.
//
// A missing file is a valid state, not an error: a fresh clone has no opinion,
// and everything that is not personal is already in the catalogue.

/// The operator's local roster, read from `Layout::roster()`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OperatorRoster {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub engines: BTreeMap<String, OperatorEngine>,
}

/// One engine's slice of the operator's roster.
///
/// Every field is an *override*: left empty, it inherits the catalogue. Empty is
/// also what "no restriction" looks like, so the two readings agree.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OperatorEngine {
    #[serde(default)]
    pub policy: OperatorPolicy,
    /// character -> voice. A `key` is written and preferred; a display name
    /// resolves too, so the file can be edited by hand.
    #[serde(default)]
    pub default_cast: BTreeMap<String, String>,
}

/// The operator's accent policy. This is where a regional preference lives —
/// e.g. `"excluded_accents": ["Northern"]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OperatorPolicy {
    #[serde(default)]
    pub allowed_accents: Vec<String>,
    #[serde(default)]
    pub excluded_accents: Vec<String>,
    #[serde(default)]
    pub note: String,
}

impl OperatorRoster {
    /// Read the roster. A missing file yields the empty roster — the
    /// fresh-clone case.
    ///
    /// A *malformed* file is an error rather than a silent fallback. Falling
    /// back to the catalogue would quietly re-admit the very voices the
    /// operator excluded, and a policy that fails open is worse than one that
    /// refuses to load.
    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| format!("parsing {}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("reading {}: {e}", path.display())),
        }
    }

    /// The operator's slice for one engine, if they have one.
    pub fn engine(&self, engine: &str) -> Option<&OperatorEngine> {
        self.engines.get(engine)
    }

    /// Apply this roster over one engine of the catalogue.
    ///
    /// A field the operator leaves empty inherits the catalogue; a field they
    /// fill replaces it. The cast is replaced outright when declared, because a
    /// cast is a set — merging two would invent characters.
    pub fn apply(&self, engine: &str, base: &EngineRoster) -> EngineRoster {
        let mut out = base.clone();
        let Some(op) = self.engine(engine) else {
            return out;
        };
        if !op.policy.allowed_accents.is_empty() {
            out.policy.allowed_accents = op.policy.allowed_accents.clone();
        }
        if !op.policy.excluded_accents.is_empty() {
            out.policy.excluded_accents = op.policy.excluded_accents.clone();
        }
        if !op.policy.note.is_empty() {
            out.policy.note = op.policy.note.clone();
        }
        if !op.default_cast.is_empty() {
            out.default_cast = op.default_cast.clone();
        }
        out
    }
}

/// One engine of the catalogue with the operator's roster applied.
///
/// The single entry point for "what does this machine actually allow", so no
/// caller has to remember to merge. `roster_path` is `Layout::roster()`.
pub fn effective_engine(roster_path: &std::path::Path, engine: &str) -> Result<EngineRoster, String> {
    let base = RosterFile::catalogue()
        .engine(engine)
        .cloned()
        .unwrap_or_default();
    Ok(OperatorRoster::load(roster_path)?.apply(engine, &base))
}

/// The effective `VoicePolicy` for `engine` on this machine.
pub fn effective_policy(
    roster_path: &std::path::Path,
    engine: &str,
) -> Result<VoicePolicy, String> {
    Ok(effective_engine(roster_path, engine)?.to_policy(engine))
}

/// The effective offline roster for `engine` on this machine.
pub fn effective_offline_voices(
    roster_path: &std::path::Path,
    engine: &str,
) -> Result<Vec<VoiceInfo>, String> {
    Ok(effective_engine(roster_path, engine)?.to_offline_voices(engine))
}

/// [`effective_engine`], but never fails: a malformed roster yields the
/// catalogue *and* the parse error.
///
/// The error must be shown, not dropped. Falling back silently would re-admit
/// every voice the operator excluded, which is the one failure mode worth being
/// loud about — the whole reason `effective_engine` returns a `Result`.
pub fn effective_engine_lenient(
    roster_path: &std::path::Path,
    engine: &str,
) -> (EngineRoster, Option<String>) {
    match effective_engine(roster_path, engine) {
        Ok(r) => (r, None),
        Err(e) => (
            RosterFile::catalogue()
                .engine(engine)
                .cloned()
                .unwrap_or_default(),
            Some(e),
        ),
    }
}

// --- key <-> name resolution ------------------------------------------------
//
// `key` is identity and `name` is presentation, so anything *persisted* — the
// cast file above all — stores keys and resolves them back to names at the
// boundary. These three functions are that boundary.

/// The catalogue key for a voice's display name.
pub fn key_for_name(engine: &str, name: &str) -> Option<String> {
    RosterFile::catalogue()
        .engine(engine)?
        .voices
        .iter()
        .find(|v| v.name == name)
        .map(|v| v.key.clone())
}

/// The display name a catalogue key refers to.
pub fn name_for_key(engine: &str, key: &str) -> Option<String> {
    RosterFile::catalogue()
        .engine(engine)?
        .voices
        .iter()
        .find(|v| v.key == key)
        .map(|v| v.name.clone())
}

/// The display name for a cast value that may be a **key** or a **name**.
///
/// Keys are tried first, because that is what the cast file migrates to and the
/// form that survives a voice being renamed. A name still resolves, and that is
/// what makes the migration optional rather than a gate: a fully migrated cast
/// file, a half-migrated one and an untouched one all render.
///
/// A value matching neither comes back unchanged, deliberately. An unknown voice
/// has to stay visible so the cast overview can flag it (`unknown voice — stale
/// cast?`); quietly substituting a valid voice would hide a real problem behind
/// a plausible-sounding render.
pub fn resolve_voice_name(engine: &str, value: &str) -> String {
    match name_for_key(engine, value) {
        Some(name) => name,
        None => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn female_hints_win_over_the_male_substring() {
        let p = vieneu_policy();
        // "female" contains "male" — the female branch must be tested first.
        assert_eq!(p.pool_for_hint("adult female, cold"), &p.female);
        assert_eq!(p.pool_for_hint("adult male, stern"), &p.male);
        assert_eq!(p.pool_for_hint("giọng nữ trẻ"), &p.female);
        assert_eq!(p.pool_for_hint(""), &p.neutral);
    }

    #[test]
    fn the_shipped_policy_restricts_nothing_and_casts_nobody() {
        // The catalogue is public. It used to declare only the Central/South
        // presets and allow-list exactly those, which made one operator's
        // regional taste look like a fact about the engine. A fresh clone must
        // offer the whole roster, and must not arrive pre-cast with a stranger's
        // characters.
        for p in [vieneu_policy(), gemini_policy()] {
            assert!(
                p.allowed.is_empty(),
                "{}: empty means no restriction",
                p.engine
            );
            assert!(
                p.default_cast.is_empty(),
                "{}: the cast is the operator's, not the catalogue's",
                p.engine
            );
        }
    }

    #[test]
    fn violations_catch_a_non_policy_voice_but_allow_enrolled_clones() {
        // A restriction has to come from somewhere now, so state one.
        let mut p = vieneu_policy();
        p.allowed = vec!["Đức Trí".to_string()];
        let bad = vec![("A".to_string(), "Bắc Giang".to_string())];
        assert_eq!(p.violations(&bad, &[]).len(), 1);
        let custom = vec![("A".to_string(), "Suneo".to_string())];
        assert!(p.violations(&custom, &["Suneo".to_string()]).is_empty());
    }

    #[test]
    fn gemini_policy_has_no_accent_restriction() {
        let p = gemini_policy();
        assert!(p.allowed.is_empty());
        assert!(p.violations(&[("A".into(), "Anything".into())], &[]).is_empty());
    }

    #[test]
    fn label_fields_are_positional_so_nam_is_male_then_south() {
        // "Nam" appears twice with two different meanings; only position
        // disambiguates them.
        let v = voice_from_label(
            "Thái Sơn — Nam · Nam · Kể chuyện",
            "thai_son",
            &vieneu_policy().allowed,
            "vieneu",
        );
        assert_eq!(v.name, "Thái Sơn");
        assert_eq!(v.gender, "male");
        assert_eq!(v.accent, "South");
        assert_eq!(v.style, "Kể chuyện");
        assert!(!v.enrolled);
        assert!(v.allowed, "Thái Sơn is a Central/South preset");
        assert_eq!(v.language, "vi-VN");
    }

    #[test]
    fn female_beats_the_male_substring_in_a_label_field() {
        let v = voice_from_label(
            "Thục Đoan — Nữ · Trung · kể chuyện",
            "thuc_doan",
            &vieneu_policy().allowed,
            "vieneu",
        );
        assert_eq!(v.gender, "female");
        assert_eq!(v.accent, "Central");
    }

    #[test]
    fn a_bare_label_is_an_enrolled_clone_that_passes_the_policy() {
        let v = voice_from_label("Suneo", "Suneo", &vieneu_policy().allowed, "vieneu");
        assert_eq!(v.name, "Suneo");
        assert!(v.enrolled);
        assert!(v.allowed, "clones are vetted at enrolment");
        assert_eq!(v.gender, "unknown");
    }

    #[test]
    fn excluding_an_accent_is_what_makes_a_northern_preset_unassignable() {
        // The shipped policy admits every preset; the exclusion is the
        // operator's. This is the path that turns
        // `"excluded_accents": ["Northern"]` into an unassignable voice.
        let mut roster = RosterFile::catalogue()
            .engine("vieneu")
            .expect("vieneu is declared")
            .clone();
        roster.policy.excluded_accents = vec!["Northern".to_string()];

        let p = roster.to_policy("vieneu");
        assert!(!p.allowed.contains(&"Minh Đức".to_string()), "Northern, excluded");
        assert!(p.allowed.contains(&"Đức Trí".to_string()), "South, kept");
        assert!(p.allowed.contains(&"Quang Sơn".to_string()), "Central, kept");
        assert_eq!(p.allowed.len(), 10, "23 declared minus the 13 Northern");

        // The same rule, applied to a live sidecar label.
        let v = voice_from_label(
            "Xuân Vĩnh — Nam · Bắc · đọc truyện",
            "xuan_vinh",
            &p.allowed,
            "vieneu",
        );
        assert_eq!(v.accent, "Northern");
        assert!(!v.allowed, "the operator's roster excludes Northern");
    }

    #[test]
    fn offline_roster_lists_every_declared_preset_exactly_once() {
        let v = offline_voices("vieneu");
        // The neutral pool aliases the male pool for VieNeu: no duplicates.
        assert_eq!(v.len(), 23, "{:?}", v.iter().map(|x| &x.name).collect::<Vec<_>>());
        assert!(
            v.iter().all(|x| x.allowed),
            "the shipped policy admits everything"
        );
        assert_eq!(v.iter().filter(|x| x.gender == "male").count(), 12);
        assert_eq!(v.iter().filter(|x| x.gender == "female").count(), 11);
        // Accents are per-voice and real, not a policy guarantee.
        assert_eq!(v.iter().find(|x| x.name == "Quang Sơn").unwrap().accent, "Central");
        assert_eq!(v.iter().find(|x| x.name == "Minh Đức").unwrap().accent, "Northern");
        assert_eq!(v.iter().find(|x| x.name == "Adam").unwrap().accent, "South");
        assert_eq!(v.iter().find(|x| x.name == "Adam").unwrap().style, "tự nhiên");
    }

    #[test]
    fn neutral_is_not_misread_as_female() {
        // "neutral" contains "nu"; only the diacritic form may mean female.
        let v = voice_from_label(
            "Puck — Neutral · unknown · breezy",
            "puck",
            &gemini_policy().allowed,
            "gemini",
        );
        assert_eq!(v.gender, "neutral");
    }

    #[test]
    fn offline_roster_for_gemini_claims_no_accent_restriction() {
        let v = offline_voices("gemini");
        assert!(v.iter().all(|x| x.accent == "unknown"));
        assert!(v.iter().all(|x| x.allowed), "gemini has no allow-list");
        assert!(policy_note(catalogue_engine("gemini")).contains("none"));
        assert!(policy_note(catalogue_engine("vieneu")).contains("none"));
    }

    // --- stage 1: the catalogue must agree with the consts ------------------
    //
    // These are the whole point of stage 1. `voices.default.json` is a
    // transcription of the consts above, nothing reads it yet, and these tests
    // are what prove the transcription faithful *before* stage 5 deletes the
    // consts. A failure here means the file and the code have drifted, and
    // stage 5 would silently change which voice speaks.

    fn catalogue_engine(engine: &str) -> &'static EngineRoster {
        RosterFile::catalogue()
            .engine(engine)
            .unwrap_or_else(|| panic!("voices.default.json declares no `{engine}` engine"))
    }

    #[test]
    fn the_embedded_catalogue_parses_and_declares_both_engines() {
        assert_eq!(RosterFile::catalogue().version, 1);
        // 23 = every VieNeu preset the SDK store ships, Northern ones included.
        // The count is asserted rather than the names because the *completeness*
        // is the point: the catalogue used to declare only the 10 Central/South
        // presets, which excluded the other 13 by omission.
        assert_eq!(catalogue_engine("vieneu").voices.len(), 23);
        assert_eq!(catalogue_engine("gemini").voices.len(), 17);
        // Sanity: the embedded copy really is the file on disk.
        assert!(CATALOGUE_JSON.contains("\"vieneu\""));
        assert!(CATALOGUE_JSON.contains("\"gemini\""));
    }

    #[test]
    fn catalogue_keys_are_slugs_that_cannot_escape_a_path() {
        // The audition cache is `.bm/voices/samples/<key>.wav`, so a key
        // containing `/` or `..` would be a traversal.
        for (engine, roster) in &RosterFile::catalogue().engines {
            let mut seen = std::collections::HashSet::new();
            for v in &roster.voices {
                assert!(seen.insert(&v.key), "{engine}: duplicate key {}", v.key);
                let slug = !v.key.is_empty()
                    && v.key.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
                    && v.key
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
                assert!(slug, "{engine}: `{}` is not an ASCII slug", v.key);
            }
        }
    }

    #[test]
    fn catalogue_policy_matches_the_consts_field_for_field() {
        for (engine, expected) in [("vieneu", vieneu_policy()), ("gemini", gemini_policy())] {
            let got = catalogue_engine(engine).to_policy(engine);
            assert_eq!(got.engine, expected.engine, "{engine}: engine tag");
            assert_eq!(got.male, expected.male, "{engine}: male pool");
            assert_eq!(got.female, expected.female, "{engine}: female pool");
            assert_eq!(got.neutral, expected.neutral, "{engine}: neutral pool");
            assert_eq!(got.allowed, expected.allowed, "{engine}: allow-list");
            // `default_cast` is order-insensitive at every call site: `cast.rs`
            // collects it into a map, `state.rs` into a BTreeSet. Compare the
            // assignments, not the iteration order.
            let as_map = |v: Vec<(String, String)>| v.into_iter().collect::<BTreeMap<_, _>>();
            assert_eq!(
                as_map(got.default_cast),
                as_map(expected.default_cast),
                "{engine}: default cast"
            );
        }
    }

    #[test]
    fn catalogue_offline_roster_matches_the_consts_voice_for_voice() {
        for engine in ["vieneu", "gemini"] {
            assert_eq!(
                catalogue_engine(engine).to_offline_voices(engine),
                offline_voices(engine),
                "{engine}: offline roster differs from the const tables"
            );
        }
    }

    #[test]
    fn the_accent_rule_is_what_produces_the_allow_list() {
        // Guards the inference stage 5 will depend on: `allowed` is *derived*
        // from the accents, so neither adding a preset to the catalogue nor
        // adding an exclusion to the operator's roster can be undone by editing
        // the other list.
        let base = catalogue_engine("vieneu");

        // The shipped catalogue restricts nothing at all.
        assert!(base.accent_allowed("Northern"));
        assert!(base.accent_allowed("Central"));
        assert!(base.to_policy("vieneu").allowed.is_empty(), "no restriction");

        // An allow-list admits by accent, and `"Central/South"` matches either
        // half of the combined region the SDK labels.
        let mut allowed_only = base.clone();
        allowed_only.policy.allowed_accents = vec!["Central".into(), "South".into()];
        assert!(allowed_only.accent_allowed("Central/South"));
        assert!(!allowed_only.accent_allowed("Northern"));

        // The veto wins over the allow-list; an empty allow-list is not a veto.
        let mut vetoed = base.clone();
        vetoed.policy.excluded_accents = vec!["Northern".into()];
        assert!(!vetoed.accent_allowed("Northern"));
        assert!(vetoed.accent_allowed("Central/South"));
        assert_eq!(vetoed.to_policy("vieneu").allowed.len(), 10);

        // Empty `allowed_accents` means no restriction, not "nothing allowed".
        assert!(catalogue_engine("gemini")
            .to_policy("gemini")
            .allowed
            .is_empty());
    }

    #[test]
    fn every_default_cast_key_resolves_within_its_own_engine() {
        for (engine, roster) in &RosterFile::catalogue().engines {
            assert_eq!(
                roster.resolve_cast().len(),
                roster.default_cast.len(),
                "{engine}: a default_cast key resolves to no declared voice"
            );
        }
    }

    #[test]
    fn no_excluded_preset_is_declared() {
        // `excluded` records a deliberate omission. If the name ever came back
        // into `voices`, the accent rule — not the list — would decide, and the
        // recorded note would be lying about why.
        for (engine, roster) in &RosterFile::catalogue().engines {
            for name in &roster.policy.excluded {
                assert!(
                    !roster.voices.iter().any(|v| &v.name == name),
                    "{engine}: `{name}` is listed as excluded but is declared"
                );
            }
        }
    }

    // --- stage 2: key <-> name resolution -----------------------------------

    #[test]
    fn keys_resolve_to_names_and_names_resolve_to_keys() {
        assert_eq!(key_for_name("vieneu", "Đức Trí").as_deref(), Some("duc-tri"));
        assert_eq!(name_for_key("vieneu", "duc-tri").as_deref(), Some("Đức Trí"));
        // A name the catalogue does not declare has no key — that is a clone.
        assert_eq!(key_for_name("vieneu", "Suneo"), None);
        assert_eq!(name_for_key("vieneu", "suneo"), None);
        // Both forms resolve to the same canonical display name, which is what
        // makes the cast migration optional rather than a gate.
        assert_eq!(resolve_voice_name("vieneu", "duc-tri"), "Đức Trí");
        assert_eq!(resolve_voice_name("vieneu", "Đức Trí"), "Đức Trí");
        // An unknown value is left exactly as it was, so the cast overview can
        // flag it instead of the pipeline quietly reassigning the speaker.
        assert_eq!(resolve_voice_name("vieneu", "Đã Biến Mất"), "Đã Biến Mất");
    }

    #[test]
    fn keys_are_scoped_to_their_own_engine() {
        // `charon` is a Gemini key; a VieNeu cast must not resolve it to a voice.
        assert_eq!(name_for_key("gemini", "charon").as_deref(), Some("Charon"));
        assert_eq!(name_for_key("vieneu", "charon"), None);
        assert_eq!(resolve_voice_name("vieneu", "charon"), "charon");
    }

    #[test]
    fn catalogue_voices_carry_their_key_and_undeclared_ones_carry_none() {
        let v = offline_voices("vieneu");
        assert!(v.iter().all(|x| !x.key.is_empty()), "every preset is catalogued");
        assert_eq!(
            v.iter().find(|x| x.name == "Đức Trí").unwrap().key,
            "duc-tri"
        );
        // A label the catalogue does not declare (an enrolled clone) gets no key
        // rather than a derived slug that the next rename would invalidate.
        let clone = voice_from_label("Suneo", "Suneo", &vieneu_policy().allowed, "vieneu");
        assert_eq!(clone.key, "");
        assert!(clone.enrolled);
    }

    #[test]
    fn both_engines_key_every_voice_they_declare() {
        for engine in ["vieneu", "gemini"] {
            for v in catalogue_engine(engine).to_offline_voices(engine) {
                assert!(!v.key.is_empty(), "{}: {} has no key", engine, v.name);
                // The key must round-trip, or a cast lookup would miss.
                assert_eq!(
                    name_for_key(engine, &v.key).as_deref(),
                    Some(v.name.as_str()),
                    "{engine}: {} does not round-trip",
                    v.key
                );
            }
        }
    }

    // --- the operator's roster ----------------------------------------------

    fn op_roster(json: &str) -> OperatorRoster {
        serde_json::from_str(json).expect("test roster parses")
    }

    #[test]
    fn an_absent_roster_leaves_the_catalogue_exactly_as_it_was() {
        let base = catalogue_engine("vieneu").clone();
        let merged = OperatorRoster::default().apply("vieneu", &base);
        assert!(merged.policy.excluded_accents.is_empty());
        assert!(
            merged.to_policy("vieneu").allowed.is_empty(),
            "still unrestricted"
        );
        assert!(merged.default_cast.is_empty());
    }

    #[test]
    fn a_missing_roster_file_is_fine_but_a_malformed_one_is_not() {
        let d = std::env::temp_dir().join("bm-operator-roster");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();

        // The fresh-clone case: no file, no opinion, no error.
        assert!(OperatorRoster::load(&d.join("voices.json")).is_ok());

        // A broken file must not be waved through. Falling back to the catalogue
        // would re-admit every voice the operator excluded, and the operator
        // would never find out.
        std::fs::write(d.join("voices.json"), "{ this is not json").unwrap();
        let e = OperatorRoster::load(&d.join("voices.json")).unwrap_err();
        assert!(e.contains("parsing"), "{e}");
    }

    #[test]
    fn the_operator_roster_narrows_the_policy_and_supplies_the_cast() {
        let base = catalogue_engine("vieneu").clone();
        let op = op_roster(
            r#"{"version":1,"engines":{"vieneu":{
                 "policy":{"excluded_accents":["Northern"]},
                 "default_cast":{"Narrator":"duc-tri","Villain":"minh-duc"}}}}"#,
        );
        let p = op.apply("vieneu", &base).to_policy("vieneu");

        assert_eq!(p.allowed.len(), 10, "23 declared minus the 13 Northern");
        assert!(!p.allowed.contains(&"Minh Đức".to_string()));
        assert!(p.allowed.contains(&"Đức Trí".to_string()));
        // The cast arrives as keys and leaves as display names.
        assert_eq!(
            p.default_cast,
            vec![
                ("Narrator".to_string(), "Đức Trí".to_string()),
                ("Villain".to_string(), "Minh Đức".to_string()),
            ]
        );
    }

    #[test]
    fn a_field_the_operator_omits_inherits_the_catalogue() {
        // Only a cast, no policy: the shipped (unrestricted) policy survives.
        let base = catalogue_engine("vieneu").clone();
        let op = op_roster(r#"{"engines":{"vieneu":{"default_cast":{"Narrator":"duc-tri"}}}}"#);
        let p = op.apply("vieneu", &base).to_policy("vieneu");
        assert!(p.allowed.is_empty(), "the catalogue's policy is untouched");
        assert_eq!(p.default_cast.len(), 1);
    }

    #[test]
    fn the_lenient_loader_reports_the_error_it_falls_back_on() {
        let d = std::env::temp_dir().join("bm-operator-roster-lenient");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("voices.json"), "{ nope").unwrap();

        let (engine, err) = effective_engine_lenient(&d.join("voices.json"), "vieneu");
        assert!(err.is_some(), "the caller must be able to show the failure");
        // It still yields a usable roster — the catalogue — so the picker works.
        assert_eq!(engine.voices.len(), 23);
    }
}
