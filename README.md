# Storycast — turn a web novel into a multi-voice audiobook, on one machine or a whole cluster

Storycast takes a novel that exists as web pages and produces finished,
multi-voice audiobook chapters: **`Ch.42 - The Title.mp3`**, with a different
voice per character, pauses, and optional ambience — automatically, chapter
after chapter, optionally spread across several computers on your LAN.

It is **not tied to one novel, one site, or one language**. Point it at any
chapter URL template and it will crawl, dramatize and speak it.

## What you get when you clone, and what you bring

The repo is the _machine_, not the _material_. A fresh clone contains:

| In the repo                           | What it is                                                                                                                                                                                                            |
| ------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `rust/` (four crates)                 | The pipeline: scheduler, workers, dashboard, provisioning                                                                                                                                                             |
| `python/`                             | The TTS sidecar — **Vieneu** by default (local voice-clone TTS), with a **Gemini TTS** engine also built in                                                                                                           |
| `prompts/analyze.txt`                 | An **example** dramatization prompt (written for Vietnamese web novels). This is the main thing you edit for another language or genre — the program only requires that it returns the JSON shape described inside it |
| `voices.default.json`                 | The built-in catalogue voices the engine ships with                                                                                                                                                                   |
| `assets/`, `Makefile`, `.env.example` | Scene maps, ambience loops, one-command operations, config template                                                                                                                                                   |

Everything _specific to your book_ is created at runtime and git-ignored, so a
fresh clone is a valid empty state:

| Created by you / at runtime (ignored by git) | What it is                                                                                                         |
| -------------------------------------------- | ------------------------------------------------------------------------------------------------------------------ |
| `url_template` in `.bm/settings.json`        | Where the chapters live — `{n}` is the chapter number. **This is the only novel-specific setting you must change** |
| Edits to `prompts/analyze.txt`               | Your own dramatization style/language, if the shipped example doesn't fit                                          |
| `voices.json`, `voice-pool.json`, `refs/`    | Your cloned voices and reference clips (skip entirely to use the catalogue voices)                                 |
| `data/`, `output/`                           | Scripts, character bible, cached audio, finished MP3s                                                              |
| `.bm/`                                       | Ledger, settings, machine registry, logs                                                                           |
| `.env`                                       | Your API keys (from `.env.example`) — **on the inductor only**. A worker gets the keys its current task needs with the task itself |

So the same program converts any novel: the language, cast and voices all come
from your prompt, your URL template and your voice files — the code only knows
how to fetch a chapter → turn prose into a script of labelled segments → speak
each segment with the right voice → glue the audio together.

---

## 1. What it actually does (the 60-second version)

For each chapter, four stages run in order:

```
URL ──crawl──▶ clean text ──digest──▶ script-NN.json ──render──▶ per-segment audio ──merge──▶ Ch.N - Title.mp3
                                │
                                └─▶ bible.json        (who the characters are, kept across chapters)
                                    cast-vieneu.json  (which voice speaks each character)
```

- **crawl** — downloads chapter `{n}` from your URL template and cleans it into
  plain text.
- **digest** — sends the text + the character bible to an LLM with your prompt
  (`prompts/analyze.txt`). The LLM answers in strict JSON: who speaks, which
  pronoun/alias belongs to whom, and the chapter split into segments with
  speaker + mood + scene. Result: `data/script-NN.json`.
- **render** — speaks every segment through the TTS engine using the voice
  assigned to that segment's speaker. Each finished segment is cached, so a
  crash costs seconds, not a chapter.
- **merge** — concatenates the segments (gaps, plus the optional effect and
  music layers and the beats between scenes) into the final `Ch.N - Title.mp3`
  in `output/`.

A scheduler — the **inductor** — owns this state and hands chapters to workers
(**agents**), on this machine and on any boxes you add over SSH.

---

## 2. Install

Requirements: **Rust** (1.75+), **Python 3** (for the Vieneu TTS sidecar), and
an analyzer of your choice: a Gemini API key, [opencode](https://opencode.ai),
an OpenRouter key, or a local Ollama.

```bash
git clone lhuthng/storycast.git
cd storycast
make build               # compiles the Rust workspace

cp .env.example .env     # then edit: put your key(s) in
#   GEMINI_API_KEY=...      (or OPENROUTER_API_KEY, or nothing if you use opencode)
#   TTS_ENGINE=vieneu       (default; `gemini` for the API engine)
```

`.env` is the **single source of truth for keys, and it never leaves this
machine.** Worker boxes are provisioned by copying files, and `.env` is
personal and git-ignored, so it is deliberately not one of them — instead the
inductor sends each worker the keys the offered stage will read, with the task.
That means you set a key once, here, and a remote digest works with nothing
configured on the box at all. It also means the keys cross your LAN in the
task offer: keep the control API on a trusted network (it is unauthenticated
plain HTTP, like every other sidecar call in this repo).

The Python side of Vieneu lives in `python/` (`tts_vieneu.py`,
`tts_server.py`). A virtualenv with its dependencies is created for you when
needed — the first build downloads ~1.7 GB of model weights, once.

### Tell it about your novel

```bash
make tui        # press c, then paste your template, e.g.:
                #   https://example.com/truyen/any-novel/chapter-{n}
```

`{n}` is where the chapter number goes. The TUI saves it to
`.bm/settings.json` and immediately probe-crawls one chapter to prove the
selector finds the text. This URL template is the **only novel-specific thing
you must change** to convert a different novel (plus, if you want a different
dramatization style, `prompts/analyze.txt`).

### Tell it who speaks (optional, for cloned voices)

- `voices.json` maps character names → a clip in `refs/`, e.g.
  `{"Narrator": "refs/narrator.mp3"}`. Both are git-ignored: they are personal.
- `voice-pool.json` is the tag-matched sample pool. Add a clip with
  `bm-inductor roster add-sample refs/young-female-4.mp3` — tags come from the
  filename, and the clip is enrolled on every worker at the next provision.
- Add nothing and the engine's built-in catalogue voices are used; a per-engine
  accent policy assigns a voice to each character automatically.

### Give it atmosphere (sound design)

Chapters don't have to be dry voices. The dramatization prompt tags every segment
with two independent things, and the merge stage turns each into its own layer
under the voice:

- **`scene` is the place** (`"market-stall-morning"`, `"forest-night"`) and drives
  **effects**, which are sparse and deliberate. Consecutive same-speaker lines
  form a *run*; the run's majority scene label is matched against ordered keyword
  rules in `assets/scene-map.json` (first match wins), and a rule names *tags*,
  never files. A window opens on a scene that names effect tags, lasts at least
  `min_span_s`, waits `cooldown_s` after the previous window closed, and the
  chapter may not spend more than `max_coverage` of its runtime on the layer — so
  a bed marks the scene instead of running under the whole chapter.
- **`music` is the mood**, and drives **background music**. It is one value from
  a *closed* palette declared in `assets/scene-map.json` (`quiet`, `warm`, `busy`,
  `battle`, `grand`, `none`), and the palette names the tags the music pool
  matches on. Consecutive segments sharing a value are one cue, so **a chapter can
  change its background music as often as its feeling changes** — a change of
  value is where the track crossfades. The layer is quiet on purpose (`level:
  0.06` against the effects' `0.08`–`0.22`), and `none` — or a mood the pool
  cannot answer — plays **no music at all**; there is no silent filler track.
  Because the vocabulary is closed and validated at digest time, "this mood has
  no track" cannot happen by accident.
- **The two are separate on purpose.** A place is where we are, a mood is what it
  feels like. One string doing both jobs is how a shop at dawn (`martial-shop-
  morning`) once came out with a hearth crackling under it: the keyword `shop`
  matched a fire rule that sat *before* the daylight rule, so `morning` never got
  a say.
- **A beat where a scene changes.** At most `pause.max_per_chapter` per chapter,
  placed only where the change is *narrated*, with narration resuming outranking
  narration handing off. The music lifts to `pause_level` inside it — which is
  the sidechain releasing, so a beat shorter than `duck.release` never lifts at
  all.
- **Everything is ducked.** One sidechain compressor, keyed on the whole voice
  track, sits on both layers as a single bus — so "the layers drop whenever
  anyone speaks" is a property of the signal path rather than a rule each layer
  has to remember, and the narrator ducks them exactly as a character does.
  Rules can also attach a reverb preset for room feel.
- To customize: add clips to `assets/effects/` or `assets/music/` and register
  them in `assets/effect-pool.json` / `assets/music-pool.json` — the registry is
  the truth: one key per *sound* with its takes under `files`, and `tags` is what
  a scene matches on (written by hand, not derived from the filename), so
  `looped: false` marks a one-shot stinger; add or retune a **mood** in `scene-map.json`'s
  `music_palette` (the digest prompt is rendered from it, so the analyzer can
  offer it immediately), or reorder the place rules; or switch a layer off with
  `"ambience": false` / `"music": false` in `.bm/settings.json` (the stock TUI run
  screen doesn't yet expose either, but both are read at merge time). Normalize
  new clips with `tools/normalize-audio.sh <src> <dest>` first: the scene map's
  `level` is a gain over a −23 LUFS source, and that only holds if every clip in
  a pool was brought to the same spec.
- Chapters digested before segments carried `music` keep merging through the
  `legacy_scene_music` shim in `scene-map.json`, which scores their `scene` label
  to a palette value. It is a migration shim, not the design — delete it once
  every script on disk carries the field.

---

## 3. Start it — three ways, easiest first

### A. "Just run it on this machine" (solo, headless)

```bash
make serve START=1 COUNT=10      # terminal 1: inductor (scheduler + API on :8901)
make agent                       # terminal 2: a local worker
```

That's it — watch `output/` fill up with `Ch.N - Title.mp3`. Stop with `Ctrl-C`;
nothing is lost (see §4).

### B. "Run it and watch it" (recommended — the TUI does everything)

```bash
make tui
```

| Key                    | Does                                                                                                                                  |
| ---------------------- | ------------------------------------------------------------------------------------------------------------------------------------- |
| `:`                    | **Command line — every operator action runs here**: `:a` add machine, `:p`/`:P` provision/force, `:d` drop, `:t` translate, `:v` voices, `:s` swap, `:e` eta, `:u` retry, `:m` reconcile, `:B` backend, `:X` stop. Words work too (`:reconcile`, `:backend`, `:quit`); actions that need input open their prompt after `Enter`. No single key can fire anything destructive. |
| **R** / **K** / **S**  | Read-only screens — system overview (`Enter` launches) / **task ledger** / cast overview                                             |
| **i**                  | Inspect the selected machine (probe output, capabilities)                                                                              |
| **arrows / k j**       | Move the selection · **PgUp PgDn** scroll the log · **G** pin to newest                                                               |
| **r**                  | Refresh now · **?** full help · **C** colour on/off · **q** quit                                                                      |

`:` + `B` with no machines registered runs a local-only cluster — the easy
first run. In the task ledger (`K`): `j/k` or arrows to move, type to filter
(e.g. `shelved`, `digest`, `42`), `u` retry / `F` force re-run the highlighted
row, `Enter` for the full error, `Esc`/`q` to close.

#### Hearing a voice before you commit to it

In the voice picker (`:s` step 2 of 2) and the cast overview (`S`) — the two screens that
know which voice a speaker has — three keys audition and **none of them assign**.
`Enter` is still the only key that changes the cast.

| Key           | Plays                                                                                |
| ------------- | ------------------------------------------------------------------------------------ |
| `t`           | the held line with the **current** voice, from cache only — zero synthesis            |
| `T`           | that same held line with the **pointed** voice (rendered)                             |
| `Ctrl+T`      | **another** line with the **pointed** voice (rendered)                                |

In the picker the character is fixed, so `t` replays the current A/B sentence
and never rolls; in the cast overview it follows the highlighted speaker.
The line is held per character, so the incumbent and the candidate are compared on
one sentence rather than two. Both screens print which line is loaded and what the
current voice is, so nothing is compared blind.

The sample **plays by itself** — there is nothing to open. Playback is `afplay`,
one sample at a time; starting one stops the last. The inductor renders and hands
the audio back as bytes, so it is written to a single temp file on *this* machine,
overwritten on every audition and removed when the TUI exits: nothing accumulates
in the repo, and the file is next to the speaker even when the inductor is on
another box. Rendering goes through the TTS sidecar exactly as the pipeline does,
so what you hear is what will be spoken.

All three keys audition, so `t`/`T` don't filter on these two screens (picker
step 1 still types every letter) — the price of leaving the Tab family, whose
`Ctrl+Tab` the terminal owns.

### C. "Spread it over the LAN" (cluster)

```bash
# 1. Remember the other box (writes .bm/machines.json, git-ignored)
make link NAME=box-1 ADDR=192.168.2.2

#    …or skip `link` and onboard straight by address:
#    make provision ADDR=192.168.2.2 KEY=~/.ssh/your-key

# 2. Onboard it over SSH: pushes sources, builds the Python venv, enrolls your
#    clone voices, starts the TTS sidecar. Cheap to re-run — a content stamp
#    makes a nothing-changed run finish in under a second.
make provision BOX=box-1

# 3. Start the cluster (re-provisions — fast now — then launches everything)
make tui     # press :B
```

Workers pull chapters from a shared queue, so idle machines pick up work
automatically. A chapter's render+merge stays on the box that rendered it, so
cached segments are never re-uploaded.

Headless or screen-reader friendly: `bm-inductor tui --once` prints one
plain-text snapshot and exits (fine in scripts, `watch`, CI).

---

## 4. Where everything lives

Everything below except the first three rows is created at runtime and
git-ignored — see the two tables at the top for the tracked/ignored split.

| Path                                        | What it is                                                                                  |
| ------------------------------------------- | ------------------------------------------------------------------------------------------- |
| `prompts/analyze.txt`                       | **The dramatization prompt — your main customization point** (tracked; an example you edit) |
| `assets/effects/`, `assets/music/`, `assets/*-pool.json`, `assets/scene-map.json`, `assets/LICENSES.json` | Sound-design clips, the pools that name them, the scene labels, and clip provenance (tracked) |
| `voices.default.json`                       | The built-in catalogue voices (tracked)                                                     |
| `output/Ch.N - Title.mp3`                   | **The finished audiobook chapters**                                                         |
| `data/chapters/NN.txt`                      | Crawled, cleaned chapter text                                                               |
| `data/script-NN.json`                       | The dramatized script (segments + speakers + moods + scenes)                                |
| `data/bible.json`                           | The growing character bible (canonical names, aliases, voice traits)                        |
| `data/cast-vieneu.json`                     | Speaker → voice assignment (one per engine)                                                 |
| `data/audio/segments-vieneu-NN/`            | Cached per-segment audio (one dir per chapter, per engine) — renders are resumable          |
| `voices.json`                               | Character → reference clip (clone voices)                                                   |
| `voice-pool.json`                           | Tagged sample pool for automatic voice assignment                                           |
| `refs/`                                     | Your voice clips                                                                            |
| `.bm/settings.json`                         | Run config: url_template, engine, start/count, speed, gap_ms, ambience, music, analyzer, models |
| `.bm/ledger.json`                           | The task ledger — which chapter/stage is in which state; survives restarts                  |
| `.bm/machines.json`                         | Linked machines (addr, ssh user/port/key)                                                   |
| `~/.bm-worker/`                             | A worker's whole world on any machine: agent binary, venv, sources, `.provision_stamp.json` |

The pipeline is **restart-safe**: all of the above is on disk. Kill anything at
any time — the inductor picks up exactly where the ledger says, and cached
segments are never re-rendered.

---

## 5. When something fails (the short version)

- A task that fails 3 times is **shelved** so it stops starving healthy
  chapters. Open **K**: the row shows _why_ — the worker's actual error, in
  full on `Enter`. `u` re-queues it (forgiving the strikes), `F` re-runs it from
  scratch and clears any partial output (e.g. a stale `script-NN.json`).
- A task whose worker dies, or whose lease expires, is re-queued automatically
  — silence is never punished as a failure.
- The Events pane records every completion, failure, expiry and operator action
  with its reason; the same stream is on `/api/state` under `events`.
- Full guide: **[docs/TROUBLESHOOTING.md](docs/TROUBLESHOOTING.md)**.
- Under the hood (crates, state machine, API, provisioning cache):
  **[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)**.
- Plans (any-provider LLM, AWS EC2 + S3): **[docs/ROADMAP.md](docs/ROADMAP.md)**.

---

## 6. Tests & development

```bash
make test                            # cargo test --workspace + clippy -D warnings
cargo test -p bm-inductor tui::      # just the TUI tests
```

The suite never touches the network: SSH targets in tests are TEST-NET
addresses, the LLM/TTS sides are stubbed, and the TUI renders to an in-memory
backend.

More design reading, kept local (`.docs/` is git-ignored, so it is not in the
repo history):

- `.docs/TUI_UX_AUDIT.md` — the reasoning behind every pane and key in the dashboard
- `.docs/VOICE_CONFIG_PROPOSAL.md` — the voice pool / accent policy design
- `.docs/PLAN.md` — the original build plan, milestone by milestone

## 7. Honest limitations

- **Vieneu is local and free but heavy**: ~1.7 GB of model weights, and it
  speaks Vietnamese best. For other languages, either use the Gemini TTS engine
  (`TTS_ENGINE=gemini`) or adapt `python/tts_router.py` to bring your own.
- **Gemini TTS is quota-limited** on the free API tier (~10 TTS calls/day); the
  Gemini _app_ subscription does not raise API limits. Pay-as-you-go in AI
  Studio costs pennies per chapter.
- **Digest burns LLM tokens** — roughly one analyzer call per chapter. Free
  model tiers work but rate-limit; the analyzer chain falls through a list of
  models automatically.
- **Crawling needs a predictable URL** (`{n}` template) and a findable chapter
  body. `c` in the TUI saves the template and probe-crawls one chapter to
  prove it before you commit to a range.
