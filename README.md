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
| `.env`                                       | Your API keys (from `.env.example`)                                                                                |

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
- **merge** — concatenates the segments (gaps + optional ambience beds) into
  the final `Ch.N - Title.mp3` in `output/`.

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

### Give it atmosphere (ambience)

Chapters don't have to be dry voices. The dramatization prompt tags every
segment with a `scene` label (`"market-stall-morning"`, `"forest-night"`), and
the merge stage turns those labels into background sound:

- Consecutive same-speaker lines form a *run*; the run's majority scene label
  is matched against ordered keyword rules in `assets/scene-map.json` — first
  match wins. `"storm"` lays `rain-storm.mp3` under the mix at 22% volume,
  `"night"` gets crickets, `"market"` gets a crowd, and so on (8 beds ship in
  `assets/ambience/`).
- The bed loops for the whole run and is **ducked automatically** whenever
  someone speaks (ffmpeg sidechain: the bed drops away under the voice and
  swells back in the pauses). Rules can also attach a reverb preset for room
  feel.
- A missing bed file is not an error — that span just plays dry voice.
- To customize: drop your own loops into `assets/ambience/`, add or reorder
  rules in `assets/scene-map.json` (`match` keywords, `bed`, `level`, optional
  `reverb`), or turn the whole thing off with `"ambience": false` in
  `.bm/settings.json` (the stock TUI run screen doesn't yet expose it, but the
  setting is read at merge time).

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
| `assets/ambience/`, `assets/scene-map.json` | Ambience loops and scene labels (tracked)                                                   |
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
| `.bm/settings.json`                         | Run config: url_template, engine, start/count, speed, gap_ms, ambience, analyzer, models    |
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
