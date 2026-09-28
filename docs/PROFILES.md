# Profiles: a genre, a language and an engine

## In plain words

*You can stop reading after this section.*

A checkout used to be bound to one thing, called a **profile** — `xianxia`.
That single bundle carried two quite different things: the **art** (the music,
the sound effects, and the rules for where they go) and the **prompts** (how the
AI casts a chapter, and how it writes the performance).

Those two only travelled together by accident. The art is about the *genre*: a
xianxia book wants the same tavern music whether it is read in Vietnamese or in
English. The prompts are about the *language*: Vietnamese prompts are no use for
an English performance, and the English ones want a different voice engine
behind them.

So a profile is now three pieces, each named and recorded separately:

| Piece | What it is | Who makes it |
| --- | --- | --- |
| **Pack** (an *asset*) | The genre: music, effects, injects, the scene map | You, or a shipped bundle |
| **Adapter** | The language: the prompts a stage renders from **and the crawlers it is read with** | You, or a shipped bundle |
| **Engine** | The voices: the weights, the binary, the voice store | Code — a new engine is a port |

**Not yet in the tree.** The crawler column above is the designed shape, not the
current one: `crawl/` still sits under `assets/` and `LIVE_DIRS` is still two
names. [ASSETS.md](ASSETS.md) carries the plan and the migration.

**The payoff.** A second language costs a pair of prompt files and, eventually,
a second engine — not a second set of music. A second genre costs art and
reuses the language you already have. Before the split, either one meant
copying everything.

## What a profile was

`profile.rs` has always described a profile as a flat set of live trees, and
that constant was the split line before anyone noticed:

```rust
pub const LIVE_DIRS: [&str; 2] = ["assets", "prompts"];
```

It is three names now — `assets`, `prompts`, `crawl` — because the crawlers
left the pack for the adapter, where the language they are written in lives.
[ASSETS.md](ASSETS.md) is the design of record for that, and for what came
with it: an asset names its dependencies and every piece cuts its own release.

A profile is `profiles/<name>.tar.zst`: those two trees plus a `manifest.json`
(`{name, version, files: {path: sha256}}`). The bundle is transfer only — day to
day the pipeline reads the unpacked tree, so no code path reaches through
decompression.

"Loaded" was a pointer file, `.bm/profile`, holding `{name, hash}` where `hash`
is recomputed over the live tree. Anything that runs calls `verify`: a
hand-edited tree, or one unpacked from a different profile, is *adopted* — the
pointer is re-stamped and a warning goes out — rather than refused. The live
tree is the source of truth; `profile pack <name>` is the verb that saves it
back to a bundle.

## The pieces, measured

| Piece | Trees | Bytes |
| --- | --- | --- |
| Pack | `assets/` | **58 MB** |
| Adapter | `prompts/` | **21 KB** (`analyze.txt` 4,985 + `script.txt` 15,930) |
| Engine | `engines/<name>/` — weights, binary, runtime, lexicon, voice store, clips | **1.0 GB** |

What each actually holds:

- **Pack** — `music/` (13 beds), `injects/` (+ `inject-pool.json`),
  `effects/` (+ `effect-pool.json`), `scene-map.json` (rules, palette, layers,
  pause, reverb, duck), `tag-aliases.json`, `LICENSES.json`. What is **gone** is
  `crawl/` (the adapter's now) and what is **new** is `pack.json`, the ordered
  list of assets this one builds on — see [ASSETS.md](ASSETS.md).
  `tag-aliases.json` is the prompt-side synonym table for the closed sound
  vocabularies, and its values must be canonical palette names — which is why it
  is pack-side and stays in English.
- **Adapter** — the two prompt templates, plus the crawler templates the source
  language is read with (`crawl/`, 72 KB, one file per site). Everything else it will
  eventually own (a text front end for the language) lives inside the engine's
  code, not here.
- **Engine** — `engines/<name>/`: `models/` (weights, the live voice store
  `voices.json`, the G2P dictionary `sea_g2p.bin`), `bm-tts`,
  `libonnxruntime.so.1`, and `refs/` + `samples/` for enrollment. The engine's
  name is in **every** path, so a second engine gets a tree of its own rather
  than sharing one `models/`, one binary and one dictionary with the first.

Two engines are already declared in the compiled catalogue
(`voices.default.json`, embedded at compile time as `CATALOGUE_JSON`):
`vieneu` with 23 presets and `gemini` with 17 cloud presets. The catalogue, the
paths and `Settings::engine` all speak the axis; §"The engine tree" is where its
*files* got an identity of their own.

## The engine tree

The engine's files used to sit at the root — `models/`, `bm-tts`,
`libonnxruntime.so.1`, the dictionary inside `models/` — and the engine's name
was nowhere in a path. `Layout` named them one method at a time and `Binding`
carried the name, which is not the same thing: a second engine had no directory
to live in, and `tts_dict()` hardcoded `models/sea_g2p.bin`.

So every engine's files hang off one tree, and the name is in it:

```rust
pub fn engine_dir(&self) -> PathBuf { self.root.join(ENGINES_DIR).join(&self.engine) }
pub fn models_dir(&self) -> PathBuf { self.engine_dir().join("models") }
pub fn tts_binary(&self) -> PathBuf { self.engine_dir().join("bm-tts") }
pub fn tts_lib_dir(&self) -> PathBuf { self.engine_dir() }          // LD_LIBRARY_PATH
pub fn tts_voices(&self) -> PathBuf { self.models_dir().join("voices.json") }
pub fn voice_refs(&self) -> PathBuf { self.engine_dir().join("refs") }
```

`engine` is a field on `Layout` beside `adapter`, read the same way — from the
load pointer, falling back to `DEFAULT_ENGINE` — and a checkout that never named
one keeps the engine it was already running.

The dictionary is the one that used to *mispronounce* rather than fail: a second
engine reading `sea_g2p.bin` gets VieNeu's Southeast-Asian lexicon. It is now
the engine's own declaration (`dict: Some("sea_g2p.bin")`), and
`Layout::tts_dict()` answers `None` for an engine that declares none — so the
sidecar is not handed a `--dict` it has nothing to read.

**The move is a rename, and it happens once at load.** Every existing checkout
has the flat tree, and no new path points at it, so `bm-inductor` calls
`Layout::migrate_engine_tree()` immediately after `load_ledger` — beside the
cache re-key, and for the same reason: leaving the files behind would make the
sidecar unspawnable and the weights unreachable. It moves into **`engines/vieneu/`
whatever this checkout now runs**, because the flat tree was VieNeu's — the
history is fixed even though `settings.engine` is a setting somebody can change.
It is rename-only (one filesystem, so no copy), never overwrites, and idempotent.

## The engine's declaration

`voices::ENGINES` is one row per engine, and it is the API: nothing outside the
table branches on an engine's *name*. It grew from `nonverbal` (which tags the
front end implements) to everything a caller otherwise hardcodes:

| Field | What it answers | Who reads it |
| --- | --- | --- |
| `nonverbal` | sounds it voices as tags | `digest::render_nonverbal` |
| `languages` | BCP-47 tags it can voice | `voices_language` (the gate is the adapter manifest's job — see below) |
| `cloning` | whether a reference clip means anything to it | `roster add-sample` refuses when it is `false` |
| `dict` | its G2P lexicon, if any | `Layout::tts_dict`, `start_tts` |
| `sample_rate` / `channels` | the format its audio comes back in | `assemble::sample_rate_for` (moved off the catalogue row, which nothing read) |

Undeclared answers the *refusing* way for all of them: no tags, no languages,
no cloning, no dictionary, and not VieNeu's 48 kHz. That is the same rule the
non-verbal slice follows, and it is what makes a typo in `settings.engine` safe.

`adapter_language(pack, adapter)` is the other half, and it is all that is built:
an adapter is `<pack>-<language>`, so `xianxia-en-US` names `en-US` and an
unnamed adapter claims nothing. Comparing that against `languages` is where a
mismatch becomes a refusal — and that belongs with the adapter manifest, below,
because until an adapter declares its language as *data* the derived tag is a
convention rather than a fact.

## The binding

`profile::Binding` is three pieces, each a `Pointer { name, hash }`:

```json
{
  "pack":    { "name": "xianxia", "hash": "6b8d5fc00761…" },
  "adapter": { "name": "vi-VN",   "hash": "…" },
  "engine":  { "name": "vieneu",  "hash": "" }
}
```

It is stored in three places, and they must agree:

| Where | What it means | Written by |
| --- | --- | --- |
| `.bm/profile` | The load pointer: what this checkout was unpacked from | `profile load`, `verify` |
| `workspaces/<name>/settings.json` | What this *book* runs under | `workspace new`, stamped from the pointer |
| The ledger's `"profile"` key | What the existing tasks were created under | Reconcile, on the first run |

The third is the gate. `check_profile` refuses to run a ledger that holds
another binding's tasks, and it now **names the pieces that moved**, because
"another profile" sends an operator looking for the wrong thing — a different
pack or adapter is a re-unpack of files, while a different engine invalidates
the segment cache and every clip already rendered.

### The shim, and why it is in `Deserialize`

A pre-split document is `{name, hash}`, one hash over `assets/` **plus**
`prompts/`. Both shapes have to keep parsing, in all three places, so the shim
lives in `Binding`'s hand-written `Deserialize` rather than at one call site:

- A legacy document becomes `pack`, with the adapter and engine left **unnamed**
  rather than guessed at. `profile::label` skips unnamed pieces, so a pre-split
  checkout still reads as plain `xianxia` instead of `xianxia ·  ·  `.
- The ledger stamp matters most. A derived `Deserialize` would turn an existing
  `{name, hash}` into an *empty* binding, which reads as "this workspace runs
  nothing" — and every workspace on disk would trip its own gate on first run.
  The shim is what makes the split a non-event for existing ledgers.

No per-piece hash can reproduce a hash taken over both trees, so the first
`verify_binding` re-stamps pack and adapter from disk. That is what `verify` has
always done on drift.

## Hashing: what is hashed, and what deliberately is not

`hash_files` reads every byte of every file it is given, on as many cores as the
machine has. That is affordable for the pack and the adapter — 58 MB and 21 KB,
against the 57 MB the pre-split tree measured at 0.60 s release — and *not*
affordable for the engine, which is 1.0 GB of weights on every `serve` and
`worker` start.

So the rule is:

- **Pack and adapter are content-hashed**, each over its own trees. An edited
  `prompts/analyze.txt` moves the adapter and leaves the pack alone; that
  property is a test, not a hope, because it is the whole reason for having
  separate names.
- **The engine is a declaration, not a digest.** Its identity is its name from
  `settings.engine` plus its own declared facts — the languages, the cloning
  capability, the lexicon, the output format (see "The engine's declaration").
  `verify_binding` takes the engine name as an argument for exactly this reason,
  and `Piece::Engine::trees()` stays empty so nobody hashes a gigabyte by adding
  the engine to a loop.

`verify_binding` still refuses a tree with *both* file-backed halves missing —
that is a missing unpack — but leaves a piece whose trees are simply absent
alone, because a checkout that has not split yet has no adapter bundle and that
is not an error.

## What each piece costs to add

The cost model falls straight out of the table above, and it is not symmetric:

| Adding | Cost |
| --- | --- |
| A **genre** | Data. A pack bundle: art, a scene map, crawlers. |
| A **language** | Data, *plus* prompts authored for that language — and a text front end in the engine's code if the language needs one. |
| An **engine** | Code. A port, a roster entry in `voices.default.json`, and around a gigabyte of files. |

That is why the phases run in that order. Nothing about adding a genre or a
language touches Rust; the engine is the only piece that is a software project.

## The fork line

Where a book splits into languages is worth stating once, because it decides
which side of the line every future stage belongs on:

- **`crawl` and `prepare` are adapter-independent** — they never read the
  adapter. The crawled text and the quote split are properties of the *source*.
- **`digest` onward is per-adapter.** The prompts *are* the adapter, and so are
  the artifacts they produce: the cast, the segment text, the segment cache.

That is a claim about the **code**, and it pairs with the rule that keeps it
tractable: **an adapter has one language, and it is both the source's and the
target's.** Nothing translates. `xianxia-en-US` is crawled in English and
written in English; `japanesefantasy-ja` would be crawled in Japanese and
written in Japanese. So two languages of one book are two crawls, and
`data/chapters/` is shared with nothing — the stages above are
adapter-independent because they do not *read* the adapter, not because a
corpus is shared between languages.

The rule is why a worker is held to a language at all: an engine that cannot
voice the adapter's language is a mismatch rather than a setting. The engine's
half is declared (`EngineDecl.languages`); the adapter's half is a convention
derived from its id (`adapter_language`), and turning the two into a refusal
waits on the adapter manifest below. A *translating* adapter — bilingual
prompts, one corpus feeding both sides — is a third shape, future work, and
deliberately not designed here.

The adapter is therefore **bound to a pack** — `xianxia-en-US`, not `en-US` —
because its prompts carry the genre's register the way the pack carries its
music: "Senior Brother", "this one", "qi", Title Case headlines. A romance pack
wants different prompts for the same language, and the binding says which pair
is in force.

## The caches: what they key on

Today, from `Layout`:

```rust
pub fn cast(&self, engine: &str) -> PathBuf {
    if engine == "vieneu" { self.data().join("cast-vieneu.json") }
    else                  { self.data().join("cast.json") }
}

pub fn seg_dir(&self, engine: &str, n: u32) -> PathBuf {
    if engine == "vieneu" { self.data().join(format!("audio/segments-vieneu-{n:02}")) }
    else                  { self.data().join(format!("audio/segments-gemini-v2-{n:02}")) }
}
```

**The bug is in the `else`.** It names `gemini` for *any* engine that is not
`vieneu`, so a third engine would read and write Gemini's segment directory —
serving one engine's audio under another engine's name. That is already wrong
today with two engines declared, and it is the single thing blocking a second
*local* engine: the moment `neutts-air` renders a chapter it would either
inherit Gemini's cache or overwrite it.

**The adapter belongs in the key as well, and that is the part worth arguing.**
A workspace is bound to exactly one adapter and the ledger gate enforces it, so
from the *workspace's* point of view naming the language is redundant. But a
segment directory is not scoped to the workspace that made it: it travels inside
an offer, gets copied between boxes, and outlives the run. A path that does not
name its language is a path that has to be *believed* — and no path should have
to be.

**Both halves are derived, then:**

```rust
pub fn cast(&self, engine: &str) -> PathBuf {
    self.data()
        .join(format!("cast-{}-{}.json", self.adapter, engine_key(engine)))
}

pub fn seg_dir(&self, engine: &str, n: u32) -> PathBuf {
    self.data().join(format!(
        "audio/segments-{}-{}-{n:02}",
        self.adapter,
        engine_key(engine)
    ))
}
```

`engine_key` is a lookup rather than an `if/else`, and it keeps the two
historical spellings so no cache is orphaned: `vieneu → vieneu`,
`gemini → gemini-v2` (its on-disk spelling), anything else → its own name. An
empty engine becomes `unknown` rather than producing `cast-default-.json`.

`adapter` is a field on `Layout`, beside `work`, and for the same reason: it is
a property of the checkout. It is read from the binding at construction
(`cache_adapter()`), and a checkout whose pointer predates the split keys under
`default` — the language it was already using, before the split gave it a name.

**And the rename is real, which is what the adapter in the key costs.** Every
existing workspace has `data/cast-vieneu.json` and
`data/audio/segments-vieneu-NN/`, and no path points at them any more. So
`bm-inductor` re-keys once at load — `Inner::migrate_cache_keys`, immediately
after `load_ledger` — and says so in the event log:

- the cast is **moved**, never rebuilt: the bytes were produced by this adapter
  and this engine, and only the name was missing a component;
- only the *pre-split* spellings move, and only for engines that ever had one
  (`LEGACY_CACHE_ENGINES`), so an engine added later has nothing to migrate;
- it never overwrites. A target that already exists wins, and a `vieneu`
  workspace does **not** pick up the plain `cast.json`, which belonged to the
  non-VieNeu engine;
- it is idempotent, so a second start is silent.

Getting this wrong is not a crash. It is every chapter already spoken being
re-synthesised under a name nobody asked for — hours of synthesis for a path
string — which is why the move lives in the code rather than in a release note.

The tests that pin it: an `en-US` layout and a `vi-VN` one never resolve to the
same cast or segment directory; a pre-split cache is renamed rather than
orphaned; and `migrate_cache_keys` runs at load, not at plan time.

> A first draft had the adapter *out* of the key, arguing that one workspace has
> one adapter so the key need not say so. That is sound about the gate and wrong
> about the file — see the paragraph above. Worth keeping as a reminder that
> "cannot happen through the UI" is a different claim from "cannot be on disk".

## The prompts: one tree per adapter

`Layout::prompt()` and `script_prompt()` were root-scoped
(`root/prompts/*.txt`), so one checkout held exactly one language. They resolve
through `prompts_base()` now:

```rust
if self.work.join("prompts").is_dir() { self.work.clone() } else { self.root.clone() }
```

- **A workspace's own `prompts/` (+ `crawl/`) is the adapter's home**, for the
  same reason `workspaces/<name>/crawl/` is a book's: `:profile load` replaces the
  adapter's trees for the *whole checkout*, so a language kept at the root is a
  language every workspace on that root must share — one language per checkout,
  which is the limit the adapter exists to remove. The checkout's own home is
  `adapters/<adapter>/{prompts,crawl}`; a scope's own tree wins over it, and it
  wins over the retired root `prompts/`.
- **The checkout's tree is the fallback**, and that is what every workspace read
  before the split, so nothing on disk changes meaning and no migration is
  needed. A workspace whose own tree is *incomplete* fails on the missing
  template, and that error names the file it wanted.
- **`prompts_base()` returns the base, not the directory**, so the same call both
  reads and ships the tree: a bundle member is a path relative to its base, so
  `prompts/analyze.txt` lands identically from either one. `provision::sources`
  cuts the tree in force — the same tree a digest on this root reads. Shipping
  the root tree unconditionally would hand a box one language's prompts while
  the inductor driving it read another's, agreeing on every file name and
  disagreeing on every word.

The adapter's tree is **live and untracked**, at `adapters/<adapter>/`, holding
both `prompts/` and `crawl/` (`.gitignore`), shipped as a release bundle the way
a pack is. Neither tree may share a directory with the workspace's own: a
book's `work/crawl/` is its own crawlers, and `profile load` replaces the
adapter wholesale. The text is the
artifact: a prompt edit becomes visible when a release is cut, and the suite
builds its own stubs under `rust/fixtures/`, so nothing here is needed to test.

Two things the `xianxia-en-US` pair settles that the Vietnamese pair never had
to:

- **The non-verbal tags are the engine's, declared by the engine, and read
  through an API.** A tag names a sound *one engine's front end* knows how to
  make — `bm-tts` maps each to an `<|emotion_N|>` token — so it cannot be prompt
  text, and it cannot be a list of `if engine == …` either. `voices::ENGINES` is
  one row per engine (`EngineDecl { name, nonverbal }`); `voices::nonverbals()`
  answers for whatever name is bound, and a name nothing declares answers
  "none". **Adding an engine is adding a row**, not editing every reader. A test
  keeps that table equal to the catalogue's engine list, so adding one without
  the other is a build failure rather than a silent half-addition.

  Which means the rule is **rendered or removed, never negated**. An engine that
  voices no tags gets a prompt with no non-verbal rule in it at all, because a
  rule that says "none" still teaches the model that brackets are a thing it may
  write — and a token the bound engine does not implement is read aloud
  literally. The prompts carry `{voice_tags}`, `{tag_laugh}`, `{tag_sigh}` and
  `{tag_throat}`, and the rule is bounded by its own heading (`7. NON-VERBAL
  SOUNDS.` through `8. MUSIC:`), so the section mechanism that rewrites rules
  1–3 for the automatic path is what takes this one out.

- **`en-US` reads English; it does not translate.** Chapter text, `mentions`
  keys, segment text and title are all English, and nothing in either prompt is
  bilingual. That is the rule and not a property of this pair — the adapter's
  language *is* the source's language, so a Japanese adapter is crawled and
  written in Japanese the same way. A translation layer is future work and is
  not designed here.

The three axes multiply, and the binding already names all three: `xianxia`
(pack) × `vi` / `en` (adapter) × `vieneu` / `gemini` (engine) covers
`xianxia · vi · vieneu`, `xianxia · en · some-english-engine` and
`xianxia · vi · gemini` with none of them a special case in code — the adapter
resolves the language and the declaration answers for the engine. What an
*engine-specific prompt* would need — an adapter whose wording differs per engine
rather than per language, as `-vi-VieNeu` versus `-vi-GeminiTTS` suggests — is
not built: today the engine reaches a prompt only through this declaration, and
a per-engine override is a larger decision than the tags it would serve.

## Not built yet

- **An adapter manifest — the mismatch gate.** An adapter declares
  `(pack, language, engine)` in `adapters/<id>/adapter.json`, and the binding
  refuses a pair that disagrees rather than deriving the language from the id.
  `EngineDecl.languages`, `adapter_language()` and the ledger gate are all in
  place; what is missing is the language being *data* on the adapter side. Which
  is also what would let a worker be held to a language before it is offered one.
- **An adapter packer.** `tools/profile.sh` packs `assets/` + `prompts/`
  together, which is the pre-split shape. `adapters/<adapter>/{prompts,crawl}`
  needs the same treatment — one `tar.zst`, a manifest, a release — and that is
  what turns the untracked tree above into something a second machine can bind.
  [ASSETS.md](ASSETS.md) splits the bundle three ways.
- **Phase 4 — provisioning and scheduling.** Bundles become per piece, and
  `sources_stages` grows the second dimension so a vi-VN box is never offered an
  en-US chapter. Two things ride with it: the worker resolves the same
  `Layout::cast()` through its own `.bm/profile`, so the binding — the adapter
  name in particular — has to travel to the box or the two ends disagree about
  the filename; and the ledger gate has to name the piece, which it now does.
- **A binding stamp inside the cache.** The key names the language and the
  engine, but nothing *checks* what it finds: a hand-edited `settings.json` can
  still point a workspace at another adapter's cast. A stamp beside the cache
  would turn that from a silent mix into a refusal. Deferred rather than
  dropped, because the rename above had to land first and the ledger gate covers
  the ordinary path.

On the second engine itself: **NeuTTS Air** is the candidate, Apache-2.0, and
architecturally the same species as the existing port — a prefill, a decode step
with its KV cache fed back, and a neural codec decoder that turns codes into
audio, which is what `engine.rs` and `codec.rs` already do for VieNeu. Two
caveats worth knowing before anyone starts: it ships as PyTorch and GGUF with the
ONNX artifacts **decoder-only**, so exporting the language model with its KV
cache in and out would be ours to do; and every file the Python path generates is
Perth-watermarked by default, which is a decision about published audio rather
than a detail.

## See also

- [ARCHITECTURE.md](ARCHITECTURE.md) — §4 provisioning, §5 voices, §6 the TUI.
- [ARTIFACTS.md](ARTIFACTS.md) — what a publish candidate is; the stamp table.
- [ROADMAP.md](ROADMAP.md) — what is queued.
