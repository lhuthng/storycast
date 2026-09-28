# Assets: a genre's art, composed, and released

> **Status: built.** `pack.json`, the `_extends/`
> fold-in, the `_extends.json` record and the `asset resolve` verb all exist —
> `bm_core::compose` is the one entry point, and `asset resolve --dry-run`
> reports what it would do. **The rules layer too.** The three pool registries
> always merged by key; now `scene-map.json`, `tag-aliases.json` and
> `LICENSES.json` do as well, so a root asset can own the world's rules and a
> genre state only its own — which is what the live tree's `common` does. See
> *What layers, and how*.
>
> **And it composes three roots now.** The live checkout is `xianxia`, a
> **preset**: `deps` `["common", "weapons", "magic"]`, its own content being the
> score and one rule. `common` is the world, `weapons` real arms, `magic` spells
> and the impact a spell makes; the latter two are roots with no parent. A
> preset is nothing but a composition, and a **workspace's pack extends its
> profile's pack** the same way, one-to-one — *not built yet*, see *What is not
> decided here*. The crawlers left the pack: they and the language's
> prompts live in `adapters/<name>/`, the binding names the language, the caches
> were re-keyed for the name it did not have before, and provisioning ships the
> language as one tree (`.gitignore`'s un-ignore chain for the bundled templates
> moved with them). The release is per piece — `profiles/<piece>/<name>.tar.zst`
> — with the manifest that records what it was built on, and the gate that
> refuses to pack behind a moved dependency. See *Releases* below.

## In plain words

*You can stop reading after this section.*

The **pack** — the genre's art — is no longer one flat puddle of sound. It is a
small **dependency tree**: an asset says which assets it builds on, and its own
files win over theirs. One door slam, written once.

The tree the live checkout is now:

```
common    the world and the body — place beds, weather, footsteps, a door, a
          body thud; the rules that score them, the mood palette, the mixer
          knobs, the synonym table, the sound-effects licence
weapons   real arms, pre-gunpowder: the strike, the shot, what a blow does to
          what it hits
magic     spells, and the impact a *spell* makes — never a physical one,
          which is why `metal-hit` and `rock-break` are the world's

xianxia   a **preset**: deps `["common", "weapons", "magic"]` and almost no
          content of its own. What it is is the composition, plus the score —
          music is a genre's taste, so the tracks and the mood answers are here
          and no root pack holds any
```

`common`, `weapons` and `magic` are **roots**: no parent, content only, and a
pack is runnable only when `common` is somewhere in its chain, because the mixer
knobs and the palette are the world's. Everything is a pack, so a preset may
depend on a preset, and a workspace's pack extends the one its profile is loaded
with — see *Composition*, and [ARCHITECTURE.md](ARCHITECTURE.md#the-map-the-book-the-machine-and-what-each-one-borrows)
for where a workspace sits.

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

And it is real now rather than planned: the live tree **is** a composition.
`assets/pack.json` names three dependencies and `assets/_extends.json` records
what each contributed. `xianxia`'s own content is its score, and the licence line
that says where it came from; the world is `common`'s, the arms `weapons`' —
including the `battle` rule, which moved there when the split gave the preset a
pack to compose — and the spells `magic`'s. Editing a wind
clip or a rain level is one edit in one place, and a second genre — or a
workspace — composes the same three and states only what it disagrees with.

Each root also starts **mostly empty on purpose**, and `magic` ships an empty
effect pool and an empty rule list: a pack is something to extend, an empty
registry is a state rather than a placeholder, and a rule with no sound is a bug
where a rule with no *entry* is honest.

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
  "deps": ["common", "weapons", "magic"]
}
```

* **Ordered, and later wins.** `deps` is a list rather than one `extends` so an
  asset can mix a generic base with a genre base, and the precedence is the list
  order: `magic` overrides `weapons` on a shared key, `weapons` overrides
  `common`, and the asset itself overrides all three. A single-parent chain is
  the one-element case, and a pack with no parent is a root — content only, which
  is what `weapons` and `magic` are.
* **A preset is just a composition.** Nothing distinguishes it: it is an asset
  whose `deps` name other assets and whose own content is small. So a *Game*
  preset depending on the `xianxia` preset needs no new mechanism, and neither
  does a workspace — a workspace's pack is the same kind of tree, extending the
  pack its profile is loaded with as its base and adding the book's own taste and
  presets after it, so the book's files win over its base and its own presets win
  over the book's.
* **Whole entry by key.** A sound is a key in a pool registry, so a child's
  `wind` replaces the parent's `wind` outright — tags, files, `mode`, `hold` and
  `level` together. Fields are not merged: `serde`'s defaults cannot tell "unset"
  from "set to the default", and a half-inherited entry is a clip whose `mode`
  came from a sound it is not.
* **Per file otherwise, and member by member for the four that layer.** A clip,
  a crawler template or any other file at the same relative path is inherited
  only if this asset does not ship one of its own. `scene-map.json`,
  `tag-aliases.json` and `LICENSES.json` are the exceptions, and what they do is
  the next section.

Which registry merges by key is `audio_pool::PoolKind::ALL`: the three pool
registries are the named-once case of a merge that is no longer only theirs.

### What layers, and how

Two files used to be all-or-nothing, and that put a ceiling on what a root asset
could be. `scene-map.json` is where the rules *and their tuning notes* live, so a
genre that shipped its own replaced the world's outright: the art could be
shared but the rules could not, and a level fixed in one genre reached none of
the others. `common` could be a clip library and never the root.

These layer now, member by member, and the merge is by **raw text**: an
inherited rule arrives with the dependency's own bytes, and every member the
file already had keeps its own. Nothing is re-serialised, so no `_note` is
reflowed and no key order is lost — the argument `save_pool` is built on, one
level down.

| file | member | how it merges |
| --- | --- | --- |
| `scene-map.json` | `rules` | **concatenated**: the file's own entries first, then each dependency's, **strongest first** |
| | `music_palette`, `reverb_presets` | by key: the file's names win, a missing one is added, a stronger dependency takes a weaker one's name |
| | everything else | whole: the file's member wins, then the strongest dependency that has one |
| `tag-aliases.json` | every member | by key, and the names **accumulate** |
| `LICENSES.json` | every member | by key, and the lines **accumulate** |

Four rules fall out of it, and each is load-bearing:

* **A keyed member accumulates across dependencies.** A member that is a map is
  *added to*, never replaced whole — which is what lets `common` state the
  world's synonyms, `weapons` the words for its own clips and `magic` its own,
  and all three survive in the composed file. This is the quietest failure in the
  file: the result still parses, and a weaker dependency's whole table is simply
  gone. An entry that a *later* (stronger) dependency also states is the one it
  takes, per name — and a name the file itself has is never taken at all.
* **A genre's rules are seen first, and that is the only order that works.**
  Rules are ordered specific-to-general and the first match wins, so an
  inherited rule may only ever sit *behind* the file's own. It is also what makes
  an override work: a genre that restates a world rule is matched first, and the
  world's copy never runs. Among dependencies the order is **strongest first**,
  because a rule list has no key to replace: the only way a stronger dependency
  can win is to be *seen* earlier.
* **A `_`-prefixed member is always whole.** `_note` is prose about *this* file,
  and half-inheriting one is a sentence about something else.
* **A map merges by name, and a member that is not an object is its own name.**
  `LICENSES.json`'s members are strings, so a category is the unit; a palette
  entry is an object, so a mood is.

One consequence is worth stating because it is a decision rather than an
accident: **the score is the genre's, so no root pack holds any.** `common`
carries an empty `music-pool.json` whose note says where the tracks are, and the
**vocabulary is still the world's** — `music_palette` lives in `common`'s scene
map, so a genre inherits the moods the prompt may name, answers the ones it has a
track for, and a mood it has none for goes silent rather than wrong. The licence
line follows the clips: the score's category sits in the genre's
`LICENSES.json`, the sound effects' in `common`'s, and a composed release carries
both.

The record says which of the three each inherited thing is, and a name is
arbitrary text — `LICENSES.json`'s categories are literally `sound effects
(effects/, injects/)` — so a record is `member`, `member/key` or `member+dep`
with the separators escaped out of the names. A list is the one that needs a
number: its hash is `<count>:<hash of the entries>`, because a list cannot be
withdrawn by value alone.

## Resolution: fill in, and remember what was filled

The dependencies are unpacked at `assets/_extends/<name>/`, so the whole thing
lives inside `assets/` — one tree, one hash, one ship. Resolving fills the live
tree in:

`bm-inductor asset resolve` does it (and `--dry-run` reports without writing),
and it touches nothing at all when the answer is already on disk:

```
assets/                      the working tree — this asset's own art, plus what it inherited
  pack.json                  authored: { deps: ["common", "weapons", "magic"] }
  _extends/common/           an unpacked dependency, an input
  _extends/weapons/          another, later in the list
  _extends/magic/            and the last, which wins a shared key
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

**An inherited file is deleted only if no dependency puts it back.** The
withdrawal marks each recorded file rather than removing it, the fill claims it,
and whatever is still unclaimed at the end is deleted. Deleting up front is how
a resolve came to rewrite every clip it had already put there — 58 MB of churn
for a change of nothing — and it made `--dry-run` lie, because a dry run cannot
delete, so its fill found every file already present and reported all of them as
withdrawn. A resolve that reaches the same answer now touches nothing at all,
which is what the paragraph above always claimed.

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
* **A genre cannot remove an inherited rule, only shadow it.** Restating the
  match set in the genre's own list puts its copy first and the world's never
  runs, but the world's entry stays in the tree: it is the dependency's, not
  yours, and the record will not withdraw it. An explicit `shadowed` list is the
  honest way to say "not here" and is not built.
* **An inherited list is appended, so a merged rule list is not in the reading
  order its author had.** A dependency's rules sit after the file's own, in
  `deps` order. That is the right order for matching and the wrong one for
  reading.
* **A workspace cannot yet have a pack of its own, and the composition record
  cannot yet be told to adopt.** The first is the item with real work behind it:
  `Layout::assets()` is the *checkout's*, unlike `prompts/` — which is
  work-scoped, so each book can have its own — so there is exactly one live pack
  and every book shares it. A workspace's pack **extends its profile's pack**,
  one-to-one, and adds the book's own taste and presets on top; that tree is
  sketched in [ARCHITECTURE.md](ARCHITECTURE.md#the-map-the-book-the-machine-and-what-each-one-borrows)
  and is not built. The second is smaller and was hit during the split: what
  marks an inherited file as the dependency's is the **record**, so a tree that
  decides to own a file it inherited without editing it — the score moving from
  `common` to the genre — has to prune the marker by hand. An `asset adopt <rel>`
  is the honest command, and is not built, as is the `asset import <file>` that
  would make the External Library box real.
* **Whether a language release also carries the tag vocabulary**, which is
  currently pack-side (`tag-aliases.json`) and stays in English by policy.

## See also

* [PROFILES.md](PROFILES.md) — the three pieces, the binding, and where a book
  becomes a language.
* [ARTIFACTS.md](ARTIFACTS.md) — what a publish candidate is; the stamp table.
