use super::attribution::build_attribution_prompt;
use super::attribution::Continuity;
use super::excerpt::previous_excerpts;
use super::parse::parse_attribution;
use super::parse::parse_staged_script;
use super::parts::merge_contexts;
use super::parts::merge_scripts;
use super::parts::part_lines;
use super::parts::part_of;
use super::parts::sound_gap;
use super::parts::window_text;
use super::parts::Part;
use super::parts::Parts;
use super::prepare::prepare_chapter;
use super::prompts::build_staging_prompt;
use super::prompts::load_map;
use super::prompts::PreparedChapter;
use super::quotes::RepairVerdict;
use super::quotes::build_repair_prompt;
use super::quotes::effective_text;
use super::quotes::judge_repair;
use super::quotes::keep_repaired;
use super::quotes::needs_proofread;
use super::quotes::quote_findings;
use super::quotes::repair_complaint;
use super::run::assemble_outcome;
use super::run::Round;
use super::*;
/// Dump a round's raw answer when `BM_DIGEST_RAW` is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManualPart {
    /// 1-based.
    pub index: usize,
    pub total: usize,
}

/// The prompt for one manual round, ready to be carried to any model.
#[derive(Debug, Clone)]
pub struct ManualPrompt {
    pub round: Round,
    pub text: String,
    /// The part this round is for, when the chapter is staged in parts. The
    pub part: Option<ManualPart>,
}

/// What a pasted answer produced.
#[derive(Debug, Clone)]
pub struct ManualAnswer {
    /// The next prompt to ask, when there is one: round 2 after a cast answer,
    pub prompt: Option<ManualPrompt>,
    /// Round 1's validated cast, set exactly when `prompt` is round 2: it is
    pub cast: Option<Value>,
    /// Every part staged and merged: the finished chapter. Never set together
    pub outcome: Option<DigestOutcome>,
}

/// The vocabulary the script validators check against, read from the same files
pub(crate) struct Vocabulary {
    pub(crate) palette: Vec<String>,
    pub(crate) effects: Vec<String>,
    pub(crate) injects: crate::audio_pool::ClipPool,
    pub(crate) aliases: TagAliases,
    /// `scene-map.json`'s `thought.sound`, when the pack declares one. Checked
    pub(crate) thought_stinger: Option<String>,
    /// Rule 10's sweep as substrings, from the adapter's `sound-cues.json`: the
    /// `silent_design` gate's phrase list, in the source language.
    pub(crate) sound_cues: Vec<String>,
    /// The engine whose non-verbal declaration the gates read: its tags are the
    /// only ones a script may carry, and an engine that voices none refuses
    /// every tag rather than another checkpoint's.
    pub(crate) engine: String,
}

/// The adapter's `sound-cues.json`, or an empty list when it ships none — which
/// turns the gate off rather than firing on another language's words.
fn sound_cues(layout: &Layout) -> Vec<String> {
    let path = layout.prompts_dir().join("sound-cues.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(v) => v
            .get("cues")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default(),
        Err(e) => {
            eprintln!(
                "{}: sound-cues.json is not valid JSON ({e}); the sound gate is off",
                path.display()
            );
            Vec::new()
        }
    }
}

pub(crate) fn vocabulary(layout: &Layout, engine: &str) -> Result<Vocabulary> {
    let effect_pool = crate::audio_pool::load_pool(&layout.assets().join("effect-pool.json"));
    let map = load_map(layout)?;
    let palette = crate::ambience::palette_names(&map);
    let effects = crate::ambience::effect_tags(&effect_pool);
    let injects = crate::audio_pool::load_pool(&layout.assets().join("inject-pool.json"));
    let aliases = TagAliases::load(&layout.assets().join("tag-aliases.json"))?;
    aliases.validate(&palette, &effects, injects.keys().cloned())?;
    let thought_stinger = thought_stinger(layout, &map, &injects)?;
    Ok(Vocabulary {
        palette,
        effects,
        injects,
        aliases,
        thought_stinger,
        sound_cues: sound_cues(layout),
        engine: engine.to_string(),
    })
}

/// The pack's declared thought sound, checked against the inject pool.
fn thought_stinger(
    layout: &Layout,
    map: &crate::ambience::SceneMap,
    injects: &crate::audio_pool::ClipPool,
) -> Result<Option<String>> {
    let name = map.thought.sound.trim();
    if name.is_empty() {
        return Ok(None);
    }
    if !injects.contains_key(name) {
        anyhow::bail!(
            "{} declares `thought.sound` = {name:?}, but assets/inject-pool.json has no such \
             sound — every thought would be lifted with a clip nobody has, so remove the rule or \
             add the clip to the inject pool",
            layout.assets().join("scene-map.json").display()
        );
    }
    Ok(Some(name.to_string()))
}

/// The chapter text and the bible, as the manual path needs them.
fn manual_inputs(layout: &Layout, n: u32) -> Result<(Value, String)> {
    let chapter_path = layout.chapter_txt(n);
    let text = std::fs::read_to_string(&chapter_path)
        .with_context(|| format!("reading {}", chapter_path.display()))?;
    Ok((load_bible(&layout.bible()), text))
}

/// Build the prompt for a manual round.
///
/// The rounds are the worker's own, in the worker's order: the operator replaces
/// the model's transport and nothing else. So a chapter that has not had its
/// proofread is asked for one here, whatever the scan said about it.
pub fn manual_prompt(
    layout: &Layout,
    engine: &str,
    n: u32,
    cast: Option<&Value>,
) -> Result<ManualPrompt> {
    let session = ManualSession::open(layout, n)?;
    if cast.is_none() && session.parts.len() == 0 && needs_proofread(layout, n, &session.original)
    {
        return proofread_step(layout, &session);
    }
    round_step(layout, engine, &session, cast)
}

/// The chapter's proofread prompt: the pass that opens a chapter, and the only
/// one that may edit the text.
fn proofread_step(layout: &Layout, session: &ManualSession) -> Result<ManualPrompt> {
    let findings = quote_findings(&session.text);
    Ok(ManualPrompt {
        round: Round::Repair,
        text: build_repair_prompt(layout, &session.text, &repair_complaint(&findings))?,
        part: None,
    })
}

/// The attribution or staging prompt for the part the session is on.
fn round_step(
    layout: &Layout,
    engine: &str,
    session: &ManualSession,
    cast: Option<&Value>,
) -> Result<ManualPrompt> {
    let n = session.n;
    let index = session.parts.len();
    let slice = session.slice(index).ok_or_else(|| {
        anyhow::anyhow!(
            "ch{n} is already staged in full — re-digest it instead of asking for another round"
        )
    })?;
    let summaries = session.parts.summaries();
    let continuity = session.continuity(index, &summaries);
    let text = match cast {
        None => build_attribution_prompt(
            layout,
            &session.bible,
            &slice,
            continuity.as_ref(),
            previous_excerpts(layout, n).as_deref(),
        )?,
        // The manual flow stages and sounds a chapter in one pass: there is no
        // worker to run the chapter-level sound pass, so it owns sound inline.
        Some(context) => build_staging_prompt(
            layout,
            engine,
            &session.bible,
            context,
            &slice,
            continuity.as_ref(),
            true,
        )?,
    };
    Ok(ManualPrompt {
        round: match cast {
            None => Round::Attribution,
            Some(_) => Round::Staging,
        },
        text,
        part: session.part(index),
    })
}

/// One chapter as the manual flow needs it: the text, the bible, and the plan —
struct ManualSession {
    pub(crate) n: u32,
    /// The chapter as it is on disk: what a proofread record is keyed by.
    pub(crate) original: String,
    pub(crate) bible: Value,
    pub(crate) text: String,
    pub(crate) prepared: PreparedChapter,
    pub(crate) windows: Vec<Window>,
    pub(crate) parts: Parts,
    pub(crate) settings: Settings,
}

impl ManualSession {
    pub(crate) fn open(layout: &Layout, n: u32) -> Result<ManualSession> {
        let (bible, original) = manual_inputs(layout, n)?;
        let text = effective_text(layout, n, &original);
        let prepared = prepare_chapter(&text);
        let settings = Settings::load(&layout.settings());
        let windows = plan_windows(&prepared, &settings.digest);
        let parts = Parts::open(layout, n, &text, &bible, &windows, &settings);
        Ok(ManualSession {
            n,
            original,
            bible,
            text,
            prepared,
            windows,
            parts,
            settings,
        })
    }

    fn total(&self) -> usize {
        self.windows.len()
    }

    /// The events of the part a round belongs to.
    fn slice(&self, index: usize) -> Option<PreparedChapter> {
        self.windows.get(index).map(|w| w.prepared(&self.prepared))
    }

    fn part(&self, index: usize) -> Option<ManualPart> {
        part_of(index, self.total()).map(|(index, total)| ManualPart { index, total })
    }

    /// The continuity block for a part, or `None` for a chapter that fits one
    fn continuity<'a>(&self, index: usize, plot: &'a [String]) -> Option<Continuity<'a>> {
        (self.total() > 1).then_some(Continuity {
            index,
            total: self.total(),
            plot,
        })
    }
}

/// Check a pasted answer for one round, and assemble what it yields.
pub fn manual_accept(
    layout: &Layout,
    engine: &str,
    n: u32,
    round: Round,
    pasted: &str,
    cast: Option<&Value>,
) -> Result<ManualAnswer> {
    let mut session = ManualSession::open(layout, n)?;
    let index = session.parts.len();
    let slice = session.slice(index).ok_or_else(|| {
        anyhow::anyhow!(
            "ch{n} is already staged in full — re-digest it instead of pasting another round"
        )
    })?;
    let summaries = session.parts.summaries();
    let continuity = session.continuity(index, &summaries);
    match round {
        Round::Sound => unreachable!("the manual flow sounds a chapter in its staging pass"),
        // The chapter's first prompt. A pasted proofread either lands in the
        // sidecar or the chapter stops here: nothing downstream edits the text.
        Round::Repair => {
            match judge_repair(pasted) {
                RepairVerdict::Fixed(fixed, changes) => {
                    keep_repaired(layout, n, &session.text, &fixed, &changes);
                }
                RepairVerdict::Reject(why) => anyhow::bail!(
                    "ch{n} the pasted proofread is not usable ({why}); the chapter is not \
                     digested — paste a corrected chapter, or fix {} by hand",
                    layout.chapter_txt(n).display()
                ),
            }
            // The chapter is on disk now, so the next prompt is rendered against
            // the text the audio will actually speak, and it is the next round:
            // the proofread is behind this chapter.
            let session = ManualSession::open(layout, n)?;
            Ok(ManualAnswer {
                prompt: Some(round_step(layout, engine, &session, None)?),
                cast: None,
                outcome: None,
            })
        }
        Round::Attribution => Ok(ManualAnswer {
            // Round 2's prompt is **not** built here: it carries the engine's
            prompt: None,
            cast: Some(parse_attribution(
                pasted,
                &session.bible,
                &slice,
                continuity.is_some(),
            )?),
            outcome: None,
        }),
        Round::Staging => {
            let context = cast.ok_or_else(|| {
                anyhow::anyhow!("round 2 needs round 1's cast — paste the cast answer first")
            })?;
            let vocab = vocabulary(layout, engine)?;
            let script = parse_staged_script(pasted, &session.bible, context, &slice, &vocab)?;
            // Every part's own prose, for rule 2 — or the chapter's own text when
            let texts: Vec<String> = if session.total() == 1 {
                vec![session.text.clone()]
            } else {
                session
                    .windows
                    .iter()
                    .map(|w| window_text(&session.prepared, w))
                    .collect()
            };
            let what = if session.total() == 1 {
                "chapter"
            } else {
                "part"
            };
            // The gates run **before** the part is stored, with the pasted answer
            let scripts: Vec<&Value> = session
                .parts
                .done
                .iter()
                .map(|p| &p.script)
                .chain(std::iter::once(&script))
                .collect();
            let scopes: Vec<&str> = texts.iter().map(String::as_str).collect();
            if let Some((gap, owner)) =
                sound_gap(&scripts, &scopes, &vocab.injects, &vocab.sound_cues, what)
            {
                match session.part(owner) {
                    Some(part) => anyhow::bail!("part {}/{}: {gap}", part.index, part.total),
                    None => anyhow::bail!("{gap}"),
                }
            }
            let summary = context
                .get("summary")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            session.parts.push(Part {
                from: session.windows[index].from,
                to: session.windows[index].to,
                summary,
                context: context.clone(),
                script,
            })?;
            if session.parts.len() < session.total() {
                let next = session.parts.len();
                let next_slice = session
                    .slice(next)
                    .ok_or_else(|| anyhow::anyhow!("ch{n} has no part {}", next + 1))?;
                let next_summaries = session.parts.summaries();
                let next_continuity = session.continuity(next, &next_summaries);
                return Ok(ManualAnswer {
                    prompt: Some(ManualPrompt {
                        round: Round::Attribution,
                        text: build_attribution_prompt(
                            layout,
                            &session.bible,
                            &next_slice,
                            next_continuity.as_ref(),
                            previous_excerpts(layout, n).as_deref(),
                        )?,
                        part: session.part(next),
                    }),
                    cast: None,
                    outcome: None,
                });
            }
            let (merged, conflicts) = merge_contexts(&session.parts.done);
            let merged_script = merge_scripts(session.parts.done.iter().map(|p| &p.script));
            let mut outcome =
                assemble_outcome(&session.bible, &merged, &merged_script, &session.text)?;
            for w in conflicts {
                outcome.log.push(format!("   WARN: {w}"));
                outcome.warnings.push(w);
            }
            if session.total() > 1 {
                for (i, line) in part_lines(&session.windows, &session.prepared, &session.settings)
                    .into_iter()
                    .enumerate()
                {
                    outcome.log.insert(1 + i, line);
                }
            }
            session.parts.clear();
            Ok(ManualAnswer {
                prompt: None,
                cast: None,
                outcome: Some(outcome),
            })
        }
    }
}
