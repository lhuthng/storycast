# Architecture

How Storycast turns chapters into audio, and where each piece lives.
Diagrams: `img/architecture-map.svg` (source: `img/architecture-map.excalidraw`).

## Pipeline

One chapter moves through four stages, each a ledger task per chapter
(`crawl:N`, `digest:N`, `render:N…`, `merge:N`):

```
site ──crawl──▶ data/chapters/chNN.txt ──digest──▶ data/script/NN.json
                                                        │
bible.json ◀── merge characters ── analyzer (LLM) ──────┘
                                                        ▼
                                              render (TTS sidecars)
                                                        │
pools (effects/injects/music) ── assemble ──────────────┘
                                                        ▼
                                              output/Ch.N - Title.mp3 + cues
```

- **Crawl** (`bm-inductor crawl`, scripts in `crawlers/known/`): fetch + clean.
  Refused fast when a page has no sentences — a crawl that cannot be staged is
  an error at the boundary, not three stages downstream.
- **Digest** (`bm-inductor digest N --write`): one analyzer call per chapter —
  attribution, staging, the chapter-wide sound pass — plus deterministic gates
  (sound cues, non-verbal tags). Writes the script and merges the bible delta.
- **Render**: segments are spoken through engine sidecars (VieNeu over HTTP on
  `:8818`), one cached wav per line per voice. Voices resolve through the cast.
- **Merge**: cached segments + ducked beds + hits + music → final MP3 and cue
  sidecar. A merge whose cues went stale requeues itself.

Ground truth is files: reconcile marks crawl done iff the chapter txt exists,
digest done iff the script exists, merge done iff the MP3 and fresh cues exist.
Deleting a script is a valid invalidation — the scheduler redigests it.

## Sound

Three pools in `assets/` (authored per-pack under `assets/_extends/<dep>/`):

| Pool | Layer | Examples |
| --- | --- | --- |
| `effect-pool.json` + `effects/` | Bed under a whole scene | `rain`, `market-crowd`, `storm` |
| `inject-pool.json` + `injects/` | One-shot on a beat | `door-knock`, `sword-slash`, `gulp` |
| `music-pool.json` + `music/` | Mood track | `war-drums`, `sorrow`, `mischief` |

Entries match by **tags**, never by name: the script names tags, the picker
rolls once for the sound and again among its takes (two takes minimum per
sound). `mode` (`hit`/`overlap`/`trail`), `hold`, `level` and `looped` say how a
clip behaves in the mix. `tag-aliases.json` maps model words to pool tags;
`scene-map.json` holds rules, palettes and reverbs. Levels are gains over known
sources: places at −26 LUFS (under the voice), moments at −20 (in front).

## Packs

A pack is a dependency tree of art, released per piece:

- Author one dependency at a time under `assets/_extends/<name>/`. Never
  hand-edit the resolved live `assets/` tree — the next `asset resolve`
  overwrites it.
- `assets/pack.json` names deps weakest-first (`common`, `weapons`, `magic`);
  pools merge whole-entry-by-key, rules concatenate yours-first.
- A release is a `tar.zst` + manifest cut by `tools/profile.sh pack <name>
  --version <V>` and published by `tools/preset-publish.sh` to the R2 bucket
  (`preset/sounds/<name>/<V>/…`, `latest.json` moved, never overwritten).
  `assets/_extends.json` records what the live tree was built on; packing
  behind a moved dep needs `--force`, on purpose.

Voice packs work the same shape in the other direction: a pack's working copy
under `.bm/packs/voices/<name>/<version>/` declares tagged slots beside one
clone per slot, and install pairs slots to clones as `<pack>/unnamed-N` ids
that casts carry forever. Both the copy and the installed pack come from R2;
nothing reads the checkout's `voices/` tree.

## Casting

Characters get voices by tag compatibility: at least one shared tag between the
character (bible `tags`, else words from `voice_hint`) and the pool sample, and
no conflicting pair (`young`↔`old`, `child`↔`adult`, `strong`↔`weak`). Among
compatible voices, least-used and not-already-in-the-chapter wins. Gender does
not conflict — a voice may serve any character it shares a tag with.

## Profiles and workspaces

A **profile** is a pack × adapter × engine triple (`profiles/presets.json`,
e.g. `xianxia-vi`). `.bm/profile` is the binding: one name/hash/version per
piece, and runners gate on it. Settings carry `packs_release` (R2 base URL) plus
the analyzer, volumes and chapter range.

A **workspace** (`workspaces/<book>/`) is one book: `settings.json`,
`data/chapters/`, `data/script/`, `data/audio/` segment caches, `bible.json`,
`cast-*.json`, `output/` finals and `ledger.json` (tasks + machine state).
Without `pack_deps` a workspace shares the checkout's live pack.

## Cluster

`bm-inductor serve` runs the control API + scheduler (`:8901`); `make tui`
attaches the dashboard. Workers (LAN or AWS) beat in, take offered tasks, and
return artifacts. Operator surgery goes through `POST /api/op`
(`retry-task` with `stage`+`chapter`+`force` requeues a digest and clears its
downstream rows — the supported way to invalidate a chapter). Dispatch starts
held: a restart never resumes handing out work until told to go.

Engines (`engines/`, git-ignored checkouts + pins) and adapters (`adapters/`,
prompts + crawlers per language) are the two swappable halves around the core:
new language, new adapter; new voice backend, new engine.
