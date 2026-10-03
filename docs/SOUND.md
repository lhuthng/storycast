# Sound: the three layers, and how to author them

**In one line:** how to make the music and effects, and what a clip has to sound
like before it is allowed in.

*You can stop reading after §1.* Sections 3 and 4 are the studio reference, and
are written to be usable by someone holding a microphone, or a Suno prompt box,
with no access to this repository's history. Nothing here needs you to read
Rust.

How to make the art. [ASSETS.md](ASSETS.md) is how *composition* works — what
merges into what and why; [ASSET-PACKS.md](ASSET-PACKS.md) is the recipe for
building one; [AUDIO-NEEDS.md](AUDIO-NEEDS.md) says which sounds are still
missing.

## 1. The three layers

Three independent layers sit under the voice. They are placed on the
**delivered** clock — after the speech has been retimed — and the merge tempos
the **speech only**, so no clip is ever heard at `atempo`.

| layer | a bed or a spot? | who chooses it | registry | clips |
| --- | --- | --- | --- | --- |
| **effect** | bed — fills a window, loops | the scene map's rules, from the script's `scene` label | `effect-pool.json` | `effects/` |
| **music** | bed — continuous, crossfaded | the scene map's `music_palette`, from the script's `music` value | `music-pool.json` | `music/` |
| **inject** | spot — placed at a seam | the script, which names the sound | `inject-pool.json` | `injects/` |

They are three layers rather than one "sound design" because they answer three
different questions. *Where are we* is the effect layer. *What does this feel
like* is the music layer. *What just happened* is the inject layer. Collapsing
any two of them is how a shop at dawn came out with a hearth crackling under it —
the place and the mood fighting over one field.

An **effect** bed is chosen by a rule and stretches to fill its window. The
layer must stay sparse: a window opens on a scene that names effect tags, lasts
at least `min_span_s`, is cut to `max_window_s`, waits `cooldown_s` of silence,
and a chapter may not spend more than `max_coverage` of its runtime on the
layer. It is a room tone, not a soundtrack.

A **music** cue is continuous and crossfades where the script's `music` value
changes — which is the only place a track changes, so `xfade_s` is what makes an
in-chapter change audible as a change rather than a cut. A mood repeated across
twenty segments is one cue, not twenty.

An **inject** is a spot effect the script places itself, at a seam: the point
between two lines, or between the two halves of one line it split. How a clip
*behaves* is a property of the clip, not a per-chapter choice, and that is what
`mode` is for.

## 2. The gain chain and the eight knobs

In order, once each:

```
voice ──▶ per-scene SoX treatment (room + character) ──▶ voice + effects
                                                   │
                     effect ──▶ trim ───────────────┤
                     music  ──▶ level ──────────────┤──▶ one sidechain ──▶ mix ──▶ limiter
                     inject ──▶ level ──────────────┘     (keyed on the voice)
```

**The treatment touches the Narrator too, at a tenth of its depth.** Every
spoken slot is run through its scene's treatment — a room, a tone, a character —
and the Narrator takes the *same* treatment at 0.1 wet: in the scene, never
standing with a character's full wet. So a chapter of narration is read inside
the place rather than dry in front of it,
which is a real change from the old "the reverb never touches the Narrator".

The chain is SoX's (`reverb`, `overdrive`, `chorus`, `echos`, `treble`, …), not
ffmpeg's. A treatment that decays declares `tail_s`, the seconds of decay
**reserved** after the line so a room rings out under the next one instead of
being chopped at the seam; the mix takes the *longest* reachable tail once and
extends only the chapter's end. Every line, treated or not, still gets a 0.1 s
edge fade. SoX's `reverb` is a true feedback network that never extends its own
output, so the tail is padded on *before* the chain runs.

**One sidechain, on the voice bus**, applied to both layers as a single bus — so
"every layer drops whenever anyone speaks" is a property of the signal path and
not a rule each layer has to remember. The key is the whole voice track, which
is why the narrator ducks them exactly as a character does.

The measured calibrations, which are what the numbers were chosen against:

* `music.level` **0.16** sits about **30 dB under the voice** — inaudible if you
  are listening for it, which is the point. A score is heard at the chapter's
  headline, inside the planned beats and between the lines.
* `duck` at **6:1, threshold 0.02** is **12–28 dB** under narration, recovering
  the moment the narrator stops.
* **Raising a layer's level instead of trusting the duck makes the mix fight
  itself.** The contrast the duck buys is the whole reason the layer exists.

### The knobs, one line each

| knob | shipped | what it is |
| --- | --- | --- |
| `effect.max_coverage` | 0.35 | the most of a chapter's runtime the effect layer may occupy |
| `effect.cooldown_s` | 45 | silence after a window closes before the next may open |
| `effect.min_span_s` | 20 | how long a scene must name effect tags before a window opens |
| `effect.max_window_s` | 75 | how long one window may run |
| `effect.fade_s` | 0.3 | a bed's own edges |
| `effect.end_fade_s` | 3.0 | a looped bed's closing edge, so it is not cut at the chapter's end |
| `effect.trim` | 0.5 | **the effect layer's master gain** |
| `music.level` | 0.16 | **the music layer's master gain** |
| `music.pause_level` | 0.20 | the same number for a planned beat; must stay **above** `level` or the beat is a hole, not a lift |
| `music.ramp_s` | 0.6 | how long that lift takes; must stay well under the shortest pause |
| `music.fade_s` | 3.0 | a cue arriving or leaving inside 0.3 s reads as a cut |
| `music.xfade_s` | 2.0 | the crossfade where the track changes |
| `inject.level` | 1.0 | the inject layer's master gain |
| `inject.fade_s` | 0.3 | a retrigger; two boils never stack +6 dB |
| `inject.stop_fade_s` | 3.0 | a `stop`; shorter reads as a cut |
| `inject.default_hold_s` | 2.0 | solo seconds before a `trail`'s tail ducks under speech |
| `inject.tail_fade_s` | 1.0 | a natural tail end, clamped to half the clip |
| `inject.loop_xfade_s` | 0.25 | the crossfade at each seam of a `looped` inject |
| `pause.pause_s` | 1.5 | a beat held before a scene's first line; must outlast `duck.release` (500 ms) or the music never lifts inside it |
| `pause.max_per_chapter` | 1 | and at most this many of them |
| `duck.threshold` | 0.02 | the voice level at which the sidechain starts |
| `duck.ratio` | 6.0 | how far it drops |
| `duck.attack` / `release` | 20 / 500 | ms |
| `duck.head_key` | 0.0 | the one exception to "drops whenever anyone speaks": while the chapter's headline is spoken the key is held at this gain, so the opening cue comes up under the title at its own level instead of being ducked under the one line it was written for |

### The three-rung rule

This is the thing people get wrong when retuning, so it is worth stating on its
own:

* a **rule's** `level` balances *scenes against each other* — how loud a storm
  is beside a hearth;
* the **layer's** `trim` / `level` balances *layers against each other* — how
  loud the whole effect layer is against the whole music layer;
* a **sound's own** `level` in the pool balances *that one clip* against the rest
  of its own layer.

Three different jobs, three different places. Putting a number in the wrong one
is why retuning somebody else's mix does not work.

## 3. The house audio spec

Every clip entering a pool is brought to one spec first, so that a `level` is a
gain over a *known* source and not a lottery. `tools/normalize-audio.sh` is the
only way in.

| | beds (`effects/`, `music/`) | foreground injects (`injects/`) |
| --- | --- | --- |
| integrated loudness | **−26 LUFS** | **−20 LUFS** |
| true peak | −3 dBTP | −1 dBTP |
| format | mono 48 kHz, 64k mp3 | same |
| silence | leading/trailing trimmed, 12 ms fade each end | same |
| metadata | all dropped — no Xing/LAME, no ID3v2 | same |

```sh
# a bed
tools/normalize-audio.sh tmp/src assets/_extends/<pack>/music

# an inject: foreground, so louder and tighter
I_TARGET=-20 TP_TARGET=-1 tools/normalize-audio.sh tmp/src assets/_extends/<pack>/injects
```

Four things about the script that are not obvious:

* **It leaves files already in the destination alone.** It is how a clip gets
  in, not a way to re-encode the pool. To replace a clip, delete it first.
* **Two-pass `loudnorm`, not one.** Single-pass is a *dynamic* normalizer and
  pumps on a bed. Pass 1 measures, pass 2 applies the measurement as a linear
  gain, so a bed's internal dynamics survive intact.
* **Intermediates go on the destination's filesystem**, not `/tmp`: a 600 s clip
  is a 57 MB wav on the way through, and `/tmp` shares a volume with the system
  disk, which is routinely the fullest thing on the machine.
* **The silence detector sits at −45 dB, not −50**, because it compares *peak*
  and a 64k mono recording's own room tone is around −45. This is why room tone
  in the recording is not optional — see §4.

## 4. Recording foley

For any sound that is a thing a person does rather than a thing a library
imagined. The foley in [`craft`](../assets/_extends/craft/RECORDING.md) and
[`court-mystery`](../assets/_extends/court-mystery/RECORDING.md) is nine clips
each, 0.4–2.2 seconds, recorded by one performer in one room.

**Why record rather than buy.** A stock bed is sixty seconds of somebody else's
room; you loop it, duck it and trim it, and what is left is a room that is
nearly right. A recorded pestle is 0.9 seconds of *this* book. The three
practical wins are a chain of title with no licence question, a clip short enough
to place precisely, and a sound that can be re-cut when the line turns out to
need it a beat earlier.

**The method, once for a whole pack:**

* 48 kHz.
* A **small dead room**. A large room puts a 200 ms tail on everything, and a
  0.4 s unstopper with a tail is a door.
* **Room tone first: 20 seconds of silence, per room.** This is the step people
  skip and it is the step that matters — see the −45 dB note in §3. A clip with
  no tone under it gets gated out of existence, and the symptom is a clip that
  sounds *fine in isolation* and is missing its bottom two octaves in the mix.
* A **contact mic and a close omni about 15 cm off**. The contact mic has the
  low end and the floor; the omni has the air. Every sound in a foley pack like
  these is small and wants the omni, because the point is that you can hear what
  it is.
* **Three takes**, and the *least noisy* one wins. Not the loudest.

**The naming rule.** `<sound>-<n>.mp3`, where the number is a **take index
inside one family** and carries no meaning. `day-1`, `day-2`, `day-3` are three
takes of `day`, never three sounds called "day one". Nothing in the mix reads
the number.

**Two takes a spot sound, not one.** `pick` rolls once for the sound and once,
decorrelated, for the take. That second roll is the only reason take 2 plays
anywhere. One take means one sound heard identically for the length of the book.

**`mode` is the clip's, not the script's.** `hit` holds its whole clip as
silence and punctuates; `overlap` costs no timeline time and runs under the
following speech; `trail` holds `hold` seconds solo and then tails under it. The
script names the sound; the clip says how it behaves. A per-chapter copy of that
is a second source of truth the model has to guess at — and it guessed wrong once
already, giving a water spell an `overlap` under a kitchen sink.

## 5. Generating music with AI

**Append to every prompt, without exception:**

```
instrumental, no vocals, no lyrics, no vocal humming, no choir, no riser,
no trailer drums, no big drop, seamless loop, 2 to 3 minutes
```

The negative list is the part that matters. "No vocal humming" because these are
beds under a narrator and a hummed line under a narrator is indistinguishable
from a mistake. "No riser, no big drop" because the layer crossfades over 2 s at
every mood change, and a track with a build in it pays off in the wrong place.

**The anatomy** — four parts in this order:

1. **register** — what kind of place or feeling this is, one clause
2. **instrument** — the specific instruments, *named*, not "orchestral"
3. **texture** — what the track does most of the time, which is usually *sparse*:
   silence inside a bed reads as space, and density reads as clutter under a voice
4. **what it must not do** — the second half of every prompt line

Worked examples, one per register, in
[`court-mystery`'s TRACKS.md](../assets/_extends/court-mystery/TRACKS.md):

```
imperial Chinese audience hall, ceremonial and still, single long dizi notes over
a low frame-drum pulse, bowed erhu drone, marble reverb, stately restrained,
sparse, no melody movement

close-mic mystery, sparse pizzicato like a pipette, dry wooden percussion,
irregular ticking pulse, low sustained erhu, forensic clinical detail,
unsettling but calm

dry deadpan humour, pipa pluck landing a beat behind the beat, woodblock, muted
and small, ironic not cheerful, sparse
```

### Length, and the loop

Clips in the pool run **110–202 s**. Generate 3:00 or longer and cut down. The
music layer renders a run longer than its track as a **crossfaded loop** — the
same graph the inject layer uses for its looped beds — so the seam is the
mixer's job, not yours. (It used to be `-stream_loop -1`, a hard butt-join,
which made the seam a small dip once every couple of minutes: measured at
−44.0 dB at the seam against −43.5 dB either side of it, on a test clip with a
50 ms end fade.)

Bake a clean loop into the file anyway, because a file whose own tail and head
do not meet still sounds wrong *under* a crossfade:

```sh
ffmpeg -i raw.wav -filter_complex \
  "[0:a]atrim=0:150,asetpts=N/SR/TB[t];[0:a]atrim=150:180,asetpts=N/SR/TB[h];\
   [h][t]acrossfade=d=4:c1=tri:c2=tri" -t 150 loop.wav
```

Then `tools/normalize-audio.sh` into `music/`.

### The registry is the truth

Nothing in the mix reads a filename. It reads the **key** and that key's
`files`. The filename spelling is a suggestion; the registry is the authority.
`soft-cute` is not a mood, it is a sound whose tags are `[romantic]`, and a
script's `music: romantic` is what selects it.

## 6. Registering a clip

Three registries, same shape: `sound -> {tags, files, ...}`. The filename only
the suggestion; the registry the truth.

```json
"mortar-grind": {
  "tags": ["mortar", "pestle", "grinding", "grind", "powder", "pharmacy", "medicine", "herbs", "apothecary"],
  "files": ["injects/mortar-grind-1.mp3", "injects/mortar-grind-2.mp3"],
  "mode": "hit", "level": 0.7, "looped": false, "dur_s": 0.9
}
```

| field | effect | music | inject |
| --- | --- | --- | --- |
| `tags` | what a rule matches on | what a palette value matches on | what the model reaches for |
| `files` | the takes | the takes | the takes |
| `looped` | **state it** — the struct's default is `true`, so an inject entry that omits it silently becomes a bed | — | |
| `mode` | — | — | `hit` / `overlap` / `trail` |
| `dur_s` | — | — | how long the clip is, so the digest never `hit`s a 51 s boil |
| `hold` | — | — | solo seconds before a `trail` tails under speech |
| `level` | this clip alone, against the rest of its layer | same | same |

### The two gates that bite

**Every palette value but `none` must have a pooled track that answers its
tags.** A value with no track is a mood the analyzer is offered and that is
silent in every chapter which declares it — not a warning, a hole. The music
layer's design is that a mood with no track is *simple silence* rather than
something wrong, which is a real property, but it is only a useful property if
you meant it. `cargo test -p bm-core --test pack_gates` checks it for every
pack under `assets/_extends/`, and `--nocapture` prints what is still pending.

**An effect tag that no pooled bed answers is silently discarded.** This is the
single most expensive mistake available in this system. The scene does not fail;
it scores zero and comes out dry, chapter after chapter, with no error anywhere.
Unlike `music`, an unpooled effect tag is *dropped* rather than refused. So: a
new effect tag needs a new **bed**, not a new alias.

A third, smaller one: a sound with **no `files`** can never be picked, and the
loader drops it without a word.

### Tags are the API

`inject_prompt` renders the pool into the digest prompt as
`mortar-grind (hit; mortar, pestle, grind, pharmacy, medicine, herbs, powder; 0.9s)`.
The model can only reach a sound whose tags contain a word it would have written
in the first place.

So the two tables divide the labour:

* the **pool** is tagged in **physical nouns** — what the sound *is*:
  `mortar`, `pestle`, `porcelain`, `silk`;
* **`tag-aliases.json`** is where the **model's words** go: `apothecary` →
  `mortar-grind`, `dispensing` → `bottle-unstopper`, `fixing-hair` →
  `hairpin-set`.

Neither file is edited when the other changes, and a synonym costs no repair
round because aliases are applied *before* validation.

The same table has an `effect` member mapping to **tags** rather than to sounds
(`town` → `street`, `apothecary` → `workshop`), and a `music` member mapping to
**palette values** (`ominous` → `tense`, `ceremonial` → `court`).

## 7. Place words, bed words, and the two vocabularies

**This is the confusion that cost the effect layer most of its reach, and it is
worth a section of its own.**

A `scene` label is matched against the scene map's **rule match words**. Those
are the **place** words. Separately, a rule names effect **tags**, and those tags
are matched against the effect pool — those are the **bed** words.

The two are different lists, and the prompt used to hand the model the bed words
and call them "the vocabulary it answers to", ending with the warning that a
label built from anything else "gets silence". On the shipped map that was
**15 of 61** match words — so `palace`, `hall`, `garden`, `gate`, `morning`,
`dusk`, `temple` and `tavern` were words the rules were waiting for and the
model had been told not to use. The existing thirteen rules were running on the
accidental intersection.

Both lists are now injected, as two lists, and rule 9 says so. If you write a
rule, its match words reach the prompt automatically — that is why
`scene_prompt` renders them from the map rather than any prompt holding its own
copy. **A pack cannot edit a prompt at all**, which is the same reason the music
palette is pack-side.

### Checking your own pack

```
cargo test -p bm-core --test pack_gates -- --nocapture
```

and, to see the ratio directly, compare the place words your rules can match
against the bed words your pool answers:

```sh
python3 - <<'PY'
import json, glob
tags = set()
for f in ['assets/effect-pool.json'] + glob.glob('assets/_extends/*/effect-pool.json'):
    for k, v in json.load(open(f, encoding='utf-8')).items():
        if not k.startswith('_'):
            tags.update(v.get('tags', []))
words = set()
for f in ['assets/scene-map.json'] + glob.glob('assets/_extends/*/scene-map.json'):
    for r in json.load(open(f, encoding='utf-8')).get('rules', []):
        words.update(r.get('match', []))
reach = words & tags
print(f'{len(reach)} of {len(words)} rule match words are bed tags')
print('unreachable:', ', '.join(sorted(words - tags)))
PY
```

**The place is written in English**, even in a Vietnamese book: the scene map
matches whole English words, and a `scene` label in Vietnamese matches nothing
and reports no error. `cung dieu` is `palace`, `vuon` is `garden`.

## 8. The mood vocabulary, and how it grows

Fifteen values, in four groups. Every one of the five new ones had to be written
as a **contrast** rather than a definition, and the table's right-hand column is
the whole reason it works.

| value | tags | it means | it is **not** |
| --- | --- | --- | --- |
| `quiet` | soft, calm | low and unobtrusive — the default under narration | |
| `warm` | warm, indoor | hearth, home, tavern, friendly company, a meal | |
| `busy` | market, busy | crowds, streets, shops, public bustle | |
| `grand` | energetic, upbeat | triumph, revelation, awe, a turning point | |
| `sad` | sad, sorrow | grief, loss, farewell, mourning | |
| `romantic` | romantic | tenderness, courtship | |
| `playful` | playful, light | humour, games, warm comedy | |
| `tense` | tense, uneasy | dread, suspicion, something wrong, waiting | |
| `none` | — | silence, deliberately | |
| `battle` | battle, intense | combat, pursuit, immediate danger | |
| `court` | court, ceremonial | the palace in session — being watched | a triumph, which is `grand` |
| `inquiry` | inquiry, clinical | close reading, a diagnosis forming | dread, which is `tense` |
| `illness` | illness, frail | a body failing, slow poison | grief, which is `sad` |
| `wicked` | scheming, whispers | intrigue as venom, a plan behind courtesy | fear, which is `tense` |
| `wry` | wry, deadpan | a flat observation, humour with **no warmth** | cheerfulness, which is `playful` |

**The vocabulary is closed on purpose.** A free-text mood label matched by keyword
is how 345 scene labels ended up fighting 14 rules. A value outside the palette
is a *refused chapter* with a repair round, not a silent one — which is also why
you cannot simply delete a value: every script already on disk that declared it
would fail to merge. `battle` ships unused in the palace preset precisely because
removing it would strand them.

**The growth rule: add a value only when an existing one is wrong for a whole
book, never for a scene.** "Wrong for a scene" is what `none` is for.

### The `note` is the interface

The prose around the palette belongs to the **adapter**; the palette belongs to
the **pack**. A pack cannot edit a prompt, so the `note` is the only channel it
has to the analyzer. That is why a good `note` says what a mood is **not**:

> a flat interior observation, humour with NO warmth in it — NOT cheerfulness,
> which is `playful`

A note that says what a mood *is* teaches nothing the analyzer did not already
assume. A note that says what it is confused with disambiguates the pair it gets
confused with, which is the only error worth spending tokens on.

### A preset must answer the whole palette

The palette lives in a root; **no root holds a score**. So a preset inherits the
*mood vocabulary* and has to answer every value of it, even the ten it did not
write. The eight world tracks in
[`court-mystery`'s music pool](../assets/_extends/court-mystery/music-pool.json)
are exactly that: not a dependency, a carry. Drop them and `quiet`, `busy`,
`grand`, `romantic` and four others become silent in every chapter that uses
them, and nothing reports it. The gate is what tells you.

## 9. Generating a clip locally

Every layer of a pack can be generated on this machine instead of pasted into a
service: **places** and **moments** from Stable Audio 3 **Small-SFX**, and a music
pack's **tracks** from Stable Audio 3 **Small-Music**. Both are 433 M params,
120 s maximum, 44.1 kHz stereo, CPU-capable, and — the reason one engine serves
both — they share the **SAME-Small** autoencoder and the same repo, venv and CLI,
differing only in the checkpoint. `tools/gen-sound.sh` wraps the pair; `--level`
picks the tier.

```sh
tools/gen-sound.sh setup                     # once: checkout, venv, revision pin
tools/gen-sound.sh one --prompt "heavy rain on tiled roofs, rolling thunder some
  distance off, no voices, no music, seamless" \
  --as storm-2 --into assets/_extends/common/effects --level place
tools/gen-sound.sh one --prompt "busy market street, pipa and clappers, quick
  light rhythm, instrumental" \
  --as market-bg-1 --into assets/music --level track --tags market,busy
tools/gen-sound.sh batch --list sounds.tsv   # one line a clip, resumable
tools/gen-sound.sh list                      # what is installed, and where
```

Stable Audio **2.0 is not an option for local work at all**: it ships no weights
and runs on Stability's platform only. Medium (1.4 B, 380 s) is CUDA-only and is
not wired here — this target is an Apple silicon laptop. So the local score is
Small-Music, and its ceiling is the one real limit of this route: **120 s per
track**, looped through the crossfade like any other music bed. A track that
wants half an hour of development has to come from a service or a recording;
`normalize-audio.sh` takes both.

Four things decide how a generated clip is treated:

* **the layer sets the rung.** A place lands at `-26` LUFS, under the voice; a
  moment at `-20`, in front of it; a track at `-23` with 96k, the music house
  spec. A moment **under about two seconds is placed by its peak instead**
  (−3 dBTP): integrated loudness does not describe a transient, which is exactly
  what the pool's note says about its own `coin`, `swoosh`, `wood-chop` and
  `bell` takes, and why those sit at −3 to −5 dBFS rather than on a LUFS rung.
  So what arrives in a pool is the same spec as anything recorded by hand.
* **a one-shot is asked for on a longer canvas than it is.** `--seconds` means
  the *finished* length; the model is asked for at least `BM_SA_CANVAS` seconds
  (3 by default) and the result is cut back to it, after its run-up is dropped
  so the event starts the file. This is not tuning: asked for 0.6 s of "a small
  temple bell struck once", the model returns a full-scale block with no event
  in it at all; asked for 4 s on the same prompt it returns a strike and a ring.
  The requested length is what decides that, not the words: one prompt that comes
  back as a hit with silence behind it at 3.0 s comes back at 3.2 s as a
  continuous block with no decay in it, and the *same seeds* flip with it. So a
  take that arrives wrong is a length to re-roll before it is a prompt to
  rewrite. The model also leaves a low-level floor under its event (around
  −57 dB), which the pool's `-45 dB` trim floor would read as silence and cut
  away — so a generated moment is trimmed at `-70 dB` and keeps its decay and its
  room.
* **a take is chosen, and be careful what you rank on.** A generation is about a
  second, so a doubtful take is answered by a handful of candidates — one prompt,
  a few lengths — kept with `--from` (which still runs it through the same cut,
  fade and level rule as a fresh generation) or pinned with `--seed N` so the
  same command makes the same clip again. But a whole-clip average cannot see
  *when* the sound happens inside the clip: a dull knock and a stone breaking
  apart can share a spectral centroid and a decay time and be completely
  different sounds. Three `common` moments shipped on exactly that mistake — a
  knock where a break should roll, a swell where a swallow should click, a
  fade-up where a flutter should start at full level — and what separated them
  was in the transient, not the average: attack time, the count and spacing of
  onsets, crest factor. **So this route is for long takes** — a place, or a music
  pack's track. A one-shot a second or two long wants a microphone.
  `--cfg` and `--negative` steer a generation and take the optimized runtime's
  flags.
* **a track is registered, not just written.** Music is a sound in a pool with
  tags the palette matches, so `--level track` hands the result to
  `add-music.sh`, which appends the take to `--sound` in `music-pool.json` and
  warns when the tags answer no mood. The sound key comes from the take name
  (`market-bg-2` is another take of `market`); `--pool` defaults to
  `music-pool.json` beside the destination directory.
* **120 s is the ceiling, and 90 s is the default for a place.** Not arbitrary:
  the effect layer still loops with `-stream_loop -1` and a window is at most
  `max_window_s` (75 s), so a bed of 90 s or more plays **once per window** and
  has no seam. A 60 s bed splices at every window boundary until that layer
  crossfades like the other two.

It never overwrites: a name already in the destination is skipped, which is what
makes `batch` resumable.

### The weights are gated; the optimized runtimes are not

`stable-audio-3-small-sfx` and `-small-music` are **gated** repos on Hugging
Face. Access is auto-approved the moment you click agree, but you do have to be
logged in, and without that the first generation stops with a 401:

```
click agree at https://huggingface.co/stabilityai/stable-audio-3-small-sfx
export HF_TOKEN=hf_...        # or, in the engine: uv run hf auth login
```

The same checkout ships `optimized/` runtimes that need **no account at all**,
because they pull the same two tiers from `stabilityai/stable-audio-3-optimized`,
which is public. On Apple silicon that is a one-time:

```sh
cd engines/stable-audio/src/optimized/mlx
./install.sh -y --download sm-sfx,sm-music     # ~1.3 GB a tier, and its own venv
```

`gen-sound.sh` then picks that runtime up on its own — `list` prints which CLI
and flavor it chose — and it is about twice as fast as CPU torch: a 90 s bed in
about 12 s end to end, model load included. `optimized/tflite` is the same idea
on plain CPU anywhere, x86 included. Both take `--dit`/`--seconds` instead of
`--model`/`--duration`, which is what `BM_SA_CLI_FLAVOR=optimized` translates;
setting `BM_SA_CLI` by hand overrides the auto-detection, and the layer rungs,
the revision pin and the normalize hand-off are unchanged either way.

One more thing the wrapper does that the model does not: it measures the peak of
what came back and attenuates if it sits above full scale. The optimized runtime
writes float wavs and can put the peak **above** 0 dBFS — a 0.6 s bell came back
at +6.7 dBFS — and converting that to the pool's int16 is a wall of static.

`setup` pins the checkout's revision in `engines/stable-audio/revision`, and
later runs refuse once it has moved — the same prompt at two revisions is two
different clips, and nothing downstream can tell them apart. The weights
download into the HF cache on first generation.

The model's licence is the **Stability AI Community Licence**: read it before
this audio is distributed, and put the line in the pack's `LICENSES.json` (§6).
Prompts are not stored in the script — they live with the reason for them, in
[AUDIO-NEEDS.md](AUDIO-NEEDS.md).

## 10. Shaping, triaging and mutating clips with SoX

SoX is now **both** an authoring tool *and* a runtime dependency of the merge,
and the split is deliberate. It is the merge's voice-treatment engine (§2): the
per-scene room and character chains in `scene-map.json`'s `reverb_presets` are
SoX effect lists, run per slot by `ambience.rs` — because SoX's `reverb` is a
true feedback network that actually rings, while ffmpeg 9 has no reverb filter
and its convolution `afir` is broken on this build. ffmpeg still does everything
else: decoding, resampling, the beds, the placement and the final mix.

Two things stay ffmpeg's for the same reason they always did. `normalize-audio.sh`
is ffmpeg because of the one thing SoX does not have: **EBU R128 integrated-
loudness normalization** — a clip's `level` means something only because every
clip sits on a known rung, and `loudnorm` is what puts it there. And ffmpeg still
**decodes** every input (48 kHz mono f32 wav), so one decoder and one resampler
serve the whole pipeline. A file SoX authors still goes through `add-sound.py`
for the spec check, the rung, the registry edit and the aliases: nothing in §3
changes, `normalize-audio.sh` is still the only way into a pool. Because the
merge now needs SoX to run at all, the worker's `merge` capability is gated on
`sox` being on PATH, beside ffmpeg (see [ARTIFACTS.md](ARTIFACTS.md)).

```sh
brew install sox            # macOS;  apt install sox  on Debian/Ubuntu
tools/shape-sound.py doctor        # what is present, and what each command needs
tools/shape-sound.py self-test     # the pure helpers, needing no sox at all
```


### `triage` — the take-ranking problem, measured

SOUND.md §9 says a whole-clip average cannot see *when* a sound happens inside a
clip. SoX `stat` gives crest factor (peak/RMS — high is a hit, low is a bed), the
deltas and DC offset; with numpy present, the raw stream adds attack time and the
count of onsets.

```sh
tools/shape-sound.py triage tmp/takes/*.wav --sort crest
```

The pick wants **two takes that differ**. Take the pair whose crest and attack
differ most — those are the two the second roll will actually distinguish.

### `variants` — a second take when nobody can re-record

`pick` rolls a second, decorrelated take, and a one-room foley session often
yields only one. A *subtle* pitch/tone/gain variation is better than nothing, and
the parameters are deterministic from `--seed`, so a variation worth keeping can
be re-made.

```sh
tools/shape-sound.py variants tmp/pestle.wav --count 2 --seed 4 \
    --pitch 25 --tone 1.0 --gain 0.5
```

These are variations, not recordings. Listen before keeping one.

### `mutate` — the palette of deliberately strange

`tools/shape-sound.py recipes` lists them: `ghost` (reverse into reverb and back
for a tail that arrives), `demon`/`giant`/`sprite` (pitch), `alien`, `underwater`,
`radio`/`megaphone`, `lofi`, `cave`, `shimmer`, `drone`, `stutter`, `robot`,
`grit`, `muffle`, `pulse`, `reverse`. `--amount` scales intensity 0.1–3.0.

```sh
tools/shape-sound.py mutate tmp/door-close.wav --recipe ghost --amount 1.4
tools/shape-sound.py mutate tmp/wind.wav --recipe demon --amount 0.6 --as wind-demon
```

A recipe makes a **clip**, and a clip's character is baked in before it enters a
pool — so naming one here hides nothing from the scene map. That is the
difference between this and a mix knob.

### `synth` — a source clip from nothing

SoX's `synth`/`tremolo` vocabulary is richer than ffmpeg's `sine`/`anoisesrc`,
and a generated bed is a legitimate starting point for a place the model or a
microphone cannot reach.

```sh
tools/shape-sound.py synth --effect "synth 3 sine 90 sine 93 tremolo 1.2 35 reverb 40" \
    --as low-drone --seconds 3
```

Every command writes into `refs/temp/clips/shape-sound/` (gitignored) and prints
the `add-sound.py` line that registers the result — which is where the rung,
the alias table and the pool check happen.

## See also

* [COMPLETING-A-PACK.md](COMPLETING-A-PACK.md) — the runbook for finishing a
  pack whose registries are authored and whose audio is not
* [ASSETS.md](ASSETS.md) — how composition, resolution and releases work
* [ASSET-PACKS.md](ASSET-PACKS.md) — how to build a pack, with a worked example
* [PROFILES.md](PROFILES.md) — the three pieces and what each costs
* `assets/_extends/*/RECORDING.md` and `TRACKS.md` — the shot lists and prompts
