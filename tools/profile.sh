#!/usr/bin/env bash
# Profiles: genre bundles (assets + prompts) as single transfer files.
#
#   tools/profile.sh pack <name> [--level N] [--version V]
#                                        tar.zst the live tree -> profiles/<name>.tar.zst
#   tools/profile.sh fetch <name> [@version]
#                                        download profiles/<name>.tar.zst from the
#                                        `<name>-v<version>` GitHub release (latest
#                                        matching release when omitted), then verify
#   tools/profile.sh unpack <name>       verify bundle -> replace live tree, write .bm/profile
#   tools/profile.sh list                bundles with their manifest name/version
#   tools/profile.sh verify <name>       bundle manifest vs. bundle contents
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
VERSION=1
cmd=${1:?usage: profile.sh 'pack|fetch|unpack|list|verify' ...}; shift || true
case "$cmd" in
  pack|unpack|verify) name=${1:?usage: profile.sh "$cmd" <name>}; shift || true;;
  fetch) name=${1:?usage: profile.sh fetch <name> [@version]}; shift || true;
    version_arg=""; case "${1:-}" in @*) version_arg=${1#@}; shift;; esac;;
esac
while [ $# -gt 0 ]; do
  case "$1" in
    --level) LEVEL=${2:?--level needs a value}; shift 2;;
    --version) VERSION=${2:?--version needs a value}; shift 2;;
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

# Verify an extracted stage against its manifest, before anything moves: a
# corrupt bundle must never half-replace the live tree. $1 = stage dir.
verify_stage() {
  python3 - "$1" <<'EOF'
import hashlib, json, os, sys
stage = sys.argv[1]
m = json.load(open(os.path.join(stage, "manifest.json")))
bad = [p for p, want in m["files"].items()
       if hashlib.sha256(open(os.path.join(stage, p), "rb").read()).hexdigest() != want]
if bad:
    sys.exit("bundle corrupt, mismatched: " + " ".join(sorted(bad)[:5]))
print("bundle ok: " + m["name"] + " v" + m.get("version", "?"))
EOF
}

# owner/repo from the origin remote (both ssh and https spellings).
repo_slug() {
  git -C "$root" remote get-url origin 2>/dev/null | python3 -c '
import re, sys
u = sys.stdin.read().strip()
m = re.search(r"github\.com[:/](.+?)(?:\.git)?$", u)
sys.exit("origin is not a github remote: " + u) if not m else print(m.group(1))'
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
    VERSION="$VERSION" python3 - "$stage" "$name" <<'EOF'
import hashlib, json, os, sys
stage, name = sys.argv[1], sys.argv[2]
version = os.environ["VERSION"]
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
    json.dump({"name": name, "version": version, "files": files},
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
    printf 'publish: gh release create %s-v%s %s --title "%s v%s" --notes "profile bundle"\n' \
      "$name" "$VERSION" "$bundle" "$name" "$VERSION"
    tar --use-compress-program=unzstd -tf "$bundle" | head -5
    ;;
  fetch)
    command -v curl >/dev/null || { echo "curl not on PATH" >&2; exit 1; }
    slug=$(repo_slug)
    mkdir -p "$root/profiles"
    # Anonymous works on a public repo; a private one needs GH_TOKEN.
    # Resolves `<name>-v<version>`, or the newest `<name>-v*` when omitted.
    url=$(NAME="$name" WANT="$version_arg" SLUG="$slug" python3 - <<'EOF'
import json, os, sys, urllib.request
slug, name, want = os.environ["SLUG"], os.environ["NAME"], os.environ["WANT"]
want = want.lstrip("v")
req = urllib.request.Request(f"https://api.github.com/repos/{slug}/releases?per_page=100",
                             headers={"Accept": "application/vnd.github+json"})
if os.environ.get("GH_TOKEN"):
    req.add_header("Authorization", "Bearer " + os.environ["GH_TOKEN"])
rels = json.load(urllib.request.urlopen(req))
cands = [r for r in rels if (r.get("tag_name", "") + "/").startswith(f"{name}-v")]
if want:
    cands = [r for r in cands if r["tag_name"] == f"{name}-v{want}"]
if not cands:
    sys.exit(f"no release for profile {name}" + (f" version {want}" if want else ""))
rel = sorted(cands, key=lambda r: r.get("created_at", ""))[-1]
zst = [a["browser_download_url"] for a in rel.get("assets", []) if a["name"].endswith(".tar.zst")]
if not zst:
    sys.exit(f"release {rel['tag_name']} holds no .tar.zst asset")
print(zst[0])
EOF
)
    out="$root/profiles/.fetch.$name.tar.zst"
    curl -fsSL ${GH_TOKEN:+-H "Authorization: Bearer $GH_TOKEN"} -o "$out" "$url"
    mv "$out" "$root/profiles/$name.tar.zst"
    "${BASH_SOURCE[0]}" verify "$name"
    ;;
  unpack)
    bundle="$root/profiles/$name.tar.zst"
    [ -f "$bundle" ] || { echo "no such profile bundle (fetch it first: profile.sh fetch $name)" >&2; exit 1; }
    stage=$(mktemp -d "${TMPDIR:-/tmp}/bm-profile.XXXXXX")
    trap 'rm -rf "$stage"' EXIT
    tar --use-compress-program=unzstd -xf "$bundle" -C "$stage"
    verify_stage "$stage"
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
    verify_stage "$stage" && echo "OK $name"
    ;;
  *) echo "unknown command $cmd (pack|fetch|unpack|list|verify)" >&2; exit 1;;
esac
