# Completing a pack

A pack's registries are authored before its audio. That is deliberate — the
sound is *designed* on paper, then recorded or generated, then normalized into
place — and it means a pack under construction is a normal state rather than a
broken one. The gate says so in words (`PENDING`, with a count) instead of
failing.

This file is the runbook for finishing one: where [`craft`](../assets/_extends/craft)
and [`court-mystery`](../assets/_extends/court-mystery) stand, the order to
work in, the check to run after each step, the failures that are **silent**, and
what to listen for before cutting a release.

The method, the spec and the reason for each number are in
[SOUND.md](SOUND.md). The pack model is in [ASSET-PACKS.md](ASSET-PACKS.md). This
file is only the sequence.

**What a book needs from a pack, and what is still missing, is
[AUDIO-NEEDS.md](AUDIO-NEEDS.md)** — places, moments and background music.
Three
decisions changed after this runbook was written — the project is non-commercial, length
no longer matters, and generation now comes before recording — so where the two
disagree, **AUDIO-NEEDS.md wins**. Read that one first.

## Where the two packs stand

```sh
python3 tools/inspect-pool.py assets/_extends/craft
python3 tools/inspect-pool.py assets/_extends/court-mystery
```

| | sounds | files | on disk |
| --- | --- | --- | --- |
| `craft` — effect | 1 bed | 2 | 0 |
| `craft` — inject | 9 foley | 17 | 0 |
| `court-mystery` — effect | 4 beds | 8 | 0 |
| `court-mystery` — music | 23 tracks | 24 | **9** (step 0, done) |
| `court-mystery` — inject | 9 foley | 18 | 0 |
| **total** | | **69** | **9** |

**Sixty-nine files, and they are not sixty-nine jobs.** Broken down by what
actually has to happen to them:

| | count | what it is | state |
| --- | --- | --- | --- |
| **copy** | 9 | the world's own tracks, already in `assets/music/` — step 0 | **done** |
| **generate** | 15 | AI, from the prompts in [`TRACKS.md`](../assets/_extends/court-mystery/TRACKS.md) | to do |
| **record, foley** | 35 | 18 sounds, three takes each, keep two | to do |
| **record, beds** | 10 | 5 room tones, two takes each | to do |
| | **69** | | **9 done, 60 to do** |

The gate agrees, and reads it per resolved composition: `court-mystery` **60
pending** (its own 41 plus `craft`'s 19, which fold in with it), `craft` **19**.

**Nothing in `common` is re-recorded.** It already carries 30 injects, 11 beds
and the thirteen world tracks, and most of this book uses them. A pack that
duplicates a sound the world already answers is a second take to keep in step
and no gain.

## What "done" means

Two things, and only the second is the interesting one:

1. `python3 tools/inspect-pool.py <pack>` prints `0 pending` and every row
   shows `48000/1` — mono, 48 kHz. A stereo file is thrown away at the first
   `aformat` in the merge, so it is not a degraded clip, it is a silent one that
   passed inspection.
2. `cargo test -p bm-core --test pack_gates -- --nocapture` prints **no
   pending lines at all** for the pack. Silence there is the signal; the test
   still passes either way, because a pack mid-build is legitimate.

Then cut a release — see the end of this file.

## Step 0 — copy the nine world tracks ✅ done

Takes the preset's pending count from 24 to 15, and the resolved composition
from 69 to 60. The eight world sounds were carried into `court-mystery`'s
registry precisely so that **a preset inherits the mood vocabulary and must
answer all of it**; their clips already exist in this checkout's live tree and
are AI-generated under the same licence line, so carrying them is a copy rather
than nine generations.

```sh
mkdir -p assets/_extends/court-mystery/music
for f in generic-energetic-bg-1 generic-soft-bg-1 market-bg-1 playful-bg-1 \
         sad-bg-1 soft-cute-bg-1 tavern-bg-1 tavern-bg-2 tense-bg-1; do
  cp -n "assets/music/$f.mp3" "assets/_extends/court-mystery/music/$f.mp3"
done
python3 tools/inspect-pool.py assets/_extends/court-mystery music | tail -2
```

`-n` so a re-run never overwrites a track you have replaced. **Copy, do not
symlink** — a release is a `tar.zst` of this tree, and a symlink in it points
at a path that does not exist on the box.

*Verified: `9/24 on disk, 15 pending`, every row `48000/1`.*

## Step 1 — room tone

Before any sound, in every room you will record in: **20 seconds of silence.**

This is the step people skip and it is not cosmetic. `tools/normalize-audio.sh`
detects near-silence to trim, and its floor is **−45 dB** — chosen because the
detector compares *peak*, and a 64k mono recording's own room tone already sits
around −45. A clip recorded with no tone under it is trimmed down to almost
nothing, and the symptom is a sound that is perfect in isolation and missing its
bottom two octaves in the mix.

Keep the tone files. They are what you check a new room against before a take is
believed.

## Step 2 — the five beds

| bed | pack | the shot |
| --- | --- | --- |
| `workshop-interior` | craft | low-ceilinged wooden room, a street two rooms away, 75 s |
| `palace-corridor` | court-mystery | marble and paper, a long space, a footstep two rooms off every 20 s |
| `rear-palace-courtyard` | court-mystery | open air, still stone, a door closing out of sight |
| `palace-garden` | court-mystery | insects, a fountain two courtyards off, a wall you can hear through |
| `pleasure-quarter-night` | court-mystery | a narrow street, a canal, a lantern, a doorway |

A bed is **room tone with a distant event in it**, not silence. A corridor is not
quiet; it is a long hard space with a far end. 75 s each, two takes, mono.

```sh
tools/normalize-audio.sh tmp/craft-src assets/_extends/craft/effects
```

`I_TARGET` stays at its default for these — a bed is **−26 LUFS**. (Note that
`inspect-pool.py` reports `mean_volume`, which reads around −22 to −24 dB for a
correctly normalized bed: `volumedetect` is an unweighted average and LUFS is
K-weighted and gated. The column is for spotting a clip that is an outlier among
its neighbours, not for hitting a number.)

## Step 3 — the eighteen foley

Three takes each, keep two. The shot list is the table in each pack's
`RECORDING.md` — the microphone, the exact action, and a fill-in row per clip
that is the receipt for the pack's `LICENSES.json` line.

Contact mic **and** a 15 cm omni. Three takes, and the *least noisy* one wins —
not the loudest, which is the one you will over-edit.

```sh
I_TARGET=-20 TP_TARGET=-1 tools/inspect-pool.py assets/_extends/craft   # before
I_TARGET=-20 TP_TARGET=-1 tools/normalize-audio.sh tmp/craft-src assets/_extends/craft/injects
I_TARGET=-20 TP_TARGET=-1 tools/inspect-pool.py assets/_extends/craft   # after
```

(Those two environment variables only apply to `normalize-audio.sh`; the prefix
on `inspect-pool.py` is a habit, not a setting.)

The **two** takes are not a luxury. `pick` rolls once for the sound and once,
decorrelated, for the take, and that second roll is the only reason take 2 plays
anywhere. One take means one sound heard identically for the length of the book.

Fill in `RECORDING.md` as you go. A provenance claim with a receipt is worth a
great deal more than one without, and re-recording a single clip later is then a
one-line change.

## Step 4 — the fifteen tracks

All fifteen prompts, the shared negative list, the loop recipe and the order are
in [`TRACKS.md`](../assets/_extends/court-mystery/TRACKS.md). Two things that
are easy to get wrong:

* **Generate 3:00 or longer and cut to 150 s.** Clips in the pool run 110–202 s
  and a 30-second track is a fault you want to see here rather than in a merge.
* **The loop is baked anyway**, even though the mixer now crossfades the seam. A
  file whose own tail and head do not meet still sounds wrong *under* a
  crossfade. The recipe is in `TRACKS.md`; it is a 4-second `acrossfade` of the
  last 30 s onto the first.

**Listen to the seam before you ship the file.** The measurement that motivated
the mixer change, on a 6-second test clip with a 50 ms end fade, mean volume
over a 0.3 s window:

```sh
for t in 1.0 4.8 5.9 10.8 11.9 16.8 19.5; do
  printf '  t=%-6s ' "$t"
  ffmpeg -v info -ss "$t" -t 0.3 -i the-clip.wav -af volumedetect -f null - 2>&1 \
    | grep mean_volume
done
```

Good: the same number at every `t`, seam and no seam. A dip at the period of
the clip's own length is a loop that was not baked.

## The gate

```sh
cargo test -p bm-core --test pack_gates -- --nocapture
```

Read its output, do not just look at the exit code. It prints, per pack, the
resolved counts and every file still pending:

```
court-mystery: 23 rules, 15 palette values, 16 effect / 23 music / 48 inject sounds — all gates pass
craft: 19 clip(s) named but not yet on disk …
```

It also checks the two things that are otherwise **silent**, and it found both
on its first run against this very pack: a palette value inherited with nothing
answering it (`battle`, after `weapons` was dropped), and two effect tags with
no bed. A mood with no track is silence in every chapter that declares it; an
effect tag with no bed is a scene that comes out dry — and unpooled effect tags
are *discarded*, not refused, so nothing reports it.

## The failures that are silent

Six ways a pack can look finished and not be.

| | symptom | check |
| --- | --- | --- |
| **a bed with no clip** | the scene is dry; the tag is discarded and nothing says so | the gate; `inspect-pool.py` |
| **a mood with no track** | silence wherever that mood is declared; no error | the gate |
| **a stereo clip** | thrown away at the first `aformat` — a silent clip that passed inspection | `inspect-pool.py` shows `48000/2` |
| **no room tone under a clip** | fine in isolation, thin in the mix, and unfixable later | re-record the tone and check the floor |
| **a bed at inject loudness** (−20) or an inject at bed loudness (−26) | the layer is wrong, and it is wrong *quietly* | compare against its neighbours in `inspect-pool.py` |
| **a track with a vocal** | a hummed line under a narrator is indistinguishable from a mistake | listen; the negative list is in `TRACKS.md` |

The common thread: **not one of these fails a build.** A merge that reaches for a
missing clip warns once and plays nothing, and a warning in a log nobody reads is
not a check.

## The audition

Nothing above proves the mix works. Two checks, in this order.

**A dry digest, before any render** — the cheapest test there is, and it is the
one that tells you whether the *vocabularies* are working. Read the `music` and
`scene` values the analyzer chose:

* **ch. 12 *The Threat*** — the tray crash at the top. Does any `scene` name a
  place, and is `tray-drop` placed at the seam?
* **ch. 13 *Nursing*** — a sickroom. Does anything reach `illness`?
* **ch. 16 *The Garden Party*** — the crowd and the fire. `court` and `busy`.

If no `scene` label says *pavilion* or *apothecary*, the place vocabulary is not
reaching the model and no amount of audio will help. If no `music` value is ever
`wry`, the palette's notes are not doing their job.

**Then one merged chapter, and listen to the mp3.** Not the numbers — the file.
The things to feel, in the order they are audible:

* the **duck**: 14–33 dB under the narration, recovering between lines. If the
  music fights the voice, the fix is the duck, not the level;
* the **loop seam**, once every couple of minutes;
* the **pause lift**: the music steps up for the 2.0 s beat at a narrated scene
  change, and only there;
* whether the **place** is audible at all. `max_coverage` is 0.28 and this book
  is mostly one character in a marble room; if the effect layer is doing nothing,
  that is the design working, not a fault.

### Before the audition, two things are still true of the source

Both are crawl defects you declined, and both are audible in the mix, so a poor
audition is not always the sound design's fault:

1. **Quoted glossary terms are read as dialogue** — `"rear palace"` and
   `"flower garden"` arrive as 11–13 character dialogue segments. A narrator
   reading a quoted term aloud in a clipped voice is a voice problem, not a mix
   problem.
2. **Segments run up to 1,930 characters**, about 129 seconds in one breath.
   Every music, duck and pause decision is calibrated against segment
   boundaries, and a 129-second unbroken stretch of narration is a long time to
   hold one cue under.

### Sweep the whole book cheaply

The effect and music layers are data, so you can ask what a chapter would *ask
for* without rendering it. Take a scene vocabulary and a mood from a real
chapter, and look them up in the pack:

```sh
python3 -c '
import json
d = json.load(open("assets/_extends/court-mystery/scene-map.json", encoding="utf-8"))
for w in ("jade pavilion", "infirmary", "pleasure district", "garden"):
    for r in d["rules"]:
        if w in r["match"]:
            print("%-20s -> effect %s  reverb %s" % (w, r["effect"], r["reverb"]))'
```

A label that resolves to `effect: []` and a reverb is not a dry scene — it is a
room. (Percent formatting rather than an f-string with a subscript, which needs
Python 3.12 and this machine runs 3.9.)

## Release

```sh
bm-inductor profile manifest craft          --piece pack --dep
bm-inductor profile manifest court-mystery  --piece pack --dep
```

`--dep` is what produces a **sanitized root pack**: the pack as itself, rather
than as somebody's dependency carrying its parents' content inside itself. Both
of these will refuse while a dependency has moved; `--force` is the deliberate
override, not the default.

The release is self-contained — a resolved `assets/` — so a box needs none of the
dependency releases. See [ASSETS.md §Releases](ASSETS.md#releases-self-contained-and-one-per-piece).

## The checklist

Per pack, in order. The ticks are the *artifact*, not the intention.

```
[✓]  9 world tracks copied into court-mystery/music/
[ ]  room tone recorded for every room, and kept
[ ]  5 beds recorded, 2 takes each, at bed loudness
[ ]  18 foley sounds recorded, 3 takes each, 2 kept, at inject loudness
[ ]  15 tracks generated, looped, at bed loudness
[ ]  RECORDING.md filled in, one row per clip  (the LICENSES receipt)
[ ]  TRACKS.md prompts used as written
[ ]  inspect-pool.py: 0 pending, every row 48000/1
[ ]  pack_gates: no pending lines for the pack
[ ]  dry digest of ch. 12 / 13 / 16 — music and scene values read
[ ]  one chapter merged, and the mp3 listened to
[ ]  released with --dep
```

## See also

* [SOUND.md](SOUND.md) — the three layers, the knobs, the spec, and how to
  record or generate a clip
* [ASSET-PACKS.md](ASSET-PACKS.md) — the pack model, and the gate explained
* [ASSETS.md](ASSETS.md) — composition, resolution, releases
* [ROADMAP.md](ROADMAP.md) — the workspace pack this is deliberately deferred
  behind
