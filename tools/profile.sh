#!/usr/bin/env bash
# Profiles: genre bundles (assets + prompts) as single transfer files.
#
#   tools/profile.sh pack <name> [--level N]   tar.zst the live tree -> profiles/<name>.tar.zst
#   tools/profile.sh unpack <name>             verify bundle -> replace live tree, write .bm/profile
#   tools/profile.sh list                      bundles with their manifest name/version
#   tools/profile.sh verify <name>             bundle manifest vs. bundle contents
#
# The bundle is transfer and archive only: day to day the pipeline reads the
# unpacked live tree, and bm-core::profile::verify gates runners on the
# pointer hash. Pack/unpack/compare live here in shell (like ssh/rsync/ffmpeg);
# hashing must match profile.rs exactly: sha256 over sorted
# `rel-path + NUL + content-sha256hex + NUL` lines.
#
# Level defaults to 3. Higher levels buy almost nothing here — the tree is
# nearly all mp3, which no level compresses further — and cost ~10x the pack
# time. Decompression speed is level-independent.

set -euo pipefail

LEVEL=3
cmd=${1:?usage: profile.sh 'pack|unpack|list|verify' ...}; shift || true
case "$cmd" in
  pack|unpack|verify) name=${1:?usage: profile.sh "$cmd" <name>}; shift || true;;
esac
while [ $# -gt 0 ]; do
  case "$1" in
    --level) LEVEL=${2:?--level needs a value}; shift 2;;
    *) echo "unknown flag $1" >&2; exit 1;;
  esac
done

command -v tar >/dev/null || { echo "tar not on PATH" >&2; exit 1; }
command -v zstd >/dev/null || { echo "zstd not on PATH (brew install zstd)" >&2; exit 1; }
command -v python3 >/dev/null || { echo "python3 not on PATH" >&2; exit 1; }

root=$(dirname "$0")/..
root=$(cd "$root" && pwd)
# Set per command below: `list` takes no bundle.
bundle=""

# Manifest hash over {rel: sha} — the same bytes profile.rs hashes.
manifest_hash() { # $1 = manifest.json -> echo hash
  python3 -c '
import hashlib, json, sys
m = json.load(open(sys.argv[1]))
h = hashlib.sha256()
for path in sorted(m["files"]):
    h.update(path.encode()); h.update(b"\0")
    h.update(m["files"][path].encode()); h.update(b"\0")
print(h.hexdigest())' "$1"
}

case "$cmd" in
  pack)
    bundle="$root/profiles/$name.tar.zst"
    [ -d "$root/assets" ] || { echo "no live assets/ to pack" >&2; exit 1; }
    [ -d "$root/prompts" ] || { echo "no live prompts/ to pack" >&2; exit 1; }
    mkdir -p "$root/profiles"
    stage=$(mktemp -d "$root/profiles/.pack.XXXXXX")
    trap 'rm -rf "$stage"' EXIT
    cp -r "$root/assets" "$root/prompts" "$stage/"
    python3 - "$stage" "$name" <<'EOF'
import hashlib, json, os, sys
stage, name = sys.argv[1], sys.argv[2]
files = {}
for dir in ("assets", "prompts"):
    for dp, _, fns in os.walk(os.path.join(stage, dir)):
        for fn in sorted(fns):
            # Mirrors the tar --exclude below: OS noise is not content and
            # must not hash-drift an unpack on another machine.
            if fn == ".DS_Store":
                continue
            p = os.path.join(dp, fn)
            rel = os.path.relpath(p, stage)
            files[rel] = hashlib.sha256(open(p, "rb").read()).hexdigest()
with open(os.path.join(stage, "manifest.json"), "w") as fh:
    json.dump({"name": name, "version": "1", "files": files},
              fh, indent=2, sort_keys=True)
    fh.write("\n")
EOF
    out="$root/profiles/.pack.$name.tar.zst"
    # OS noise never enters a bundle: a .DS_Store would hash-drift every
    # unpack on a different machine for zero content.
    tar --exclude=.DS_Store -cf - -C "$stage" assets prompts manifest.json | zstd -"$LEVEL" -o "$out"
    mv "$out" "$bundle"
    trap - EXIT; rm -rf "$stage"
    printf 'packed %s  (%s at level %s, manifest %s)\n' \
      "$bundle" "$(du -h "$bundle" | cut -f1)" "$LEVEL" \
      "$(tar --use-compress-program=unzstd -xOf "$bundle" manifest.json | manifest_hash /dev/stdin)"
    tar --use-compress-program=unzstd -tf "$bundle" | head -5
    ;;
  unpack)
    bundle="$root/profiles/$name.tar.zst"
    [ -f "$bundle" ] || { echo "no such profile bundle: $bundle" >&2; exit 1; }
    stage=$(mktemp -d "${TMPDIR:-/tmp}/bm-profile.XXXXXX")
    trap 'rm -rf "$stage"' EXIT
    tar --use-compress-program=unzstd -xf "$bundle" -C "$stage"
    # Verify before anything moves: a corrupt bundle must never half-replace
    # the live tree.
    python3 - "$stage" <<'EOF'
import hashlib, json, os, sys
stage = sys.argv[1]
m = json.load(open(os.path.join(stage, "manifest.json")))
bad = [p for p, want in m["files"].items()
       if hashlib.sha256(open(os.path.join(stage, p), "rb").read()).hexdigest() != want]
if bad:
    sys.exit("bundle corrupt, mismatched: " + " ".join(sorted(bad)[:5]))
print("bundle ok: " + m["name"] + " v" + m.get("version", "?"))
EOF
    rm -rf "$root/assets" "$root/prompts"
    mv "$stage/assets" "$stage/prompts" "$root/"
    mkdir -p "$root/.bm"
    python3 - "$stage/manifest.json" "$root/.bm/profile" <<'EOF'
import json, sys
manifest_path, pointer_path = sys.argv[1], sys.argv[2]
m = json.load(open(manifest_path))
import hashlib
h = hashlib.sha256()
for path in sorted(m["files"]):
    h.update(path.encode()); h.update(b"\0")
    h.update(m["files"][path].encode()); h.update(b"\0")
json.dump({"name": m["name"], "hash": h.hexdigest()},
          open(pointer_path, "w"), indent=2)
open(pointer_path, "a").write("\n")
print("loaded profile " + m["name"] + " (" + h.hexdigest()[:12] + ")")
EOF
    trap - EXIT; rm -rf "$stage"
    ;;
  list)
    shopt -s nullglob
    for b in "$root"/profiles/*.tar.zst; do
      man=$(tar --use-compress-program=unzstd -xOf "$b" manifest.json 2>/dev/null || echo '{}')
      name_v=$(echo "$man" | python3 -c 'import json,sys; m=json.load(sys.stdin); print(m.get("name","?")+" v"+m.get("version","?"))')
      printf '  %-24s %s  %s\n' "$(basename "$b")" "$name_v" "$(du -h "$b" | cut -f1)"
    done
    ;;
  verify)
    bundle="$root/profiles/$name.tar.zst"
    [ -f "$bundle" ] || { echo "no such profile bundle: $bundle" >&2; exit 1; }
    stage=$(mktemp -d "${TMPDIR:-/tmp}/bm-profile.XXXXXX")
    trap 'rm -rf "$stage"' EXIT
    tar --use-compress-program=unzstd -xf "$bundle" -C "$stage"
    python3 - "$stage" <<'EOF'
import hashlib, json, os, sys
stage = sys.argv[1]
m = json.load(open(os.path.join(stage, "manifest.json")))
bad = [p for p, want in m["files"].items()
       if hashlib.sha256(open(os.path.join(stage, p), "rb").read()).hexdigest() != want]
sys.exit(("MISMATCH: " + " ".join(sorted(bad))) if bad else print("OK " + m["name"]))
EOF
    ;;
  *) echo "unknown command $cmd (pack|unpack|list|verify)" >&2; exit 1;;
esac
