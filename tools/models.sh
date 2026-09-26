#!/usr/bin/env bash
# Models: the baked TTS weights as a single transfer file.
#
#   tools/models.sh pack [--level N]
#                                     verify the bake -> models/models.tar.zst
#   tools/models.sh verify            bundle manifest vs. bundle contents
#   tools/models.sh publish [--notes "..."]
#                                     gh release create models-v<hash> with the bundle
#   tools/models.sh list              the local bundle, its hash and size
#
# The bundle is transfer and archive only: provisioning still rsyncs the
# directory, and the unpacked tree is what the sidecar reads. This exists so a
# new box can fetch 363 MB from a CDN instead of receiving 668 MB from your
# connection, and verify what arrived against a manifest that travelled in the
# same archive. The design is docs/ARTIFACTS.md; the fetch half is not built.
#
# What goes in, and what deliberately does not: the files `models/manifest.json`
# lists — the 16 immutable weights — plus that manifest. `models/voices.json`
# sits in the same directory and is **excluded**, because it is not a bake
# output: `pool::bake_missing_voices` rewrites it on the inductor during
# provisioning, and its source is a pip-installed package rather than a pinned
# revision. `bake-models.py` already leaves it out of the record; the file list
# here is read from that record, so the exclusion is one decision made once.
#
# The tag is named by the manifest hash — sha256 over sorted
# `name + NUL + content-sha256hex + NUL` lines, the same rule profile.sh uses,
# read out of a manifest whose entries carry `bytes` alongside the hash. That
# makes the name a function of the *contents* only: two machines with the same
# bake produce the same tag, and a tag can never name bytes it does not hold.
# `gh release create` refuses an existing tag, so immutability is enforced by
# the host rather than promised here.
#
# Level defaults to 3. These are fp32 ONNX graphs, which compress far better
# than their extension suggests — measured 668 MiB -> 363 MiB in 1.25 s — and
# decompression speed is level-independent.

set -euo pipefail

LEVEL=3
cmd=${1:?usage: models.sh 'pack|verify|publish|list'}; shift || true
NOTES=""
while [ $# -gt 0 ]; do
  case "$1" in
    --level) LEVEL=${2:?--level needs a value}; shift 2;;
    --notes) NOTES=${2:?--notes needs a value}; shift 2;;
    *) echo "unknown flag $1" >&2; exit 1;;
  esac
done

command -v tar >/dev/null || { echo "tar not on PATH" >&2; exit 1; }
command -v zstd >/dev/null || { echo "zstd not on PATH (brew install zstd)" >&2; exit 1; }
command -v python3 >/dev/null || { echo "python3 not on PATH" >&2; exit 1; }

root=$(dirname "$0")/..; root=$(cd "$root" && pwd)
models="$root/models"
bundle="$models/models.tar.zst"
manifest="$models/manifest.json"

# The manifest hash. Same rule as profile.sh::manifest_hash, but the models
# manifest stores `{bytes, sha256}` per entry rather than a bare hash string,
# so this reads one level deeper. Both hash `name` then `sha256`, NUL-separated,
# names sorted — that pairing is what makes the two scripts agree on what "the
# same bundle" means without either importing the other.
manifest_hash() { # $1 = manifest.json -> echo hash
  python3 -c '
import hashlib, json, sys
m = json.load(open(sys.argv[1]))
h = hashlib.sha256()
for name in sorted(m["files"]):
    h.update(name.encode()); h.update(b"\0")
    h.update(m["files"][name]["sha256"].encode()); h.update(b"\0")
print(h.hexdigest())' "$1"
}

# The bundle's members, in the order they are written: the manifest first, so a
# reader that stops early has already seen the index. One name per line into a
# file rather than an argv expansion — 16 is safe today, and a bake with a
# thousand shards would not be a quoting accident waiting to happen.
bundle_members() { # $1 = manifest.json -> echo member names, one per line
  python3 -c '
import json, sys
m = json.load(open(sys.argv[1]))
print("manifest.json")
for name in sorted(m["files"]):
    print(name)' "$1"
}

short() { printf %s "${1:0:12}"; }

case "$cmd" in
  list)
    if [ ! -f "$bundle" ]; then echo "no bundle at $bundle (run: models.sh pack)" >&2; exit 1; fi
    hash=$(manifest_hash "$manifest")
    printf 'bundle  %s\n' "$bundle"
    printf 'size    %s\n' "$(du -h "$bundle" | cut -f1)"
    printf 'hash    %s\n' "$hash"
    printf 'tag     models-v%s\n' "$(short "$hash")"
    ;;

  pack)
    # The gate. Not a formality: `--check` is the only thing that compares the
    # weights to what the bake recorded, and a bundle cut while it is failing
    # ships bytes that disagree with the manifest inside it — which the box
    # would then reject, having spent 363 MB finding out.
    if ! python3 "$root/tools/bake-models.py" --check; then
      echo "refusing to pack: the bake does not match its manifest (above)" >&2
      echo "re-bake with: python3 tools/bake-models.py" >&2
      exit 1
    fi
    members=$(mktemp "${TMPDIR:-/tmp}/bm-models-members.XXXXXX")
    trap 'rm -f "$members"' EXIT
    bundle_members "$manifest" > "$members"
    mkdir -p "$models"
    # -T reads names relative to -C, so the archive holds `manifest.json` and the
    # weights at the top level — no `models/` prefix to strip on the way out,
    # which is what lets the fetch side untar straight into the worker root.
    tmp="$bundle.tmp"
    tar -cf - -C "$models" -T "$members" | zstd -"$LEVEL" -o "$tmp"
    mv "$tmp" "$bundle"
    hash=$(manifest_hash "$manifest")
    printf 'packed %s  (%s at level %s)\n' "$bundle" "$(du -h "$bundle" | cut -f1)" "$LEVEL"
    # Members are the manifest plus every weight, so the weight count is one
    # less — said precisely because "17 files" reads as 17 weights.
    printf 'files  %s weights + manifest.json\n' "$(( $(grep -c . "$members") - 1 ))"
    printf 'hash   %s\n' "$hash"
    printf 'tag    models-v%s\n' "$(short "$hash")"
    printf 'publish: tools/models.sh publish\n'
    ;;

  verify)
    [ -f "$bundle" ] || { echo "no bundle at $bundle (run: models.sh pack)" >&2; exit 1; }
    stage=$(mktemp -d "${TMPDIR:-/tmp}/bm-models.XXXXXX")
    trap 'rm -rf "$stage"' EXIT
    tar --use-compress-program=unzstd -xf "$bundle" -C "$stage"
    python3 - "$stage" <<'PY'
import hashlib, json, os, sys
stage = sys.argv[1]
man_path = os.path.join(stage, "manifest.json")
if not os.path.isfile(man_path):
    sys.exit("bundle holds no manifest.json — nothing to verify against")
man = json.load(open(man_path))
want = man["files"]

# What is actually in the archive, relative to the stage.
got = set()
for dp, _, fns in os.walk(stage):
    for fn in fns:
        got.add(os.path.relpath(os.path.join(dp, fn), stage))
got.discard("manifest.json")

listed = set(want)
extra = sorted(got - listed)
missing = sorted(listed - got)
if extra or missing:
    # `extra` is the one worth naming out loud: an unlisted file travelling in
    # the archive is either a mistake here or a mistake in the bake's record,
    # and either way the box would be checking a set it was not told about.
    for name in extra:
        print(f"  UNLISTED {name} — in the bundle, absent from the manifest")
    for name in missing:
        print(f"  MISSING {name} — listed by the manifest, absent from the bundle")
    sys.exit(f"{len(listed)} listed, {len(got)} present — bundle and manifest disagree")

bad = []
for name, entry in sorted(want.items()):
    if entry.get("sha256") == hashlib.sha256(open(os.path.join(stage, name), "rb").read()).hexdigest():
        continue
    bad.append(name)
if bad:
    for name in bad:
        print(f"  CORRUPT {name}")
    sys.exit(f"{len(bad)} file(s) do not match the manifest")

h = hashlib.sha256()
for name in sorted(want):
    h.update(name.encode()); h.update(b"\0")
    h.update(want[name]["sha256"].encode()); h.update(b"\0")
print(f"{len(want)} files verified, none unlisted")
print(f"hash {h.hexdigest()}")
PY
    ;;

  publish)
    [ -f "$bundle" ] || { echo "no bundle at $bundle (run: models.sh pack)" >&2; exit 1; }
    command -v gh >/dev/null || { echo "gh not on PATH" >&2; exit 1; }
    hash=$(manifest_hash "$manifest")
    tag="models-v$(short "$hash")"
    if gh release view "$tag" >/dev/null 2>&1; then
      echo "$tag already exists — a hash names one bundle, so there is nothing to make" >&2
      exit 1
    fi
    # The public warning is not boilerplate: this repository is public, so the
    # asset is world-readable the moment it lands. The weights are baked from
    # public models and hold nothing secret, and saying so here is cheaper than
    # someone rediscovering it from the release page.
    weights=$(( $(bundle_members "$manifest" | grep -c .) - 1 ))
    notes=${NOTES:-"Baked TTS weights for Storycast: $weights weight files + their manifest, $(du -h "$bundle" | cut -f1) compressed.

manifest hash \`$hash\`
backbone \`$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["backbone_rev"])' "$manifest")\`
codec \`$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["codec_rev"])' "$manifest")\`

The tag is the first 12 hex of the manifest hash, which is a function of the
file contents alone — so this tag names exactly these bytes and can never name
others. This repository is public and so is this asset; it contains nothing but
public model weights and the manifest that describes them."}
    gh release create "$tag" "$bundle" --title "$tag" --notes "$notes"
    printf 'published %s\n' "$tag"
    ;;

  *) echo "unknown command $cmd" >&2; exit 1;;
esac
