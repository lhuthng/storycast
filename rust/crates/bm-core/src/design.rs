//! Has the sound design changed since a chapter was merged?
//!
//! The mix reads four registries and seven knobs, and every one of them can be
//! edited after a chapter is published: `:mix` writes the knobs, `:sound` writes
//! the pools, the scene map is a hand-edited file. Nothing about a finished mp3
//! records which design produced it, so the only way to know whether it is still
//! current was for the operator to remember — which is a ritual, not a property,
//! and `:sound` is where the ritual got forgotten.
//!
//! So a merge carries a **fingerprint** of the design it was produced under, and
//! a merge whose fingerprint no longer matches the design on disk is not done.
//!
//! ## Only what the chapter reaches
//!
//! The fingerprint is per chapter, and it hashes the registry slice that
//! *that* chapter's own script reaches — its scene labels' resolved rules, the
//! palette entries its `music` values name, the pool entries `pick` can return
//! for those tags, and the inject sounds it places. Retuning a clip no chapter
//! in range uses changes nothing; retuning one a chapter uses changes exactly
//! that chapter. A global knob (speed, the layer trims, `gap_ms`) is reached by
//! every chapter, so changing one invalidates everything — which is correct,
//! because it does change every mix.
//!
//! The reachability is a **superset** on purpose: `run_scenes` summarises a run
//! by majority, so a label a script declares but no run lands on is still
//! counted here. Over-counting can invalidate a chapter that would have come
//! out identical; under-counting would leave a stale mp3 looking current, which
//! is the failure this exists to prevent.
//!
//! ## What it deliberately does not cover
//!
//! The script's *text*. That is the render's input, and a script rewrite
//! requeues render + merge through `reconcile` already. The design fields
//! (`scene`, `music`, the sound items) are here because they change the mix
//! without changing a single wav.

use crate::ambience::{
    self, Duck, Layers, LegacyMusic, PaletteEntry, PausePlan, SceneMap, SceneRule,
};
use crate::assemble::Planned;
use crate::audio_pool::{self, ClipPool, PoolKind, Sound};
use crate::Layout;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

/// The merge's knobs: everything `assemble` takes that is not a file.
///
/// One struct rather than seven arguments so a caller cannot pass them in the
/// wrong order, and so adding a knob to the mix is an edit here rather than a
/// forgotten argument at four call sites.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Knobs {
    pub gap_ms: u32,
    pub speed: f64,
    /// The two layer switches and the three master gains.
    pub on: ambience::LayerSwitch,
}

/// The registries a mix reads, loaded once for as many chapters as you like.
///
/// Loading never fails. A missing or unparseable registry degrades to "no sound
/// design", which is exactly what `assemble` does with the scene map — a
/// fingerprint computed from a degraded read is still a fingerprint, and
/// refusing to produce one would mean a merge that cannot be stamped at all.
#[derive(Debug, Clone, Default)]
pub struct MergeDesign {
    map: SceneMap,
    effect_pool: ClipPool,
    music_pool: ClipPool,
    inject_pool: ClipPool,
}

/// What one chapter's mix actually depends on. Serialized and hashed; every
/// field is either reached by this chapter or global.
#[derive(Serialize)]
struct Reachable<'a> {
    knobs: Knobs,
    /// Scene label -> the rule it resolves to. Keyed by the label so that two
    /// labels resolving to the same rule are still two entries: which labels a
    /// script names is part of the design.
    scenes: BTreeMap<String, SceneRule>,
    /// Reverb name -> filter, for the names those rules reach.
    reverbs: BTreeMap<String, String>,
    /// Mood -> palette entry, for the moods the script declares.
    music: BTreeMap<String, PaletteEntry>,
    /// The sounds `pick` can return for the tags those rules reach.
    effect_clips: BTreeMap<String, Sound>,
    music_clips: BTreeMap<String, Sound>,
    /// The inject sounds the script places, and the entries they name.
    injects: BTreeMap<String, Sound>,
    /// Global: every chapter is mixed through these.
    layers: &'a Layers,
    duck: &'a Duck,
    pause: &'a PausePlan,
    /// The migration shim for scripts that predate the `music` field. A chapter
    /// on disk that reads it is affected by an edit to it like any other input.
    legacy_scene_music: &'a LegacyMusic,
}

impl MergeDesign {
    pub fn load(layout: &Layout) -> Self {
        MergeDesign {
            map: ambience::load_map(&layout.scene_map()).unwrap_or_default(),
            effect_pool: audio_pool::load_pool(&layout.pool(PoolKind::Effect)),
            music_pool: audio_pool::load_pool(&layout.pool(PoolKind::Music)),
            inject_pool: audio_pool::load_pool(&layout.pool(PoolKind::Inject)),
        }
    }

    /// The design stamp for one chapter's script.
    pub fn fingerprint(&self, script: &Value, knobs: Knobs) -> String {
        let segments = script
            .get("segments")
            .and_then(|s| s.as_array())
            .cloned()
            .unwrap_or_default();

        let mut scenes: BTreeMap<String, SceneRule> = BTreeMap::new();
        let mut music: BTreeMap<String, PaletteEntry> = BTreeMap::new();
        for seg in &segments {
            let label = seg
                .get("scene")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            scenes
                .entry(label.clone())
                .or_insert_with(|| ambience::match_scene(&label, &self.map));
            if let Some(m) = seg.get("music").and_then(|v| v.as_str()).map(str::trim) {
                if !m.is_empty() {
                    music.entry(m.to_string()).or_insert_with(|| {
                        self.map.music_palette.get(m).cloned().unwrap_or_default()
                    });
                }
            }
        }

        // The reverb presets those rules name. A preset nothing here reaches is
        // somebody else's chapter's problem.
        let mut reverbs: BTreeMap<String, String> = BTreeMap::new();
        for rule in scenes.values() {
            if let Some(name) = &rule.reverb {
                if let Some(fx) = self.map.reverb_presets.get(name) {
                    reverbs.insert(name.clone(), fx.clone());
                }
            }
        }

        // The clips the mix can land on. Asked through `audio_pool::candidates`,
        // the same function `pick` uses, so "what this scene can reach" and
        // "what this scene got" cannot be two different answers.
        let effect_tags: Vec<String> = scenes
            .values()
            .flat_map(|r| r.effect.iter().cloned())
            .collect();
        let effect_clips = named_candidates(&self.effect_pool, &effect_tags);
        let music_tags: Vec<String> = music
            .values()
            .flat_map(|p| p.tags.iter().cloned())
            .collect();
        let music_clips = named_candidates(&self.music_pool, &music_tags);

        // The inject sounds the script places. `Planned` lifts the sound items
        // out of the lines and `injects_of` resolves them against the pool —
        // both the merge's own calls, so this cannot drift from what plays.
        let planned = Planned::plan(&segments);
        let mut injects: BTreeMap<String, Sound> = BTreeMap::new();
        for fires in &planned.fires {
            for inj in ambience::injects_of(
                fires,
                &self.inject_pool,
                self.map.layers.inject.default_hold_s,
            ) {
                let name = match &inj {
                    ambience::Inject::Start { sound, .. } | ambience::Inject::Stop { sound } => {
                        sound.clone()
                    }
                };
                if let Some(entry) = self.inject_pool.get(&name) {
                    injects.insert(name, entry.clone());
                }
            }
        }

        let reachable = Reachable {
            knobs,
            scenes,
            reverbs,
            music,
            effect_clips,
            music_clips,
            injects,
            layers: &self.map.layers,
            duck: &self.map.duck,
            pause: &self.map.pause,
            legacy_scene_music: &self.map.legacy_scene_music,
        };
        // Serialization cannot fail for these types (no maps with non-string
        // keys, no floats that are NaN in a registry a `load` accepted), but an
        // empty string is the honest fallback: every chapter then hashes the
        // same, so a design change invalidates all of them rather than none.
        let json = serde_json::to_string(&reachable).unwrap_or_default();
        stamp(&json)
    }
}

/// The candidate set, keyed by sound name so the hash is a stable value.
fn named_candidates(pool: &ClipPool, tags: &[String]) -> BTreeMap<String, Sound> {
    audio_pool::candidates(pool, tags)
        .into_iter()
        .map(|(name, sound)| (name.to_string(), sound.clone()))
        .collect()
}

/// A hash prefix of the serialized design: identity without the whole blob.
///
/// Stable across runs and Rust versions — it is persisted on a task, so a hash
/// that moved when the toolchain did would invalidate a whole library on an
/// upgrade. `sha2` rather than `DefaultHasher` for exactly that reason.
fn stamp(json: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(json.as_bytes());
    h.finalize()
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &std::path::Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    /// `name` keeps two tests from sharing a tree — this repo's `tmpdir`
    /// convention, since `bm-core` has no `tempfile`.
    fn tmpdir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("bm-design-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Two scenes, two effect sounds, one music mood, one inject.
    fn fixture(name: &str) -> (std::path::PathBuf, Layout) {
        let root = tmpdir(name);
        write(
            &root,
            "assets/scene-map.json",
            r#"{
              "rules": [
                {"match": ["mountain"], "effect": ["wind"], "level": 0.3,
                 "reverb": "cave", "pause_before_s": 0.4},
                {"match": ["kitchen"], "effect": ["fire"], "level": 0.2}
              ],
              "default": {"effect": [], "level": 0.0},
              "music_palette": {
                "calm": {"tags": ["soft"], "note": "gentle"},
                "tense": {"tags": ["driving"], "note": "urgent"}
              },
              "reverb_presets": {"cave": "aecho=0.8:0.9:40|60:0.4|0.3"},
              "layers": {"effect": {"trim": 1.0}, "music": {"level": 0.6},
                         "inject": {"level": 0.8, "default_hold_s": 1.5}},
              "pause": {"pause_s": 0.6}
            }"#,
        );
        write(
            &root,
            "assets/effect-pool.json",
            r#"{
              "_note": "place beds",
              "wind": {"tags": ["wind"], "files": ["effects/wind-1.mp3"]},
              "gale": {"tags": ["wind"], "files": ["effects/gale-1.mp3"]},
              "fire": {"tags": ["fire"], "files": ["effects/fire-1.mp3"]},
              "rain": {"tags": ["rain"], "files": ["effects/rain-1.mp3"]}
            }"#,
        );
        write(
            &root,
            "assets/music-pool.json",
            r#"{
              "soft-relax": {"tags": ["soft"], "files": ["music/soft-1.mp3"]},
              "driving-tension": {"tags": ["driving"], "files": ["music/tens-1.mp3"]},
              "unused-track": {"tags": ["unused"], "files": ["music/none-1.mp3"]}
            }"#,
        );
        write(
            &root,
            "assets/inject-pool.json",
            r#"{
              "page-turn": {"tags": ["paper"], "files": ["injects/page-1.mp3"],
                            "mode": "hit", "dur_s": 0.4},
              "sword": {"tags": ["steel"], "files": ["injects/sword-1.mp3"],
                        "mode": "hit", "dur_s": 0.7}
            }"#,
        );
        let layout = Layout::new(&root);
        (root, layout)
    }

    fn knobs() -> Knobs {
        Knobs {
            gap_ms: 300,
            speed: 1.0,
            on: ambience::LayerSwitch::new(true, true, 1.0, 1.0, 1.0),
        }
    }

    /// A chapter in the mountains, calm, with one page turn.
    fn mountain_script() -> Value {
        serde_json::json!({
            "segments": [
                {"speaker": "A", "text": "Chương 1: Khởi đầu"},
                {"speaker": "A", "text": "Trên núi.", "scene": "mountain", "music": "calm"},
                {"sound": "page-turn"},
                {"speaker": "B", "text": "Xuống núi.", "scene": "mountain"}
            ]
        })
    }

    /// A chapter in a kitchen, tense, and nothing else.
    fn kitchen_script() -> Value {
        serde_json::json!({
            "segments": [
                {"speaker": "A", "text": "Chương 2: Bếp"},
                {"speaker": "A", "text": "Trong bếp.", "scene": "kitchen", "music": "tense"}
            ]
        })
    }

    #[test]
    fn the_same_design_and_script_fingerprint_the_same() {
        let (_root, layout) = fixture("stable");
        let design = MergeDesign::load(&layout);
        assert_eq!(
            design.fingerprint(&mountain_script(), knobs()),
            design.fingerprint(&mountain_script(), knobs()),
            "a stamp that moved on its own would invalidate the library every pass"
        );
    }

    #[test]
    fn two_chapters_that_reach_different_scenes_fingerprint_differently() {
        let (_root, layout) = fixture("per-chapter");
        let design = MergeDesign::load(&layout);
        assert_ne!(
            design.fingerprint(&mountain_script(), knobs()),
            design.fingerprint(&kitchen_script(), knobs()),
            "the stamp has to be per chapter, or `only what it reaches` is impossible"
        );
    }

    #[test]
    fn a_global_knob_reaches_every_chapter() {
        let (_root, layout) = fixture("global-knob");
        let design = MergeDesign::load(&layout);
        for script in [mountain_script(), kitchen_script()] {
            let before = design.fingerprint(&script, knobs());
            let mut louder = knobs();
            louder.on.effect_volume = 1.5;
            assert_ne!(
                design.fingerprint(&script, louder),
                before,
                "a master gain changes every mix it is applied to"
            );
            let mut slower = knobs();
            slower.speed = 0.9;
            assert_ne!(design.fingerprint(&script, slower), before, "speed");
            let mut wider = knobs();
            wider.gap_ms = 500;
            assert_ne!(design.fingerprint(&script, wider), before, "gap_ms");
            let mut dry = knobs();
            dry.on = ambience::LayerSwitch::new(false, true, 1.0, 1.0, 1.0);
            assert_ne!(design.fingerprint(&script, dry), before, "the switch");
        }
    }

    /// Re-read the design from disk, as the inductor does after a write.
    fn reload(layout: &Layout) -> MergeDesign {
        MergeDesign::load(layout)
    }

    #[test]
    fn retuning_a_clip_no_chapter_reaches_changes_nothing() {
        let (root, layout) = fixture("unreached-clip");
        let design = MergeDesign::load(&layout);
        let mountain = design.fingerprint(&mountain_script(), knobs());
        let kitchen = design.fingerprint(&kitchen_script(), knobs());

        // `rain` carries a tag no rule in the fixture names.
        write(
            &root,
            "assets/effect-pool.json",
            r#"{
              "wind": {"tags": ["wind"], "files": ["effects/wind-1.mp3"]},
              "gale": {"tags": ["wind"], "files": ["effects/gale-1.mp3"]},
              "fire": {"tags": ["fire"], "files": ["effects/fire-1.mp3"]},
              "rain": {"tags": ["rain"], "files": ["effects/rain-1.mp3"], "level": 0.1}
            }"#,
        );
        let after = reload(&layout);
        assert_eq!(
            after.fingerprint(&mountain_script(), knobs()),
            mountain,
            "retuning a clip nobody reaches must not invalidate a chapter"
        );
        assert_eq!(after.fingerprint(&kitchen_script(), knobs()), kitchen);
    }

    #[test]
    fn retuning_a_clip_reaches_exactly_the_chapters_that_use_it() {
        let (root, layout) = fixture("reached-clip");
        let design = MergeDesign::load(&layout);
        let mountain = design.fingerprint(&mountain_script(), knobs());
        let kitchen = design.fingerprint(&kitchen_script(), knobs());

        // `gale` answers the same `wind` tag the mountain rule asks for, so it
        // is in that chapter's candidate set and not in the kitchen's.
        write(
            &root,
            "assets/effect-pool.json",
            r#"{
              "wind": {"tags": ["wind"], "files": ["effects/wind-1.mp3"]},
              "gale": {"tags": ["wind"], "files": ["effects/gale-1.mp3"], "level": 0.2},
              "fire": {"tags": ["fire"], "files": ["effects/fire-1.mp3"]},
              "rain": {"tags": ["rain"], "files": ["effects/rain-1.mp3"]}
            }"#,
        );
        let after = reload(&layout);
        assert_ne!(
            after.fingerprint(&mountain_script(), knobs()),
            mountain,
            "the mountain chapter can land on `gale`, so it is now stale"
        );
        assert_eq!(
            after.fingerprint(&kitchen_script(), knobs()),
            kitchen,
            "the kitchen chapter cannot, so it is not"
        );
    }

    #[test]
    fn a_mood_reaches_only_the_chapters_that_declare_it() {
        let (root, layout) = fixture("mood");
        let design = MergeDesign::load(&layout);
        let mountain = design.fingerprint(&mountain_script(), knobs());
        let kitchen = design.fingerprint(&kitchen_script(), knobs());

        // `calm` resolves to the `soft` tag; `soft-relax` answers it.
        write(
            &root,
            "assets/music-pool.json",
            r#"{
              "soft-relax": {"tags": ["soft"], "files": ["music/soft-1.mp3"], "level": 0.4},
              "driving-tension": {"tags": ["driving"], "files": ["music/tens-1.mp3"]},
              "unused-track": {"tags": ["unused"], "files": ["music/none-1.mp3"]}
            }"#,
        );
        let after = reload(&layout);
        assert_ne!(
            after.fingerprint(&mountain_script(), knobs()),
            mountain,
            "the mountain chapter declares `calm`"
        );
        assert_eq!(
            after.fingerprint(&kitchen_script(), knobs()),
            kitchen,
            "the kitchen chapter declares `tense`, which is a different track"
        );
    }

    #[test]
    fn a_scene_map_rule_reaches_only_the_chapters_that_match_it() {
        let (root, layout) = fixture("rule");
        let design = MergeDesign::load(&layout);
        let mountain = design.fingerprint(&mountain_script(), knobs());
        let kitchen = design.fingerprint(&kitchen_script(), knobs());

        // The kitchen rule's level, and the reverb preset the mountain rule
        // names — two different edits, two different chapters.
        write(
            &root,
            "assets/scene-map.json",
            r#"{
              "rules": [
                {"match": ["mountain"], "effect": ["wind"], "level": 0.3,
                 "reverb": "cave", "pause_before_s": 0.4},
                {"match": ["kitchen"], "effect": ["fire"], "level": 0.9}
              ],
              "default": {"effect": [], "level": 0.0},
              "music_palette": {
                "calm": {"tags": ["soft"], "note": "gentle"},
                "tense": {"tags": ["driving"], "note": "urgent"}
              },
              "reverb_presets": {"cave": "aecho=0.8:0.9:40|60:0.4|0.3"},
              "layers": {"effect": {"trim": 1.0}, "music": {"level": 0.6},
                         "inject": {"level": 0.8, "default_hold_s": 1.5}},
              "pause": {"pause_s": 0.6}
            }"#,
        );
        let after = reload(&layout);
        assert_eq!(
            after.fingerprint(&mountain_script(), knobs()),
            mountain,
            "the mountain rule is untouched"
        );
        assert_ne!(
            after.fingerprint(&kitchen_script(), knobs()),
            kitchen,
            "the kitchen rule's level moved"
        );

        // Now the reverb preset the mountain rule reaches.
        write(
            &root,
            "assets/scene-map.json",
            r#"{
              "rules": [
                {"match": ["mountain"], "effect": ["wind"], "level": 0.3,
                 "reverb": "cave", "pause_before_s": 0.4},
                {"match": ["kitchen"], "effect": ["fire"], "level": 0.9}
              ],
              "default": {"effect": [], "level": 0.0},
              "music_palette": {
                "calm": {"tags": ["soft"], "note": "gentle"},
                "tense": {"tags": ["driving"], "note": "urgent"}
              },
              "reverb_presets": {"cave": "aecho=0.9:0.9:80|120:0.5|0.4"},
              "layers": {"effect": {"trim": 1.0}, "music": {"level": 0.6},
                         "inject": {"level": 0.8, "default_hold_s": 1.5}},
              "pause": {"pause_s": 0.6}
            }"#,
        );
        let kitchen_after = after.fingerprint(&kitchen_script(), knobs());
        let after2 = reload(&layout);
        assert_ne!(
            after2.fingerprint(&mountain_script(), knobs()),
            mountain,
            "the mountain chapter is the one in a cave"
        );
        assert_eq!(
            after2.fingerprint(&kitchen_script(), knobs()),
            kitchen_after,
            "the kitchen rule names no reverb"
        );
    }

    #[test]
    fn the_default_rule_reaches_every_chapter_through_its_headline() {
        let (root, layout) = fixture("default-rule");
        let design = MergeDesign::load(&layout);
        let mountain = design.fingerprint(&mountain_script(), knobs());
        let kitchen = design.fingerprint(&kitchen_script(), knobs());

        // Every chapter's first turn is the headline, and `plan_turns` gives it
        // `scene: ""` — which `match_scene` resolves through `default`. So
        // `default` is not "the rule for chapters with no scene tag"; it is the
        // rule the opening turn of *every* chapter is mixed with, and an edit to
        // it invalidates the lot. Easy to assume otherwise, so it is pinned.
        write(
            &root,
            "assets/scene-map.json",
            r#"{
              "rules": [
                {"match": ["mountain"], "effect": ["wind"], "level": 0.3,
                 "reverb": "cave", "pause_before_s": 0.4},
                {"match": ["kitchen"], "effect": ["fire"], "level": 0.2}
              ],
              "default": {"effect": ["rain"], "level": 0.5},
              "music_palette": {
                "calm": {"tags": ["soft"], "note": "gentle"},
                "tense": {"tags": ["driving"], "note": "urgent"}
              },
              "reverb_presets": {"cave": "aecho=0.8:0.9:40|60:0.4|0.3"},
              "layers": {"effect": {"trim": 1.0}, "music": {"level": 0.6},
                         "inject": {"level": 0.8, "default_hold_s": 1.5}},
              "pause": {"pause_s": 0.6}
            }"#,
        );
        let after = reload(&layout);
        assert_ne!(after.fingerprint(&mountain_script(), knobs()), mountain);
        assert_ne!(after.fingerprint(&kitchen_script(), knobs()), kitchen);

        // A chapter that names no scene at all is still distinguishable from
        // one that does — it reaches fewer rules, and that is the whole point of
        // a per-chapter stamp.
        let unlabelled = serde_json::json!({
            "segments": [
                {"speaker": "A", "text": "Chương 3"},
                {"speaker": "A", "text": "Ở đâu đó."}
            ]
        });
        let design = MergeDesign::load(&layout);
        assert_ne!(
            design.fingerprint(&unlabelled, knobs()),
            design.fingerprint(&mountain_script(), knobs()),
            "an unlabelled chapter reaches `default` alone; the mountain one reaches a rule too"
        );
    }

    #[test]
    fn an_inject_the_script_places_reaches_that_chapter() {
        let (root, layout) = fixture("inject");
        let design = MergeDesign::load(&layout);
        let mountain = design.fingerprint(&mountain_script(), knobs());
        let kitchen = design.fingerprint(&kitchen_script(), knobs());

        // `sword` is in no chapter's script; `page-turn` is in the mountain's.
        write(
            &root,
            "assets/inject-pool.json",
            r#"{
              "page-turn": {"tags": ["paper"], "files": ["injects/page-1.mp3"],
                            "mode": "hit", "dur_s": 0.4, "level": 0.3},
              "sword": {"tags": ["steel"], "files": ["injects/sword-1.mp3"],
                        "mode": "hit", "dur_s": 0.7}
            }"#,
        );
        let after = reload(&layout);
        assert_ne!(
            after.fingerprint(&mountain_script(), knobs()),
            mountain,
            "the mountain script places `page-turn`"
        );
        assert_eq!(
            after.fingerprint(&kitchen_script(), knobs()),
            kitchen,
            "the kitchen script places nothing"
        );
    }

    #[test]
    fn a_changed_clip_file_or_take_set_is_a_changed_design() {
        // The mix reads a *file* out of the entry, so replacing which takes an
        // entry names changes the audio even though the tags and level did not.
        let (root, layout) = fixture("take-set");
        let design = MergeDesign::load(&layout);
        let mountain = design.fingerprint(&mountain_script(), knobs());

        write(
            &root,
            "assets/effect-pool.json",
            r#"{
              "wind": {"tags": ["wind"], "files": ["effects/wind-2.mp3"]},
              "gale": {"tags": ["wind"], "files": ["effects/gale-1.mp3"]},
              "fire": {"tags": ["fire"], "files": ["effects/fire-1.mp3"]},
              "rain": {"tags": ["rain"], "files": ["effects/rain-1.mp3"]}
            }"#,
        );
        assert_ne!(
            reload(&layout).fingerprint(&mountain_script(), knobs()),
            mountain,
            "which take plays is part of the mix"
        );
    }
}
