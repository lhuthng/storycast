# Artifacts — the weights, the voice store, and who fetches what

## In plain words

*You can stop reading after this section.*

Provisioning a new worker means sending it about 886 MB. Three quarters of that
is one directory of TTS weights — the same 667 MB for every box, forever, because
the weights only change when you deliberately re-bake them.

That is the wrong shape. The files are immutable, they are large, and they are
identical on every machine, so they behave exactly like a released artifact: name
them by their content, publish them once, and let each box download them itself.
That is what this document designs.

What stays on the rsync is everything that genuinely differs per box or changes
often: the prompts, the crawlers, the cast files, the small JSON manifests, and
one 492 KB voice store that is rewritten every time a voice is enrolled.

The result is that provisioning a new box sends roughly **24 MB from your
machine** instead of 886 MB. The box still downloads the weights — from a CDN,
in parallel with every other box, instead of serially through your home uplink.

The rule the whole design turns on: **a file belongs in the artifact if and only
if its bytes are decided by a version, not by a workspace, a profile, or an
operation.** The voice store fails that test. The weights pass it.

## What is in the plane, and what is not

Four pushes dominate provisioning. Only one of them is a candidate for
publishing, and the reasons differ for each.

| Push | Size | Verdict |
|---|---|---|
| `models/` | 668 MB | **publish** — 16 immutable weight files |
| `refs/` | 125 MB | candidate — changes only when a clip is added |
| `assets/` media | 55 MB | candidate — same |
| binaries + runtime | 52 MB | ship, but small enough not to matter |

Everything else — prompts (24 KB), the crawlers (72 KB), cast files, the scene
map, the three pool registries, `voices.json` (492 KB) — is under 1 MB combined
and is never worth optimizing.

## The split: weights out, voice store stays

`models/` is not one thing. It is a large immutable body and one small mutable
file that happens to sit in the same directory, and the two have opposite
requirements.

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

* `--check` is now a real gate — it reads `16/16 files match` and exits 0,
  where before it could never pass on a cluster that had ever enrolled a voice.
* No consumer needs a special case. The publish gate below is literally the
  `--check` result, and the `sha256sum -c` list provisioning writes needs no
  exclusion rule — it is built from `files`, which no longer contains the store.
* `total_bytes` equals the sum of `files`, which is the property a receipt
  should have. It was previously inflated by a file it could not vouch for.

The deeper reason is provenance, not mutability. Of the seventeen sources, only
`backbone_rev` and `codec_rev` cover pinned HuggingFace revisions; `sea_g2p.bin`
is pinned by being vendored in-tree. **`voices.json` was copied from a
pip-installed package** — nothing pinned it, in-tree or by revision — and it was
the one entry that had drifted. A receipt that cannot stand behind a hash is
worse than a receipt that omits the field.

**The split, concretely:**

```
artifact  models/<hash>.tar.zst          17 files, 667.5 MB → 363 MB compressed
         manifest.json + the 16 weight files
         pinned by a version, verified per file, never rewritten in place

rsync     models/voices.json             1 file, 492 KB
         rewritten on enrollment; the stamp already gates it correctly
```

492 KB is 0.07% of the old payload. Leaving it on the rsync costs nothing and
keeps every existing voice behavior — the delta is what actually moves, and the
stamp's `models_need_push` / `voice_store_covers` pair already handles it. **This
split is a correctness requirement, not an optimization.**

## Naming: the hash is the name

The artifact is named by the hash of the bytes it contains, and that hash is
computed from the manifest the box already holds.

This is the pattern `profile_object()` already used for the profile plane
(`s3://<bucket>/profiles/<hash>.tar.zst`), and it is worth keeping the reasoning
rather than just the shape: because the object key is derived from a hash the box
already verifies against, **there is no mapping to keep in sync, and a box cannot
be handed a bundle that disagrees with the pointer it checks.** A box either asks
for the artifact matching what it wants, or it asks for nothing.

Publishing is therefore idempotent. Re-packing identical content at a different
compression level overwrites the same key with the same tree, which is the
correct outcome — and the manifest inside is what gets checked, file by file,
before anything moves.

With GitHub Releases the key becomes the release tag:

```
https://github.com/<owner>/<repo>/releases/download/models-v<hash>/models.tar.zst
```

and the mechanism is one this repo already runs: `tools/profile.sh fetch` queries
`api.github.com/…/releases`, selects the release whose `tag_name` matches, picks
the asset ending in `.tar.zst`, and curls its `browser_download_url`, with
`GH_TOKEN` used only when set. Public repo means no token on the box.

## Publishing

A `make models-publish` (or `tools/models.sh pack`) that:

1. runs `python3 tools/bake-models.py --check` and **refuses to publish on
   anything but `16/16 files match`** — the `--check` gate is the whole safety
   story, and it already exists;
2. tars the 17 files, excluding `voices.json` by name, at the level
   `tools/profile.sh` already uses;
3. writes the bundle, records its sha256, and creates the release asset.

The gate is `python3 tools/bake-models.py --check` reporting `16/16 files match`
and exiting 0. It did not used to be usable as a gate, because the roster sat in
the record and drifted on every enrollment; now that it is out, the gate and the
bundle are the same set and neither needs an exclusion rule of its own.

The pack must still select by **name, not by manifest membership**, for the
reason every content-addressed system needs that rule: `voices.json` exists in
the directory and is absent from the record, so "everything the manifest lists"
and "everything in `models/`" are different questions and only one of them is
the bundle.

## Fetching, on the box

The box needs **nothing preinstalled except `curl`**, which the probe already
checks (`provision/steps.rs:291`).

`bm-agent` gains a subcommand:

```
bm-agent fetch-artifact <url> <dest-dir>
```

which streams to a temp file beside the destination, decompresses, verifies every
file against the manifest's per-file sha256, and only then `rename()`s the
directory into place.

Two decisions inside that:

**The agent does the decompression, not a shipped `zstd` binary.** The `zstd`
crate is a pure-Rust decoder, and this repo already vendors C where it earns its
keep (`sea-g2p`). A shipped per-target `zstd` binary would be a second
cross-built binary to version-gate — reproducing exactly the `bm-tts` staleness
bug that was just fixed, to save a megabyte. `apt-get install zstd` on the box
would be a second `sudo -n` gamble alongside the ffmpeg one.

**The box does not fetch a tarball over a directory it is using.** A failed
download that leaves a partial `models/` in place is the failure mode this design
exists to remove, and the current code cannot even detect it: `MODELS-OK` tests
only that `manifest.json` **exists**. So today a box that dies mid-rsync passes
its own readiness check. Fetch-to-temp, verify all 17 hashes, rename is what
makes a half-fetched box unrepresentable rather than merely unlikely.

Compression is worth doing here, and the measurement is the reason to be
specific about the level:

```
668 MiB => 363 MiB   (54.31%)   zstd -3, 1.25s on an M-series laptop
```

fp32 ONNX weights compress nearly 2:1 — they are not the incompressible blob that
plain `.onnx` suggests. Note what this does and does not save: `rsync -z` is
*already* achieving this on the wire today. The artifact does not compress better
than the rsync. **What it changes is whose uplink pays for it** — 363 MB that
currently leaves your house once per box, serially, now comes off a CDN with
every box fetching at once.

## ffmpeg is a different problem, and already solved

ffmpeg is not part of this plane and needs no work.

It is not a library call — `bm-core/src/ambience.rs` spawns
`Command::new("ffmpeg")` by name, so linking libav\* into the agent would not
change the code path at all. And linking it is not a real option: libavcodec,
libavformat, libavfilter, libswscale and libswresample are tens of megabytes with
hundreds of external codec dependencies and LGPL/GPL exposure.

More to the point, **the box-without-ffmpeg case is already the designed-for
case.** `bm-agent` advertises the `merge` capability *only when ffmpeg is on
PATH*; `ensure_ffmpeg` tries `apt-get`/`dnf`/`yum` under `sudo -n`; a refusal is
a warning, never fatal, and the machine summary says
`· NO FFMPEG — merges will fail here`. Merge goes off that box, crawl, digest and
render keep working.

And ffmpeg is **0 bytes in the payload** — it is a package-manager install on the
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
answer is to stop and say which file mismatched, not to try harder.## What the stamp changes — **done**

This section and the two sections after it describe work that has landed. What
remains is the artifact itself (publish, fetch, verify on the box); everything
below is in the tree.

`tts_hash` was `models/manifest.json` content plus a directory signature of
`models/` — which is one fact where there are two, because the directory holds
an immutable bake and one mutable file. It is now four digests, each gating a
different push:

| digest | covers | gates |
|---|---|---|
| `sources_hash` | prompts, small manifests, casts, clips, crawls, **`refs/`**, agent version | `install_sources` |
| `tts_hash` | the bake **minus `models/voices.json`**, by signature, + `manifest.json` by content | the weights push |
| `voices_hash` | `models/voices.json` by content | the weights push, alongside `tts_hash` |
| `tts_bin_hash` | the `bm-tts` bytes | the sidecar push |

Three of those used to be wrong or absent, all in the same direction — a gate
that was not where the push was:

- **`refs/` was gated by nothing.** It is pushed by `install_sources`, but it
  was only folded into `voices_hash`, which provisioning computed, carried, and
  never read. So adding or editing a 125 MB reference clip drifted no gate that
  any push consulted. It is now in `sources_hash`, which is the digest the push
  that carries it actually reads. (The test that asserted the old behaviour —
  "refs/ is not part of the sources hash" — is now its inverse.)
- **The sidecar never redeployed.** `install_tts_runtime` was only called in the
  `else` of `if already`, and `tts_hash` covers `models/`, not the binary. A
  rebuilt `bm-tts` stayed on the inductor for ever while the box served the old
  one. `tts_bin_hash` is `agent_hash`'s pattern applied to the second binary,
  and the push now also recycles the sidecar — a replaced binary on disk does
  nothing while the old process is still running it.
- **A newly enrolled voice could be invisible.** With `voices.json` excluded
  from `tts_hash`, nothing would have covered the roster at all. `voices_hash`
  is consulted again, narrowed to that one file: an enrollment reaches the box
  without re-sending 668 MB, and a re-bake still resyncs without looking like an
  enrollment.

Two smaller things landed with them:

- **The weights are verified, not assumed.** rsync exiting 0 says the transfer
  worked, not that the bytes are intact — a box that dies mid-push, a source
  file already corrupt, or a `--delete` racing a writer all produce a directory
  rsync is happy with and the sidecar is not. `install_models` now writes the
  bake's own `sha256` entries out as a `sha256sum -c` list and checks them on
  the box. The check used to be that `manifest.json` **exists**, which a
  half-written bake satisfies perfectly. `models/voices.json` is excluded from
  the list for the same reason it is excluded from `tts_hash`: its manifest
  entry is stale by design, because enrollment rewrites the file after the bake.
- **The probe no longer needs Python.** It read the voice roster by shelling out
  to `python3 -c "import json…"` against `models/voices.json`. It now asks the
  sidecar's `/voices` endpoint instead — `curl` was already required for
  `/health` — which is also the better answer: the roster a render will actually
  find, rather than what a file claims. An **unknown** roster is no longer read
  as a missing one, which matters because reading it that way answered a down
  sidecar with a 668 MB push.

## What the inductor still sends

| | today | after |
|---|---|---|
| `models/` | 363 MB (`-z`) | **0** — box fetches |
| `libonnxruntime.so*` | 28 MB | 0 — rides in the same artifact |
| `refs/` | 125 MB | 125 MB |
| `assets/` media | 55 MB | 55 MB |
| `bm-agent` | 15 MB | 15 MB |
| `bm-tts` | 9.4 MB | 9.4 MB |
| casts, prompts, crawlers, `voices.json` | 1 MB | 1 MB |
| **from your machine** | **~595 MB** | **~205 MB** |

`bm-agent` is irreducibly 15 MB: it is the thing doing the fetching, so it has to
be on the box first. Chasing that last 15 MB means a bootstrap that cannot verify
what it downloaded, which is the trade this design exists to avoid.

Publishing `refs/` and the `assets/` media as well — they qualify under the same
rule, changing only when a clip is added — takes the final figure to **~24 MB**.
`refs/` needs its own hash gate first (see above); that gate is the prerequisite,
not an optional extra.

## Failure modes

| What happens | What the operator sees |
|---|---|
| Release missing for the wanted hash | falls back to rsync, names the URL it tried |
| Download truncated | per-file sha256 mismatch, the file named, the box left untouched |
| Weights swapped under an unchanged name | impossible — the name is the hash |
| Artifact published with `voices.json` inside | the box rejects it; `bake --check` is the gate meant to prevent it |
| `bm-agent` too old to have `fetch-artifact` | falls back to rsync |
| GitHub unreachable | falls back to rsync, slowly, and says so |

Every one of these is *slower* or *louder* than today's behavior. None is silent,
and that is the requirement: a box that is quietly holding the wrong weights is
the failure mode worth engineering against.

## Removing the S3 bucket — **done**

The bucket was sketched for this and never used — an empty `bucket` had always
meant "rsync from here", which is a working configuration. With models and
profiles both headed for Releases it had no consumer left, so the whole concept
is gone rather than left as a second, unbuilt path:

- `AwsConfig::bucket`, its initializer, and `publishes_assets()` (`aws.rs`);
- the `summary()` branch that printed `s3://…` vs `assets: rsync from here`;
- `profile_object()` and its `aws up` call site in `main.rs` — plus the
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
effect — **the worker role needs no permissions at all**, which is three fewer
paragraphs to keep true across three documents. The role is still required,
because every launch names an instance profile.

## Open questions

1. **Tag or release per hash?** A `models-v<hash>` tag per bake is immutable and
   obvious but pollutes the tag list. One rolling `models-latest` release
   overwrites in place and relies on the per-file verify for correctness. The
   first is safer; the second is tidier.
2. **Pruning.** Content-addressed names never overwrite, so every re-bake leaves
   363 MB behind forever. `tools/profile.sh` has no prune either.
3. **Public repo.** A Release asset on a public repo is world-readable. The
   weights are fine — they are baked from public models and contain nothing
   secret — but this is a decision to make deliberately, and the answer changes
   if anyone ever bakes private material into `models/`.
4. **`refs/` and `assets/` media.** Worth publishing for the same reason —
   they change only when a clip is added, not per-provision. `refs/` now has the
   gate it needed (`sources_hash`, above), so it is unblocked; `assets/effects`,
   `assets/music` and `assets/injects` were already gated by signature there.
   What is still missing is a *content* hash for them, since a signature is
   size+mtime and the artifact name has to be a content hash to be worth
   anything. That is a 180 MB sha256 pass on the inductor, run once per publish,
   not per provision.
5. **Verifying the weights on a box that did not just receive them.** The
   `sha256sum -c` check runs inside `install_models`, so it runs only when the
   gate decided to push. A weight swapped at the same size within the same
   second is invisible to the directory signature *and* skips the verify. Today
   `bake-models.py --check` catches that on the inductor, and the publish gate
   (`16/16 files match`) catches it before a release; a box that is already in
   sync is taken on trust. Making the verify unconditional would cost a hash of
   668 MB on every provision of a healthy cluster, which is the cost the stamp
   exists to avoid — so it wants to be deliberate, not folded in.
