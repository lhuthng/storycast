# Authoring a pack

**In one line:** making a new set of sounds for a genre, as something other
people can install.

**You want this file** if you are creating a pack. You want
[ASSETS.md](ASSETS.md) if you want to understand *why* packs are a dependency
tree. You want [SOUND.md](SOUND.md) if you want to know what a clip must sound
like. This file is the recipe, in the order you do the steps.

[ASSETS.md](ASSETS.md) explains the model: what a dependency is, what merges
into what, how resolution remembers what it filled in, and what a release is.
[SOUND.md](SOUND.md) is the studio reference for the art itself. This file is the
recipe — what to create, in what order, and which two mistakes are expensive
enough to check for.

## The three shapes

| shape | `pack.json` | holds a score? | is it runnable? |
| --- | --- | --- | --- |
| **root** | absent, or `"deps": []` | **no, never** | only if it names `common` |
| **preset** | names other packs | yes | only if its closure reaches `common` |
| **workspace** | *not built* — see [ROADMAP](ROADMAP.md) | yes | — |

A **preset** is not a new mechanism: it is an asset whose `deps` name other
assets and whose own content is small. That is the whole difference.

**A pack is runnable only when `common` is in its chain** — directly or through
its dependencies. A pack with no `common` has no world: no place vocabulary, no
mood vocabulary, no beds. It will resolve, and it will be nearly silent, and
nothing will say so. The gate in §6 of this file checks the symptom rather than
the cause, which is why the cause is worth stating.

**No root holds music.** A root is the world — place beds, spot effects, the
rules that score them, the synonym table — and a score is a genre's identity, so
`common`, `weapons` and `craft` all ship an empty `music-pool.json` whose note
says where the tracks are instead. The consequence, which is easy to miss until
it bites: **a preset inherits the mood vocabulary and must answer every value of
it**, including the ones it did not write. See [SOUND.md §8](SOUND.md#8-the-mood-vocabulary-and-how-it-grows).

## A worked example: `craft`

A root pack for a dispensary. This is the whole of it.

```
assets/_extends/craft/
  effect-pool.json     one place bed
  inject-pool.json     nine recorded foley sounds
  music-pool.json      empty, with a note saying why
  scene-map.json       two rules
  tag-aliases.json     the model's words for its own sounds
  LICENSES.json        provenance, per category
  RECORDING.md         the shot list — the receipt for the LICENSES line
  effects/  injects/   the clips, once they are recorded
```

**1. Decide what the pack is a *place* for.** `craft` is the apothecary's shop
and its back room — not medicine in general, not science, not a hospital. The
narrower the better: a pack that claims a whole category ends up re-recording
what the world already has.

**2. Write the registries with the audio in mind but not yet on disk.** Every
entry names its `files` even before a clip exists. That is the normal state of a
new pack, and the gate reports it as *pending* rather than failing, so the
authoring of the sound and the recording of it are separate steps that do not
block each other.

**3. `scene-map.json` — rules, and nothing else a root should not touch.** A root
may own rules (`weapons` owns the battle scene) but not a reverb, because a
reverb is a taste and a preset's rules are read first — a root that named one
here would win against the genre's by being matched earlier. Rules **concatenate
with yours first**, which is what lets a root shadow a world rule for its own
place.

**4. `tag-aliases.json` — nouns in the pool, model words here.** The pool is
tagged in physical nouns; every word the model might reach for goes in this file.
See [SOUND.md §6](SOUND.md#tags-are-the-api).

**5. `LICENSES.json` — a category is the unit, and a category is arbitrary
text.** `"sound effects (effects/, injects/)"` is a category, not a path. Your
line **accumulates** alongside your dependencies', so a release built on both
carries both. For recorded foley, point the line at the log: *"Foley, recorded in
house. Date, room, microphone and raw take per clip are in RECORDING.md."* A
provenance claim with a receipt is worth a great deal more than one without.

**6. Check it.** See §6.

## What merges how

The same table as [ASSETS.md](ASSETS.md#what-layers-and-how), restated as an
instruction rather than an explanation.

| file | member | how it merges |
| --- | --- | --- |
| the three pools | every sound, by key | **whole entry by key.** A child's `wind` replaces the parent's `wind` outright — tags, files, `mode`, `hold` and `level` together. Fields are *not* merged: a half-inherited entry is a clip whose `mode` came from a sound it is not. |
| `scene-map.json` | `rules` | **concatenated, yours first**, then each dependency's, strongest first |
| | `music_palette`, `reverb_presets` | **by key** — yours wins a name you state, a stronger dependency beats a weaker one |
| | `layers`, `pause`, `duck`, `default` | **whole** — yours replaces, or the strongest dependency that has one |
| `tag-aliases.json` | every member | **by key, and the names accumulate** |
| `LICENSES.json` | every member | **by key, and the lines accumulate** |
| anything else at the same relative path | the file | inherited only if you ship none of your own |

Three rules fall out, and each is load-bearing:

* **Your rules are seen first, and that is the only order that works.** Rules are
  ordered specific-to-general and the first match wins, so an inherited rule may
  only ever sit *behind* yours. It is also what makes an override work: restate
  a world rule's match set and the world's copy never runs.
* **A stronger dependency is seen earlier, because a list has no key to
  replace.** `deps` order is your precedence list, weakest first.
* **A `_`-prefixed member is always whole.** `_note` is prose about *that* file,
  and half-inheriting one is a sentence about something else.

## Staleness, and `--force`

Editing a dependency moves its hash. Every pack that names it is therefore
**stale**, and staleness is a comparison, not a guess: the pack's manifest holds
the hash it was built against.

```sh
bm-inductor profile manifest <name> --piece pack     # refuses while a dep has moved
bm-inductor profile manifest <name> --piece pack --force   # packs behind a moved parent, on purpose
```

`asset pack` refuses a stale tree unless asked to rebuild, and
`asset rebuild-deps` sweeps the children of a changed pack. The rebuild rule, in
one line: **`asset resolve` withdraws everything the marker says was inherited,
folds the current `deps` back in, and records the result** — which is what makes
the tree packable again.

## Where the art is edited

**Develop a new pack under `assets/_extends/<name>/` and release it as itself
with `--dep`. Never hand-edit the resolved live tree.**

This is the answer to the question [ASSETS.md](ASSETS.md#what-is-not-decided-here)
left open, and it is worth stating because the alternative is a trap. The live
tree is *already resolved* — it holds your files **and** everything inherited,
side by side. The composition record is the only thing that tells the two apart,
and a hand edit is one `asset resolve` from being overwritten, silently, because
the record still says that entry is the dependency's.

A dependency directory is the honest unit of authorship: it is yours alone, it has
no inherited content in it, and it is what `--dep` turns into a self-contained
release.

## What a release is

```sh
bm-inductor profile manifest court-mystery --piece pack --dep
```

A `tar.zst` with a manifest, whose keys are the paths the release unpacks to.
**Self-contained**: it carries a *resolved* `assets/`, so a box needs none of the
dependency releases and provisioning is unchanged. Composition happens at **pack**
time rather than at read time, which is the price.

The one trap: a dependency released **without** `--dep` sanitization carries its
own parents' content inside itself, so the duplication a flat tree exists to
avoid comes straight back. `--dep` is what produces the *sanitized root pack* —
the pack as itself rather than as somebody's dependency.

## The gate

```sh
cargo test -p bm-core --test pack_gates -- --nocapture
```

It walks every pack under `assets/_extends/`, copies each to a scratch root with
the whole closure beside it, **resolves it there** (never in the checkout), and
checks three things:

1. every palette value but `none` is answered by a pooled track;
2. every rule's effect tags are answered by a pooled bed;
3. every named file is present, **or not yet recorded** — a pending clip is
   reported by name, while a sound with no `files` at all, or with *some* of its
   takes present and some absent, fails.

Gate 1 is skipped for a pack that ships no music, because a root's empty pool is
the documented shape.

It prints what is still pending, per clip, and by how many — which is the honest
state of a pack whose registries are authored ahead of its audio, and the reason
the registries can be written first.

**The two failures worth knowing about**, both found by this gate on its first
run against a real pack:

* a palette value **inherited without an answer** — drop the dependency that
  answered it and the value is silently silent everywhere. Answering the world's
  `battle` value in a palace preset, rather than carrying `weapons` to get it for
  free, is the shape of the fix.
* an **effect tag with no bed** — the scene comes out dry with no error, because
  unpooled effect tags are *discarded* rather than refused. A new effect tag needs
  a new clip, not a new alias.

## See also

* [COMPLETING-A-PACK.md](COMPLETING-A-PACK.md) — the runbook for finishing one
* [ASSETS.md](ASSETS.md) — the model this implements
* [SOUND.md](SOUND.md) — the three layers, the knobs, the spec, and the art
* [PROFILES.md](PROFILES.md) — the three pieces, the binding, and what a box fetches
