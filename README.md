# Storycast

**A web novel in, a multi-voice audiobook out.**

Give Storycast a book — a website, or text files you already have — and it
produces one MP3 per chapter, with a different voice for every character, music
under the quiet parts and sound effects on the beats. It runs on one laptop or
on a cluster of rented machines, and every step can be resumed.

Built for Vietnamese web novels, but nothing in the code is tied to one site or
one language.

---

## Run it

Prerequisites: Rust, ffmpeg, sox, Python 3.

```bash
git clone lhuthng/storycast.git && cd storycast
make build && make tui
```

Press **`L`** in the dashboard and paste an LLM provider key. Then:

```bash
./backend/target/debug/bm-inductor --root . workspace new mybook --profile xianxia-vi
make serve   # control API + scheduler on :8901; the TUI from `make tui` drives it
```

Useful commands (all take `--root .` from the checkout):

| Command | What it does |
| --- | --- |
| `bm-inductor check <url>` | Probe whether a site will serve chapters at all |
| `bm-inductor crawl` | Fetch chapters into `data/chapters/` |
| `bm-inductor digest <N> --write` | Digest one chapter through the analyzer, write `data/script/NN.json` |
| `bm-inductor asset resolve` | Fold pack dependencies into the live `assets/` tree |
| `bm-inductor retag --dry-run` | Preview rewriting interjections into engine tags |
| `make test` | `cargo test --workspace` + `clippy --all-targets -- -D warnings` |

## How a chapter flows

`crawl` → `digest` → `render` → `merge`, tracked as ledger tasks per chapter:

1. **Crawl** — site-specific scripts under `crawlers/known/` fetch chapter text.
2. **Digest** — the analyzer (LLM) attributes speakers, stages scenes, places
   sound cues, and merges character state into `data/bible.json`. Output is the
   script: `workspaces/<book>/data/script/NN.json`.
3. **Render** — TTS sidecars speak each segment; voices come from installed
   voice packs, matched to characters by tags.
4. **Merge** — segments plus beds, hits and music are mixed into the final MP3.

Sound is three pools: **effects** (beds under scenes), **injects** (one-shots on
beats), **music** (mood tracks). Missing a cue because a pack lacks the sound is
a redigest, not an edit — see `docs/tools/`.

## Packs and releases

Art ships as versioned bundles from an R2 bucket, never in git:

- Sound packs compose: `assets/_extends/<dep>/` authors one dependency,
  `asset resolve` folds them into the live `assets/` tree.
- `tools/profile.sh pack <name> --version <V>` cuts a release into
  `profiles/pack/`; `tools/preset-publish.sh sounds <name> <V> <bundle>`
  uploads it and moves `latest.json`.

## Layout

| Path | What lives there |
| --- | --- |
| `backend/crates/` | `bm-core` (pipeline), `bm-inductor` (scheduler/TUI), `bm-tts`, `bm-proto` |
| `tools/` | Sound and release scripts, each with a guide in `docs/tools/` |
| `assets/_extends/` | Authored sound packs (`common`, `weapons`, `magic`, `craft`, …) |
| `profiles/` | Cut releases + `presets.json` (pack × adapter × engine triples) |
| `workspaces/` | One directory per book: settings, scripts, audio, ledger |
| `voices/` | Clone takes + `presets/` voice packs |
| `adapters/` | Per-language prompts and crawlers |
| `docs/` | This architecture file's sibling: `ARCHITECTURE.md` + tool guides |

`docs/ARCHITECTURE.md` is the map of how the pieces fit. Everything else that
used to be in `docs/` was either stale or moved next to the tool it describes.
