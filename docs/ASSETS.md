# Assets: a genre's art, composed, and released

> **Status: built.** `pack.json`, the `_extends/`
> fold-in, the `_extends.json` record and the `asset resolve` verb all exist —
> `bm_core::compose` is the one entry point, and `asset resolve --dry-run`
> reports what it would do. The crawlers left the pack: they and the language's
> prompts live in `adapters/<name>/`, the binding names the language, the caches
> were re-keyed for the name it did not have before, and provisioning ships the
> language as one tree (`.gitignore`'s un-ignore chain for the bundled templates
> moved with them). The release is per piece — `profiles/<piece>/<name>.tar.zst`
> — with the manifest that records what it was built on, and the gate that
> refuses to pack behind a moved dependency. See *Releases* below.

## In plain words

*You can stop reading after this section.*

The **pack** — the genre's art — is no longer one flat puddle of sound. It is a
small **dependency tree**: a genre asset says which assets it builds on, and its
own files win over theirs. A `common` asset holds the sounds every book wants
(door slam, wind, a body thud) and a genre asset — `xianxia` — depends on it and
adds what only that genre has. One door slam, written once.

Two corrections came with it:

* **Crawlers are not the pack's.** A crawler is one website, read in one
  language — which makes it the **adapter's**, the way the prompts are. The pack
  is art; the language is how you read and write. Moving them is what this
  document calls the split.
* **Each piece has its own release.** Today one `profiles/<name>.tar.zst` holds
  `assets/` **and** `prompts/`, so a genre and a language travel as one thing and
  a second language means a second copy of the art. They split: an **asset**
  release carries its art, a **language** release carries its prompts and its
  crawlers, and a genre is composed from other assets rather than copied.

The point of the dependency tree is editing: change `common` once, and every
asset that depends on it is rebuildable rather than nine copies to fix by hand.

## The three pieces, corrected

| Piece | What it is | Trees |
| --- | --- | --- |
| **Pack** (an *asset*) | The genre: music, effects, injects, the scene map, aliases | `assets/` |
| **Adapter** (a *language*) | How a language is read and written: the prompts **and the crawlers** | `prompts/`, `crawl/` |
| **Engine** | The voices: weights, binary, runtime, lexicon, voice store | `engines/<name>/` |

`profiles/LIVE_DIRS` was `["assets", "prompts"]` and that constant was the split
line before anyone noticed. It becomes three names, and — see *What this breaks*
— it cannot stay a `&'static [&'static str]`.

### Why the crawlers are the adapter's

A crawler is not art. It is a `discover()` and a `chapter()` for one site —
`storya.click/truyen/…` — and the site's text is in the *source* language, which
the fork line already says is the adapter's language, because **an adapter has
one language and it is both the source's and the target's**. A Vietnamese
crawler under `vi-VN` and an English one under `en-US` are two files for two
sites, and neither is a `xianxia` fact: the same `xianxia` book crawled in
English is a different site, not a different genre.

They are also read by a stage that never opens the pack: `crawl` is
adapter-independent in the sense that it never reads an *adapter's prompts* —
but it does read the adapter's crawlers, which is the one thing this move makes
explicit rather than implied.

The bundled templates (`crawl::DEFAULT_SCRIPT`, every `KnownSite::script`) move
with them, which is a real change to what a fresh clone can do: the tracked
templates live under a language's tree now, so a clone with no language loaded
can neither crawl nor claim it could. That is the honest consequence of the
split, and it retires the `/assets/crawl/templates/` un-ignore rule in
`.gitignore` for a rule under the language's tree.

## Composition: `assets/pack.json`

```json
{
  "_note": "The genre's dependencies, weakest first. Each names a directory under assets/_extends/.",
  "deps": ["common", "xianxia-base"]
}
```

* **Ordered, and later wins.** `deps` is a list rather than one `extends` so an
  asset can mix a generic base with a genre base, and the precedence is the list
  order: `xianxia-base` overrides `common` on a shared key, and the asset itself
  overrides both. A single-parent chain is the one-element case.
* **Whole entry by key.** A sound is a key in a pool registry, so a child's
  `wind` replaces the parent's `wind` outright — tags, files, `mode`, `hold` and
  `level` together. Fields are not merged: `serde`'s defaults cannot tell "unset"
  from "set to the default", and a half-inherited entry is a clip whose `mode`
  came from a sound it is not.
* **Per file otherwise.** A clip, a crawler template or `scene-map.json` at the
  same relative path is inherited only if this asset does not ship one of its
  own. Key-level merging for `scene-map.json` and `tag-aliases.json` is the
  obvious next step and is *not* built: the formatting-preserving writer exists
  for the pool registries (`audio_pool::save_pool`, which keeps every untouched
  entry's own bytes and its `_note` in prose), and the scene map has no such
  writer. Re-serialising it would destroy the notes that are the only written
  record of why the palette is shaped the way it is.

Which registry merges by key is `audio_pool::PoolKind::ALL` and nothing else:
the three pool registries are the layered ones, named once.

## Resolution: fill in, and remember what was filled

The dependencies are unpacked at `assets/_extends/<name>/`, so the whole thing
lives inside `assets/` — one tree, one hash, one ship. Resolving fills the live
tree in:

`bm-inductor asset resolve` does it (and `--dry-run` reports without writing),
and it touches nothing at all when the answer is already on disk:

```
assets/                      the working tree — this asset's own art, plus what it inherited
  pack.json                  authored: { deps: [...] }
  _extends/common/           an unpacked dependency, an input
  _extends/xianxia-base/     another, later in the list
  _extends.json              generated: what was inherited, and from where
  effect-pool.json           this asset's sounds + the deps' missing ones
  effects/… injects/… music/…
```

Fill-in only, so a child always wins and a second resolve writes nothing. The
one thing that cannot be left implicit is **what was inherited**, or a
re-resolve cannot withdraw an entry the parent has since dropped:
`assets/_extends.json` records, per registry, the keys inserted and a hash of
the value inserted, plus every plain file copied in and its content hash. On the
next resolve:

* an inherited key whose recorded hash *still matches* is withdrawn and re-filled
  from the dependency — so a parent's change propagates;
* one whose hash **differs** has been edited by the operator, who has adopted
  it: it stays, and it stops being tracked. Nothing is ever silently thrown away.

That is what makes the merge both idempotent (no change in, no change out) and
regenerable, and it is why the marker is a file rather than an assumption.

`_extends/` and `_extends.json` are inert to every reader: the pools are read
from `assets/<registry>.json`, clips resolve at `assets/<rel>`, and provisioning
selects by registry, so a dependency is never shipped as if it were content.

## Releases: self-contained, and one per piece

A release is a `tar.zst` with a manifest, as a profile is today — but there are
now three kinds, and `profiles/<name>.tar.zst` stops meaning "all of it":

| Release | Carries | Depends on |
| --- | --- | --- |
| asset | a **resolved** `assets/` — its own art with its dependencies already filled in | its `deps`, by name and hash |
| language | `prompts/` + `crawl/` | nothing |
| engine | not a bundle: `engines/<name>/` is provisioned from the models release | — |

In practice: `tools/profile.sh pack <name> --piece pack|adapter`, a file per
piece under `profiles/<piece>/`, and a GitHub release tagged
`<name>-<piece>-v<version>` holding the plain `<name>.tar.zst`. The manifest is
computed by `bm-inductor profile manifest` rather than by the shell script — it
needs the piece's live trees and the composition record, and duplicating either
is how the two would drift. Tar and zstd stay in shell, where they have always
been.

**The manifest's keys are the paths the release unpacks to** (`assets/…` for a
pack, `adapters/<name>/…` for a language), so `manifest_hash` over them is the
same number `verify_binding` computes for the live piece. A release and the tree
it came from therefore agree *by construction*, and loading one never re-stamps
a hash it just changed — which is what the old combined bundle could not do.

**Self-contained**, deliberately: the asset release is the resolved tree, so a
box needs none of the dependency releases and provisioning is unchanged — one
tree, registry-selected, as `assets/` already is. The cost is that composition
happens at **pack** time rather than at read time, which is exactly the rebuild
rule:

* `asset resolve` withdraws everything the marker says was inherited, folds the
  current `deps` back in, and records the result — which is what makes the tree
  packable again.
* `profile.sh pack … --piece pack` then records each dependency's name and hash
  in the manifest. It **refuses** while a dependency has moved: the check is
  `stale_dependencies`, and `--force` packs behind a moved parent on purpose.
* Editing a parent moves the parent's hash. Every child that names it is
  therefore **stale**, which is a comparison, not a guess: the child's manifest
  holds the hash it was built against.
* `asset pack` refuses a stale tree unless asked to rebuild, and a sweep
  (`asset rebuild-deps`, or a flag) re-packs the children of a changed asset.
  That is the "editing a parent triggering the rebuilding of the children".

The binding's pack pointer already holds `{name, hash}`, and the hash stays what
it is today — a content hash over the live `assets/` — so a stale child is also
visible as ordinary drift.

## What this breaks, honestly

This is not a small edit, and two items are structural:

1. **`Piece::trees()` and `LIVE_DIRS` can no longer be static string lists.**
   The adapter's trees are no longer `root/prompts` — they are the language's own
   home, `adapters/<adapter>/{prompts,crawl}`, which is what `/adapters/` in
   `.gitignore` and the already-authored `adapters/xianxia-en-US/prompts/` were
   reaching for. So `Piece::Adapter.trees()` has to be computed from the binding,
   and `verify_binding` has to hash `adapters/<name>/…` rather than `root/…`.
   `LIVE_DIRS` keeps its job (the fixture, the both-missing check) as the *union*
   of names, but the hashing path stops reading it.
2. **Crawlers resolve from two trees, and the book's own must not be one of the
   language's.** `work/crawl/` is a *book's* crawlers (already searched first,
   already shipped separately by `provision::sources`). It cannot double as the
   language's, or `profile load` — which replaces a language wholesale — would
   delete a book's own crawler. So: `work/crawl` (the book) → the language's
   `crawl/` → the retired `assets/crawl` for one release cycle. `Layout` gains
   the language's home; `crawl::resolve_script`'s base list follows.

The rest is mechanical: `provision::sources` (the crawl stage ships the
language's tree), the stamp (its signature covers the language's `crawl/`), the
tracked templates' new `$HOME`-relative home, and one migration that moves this
checkout's `assets/crawl/` under the language it belongs to — rename-only and
idempotent, the way the engine tree's move was.

## What is not decided here

* **Where an asset's own art is edited** once it has dependencies and a release.
  The live tree is the working tree, so editing it edits the resolved result; the
  marker is what still distinguishes the two, and it is enough — but a screen
  that shows "inherited from `common`" next to "yours" is the honest UI and is
  not built.
* **Key-level merging for `scene-map.json` and `tag-aliases.json`.** Needs a
  formatting-preserving writer for arbitrary JSON objects, not just pools.
* **Whether a language release also carries the tag vocabulary**, which is
  currently pack-side (`tag-aliases.json`) and stays in English by policy.

## See also

* [PROFILES.md](PROFILES.md) — the three pieces, the binding, and where a book
  becomes a language.
* [ARTIFACTS.md](ARTIFACTS.md) — what a publish candidate is; the stamp table.
