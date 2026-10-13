//! The recorded render plan: the single namer for a chapter's audio.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Bump when an input stops being included in [`take_key`] or the file-naming
pub const PLAN_VERSION: u32 = 2;

/// The extensions a stored take may carry — the one list the namer, the store's
pub const TAKE_EXTENSIONS: [&str; 2] = ["wav", "mp3"];

/// How a take is stored, from `settings.take_quality`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TakeQuality {
    /// The sidecar's own PCM wav, untouched.
    Raw,
    /// 64k mono mp3 — the published output's own rate.
    Small,
    /// 96k mono mp3 — the default. A casual listener cannot hear it against
    Balanced,
    /// 128k mono mp3, for an operator who re-masters from takes.
    High,
}

impl TakeQuality {
    /// Parse the setting. An unknown name takes the default rather than
    pub fn parse(setting: &str) -> Self {
        match setting.trim().to_ascii_lowercase().as_str() {
            "raw" => Self::Raw,
            "small" => Self::Small,
            "high" => Self::High,
            _ => Self::Balanced,
        }
    }

    /// The stored file's extension, and so the tier's half of the take name.
    pub fn extension(&self) -> &'static str {
        match self {
            Self::Raw => "wav",
            Self::Small | Self::Balanced | Self::High => "mp3",
        }
    }

    /// The mp3 bitrate in kbps, or `None` for [`TakeQuality::Raw`]. This is
    pub fn mp3_kbps(&self) -> Option<u32> {
        match self {
            Self::Raw => None,
            Self::Small => Some(64),
            Self::Balanced => Some(96),
            Self::High => Some(128),
        }
    }
}

/// Hex characters kept from the SHA-256 take hash. Eight bytes is far past
const TAKE_KEY_CHARS: usize = 16;

/// The size below which a wav is a half-write, not a take. The same threshold
const MIN_TAKE_BYTES: u64 = 1000;

/// One unit of render work, fully specified and durably named.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Take {
    /// Position in the mix. [`RenderPlan::takes`] is already in order; this is
    pub pos: usize,
    /// Human label — `title`, `0004-0011`. Display and progress only.
    pub tag: String,
    pub speaker: String,
    /// The voice as the pipeline speaks it (a display name).
    pub voice: String,
    /// The catalogue key the `take_key` hashes, so renaming a voice does not
    #[serde(default)]
    pub voice_key: String,
    pub text: String,
    pub temperature: f64,
    pub silence_p: f64,
    /// Hash of the inputs that decide the audio. The identity that survives a
    pub take_key: String,
    /// The name in the store. Content-addressed for a new take; a legacy
    pub file: String,
    /// The legacy name this take would have had. Kept so a first plan can adopt
    #[serde(default)]
    pub legacy: Option<String>,
    /// Carried over from a pre-plan cache rather than produced under a
    #[serde(default)]
    pub adopted: bool,
}

/// A chapter's recorded render plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RenderPlan {
    pub chapter: u32,
    pub engine: String,
    #[serde(default = "default_plan_version")]
    pub plan_version: u32,
    #[serde(default)]
    pub generated: u64,
    /// Hash of the voice collection this chapter uses, keyed per speaker. The
    #[serde(default)]
    pub cast_hash: String,
    pub takes: Vec<Take>,
}

fn default_plan_version() -> u32 {
    PLAN_VERSION
}

/// What a re-plan changed. The work is `dirty`; the deletions are `stale`.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanUpdate {
    pub plan: RenderPlan,
    /// Positions (into `plan.takes`) that must be rendered.
    pub dirty: Vec<usize>,
    /// Files the store must delete: inputs that changed or disappeared.
    pub stale: Vec<String>,
    /// How many takes were adopted from a pre-plan cache (first plan only).
    pub adopted: usize,
}

impl RenderPlan {
    /// Build the canonical plan for a chapter's planned units.
    pub fn build(
        chapter: u32,
        engine: &str,
        units: &[super::plan::RenderUnit],
        quality: TakeQuality,
    ) -> RenderPlan {
        let takes: Vec<Take> = units
            .iter()
            .enumerate()
            .map(|(pos, u)| {
                let voice_key = crate::voices::key_for_name(engine, &u.voice)
                    .unwrap_or_else(|| fold_voice(&u.voice));
                let key = take_key(engine, &voice_key, &u.text, u.temperature, u.silence_p);
                Take {
                    pos,
                    tag: u.tag.clone(),
                    speaker: u.speaker.clone(),
                    voice: u.voice.clone(),
                    voice_key,
                    text: u.text.clone(),
                    temperature: u.temperature,
                    silence_p: u.silence_p,
                    take_key: key.clone(),
                    file: take_file(&key, quality),
                    legacy: u
                        .dest
                        .file_name()
                        .and_then(|s| s.to_str())
                        .map(str::to_string),
                    adopted: false,
                }
            })
            .collect();
        let cast_hash = cast_hash(&takes);
        RenderPlan {
            chapter,
            engine: engine.to_string(),
            plan_version: PLAN_VERSION,
            generated: now_secs(),
            cast_hash,
            takes,
        }
    }

    /// Read a stored plan. `None` on missing or unparseable, which is "no plan"
    pub fn load(path: &Path) -> Option<RenderPlan> {
        let plan: RenderPlan = crate::util::read_json(path).ok()?;
        (plan.plan_version == PLAN_VERSION).then_some(plan)
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        crate::util::write_json(path, self)
    }

    /// Files present in `seg_dir`, in mix order.
    pub fn files(&self) -> Vec<String> {
        self.takes.iter().map(|t| t.file.clone()).collect()
    }

    /// Planned takes whose file the store does not hold. Empty means covered.
    pub fn missing(&self, seg_dir: &Path) -> Vec<String> {
        self.takes
            .iter()
            .filter(|t| !present(seg_dir, &t.file))
            .map(|t| t.file.clone())
            .collect()
    }

    /// Every planned take is on disk. The merge gate, and the render task's
    pub fn covered(&self, seg_dir: &Path) -> bool {
        self.takes.iter().all(|t| present(seg_dir, &t.file))
    }

    /// Whether any planned take carries an input different from `other`'s.
    pub fn differs_from(&self, other: &RenderPlan) -> bool {
        self.takes.len() != other.takes.len()
            || self
                .takes
                .iter()
                .zip(&other.takes)
                .any(|(a, b)| a.take_key != b.take_key)
    }
}

/// Diff a freshly built plan against the stored one and produce the work.
pub fn reconcile(old: Option<&RenderPlan>, new: RenderPlan, seg_dir: &Path) -> PlanUpdate {
    reconcile_with(old, new, seg_dir, true)
}

/// [`reconcile`], with adoption under the caller's control.
pub fn reconcile_with(
    old: Option<&RenderPlan>,
    new: RenderPlan,
    seg_dir: &Path,
    adopt: bool,
) -> PlanUpdate {
    let mut plan = new;
    let mut dirty = Vec::new();
    let mut stale: Vec<String> = Vec::new();
    let mut adopted = 0usize;

    let old_by_key: BTreeMap<String, &Take> = old
        .map(|o| o.takes.iter().map(|t| (t.take_key.clone(), t)).collect())
        .unwrap_or_default();
    let new_keys: BTreeSet<String> = plan.takes.iter().map(|t| t.take_key.clone()).collect();

    for i in 0..plan.takes.len() {
        let take_key = plan.takes[i].take_key.clone();

        if let Some(o) = old_by_key.get(&take_key) {
            // Same inputs, same audio: trust the recorded file. If it has since
            let (file, was_adopted) = (o.file.clone(), o.adopted);
            plan.takes[i].file = file;
            plan.takes[i].adopted = was_adopted;
            if !present(seg_dir, &plan.takes[i].file) {
                dirty.push(i);
            }
            continue;
        }

        // A new or changed take. Only a *first* plan adopts a legacy file, and
        if old.is_none() && adopt {
            if let Some(legacy) = plan.takes[i].legacy.clone() {
                if present(seg_dir, &legacy) {
                    plan.takes[i].file = legacy;
                    plan.takes[i].adopted = true;
                    adopted += 1;
                    continue;
                }
            }
        }

        if !present(seg_dir, &plan.takes[i].file) {
            dirty.push(i);
        }
        // The legacy name is stale whenever it is not the file now in use —
        if let Some(legacy) = plan.takes[i].legacy.clone() {
            if legacy != plan.takes[i].file && present(seg_dir, &legacy) {
                stale.push(legacy);
            }
        }
    }

    // Takes the new plan no longer contains: their files are stale.
    if let Some(o) = old {
        for old_take in &o.takes {
            if !new_keys.contains(&old_take.take_key) {
                stale.push(old_take.file.clone());
            }
        }
    }

    stale.sort_unstable();
    stale.dedup();
    PlanUpdate {
        plan,
        dirty,
        stale,
        adopted,
    }
}

/// The input hash that names a take.
pub fn take_key(
    engine: &str,
    voice_key: &str,
    text: &str,
    temperature: f64,
    silence_p: f64,
) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"bm-take-v1");
    h.update([0]);
    h.update(engine.as_bytes());
    h.update([0]);
    h.update(voice_key.as_bytes());
    h.update([0]);
    h.update(format!("{temperature:.4}|{silence_p:.4}").as_bytes());
    h.update([0]);
    h.update(text.as_bytes());
    let digest = h.finalize();
    digest
        .iter()
        .take(TAKE_KEY_CHARS / 2)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The store name for a take: content-addressed, so the name is a claim about
pub fn take_file(take_key: &str, quality: TakeQuality) -> String {
    format!("t-{}.{}", take_key, quality.extension())
}

/// A hash of the voice collection this plan uses: sorted unique
fn cast_hash(takes: &[Take]) -> String {
    use sha2::{Digest, Sha256};
    let mut pairs: Vec<(&str, &str)> = takes
        .iter()
        .map(|t| (t.speaker.as_str(), t.voice_key.as_str()))
        .collect();
    pairs.sort_unstable();
    pairs.dedup();
    let mut h = Sha256::new();
    for (speaker, voice) in pairs {
        h.update(speaker.as_bytes());
        h.update([0]);
        h.update(voice.as_bytes());
        h.update([0]);
    }
    h.finalize()
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A voice with no catalogue key (an un-enrolled clone) still needs a stable
fn fold_voice(name: &str) -> String {
    crate::util::fold(name)
}

fn present(seg_dir: &Path, name: &str) -> bool {
    seg_dir
        .join(name)
        .metadata()
        .map(|m| m.len() > MIN_TAKE_BYTES)
        .unwrap_or(false)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assemble::{plan_render, Planned};
    use crate::cast::Cast;
    use serde_json::json;
    use std::path::PathBuf;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bm-renderplan-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn cast(pairs: &[(&str, &str)]) -> Cast {
        let mut c = Cast::new();
        for (k, v) in pairs {
            c.insert(k.to_string(), v.to_string());
        }
        c
    }

    fn units(
        segs: &[serde_json::Value],
        cast: &Cast,
        seg_dir: &Path,
    ) -> Vec<super::super::plan::RenderUnit> {
        plan_render(&Planned::plan(segs), cast, seg_dir, true, None).unwrap()
    }

    fn plan_of(segs: &[serde_json::Value], cast: &Cast, seg_dir: &Path) -> RenderPlan {
        RenderPlan::build(1, "vieneu", &units(segs, cast, seg_dir), TakeQuality::Raw)
    }

    fn write_take(seg_dir: &Path, name: &str) {
        std::fs::create_dir_all(seg_dir).unwrap();
        std::fs::write(seg_dir.join(name), vec![0u8; 2000]).unwrap();
    }

    #[test]
    fn take_key_is_stable_and_input_sensitive() {
        let base = take_key("vieneu", "duc-tri", "xin chào", 0.5, 0.15);
        assert_eq!(
            base,
            take_key("vieneu", "duc-tri", "xin chào", 0.5, 0.15),
            "a key that moved on its own would re-render the book every pass"
        );
        assert_ne!(
            base,
            take_key("vieneu", "duc-tri", "xin chào!", 0.5, 0.15),
            "text"
        );
        assert_ne!(
            base,
            take_key("vieneu", "adam", "xin chào", 0.5, 0.15),
            "voice"
        );
        assert_ne!(
            base,
            take_key("vieneu", "duc-tri", "xin chào", 0.6, 0.15),
            "temperature"
        );
        assert_ne!(
            base,
            take_key("vieneu", "duc-tri", "xin chào", 0.5, 0.20),
            "silence"
        );
        assert_ne!(
            base,
            take_key("gemini", "duc-tri", "xin chào", 0.5, 0.15),
            "engine"
        );
        assert_eq!(base.len(), TAKE_KEY_CHARS);
        // The tier rides the name: raw stays `.wav`, every mp3 tier names
        assert_eq!(take_file(&base, TakeQuality::Raw), format!("t-{base}.wav"));
        assert_eq!(
            take_file(&base, TakeQuality::Balanced),
            format!("t-{base}.mp3")
        );
    }

    #[test]
    fn the_same_inputs_build_the_same_plan() {
        let dir = tmpdir("stable");
        let c = cast(&[("A", "Đức Trí")]);
        let segs = vec![json!({"speaker": "A", "text": "một"})];
        let a = plan_of(&segs, &c, &dir);
        let b = plan_of(&segs, &c, &dir);
        assert_eq!(a.takes, b.takes);
        assert_eq!(a.cast_hash, b.cast_hash);
    }

    #[test]
    fn a_first_plan_adopts_existing_wavs_and_dirties_none() {
        // The pre-migration case: wavs named the legacy way, no plan on disk.
        let dir = tmpdir("adopt");
        let c = cast(&[("A", "Đức Trí"), ("B", "Adam")]);
        let segs = vec![
            json!({"speaker": "A", "text": "một"}),
            json!({"speaker": "B", "text": "hai"}),
        ];
        let plan = plan_of(&segs, &c, &dir);
        for t in &plan.takes {
            write_take(&dir, t.legacy.as_deref().unwrap());
        }
        let up = reconcile(None, plan, &dir);
        assert!(
            up.dirty.is_empty(),
            "adopted takes are not work: {:?}",
            up.dirty
        );
        assert!(
            up.stale.is_empty(),
            "nothing was superseded: {:?}",
            up.stale
        );
        assert_eq!(up.adopted, 2);
        assert!(up.plan.takes.iter().all(|t| t.adopted));
        assert!(up.plan.covered(&dir));
    }

    #[test]
    fn a_known_change_never_adopts_an_unrecorded_cache() {
        // The invalidation case: the caller *knows* an input moved and there is
        let dir = tmpdir("no-adopt");
        let c = cast(&[("A", "Đức Trí")]);
        let plan = plan_of(&[json!({"speaker": "A", "text": "một"})], &c, &dir);
        let legacy = plan.takes[0].legacy.clone().unwrap();
        write_take(&dir, &legacy);

        let up = reconcile_with(None, plan, &dir, false);
        assert_eq!(up.dirty, vec![0], "a known change is work");
        assert_eq!(up.adopted, 0, "nothing is adopted on a known change");
        assert!(
            up.stale.contains(&legacy),
            "and the old bytes are named stale: {:?}",
            up.stale
        );
        assert!(up.plan.takes[0].file.starts_with("t-"));
        assert!(!up.plan.covered(&dir));

        // The routine pass over the same store adopts, which is what keeps a
        let relaxed = plan_of(&[json!({"speaker": "A", "text": "một"})], &c, &dir);
        let up = reconcile(None, relaxed, &dir);
        assert!(up.dirty.is_empty(), "routine adoption re-speaks nothing");
        assert_eq!(up.adopted, 1);
    }

    #[test]
    fn an_absent_take_is_dirty_on_a_first_plan() {
        let dir = tmpdir("first-missing");
        let c = cast(&[("A", "Đức Trí"), ("B", "Adam")]);
        let segs = vec![
            json!({"speaker": "A", "text": "một"}),
            json!({"speaker": "B", "text": "hai"}),
        ];
        let plan = plan_of(&segs, &c, &dir);
        // Only A's wav exists.
        write_take(&dir, plan.takes[0].legacy.as_deref().unwrap());
        let up = reconcile(None, plan, &dir);
        assert_eq!(up.dirty.len(), 1, "the missing take is the work");
        assert_eq!(up.dirty[0], 1);
        assert!(!up.plan.covered(&dir));
        assert_eq!(up.plan.missing(&dir).len(), 1);
    }

    #[test]
    fn a_text_change_dirties_only_that_take() {
        let dir = tmpdir("text-change");
        let c = cast(&[("A", "Đức Trí"), ("B", "Adam")]);
        let before = vec![
            json!({"speaker": "A", "text": "một"}),
            json!({"speaker": "B", "text": "hai"}),
        ];
        let plan1 = plan_of(&before, &c, &dir);
        for t in &plan1.takes {
            write_take(&dir, t.legacy.as_deref().unwrap());
        }
        let adopted = reconcile(None, plan1, &dir).plan;
        assert!(adopted.covered(&dir));

        // B's line is retagged; A's is untouched.
        let after = vec![
            json!({"speaker": "A", "text": "một"}),
            json!({"speaker": "B", "text": "hai [thở dài]"}),
        ];
        let plan2 = plan_of(&after, &c, &dir);
        let up = reconcile(Some(&adopted), plan2, &dir);
        assert_eq!(
            up.dirty.len(),
            1,
            "only the edited take is work: {:?}",
            up.dirty
        );
        assert_eq!(
            up.plan.takes[0].take_key, adopted.takes[0].take_key,
            "A is carried"
        );
        assert_eq!(
            up.plan.takes[0].file, adopted.takes[0].file,
            "and keeps its file"
        );
        assert_ne!(
            up.plan.takes[1].take_key, adopted.takes[1].take_key,
            "B changed"
        );
        assert!(
            up.stale.contains(&adopted.takes[1].file),
            "B's old bytes are stale: {:?}",
            up.stale
        );
        assert!(
            !up.stale.contains(&adopted.takes[0].file),
            "A's file must survive"
        );
    }

    #[test]
    fn a_voice_change_dirties_only_that_speakers_takes() {
        let dir = tmpdir("voice-change");
        let before = cast(&[("A", "Đức Trí"), ("B", "Adam")]);
        // A, B, A: three runs, so A owns two takes and B one. Consecutive A
        let segs = vec![
            json!({"speaker": "A", "text": "một"}),
            json!({"speaker": "B", "text": "ba"}),
            json!({"speaker": "A", "text": "bốn"}),
        ];
        let plan1 = plan_of(&segs, &before, &dir);
        assert_eq!(plan1.takes.len(), 3, "A, B, A is three runs");
        for t in &plan1.takes {
            write_take(&dir, t.legacy.as_deref().unwrap());
        }
        let adopted = reconcile(None, plan1, &dir).plan;

        let after = cast(&[("A", "Minh Triết"), ("B", "Adam")]);
        let up = reconcile(Some(&adopted), plan_of(&segs, &after, &dir), &dir);
        assert_eq!(up.dirty, vec![0, 2], "A's two runs only");
        assert_eq!(
            up.plan.takes[1].take_key, adopted.takes[1].take_key,
            "B is carried"
        );
        assert_eq!(up.plan.takes[1].file, adopted.takes[1].file);
        assert_eq!(up.plan.cast_hash.len(), 16);
        assert_ne!(
            up.plan.cast_hash, adopted.cast_hash,
            "the voice collection moved"
        );
        assert!(
            !up.stale.contains(&adopted.takes[1].file),
            "B's file survives"
        );
    }

    #[test]
    fn a_take_dropped_by_a_replan_is_stale() {
        let dir = tmpdir("dropped");
        let c = cast(&[("A", "Đức Trí"), ("B", "Adam")]);
        let plan1 = plan_of(
            &[
                json!({"speaker": "A", "text": "một"}),
                json!({"speaker": "B", "text": "hai"}),
            ],
            &c,
            &dir,
        );
        for t in &plan1.takes {
            write_take(&dir, t.legacy.as_deref().unwrap());
        }
        let adopted = reconcile(None, plan1, &dir).plan;
        // B's line is gone from the new script.
        let up = reconcile(
            Some(&adopted),
            plan_of(&[json!({"speaker": "A", "text": "một"})], &c, &dir),
            &dir,
        );
        assert!(
            up.dirty.is_empty(),
            "the surviving take keeps its cache: {:?}",
            up.dirty
        );
        assert_eq!(up.plan.takes[0].take_key, adopted.takes[0].take_key);
        assert_eq!(up.plan.takes[0].file, adopted.takes[0].file);
        assert!(
            up.stale.contains(&adopted.takes[1].file),
            "the dropped take's file is stale: {:?}",
            up.stale
        );
    }

    #[test]
    fn reverting_a_change_reuses_the_content_addressed_file() {
        // A take rendered under content addressing keeps its name if the inputs
        let dir = tmpdir("revert");
        let c = cast(&[("A", "Đức Trí")]);
        let plan1 = plan_of(&[json!({"speaker": "A", "text": "một"})], &c, &dir);
        let up1 = reconcile(None, plan1, &dir);
        // Simulate the take having been rendered under its canonical name.
        write_take(&dir, &up1.plan.takes[0].file);
        let up_a = reconcile(
            Some(&up1.plan),
            plan_of(&[json!({"speaker": "A", "text": "hai"})], &c, &dir),
            &dir,
        );
        assert_eq!(up_a.dirty.len(), 1);
        let up_b = reconcile(
            Some(&up_a.plan),
            plan_of(&[json!({"speaker": "A", "text": "một"})], &c, &dir),
            &dir,
        );
        assert!(
            up_b.dirty.is_empty(),
            "the original file is still there under its content name"
        );
        assert!(up_b.plan.covered(&dir));
    }

    #[test]
    fn a_plan_round_trips_and_a_wrong_version_is_rebuilt() {
        let dir = tmpdir("roundtrip");
        let c = cast(&[("A", "Đức Trí")]);
        let plan = plan_of(&[json!({"speaker": "A", "text": "một"})], &c, &dir);
        let path = dir.join("render-01.json");
        plan.save(&path).unwrap();
        assert_eq!(RenderPlan::load(&path).unwrap().takes, plan.takes);

        std::fs::write(
            &path,
            r#"{"chapter":1,"engine":"vieneu","plan_version":999,"takes":[]}"#,
        )
        .unwrap();
        assert!(
            RenderPlan::load(&path).is_none(),
            "a future version must be rebuilt, not mis-trusted"
        );
    }

    #[test]
    fn missing_names_what_coverage_denies() {
        let dir = tmpdir("coverage");
        let c = cast(&[("A", "Đức Trí")]);
        let plan = plan_of(&[json!({"speaker": "A", "text": "một"})], &c, &dir);
        assert_eq!(plan.missing(&dir), vec![plan.takes[0].file.clone()]);
        assert!(!plan.covered(&dir));
        write_take(&dir, &plan.takes[0].file);
        assert!(plan.missing(&dir).is_empty());
        assert!(plan.covered(&dir));
        // A half-written file is not a take.
        std::fs::write(dir.join(&plan.takes[0].file), vec![0u8; 10]).unwrap();
        assert!(!plan.covered(&dir));
    }
}
