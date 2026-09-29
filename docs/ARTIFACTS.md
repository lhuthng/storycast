# Artifacts: the weights, the voice store, and who fetches what

## In plain words

*You can stop reading after this section.*

Provisioning a new worker means sending it about 886 MB. Three quarters of that
is one directory of TTS weights, the same 667 MB for every box, forever, because
the weights only change when you deliberately re-bake them.

That is the wrong shape. The files are immutable, they are large, and they are
identical on every machine, so they behave exactly like a released artifact: name
them by their content, publish them once, and let each box download them itself.
That is what this document designs.

What stays on the rsync is everything that genuinely differs per box or changes
often: the prompts, the crawlers, the cast files, the small JSON manifests, and
one 492 KB voice store that is rewritten every time a voice is enrolled. What
has left the rsync entirely is `refs/`, the reference clips: those are enrolled
here, on this machine, and a box receives the *encoded* voice store instead.

The result is that provisioning a new box sends roughly **24 MB from your
machine** instead of 886 MB. The box still downloads the weights, from a CDN,
in parallel with every other box, instead of serially through your home uplink.

The rule the whole design turns on: **a file belongs in the artifact if and only
if its bytes are decided by a version, not by a workspace, a profile, or an
operation.** The voice store fails that test. The weights pass it.

## What is in the plane, and what is not

Three pushes dominate provisioning. Two of them are candidates for publishing,
and the reasons differ for each.

| Push | Size | Verdict |
|---|---|---|
| the engine's `engines/<name>/models/` | 668 MB | **publish**, 16 immutable weight files |
| `assets/` media | 58 MB | candidate, changes only when a clip is added |
| binaries + runtime | 52 MB | ship, but small enough not to matter |
| `refs/` | 144 MB | **not pushed at all** (see below) |

`refs/` used to be the second-largest push and the worst one: 144 MB per box per
push, 107 MB of it the operator's own download staging under `refs/temp/`, which
no worker has ever read. It is not a publish candidate, it is simply not sent.
What it and the two trees beside it became is one `sources.tar.zst` per policy —
about 60 MB for a box that runs every stage — carrying only the files that box's
stages open. That is the change this document's tables are measured after, and
the module that does it is `bm-core/src/provision/sources.rs`.

`prompts/` is the one exception: 21 KB that rides **every** bundle whatever the
policy says. A box can gain the `digest` stage with a single keypress, and a
stage whose whole input is a file does not degrade when the file is missing — it
fails, on every retry, until somebody re-provisions. The rest of the selection
still follows the stage list, which is what the manifest records and what the
scheduler reads back from the box before offering it a stage.

> `models/` used to be a directory of its own at the root. It is the **engine's**
> now — `engines/<name>/models/`, beside that engine's own binary and runtime
> (see [PROFILES.md](PROFILES.md), "The engine tree") — and the sidecar resolves
> it from there. Every bare `models/…` below reads as that one.

Everything else, prompts (21 KB), the crawlers (72 KB), cast files, the scene
map, the three pool registries, `voices.json` (492 KB), is under 1 MB combined
and is never worth optimizing — which is why they ride in that same bundle
rather than getting a plane of their own.

## The split: weights out, voice store stays

The engine's `models/` is not one thing. It is a large immutable body and one
small mutable file that happens to sit in the same directory, and the two have
opposite requirements.

**The 16 weight files** are decided by `tools/bake-models.py`, which pins two
upstream HuggingFace commits (`backbone_rev`, `codec_rev`) and a vendored
sea-g2p hash. Given those pins, the bytes are fully determined. They change when
you re-bake, which is a deliberate act.

**`models/voices.json` is the cluster's voice roster**, and it is rewritten by
enrollment. It is why `voice_store_covers` and `remote_voice_store_complete`
exist in the first place: a box holding an incomplete roster answers
`unknown voice "Narrator 2"` and fails every render that names it.

The data said so before any of this was built. The bake's manifest used to record
a size and hash for the roster, and they were wrong:

```
$ python3 tools/bake-models.py --check      # before
16/17 files match the manifest
  CHANGED voices.json
    want 2ae93200ed4a283f361e8d7f55043cc7c871586a92c3f0a348a45d725d1731bd
    got  2e0f9dc029f86156ffaed98a9da5936ed33a59143f7fb25534ebaa90d7eaaee0
```

905,275 bytes recorded, 491,881 on disk. Every one of the sixteen weights
matched; the single mismatch was the one file that is not a bake output.

That has since been fixed at the source: **`voices.json` is no longer in
`manifest.json` at all.** It is still copied into `models/` for the sidecar to
load, but it is listed under `DERIVED` in `bake-models.py` and skipped when the
record is written. Three things fall out:

* `--check` is now a real gate, it reads `16/16 files match` and exits 0,
  where before it could never pass on a cluster that had ever enrolled a voice.
* No consumer needs a special case. The publish gate below is literally the
  `--check` result, and the `sha256sum -c` list provisioning writes needs no
  exclusion rule, it is built from `files`, which no longer contains the store.
* `total_bytes` equals the sum of `files`, which is the property a receipt
  should have. It was previously inflated by a file it could not vouch for.

The deeper reason is provenance, not mutability. Of the seventeen sources, only
`backbone_rev` and `codec_rev` cover pinned HuggingFace revisions; `sea_g2p.bin`
is pinned by being vendored in-tree. **`voices.json` was copied from a
pip-installed package**, nothing pinned it, in-tree or by revision, and it was
the one entry that had drifted. A receipt that cannot stand behind a hash is
worse than a receipt that omits the field.

**The split, concretely:**

```
artifact  models.tar.zst                 17 files, 667.5 MB → 363 MB compressed
         manifest.json + the 16 weight files
         pinned by a version, verified per file, never rewritten in place

rsync     engines/<name>/models/voices.json   1 file, 492 KB
         rewritten on enrollment; the stamp already gates it correctly
```

492 KB is 0.07% of the old payload. Leaving it on the rsync costs nothing and
keeps every existing voice behavior, the delta is what actually moves, and the
stamp's `models_need_push` / `voice_store_covers` pair already handles it. **This
split is a correctness requirement, not an optimization.**

## Naming: the hash is the name

The artifact is named by the hash of the bytes it contains, and that hash is
computed from the manifest the box already holds.

The reasoning behind content naming is worth keeping rather than just the shape:
because the artifact key is derived from a hash the box
already verifies against, **there is no mapping to keep in sync, and a box cannot
be handed a bundle that disagrees with the pointer it checks.** A box either asks
for the artifact matching what it wants, or it asks for nothing.

Publishing is therefore idempotent. Re-packing identical content at a different
compression level overwrites the same key with the same tree, which is the
correct outcome, and the manifest inside is what gets checked, file by file,
before anything moves.

With GitHub Releases the key becomes the release tag:

```
https://github.com/<owner>/<repo>/releases/download/models-v<hash>/models.tar.zst
```

and the mechanism is one this repo already runs: `tools/profile.sh fetch` queries
`api.github.com/…/releases`, selects the release whose `tag_name` matches, picks
the asset ending in `.tar.zst`, and curls its `browser_download_url`, with
`GH_TOKEN` used only when set. Public repo means no token on the box.

## Publishing, **done**

`tools/models.sh`, a sibling of `tools/profile.sh` with the same four verbs:

```
tools/models.sh pack     [--level N] [--engine <name>]
                         verify the bake -> engines/<engine>/models/models.tar.zst
tools/models.sh verify   [--engine <name>]
                         bundle manifest vs. bundle contents
tools/models.sh publish  [--notes "…"] [--engine <name>]
                         gh release create models-v<hash> with the bundle
tools/models.sh list     [--engine <name>]
                         the local bundle, its hash and size
```

`--engine` defaults to `vieneu`, or to `$BM_ENGINE` when that is set, and every
path is derived from it — so a second engine is `--engine pocket` and its own
`engines/pocket/models/`, beside its own binary and runtime. The name is checked
rather than trusted, because it lands in a path: letters, digits, dot, dash and
underscore only, and an `engines/<name>/` that does not exist is an error naming
the ones that do. An engine that ships its own bake (`engines/<name>/bake.py`)
has that bake used instead of `tools/bake-models.py`, so what a second engine's
weights *are* stays that engine's business.

**The tag is deliberately not per engine.** It stays `models-v<first 12 of the
manifest hash>`, a function of the contents alone, and the `name` field of the
pointer is what says which engine a release is for — which is the same job the
pack pointer's `version` does for a pack. A content address needs no help
naming its bytes, and adding a free-text component to one only makes it easier
to get wrong. One engine pointer per workspace is the limit this scheme has.

`pack` runs the engine's bake with `--check` first and **refuses unless every
file matches**. That gate is the whole safety story, and it
could not be used as a gate until the roster left the record, before that, it
reported a `CHANGED` that no re-bake could clear. It also selects members from
the manifest rather than globbing `models/`, which is the same content-addressing
rule as everywhere else: `voices.json` exists in the directory and is absent from
the record, so "everything the manifest lists" and "everything in `models/`" are
different questions and only one of them is the bundle. `verify` asserts both
directions, nothing missing, and **nothing unlisted**, so a stray file in the
archive is a failure rather than a surprise on a box.

**The name is the manifest hash**: sha256 over sorted `name + NUL +
content-sha256 + NUL` lines, the rule `profile.sh` already uses, read one level
deeper because the models manifest stores `bytes` beside each hash. Two machines
with the same bake produce the same tag, and, the point, a tag can never name
bytes it does not hold. The tag is the first 12 hex of it, with the full hash in
the release notes. `gh release create` refuses an existing tag, so immutability
is enforced by the host rather than promised by a comment.

The first one is cut:

```
models-vdda4efee13df   models.tar.zst   380,099,956 bytes (363 MiB, level 3)
```

and the round trip was checked by downloading it back: the asset's sha256 is
byte-identical to the local bundle.

**That first asset also carried 17 AppleDouble sidecars** — see
[Fetching](#fetching-on-the-box-done) — so it has been re-uploaded over the same
tag, and the round trip was checked again the hard way: `bm-agent fetch-artifact`
against the live public URL, which landed `FETCH-OK (16 files, 667 MiB,
models-vdda4efee13df)` at 18 MiB/s off the CDN. If a bundle ever needs
re-cutting, the same tag is the right one to clobber and not a new one:

```bash
tools/models.sh pack && tools/models.sh verify
gh release upload models-v<hash> models/models.tar.zst --clobber
```

The tag names the *contents* by the manifest hash, and the contents have not
changed — only the archive around them, which is not byte-addressed and never
was.

## Fetching, on the box, **done**

`bm-core::artifact` names the artifact, and `bm-agent` takes delivery of it:

```
bm-agent fetch-artifact <url> <models-dir> [--expect <manifest-hash>]
```

It streams to a file beside the destination, decompresses, verifies every file
against the manifest that travelled in the same archive — in **both**
directions, so a member nobody listed is a failure too — and only then swaps the
directory into place.

What is set, and where:

| thing | value |
|---|---|
| release repo | `settings.json`'s `models_release` (`owner/name`), the TUI's `:release`, or `--release-repo` on a one-shot `bm-inductor provision` |
| the tag | `models-v<first 12 of the manifest hash>`, computed on the inductor from its own `engines/<engine>/models/manifest.json`. One tag per bundle, whatever the engine — the pointer's `name` says which engine a tag is for |
| the URL | `https://github.com/<owner>/<name>/releases/download/<tag>/models.tar.zst` |
| the hash check | `--expect <full manifest hash>`, the one the inductor read — *not* the one that arrived |
| exit `0` | landed and verified; the push is skipped |
| exit `20` | bytes arrived and disagree — **no fallback**, the provision stops and names the file |
| exit `21` | the artifact was not there — the log says so and the rsync push runs instead |

The codes are 20 and 21 rather than 2 and 3 because `clap` exits **2** on a usage
error, and an unrecognised subcommand is exactly what an agent predating this
one answers. Read as corruption that would stop a provision which should have
fallen back to the push, and say the one thing that is not true.

## Fetching the profile pack, **done**

A profile **pack** is `assets/` — the registries, the clips they register, the
attribution, the language's bundled crawlers: ~60 MB that is byte-identical on
every box and changes only when the operator publishes a profile. It is the
second payload with that shape, so it is published and fetched the same way, and
the box pays the uplink zero times for it.

Publishing it is [`tools/profile.sh pack`](ASSETS.md) plus a `gh release create`
under the tag it prints. What provision does with it is the new half:

| thing | value |
|---|---|
| release repo | `settings.json`'s `packs_release` (`owner/name`), or the TUI's `:packrelease` |
| the tag | `<name>-pack-v<version>`, read off the **load pointer** |
| the URL | `https://github.com/<owner>/<name>/releases/download/<tag>/<name>.tar.zst` |
| the hash check | `--expect <the pointer's hash>` — the hash of the *live* tree on the inductor |
| exit `0` | landed and verified; `assets/` stays out of the bundle |
| exit `20` | the bundle is not the profile this cluster is running — **no fallback**, the provision stops |
| exit `21` | unreachable, or an agent predating `--strip-prefix` — the log says so and `assets/` is rsynced instead |

**The tag comes from the pointer, not from the setting.** `models_release` can
name only a repo, because `models-v<hash>` is derived from the content. A pack
is not content-addressed — `tools/profile.sh pack xianxia --version 0.1.0` is an
operator choosing a version — so the tag needs one, and the place it lives is
`Pointer::version`, written by `pack`/`unpack` from the manifest they just
produced. That makes the pointer and the URL a box fetches two readings of one
string rather than two strings to keep in step. **A pointer with no version
resolves to no release**, which is the push: every checkout from before this
existed keeps working with no edit, and `packs_release` is safe to set before
re-publishing.

**The hash check is the pointer's, which is the stronger of the two.** The
weights check themselves: a different bake is a different manifest hash. A pack
checks the *live* tree, so a release that is perfectly self-consistent and is a
different profile anyway is still refused — and a box cannot be talked into
running a profile the cluster is not.

**`assets/` leaves the bundle entirely, not partly.** A pack release is
`assets/` minus `assets/_extends/`, which is a **superset** of every
`assets/`-rooted member the sources plan selects (the registries, the clips they
register, the attribution). `Sources::plan_for` therefore drops the whole
subtree when a release is configured, and `compute_provision_stamp` plans with
the *same* argument — otherwise the stamp would be hashing an artifact that was
never sent. On this checkout that is 124 files, 63 MB, off every push.

**And the pack is its own stamp field.** With `assets/` gone from the bundle,
two different packs produce the **same** `sources.tar.zst`; the bundle cannot
see the difference. So `pack_release` records the pointer hash, `pack_in_sync`
gates on it, and a re-pointed profile reaches the boxes whose bundle is
otherwise byte-identical — instead of every log saying "in sync" while the merge
quietly runs the old one.

**The pack lands *after* the sources extract.** The bundle's delivery is a
*replacement*: it prunes `$D/assets` before it extracts. A pack landing first
would be deleted by the step that follows, and the box would come up with a
profile and no assets — the one combination nothing downstream reports.

**One subcommand, two shapes.** A pack bundle is a manifest *beside* an
`assets/` subtree, because that is the path system a pack is keyed by and the
hash of the live tree folds over. `--strip-prefix assets` is the only difference,
so the part that has to be right — verify against the manifest that travelled in
the archive, then swap in two renames — stays one implementation. A second verb
would be a second `match` on exit codes, and the day one of them read `2` as
corruption the cluster would stop provisioning.

**Separate settings, on purpose.** A checkout with a released pack and an
unreleased bake of the weights is the ordinary case, and one setting that could
only say both or neither would make the operator choose a 668 MB push to save a
60 MB one.

Three decisions inside that, and they are still the right ones:

**The agent does the decompression, not a shipped `zstd` binary.** The `tar`
crate is pure Rust, and so is `ruzstd`, the decoder — so the cross-built agent
gains no C dependency and the box needs no `zstd` package. A shipped per-target
`zstd` *binary* is the option rejected: that is a second cross-built artifact to
version-gate, reproducing exactly the `bm-tts` staleness bug that was just fixed,
to save a megabyte. And `apt-get install zstd` on the box would be a second
`sudo -n` gamble alongside the ffmpeg one.

**The box does not fetch a tarball over a directory it is using.** A failed
download that leaves a partial `models/` in place is the failure mode this design
exists to remove, and the old code could not even detect it: `MODELS-OK` tested
only that `manifest.json` **exists**. So a box that died mid-rsync passed its own
readiness check. Extract beside, verify all 17 hashes, two `rename`s: a
half-fetched tree is unrepresentable rather than merely unlikely, and a crash
between the renames leaves the old tree beside the new one, never a mixed one.

**Absence falls back, corruption does not.** A GitHub incident, a 404 from a tag
nobody cut, a box behind a firewall — the push is a real answer to all of them,
and a box that cannot reach a release is still a box that can be provisioned.
Bytes that disagree with the manifest are a different thing: retrying harder, or
pushing the same bytes again, is how a corrupt tree becomes a permanent one. So
exit `20` stops, and the only way out is a re-pack or a cleared setting.

Two things this found, both of which were true before it existed:

- **The first published bundle carried 17 AppleDouble sidecars.**
  `models-vdda4efee13df` was packed by macOS `tar` (libarchive), which writes a
  `._name` sidecar for every member carrying an extended attribute and *hides
  them from its own `tar -t`*. A Linux box unpacked them as 17 real files, and
  the "nothing unlisted" check refuses that bundle by name — which is how a
  packer nobody audited gets caught. `tools/models.sh pack` now sets
  `COPYFILE_DISABLE=1` (the env var, not `--no-mac-metadata`, which GNU tar
  rejects as unknown). The same bug shipped 115 sidecars per push in the sources
  bundle before that was fixed.
  **That tag is clean now** (re-audited 2026-09-29): a later re-cut of it was
  clobbered onto the same content-addressed tag, and the published asset is
  byte-identical — sha256 `22c1ae89…` — to a fresh pack of `engines/vieneu/models`
  with the variable set. It lists 17 members and carries no sidecars, so a box
  fetches it without complaint. The four `-pack-v0.1.0` profile releases are the
  ones still carrying the junk, one per member: `common` 80, `xianxia` 129,
  `weapons` 35, `magic` 18. Worth recording because the count of the members a
  dirty bundle had (17) is also the count of the members a clean one has, which
  is exactly how a stale note goes unnoticed.
- **The push no longer carries the bundle.** `models.tar.zst` sits inside
  `models/`, so the rsync was shipping 380 MB that stands for the 668 MB
  travelling next to it — to a box that has no use for a second copy of the same
  tree. It is now excluded in both directions, which also stops `--delete` from
  putting one back on a box that fetched a bundle.

Verified on the real artifact over a local HTTP server, at the time the published
bundle was still the dirty one: it was refused with the 17 names, the re-packed
one landed — `FETCH-OK (16 files, 667 MiB, models-vdda4efee13df)` — and a bundle
for another bake left the existing `models/` untouched. The published asset has
since been replaced by that clean re-cut, so the refusal half of that experiment
no longer reproduces against the live tag.

Compression is worth doing here, and the measurement is the reason to be
specific about the level:

```
668 MiB => 363 MiB   (54.31%)   zstd -3, 1.25s on an M-series laptop
```

fp32 ONNX weights compress nearly 2:1, they are not the incompressible blob that

Compression is worth doing here, and the measurement is the reason to be
specific about the level:

```
668 MiB => 363 MiB   (54.31%)   zstd -3, 1.25s on an M-series laptop
```

fp32 ONNX weights compress nearly 2:1, they are not the incompressible blob that
plain `.onnx` suggests. Note what this does and does not save: `rsync -z` is
*already* achieving this on the wire today. The artifact does not compress better
than the rsync. **What it changes is whose uplink pays for it**, 363 MB that
currently leaves your house once per box, serially, now comes off a CDN with
every box fetching at once.

## ffmpeg is a different problem, and already solved

ffmpeg is not part of this plane and needs no work.

It is not a library call, `bm-core/src/ambience.rs` spawns
`Command::new("ffmpeg")` by name, so linking libav\* into the agent would not
change the code path at all. And linking it is not a real option: libavcodec,
libavformat, libavfilter, libswscale and libswresample are tens of megabytes with
hundreds of external codec dependencies and LGPL/GPL exposure.

More to the point, **the box-without-ffmpeg case is already the designed-for
case.** `bm-agent` advertises the `merge` capability *only when ffmpeg is on
PATH*; `ensure_ffmpeg` tries `apt-get`/`dnf`/`yum` under `sudo -n`; a refusal is
a warning, never fatal, and the machine summary says
`· NO FFMPEG, merges will fail here`. Merge goes off that box, crawl, digest and
render keep working.

And ffmpeg is **0 bytes in the payload**, it is a package-manager install on the
box, never a push. It was never part of the 886 MB.

## Fallback order

An artifact host is a new way for a new box to fail, so the rsync path stays and
stays reachable. The order is deliberate:

1. **stamp says in sync** → nothing moves (this is still the common case, and it
   is what a re-provision of a healthy cluster costs).
2. **stamp drifted, release reachable** → box fetches and verifies the artifact.
3. **release unreachable or hash mismatch** → fall back to the rsync push, and
   say so in the log. A slow box is recoverable; a box that refuses to provision
   because GitHub had an incident is not.

A verification failure is never a fallback trigger: if the bytes are *wrong*, the
answer is to stop and say which file mismatched, not to try harder.

## What the stamp changes, **done**

This section and the two sections after it describe work that has landed. What
remains is the artifact itself (publish, fetch, verify on the box); everything
below is in the tree.

`tts_hash` was `models/manifest.json` content plus a directory signature of
`models/`, which is one fact where there are two, because the directory holds
an immutable bake and one mutable file. It is now four digests, each gating a
different push:

| digest | covers | gates |
|---|---|---|
| `sources_hash` | the bundle's manifest: one sha256 per file, keyed by the path it lands on the worker, plus the `(stage, adapter)` slots the box's policy covers and the agent version | `install_sources` |
| `tts_hash` | the bake **minus `models/voices.json`**, by signature, + `manifest.json` by content | the weights push |
| `voices_hash` | `models/voices.json` by content | the weights push, alongside `tts_hash` |
| `tts_bin_hash` | the `bm-tts` bytes | the sidecar push |

Three of those used to be wrong or absent, all in the same direction, a gate
that was not where the push was:

- **`refs/` was gated by nothing — and is now not pushed at all.**
  `install_sources` carried it, but only `voices_hash` covered it, and that was
  a digest provisioning computed, carried and never read. So editing a 144 MB
  reference clip drifted no gate any push consulted. The answer turned out not
  to be a better gate but a narrower push: **no worker reads `refs/`.**
  Enrollment runs on the inductor, which is the machine with the encoder, and
  what crosses to a box is the *encoded* store, `models/voices.json` — 0 of its
  75 presets names a clip path. A new reference clip therefore reaches a box as
  a **bake**, through `voices_hash`. `install_sources` no longer touches `refs/`
  at all, and the extract line prunes the tree from boxes that predate this.
  (The test that asserted the old behaviour, "refs/ is not part of the sources
  hash", is replaced by one asserting the clone manifest is a source and a
  reference clip is not.)
- **The sidecar never redeployed.** `install_tts_runtime` was only called in the
  `else` of `if already`, and `tts_hash` covers `models/`, not the binary. A
  rebuilt `bm-tts` stayed on the inductor for ever while the box served the old
  one. `tts_bin_hash` is `agent_hash`'s pattern applied to the second binary,
  and the push now also recycles the sidecar, a replaced binary on disk does
  nothing while the old process is still running it.
- **A newly enrolled voice could be invisible.** With `voices.json` excluded
  from `tts_hash`, nothing would have covered the roster at all. `voices_hash`
  is consulted again, narrowed to that one file: an enrollment reaches the box
  without re-sending 668 MB, and a re-bake still resyncs without looking like an
  enrollment.

Two smaller things landed with them:

- **The weights are verified, not assumed.** rsync exiting 0 says the transfer
  worked, not that the bytes are intact, a box that dies mid-push, a source
  file already corrupt, or a `--delete` racing a writer all produce a directory
  rsync is happy with and the sidecar is not. `install_models` now writes the
  bake's own `sha256` entries out as a `sha256sum -c` list and checks them on
  the box. The check used to be that `manifest.json` **exists**, which a
  half-written bake satisfies perfectly. `models/voices.json` is excluded from
  the list for the same reason it is excluded from `tts_hash`: its manifest
  entry is stale by design, because enrollment rewrites the file after the bake.
- **The probe no longer needs Python.** It read the voice roster by shelling out
  to `python3 -c "import json…"` against `models/voices.json`. It now asks the
  sidecar's `/voices` endpoint instead, `curl` was already required for
  `/health`, which is also the better answer: the roster a render will actually
  find, rather than what a file claims. An **unknown** roster is no longer read
  as a missing one, which matters because reading it that way answered a down
  sidecar with a 668 MB push.

## What the inductor still sends

Sizes measured on this repo. "before the bundle" is what the three whole-tree
rsyncs cost; "now" is one `sources.tar.zst` selected by the box's policy.

| | before the bundle | now | after the artifact |
|---|---|---|---|
| `models/` | 363 MB (`-z`) | 363 MB (`-z`) | **0**, box fetches |
| `libonnxruntime.so*` | 28 MB | 28 MB | 0, rides in the same artifact |
| `refs/` | 144 MB | **0** | 0 |
| `assets/` + prompts + crawlers + casts + `voices.json` | 58 MB, three rsyncs | **60 MB**, one file | 0, box fetches |
| `bm-agent` | 15 MB | 15 MB | 15 MB |
| `bm-tts` | 9.4 MB | 9.4 MB | 9.4 MB |
| **from your machine** | **~617 MB** | **~475 MB** | **~24 MB** |

`bm-agent` is irreducibly 15 MB: it is the thing doing the fetching, so it has to
be on the box first. Chasing that last 15 MB means a bootstrap that cannot verify
what it downloaded, which is the trade this design exists to avoid.

Publishing the `assets/` media as well — it qualifies under the same rule,
changing only when a clip is added — takes the final figure to **~24 MB**, the
number the top of this document quotes. The media is now one artifact instead of
three directories, and `sources_hash` is already a *content* hash of exactly that
set, so the name is there; what it still needs is the fetch half (release,
download, verify, extract on the box) that the weights are waiting on too.
`refs/` is out of scope now and permanently: it is not published, it is not
pushed, and nothing on a box reads it.

## Failure modes

| What happens | What the operator sees |
|---|---|
| Release missing for the wanted hash | falls back to rsync, names the URL it tried |
| Download truncated | per-file sha256 mismatch, the file named, the box left untouched |
| Weights swapped under an unchanged name | impossible, the name is the hash |
| Artifact published with `voices.json` inside | the box rejects it; `bake --check` is the gate meant to prevent it |
| `bm-agent` too old to have `fetch-artifact` | falls back to rsync |
| GitHub unreachable | falls back to rsync, slowly, and says so |
| `packs_release` set, pointer has no version | the push, and the log names the re-publish command that fixes it |
| pack release is a different profile | the box refuses it and the provision stops naming the tag — never pushed over the top |
| pack release carries an AppleDouble `._` member | the box refuses it by name, and `tools/profile.sh pack`/`verify` now refuse to cut or bless one — `COPYFILE_DISABLE=1` is what makes them pass rather than a substitute for checking |

Every one of these is *slower* or *louder* than today's behavior. None is silent,
and that is the requirement: a box that is quietly holding the wrong weights is
the failure mode worth engineering against.

## Removing the S3 bucket, **done**

The bucket was sketched for this and never used, an empty `bucket` had always
meant "rsync from here", which is a working configuration. With models and
profiles both headed for Releases it had no consumer left, so the whole concept
is gone rather than left as a second, unbuilt path:

- `AwsConfig::bucket`, its initializer, and `publishes_assets()` (`aws.rs`);
- the `summary()` branch that printed `s3://…` vs `assets: rsync from here`;
- `profile_object()` and its `aws up` call site in `main.rs`, plus the
  `provision.rs` re-export;
- the setup wizard text that asked for a bucket;
- `bucket` and `_note_bucket` in `aws.default.json` and `.bm/aws.json`, along
  with the four other notes that referenced them;
- the test asserting a bucketless pool says so. It is replaced by one asserting
  the summary names the plane it actually has.
- the S3 read policy and the "only if you set `bucket`" step in
  `AWS-CREDENTIALS.md`, `AWS-IAM-USER.md` and `AWS-WORKERS.md`.

The asset plane is now: one path (rsync from this machine), one host for the
artifact when it lands (a GitHub Release). The IAM story simplified as a side
effect, **the worker role needs no permissions at all**, which is three fewer
paragraphs to keep true across three documents. The role is still required,
because every launch names an instance profile.

## Open questions

1. ~~**Tag or release per hash?**~~ **Decided: tag per hash.** A `models-v<hash>`
   tag per bake is immutable and unambiguous, and the immutability is enforced by
   `gh release create` refusing a tag that exists rather than by convention. The
   rolling `models-latest` alternative is tidier but a box fetching mid-overwrite
   can receive a bundle that disagrees with what it asked for, and the tag would
   say nothing about which bytes it holds. The cost accepted is tag-list clutter.
2. **Pruning.** Content-addressed names never overwrite, so every re-bake leaves
   363 MB behind forever. `tools/profile.sh` has no prune either.
3. **Public repo.** A Release asset on a public repo is world-readable. The
   weights are fine, they are baked from public models and contain nothing
   secret, but this is a decision to make deliberately, and the answer changes
   if anyone ever bakes private material into `models/`.
4. **The `assets/` media.** Worth publishing for the same reason it changes
   only when a clip is added, not per-provision, and it is now a single artifact
   rather than four directories, so it is one release instead of five. The gate
   it needed is already there: `sources_hash` is a content hash of exactly that
   set, one sha256 per file keyed by where it lands. What is missing is the
   fetch half. `refs/` is no longer part of this question at all — it is never
   pushed and never read on a box.
5. **Verifying the weights on a box that did not just receive them.** The
   `sha256sum -c` check runs inside `install_models`, so it runs only when the
   gate decided to push. A weight swapped at the same size within the same
   second is invisible to the directory signature *and* skips the verify. Today
   `bake-models.py --check` catches that on the inductor, and the publish gate
   (`16/16 files match`) catches it before a release; a box that is already in
   sync is taken on trust. Making the verify unconditional would cost a hash of
   668 MB on every provision of a healthy cluster, which is the cost the stamp
   exists to avoid, so it wants to be deliberate, not folded in.
