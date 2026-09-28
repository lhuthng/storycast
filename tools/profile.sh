#!/usr/bin/env bash
# Profile releases: one bundle per piece, as transfer files.
#
#   tools/profile.sh pack <name> [--piece pack|adapter] [--level N] [--version V]
#                                        tar.zst the live piece -> profiles/<piece>/<name>.tar.zst
#   tools/profile.sh fetch <name> [@version] [--piece p]
#                                        download it from the `<name>-<piece>-v<version>`
#                                        GitHub release (latest matching when omitted), then verify
#   tools/profile.sh unpack <name> [--piece p]
#                                        verify -> replace that piece, merge `.bm/profile`
#   tools/profile.sh list                releases, with their piece and version
#   tools/profile.sh verify <name> [--piece p]
#                                        release manifest vs. release contents
#
# **One release per piece.** A pack is the genre's art; a language is its prompts
# and its crawlers. They used to travel as one file, which meant a second
# language cost a second copy of 58 MB of music. `--piece` is which half, and it
# defaults to `pack`.
#
# The release is transfer and archive only: day to day the pipeline reads the
# unpacked live tree, and `bm_core::profile::verify_binding` gates runners on the
# pointer hash. Tar and zstd live here in shell (like ssh/rsync/ffmpeg), but the
# **manifest does not**: it needs the piece's binding-aware trees and, for a pack,
# the composition record of what it was built on, so it is computed by
# `bm-inductor profile manifest` — which also refuses to pack a tree whose
# dependencies have moved. Re-implementing either here is how the two would drift.
#
# The manifest's keys are the paths the release unpacks to, so the release and
# the live tree it came from hash to the same number, and `unpack` never has to
# re-stamp a hash it just changed.
#
# Level defaults to 3. Higher levels buy almost nothing here — the tree is
# nearly all mp3, which no level compresses further — and cost ~10x the pack
# time. Decompression speed is level-independent.

set -euo pipefail

LEVEL=3
VERSION=
PIECE=pack
DEP=
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
    --piece) PIECE=${2:?--piece needs a value}; shift 2;;
    --dep) DEP=1; shift;;
    *) echo "unknown flag $1" >&2; exit 1;;
  esac
done
case "$PIECE" in pack|adapter) ;; *) echo "--piece must be 'pack' or 'adapter'" >&2; exit 1;; esac

command -v tar >/dev/null || { echo "tar not on PATH" >&2; exit 1; }
command -v zstd >/dev/null || { echo "zstd not on PATH (brew install zstd)" >&2; exit 1; }
command -v python3 >/dev/null || { echo "python3 not on PATH" >&2; exit 1; }

root=$(dirname "$0")/..
root=$(cd "$root" && pwd)
# Set per command below: `list` takes no bundle.
bundle=""

# The manifest comes from the Rust side: it knows the piece's live trees and the
# composition record, and it is where the staleness gate lives.
#
# The workspace's own build comes first and **debug before release**, the order
# `Layout::sidecar_binary` uses and for the same reason: `cargo build` keeps the
# debug binary fresh, so a release binary from last week is the stale one that
# would answer "unrecognized subcommand" to a flag added this morning. A release
# build is the fallback, then `cargo run`, and `BM_INDUCTOR` overrides all three.
inductor() {
  if [ -n "${BM_INDUCTOR:-}" ]; then "$BM_INDUCTOR" --root "$root" "$@"; return; fi
  for b in "$root/rust/target/debug/bm-inductor" "$root/rust/target/release/bm-inductor"; do
    if [ -x "$b" ]; then "$b" --root "$root" "$@"; return; fi
  done
  (cd "$root/rust" && cargo run --quiet -p bm-inductor -- --root "$root" "$@")
}

# The trees a release carries, relative to the checkout root — and therefore the
# keys its manifest uses, because the manifest says where a release lands. A
# language lands in its own home, not in a flat pair the next language would
# collide with.
piece_members() { # $1 = piece, $2 = name
  case "$1" in
    pack) echo "assets";;
    adapter) echo "adapters/$2";;
  esac
}

# The directories of a member tree that never enter a bundle. A pack release is
# the **resolved** content every reader wants — `_extends/` is the unpacked
# *input* side of the composition, and shipping it inside `assets/` would fold
# composition inputs into whatever unpacked the bundle (a worker, a workspace,
# a fresh checkout). `bm-inductor profile manifest` already refuses `_extends`
# keys in a pack manifest — and a manifest that says one thing while the
# tarball holds another is exactly the drift this toolchain exists to refuse.
piece_excludes() { # $1 = piece
  case "$1" in
    pack) echo "--exclude=assets/_extends";;
    adapter) ;;
  esac
}

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
print("bundle ok: " + m.get("piece", "pack") + " " + m["name"] + " v" + m.get("version", "?"))
EOF
}

# Merge one piece into the load pointer.
#
# The pointer is a **binding** — one name and hash per piece — and this mirrors
# `profile.rs`'s shim exactly: a pre-split document is `{name, hash}` and was the
# pack, because that is what the one bundle held. $1 = manifest, $2 = pointer.
stamp_piece() {
  python3 - "$1" "$2" "$PIECE" <<'EOF'
import hashlib, json, os, sys
manifest_path, pointer_path, piece = sys.argv[1], sys.argv[2], sys.argv[3]
m = json.load(open(manifest_path))
h = hashlib.sha256()
for path in sorted(m["files"]):
    h.update(path.encode()); h.update(b"\0")
    h.update(m["files"][path].encode()); h.update(b"\0")
binding = {p: {"name": "", "hash": ""} for p in ("pack", "adapter", "engine")}
if os.path.exists(pointer_path):
    old = json.load(open(pointer_path))
    if "name" in old or "hash" in old:
        binding["pack"] = {"name": old.get("name", ""), "hash": old.get("hash", "")}
    else:
        for p in binding:
            if isinstance(old.get(p), dict):
                binding[p] = {"name": old[p].get("name", ""), "hash": old[p].get("hash", "")}
binding[piece] = {"name": m["name"], "hash": h.hexdigest()}
os.makedirs(os.path.dirname(pointer_path), exist_ok=True)
with open(pointer_path, "w") as fh:
    json.dump(binding, fh, indent=2)
    fh.write("\n")
print("loaded " + piece + " " + m["name"] + " (" + h.hexdigest()[:12] + ")")
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
    # `--dep` releases a dependency tree itself, so the bundle is named by the
    # dependency and lands in the same per-piece directory.
    if [ -n "$DEP" ]; then
      PIECE=pack
    fi
    bundle="$root/profiles/$PIECE/$name.tar.zst"
    mkdir -p "$root/profiles/$PIECE"
    stage=$(mktemp -d "$root/profiles/.pack.XXXXXX")
    trap 'rm -rf "$stage"' EXIT
    # The manifest first, deliberately: it is where a stale tree is refused, so
    # nothing is copied or compressed for a release that will not be true.
    inductor profile manifest "$name" --piece "$PIECE" ${DEP:+--dep} \
      ${VERSION:+--version "$VERSION"} \
      > "$stage/manifest.json"
    # A `--dep` release is a dependency of the live pack, sanitized: the tree
    # at `assets/_extends/<name>` is copied **to `assets/`**, where the pack's
    # own resolution reads it, and the manifest the gate just refused to lie
    # about is written beside it as the generated `assets/pack.json`.
    if [ -n "${DEP:-}" ]; then
      src="$root/assets/_extends/$name"
      [ -d "$src" ] || { echo "no dependency tree at assets/_extends/$name — resolve first" >&2; exit 1; }
      # Rename semantics on purpose: `$stage/assets` does not exist yet, so the
      # dependency's tree BECOMES `assets/`. Pre-creating it would nest the tree
      # inside (`assets/<name>/…`) — which is exactly the paths the manifest
      # does not list.
      cp -R "$src" "$stage/assets"
      # Bookkeeping is not content, and the manifest refuses it — the same rule
      # `compose` applies when folding a dependency in. `pack.json` is written
      # here instead so the release states its own, empty extension point: a
      # consumer fills theirs in, they never inherit this one's.
      rm -f "$stage/assets/_extends.json"
      rm -f "$stage/assets/pack.json"
      python3 - "$stage/assets/pack.json" <<'EOF'
import json, sys
open(sys.argv[1], "w").write(json.dumps({
    "_note": "Generated at release time: this is the dependency tree of the live"
             " checkout, unpacked to assets/ — what a pack that extends this one"
             " names in its own deps (weakest first). Own content wins over every"
             " dependency.",
    "deps": [],
}, indent=2) + "\n")
EOF
    else
      for member in $(piece_members "$PIECE" "$name"); do
        [ -d "$root/$member" ] || { echo "no live $member/ to pack" >&2; exit 1; }
        mkdir -p "$stage/$(dirname "$member")"
        cp -r "$root/$member" "$stage/$member"
      done
    fi
    out="$root/profiles/.pack.$name.tar.zst"
    # OS noise never enters a bundle: a .DS_Store would hash-drift every
    # unpack on a different machine for zero content.
    tar --exclude=.DS_Store $(piece_excludes "$PIECE") -cf - -C "$stage" \
      $(piece_members "$PIECE" "$name") manifest.json \
      | zstd -"$LEVEL" -o "$out"
    mv "$out" "$bundle"
    trap - EXIT; rm -rf "$stage"
    printf 'packed %s %s  (%s at level %s, manifest %s)\n' \
      "$PIECE" "$bundle" "$(du -h "$bundle" | cut -f1)" "$LEVEL" \
      "$(tar --use-compress-program=unzstd -xOf "$bundle" manifest.json | manifest_hash /dev/stdin)"
    printf 'publish: gh release create %s-%s-v%s %s --title "%s %s v%s" --notes "profile %s"\n' \
      "$name" "$PIECE" "$VERSION" "$bundle" "$name" "$PIECE" "$VERSION" "$PIECE"
    tar --use-compress-program=unzstd -tf "$bundle" | head -5
    ;;
  fetch)
    command -v curl >/dev/null || { echo "curl not on PATH" >&2; exit 1; }
    slug=$(repo_slug)
    mkdir -p "$root/profiles/$PIECE"
    # Anonymous works on a public repo; a private one needs GH_TOKEN.
    # One release per (name, piece), so the tag carries both and the asset keeps
    # the plain `<name>.tar.zst` a local pack produces.
    url=$(NAME="$name" WANT="$version_arg" SLUG="$slug" PIECE="$PIECE" python3 - <<'EOF'
import json, os, sys, urllib.request
slug, name, want, piece = os.environ["SLUG"], os.environ["NAME"], os.environ["WANT"], os.environ["PIECE"]
want = want.lstrip("v")
tag = lambda v: f"{name}-{piece}-v{v}"
req = urllib.request.Request(f"https://api.github.com/repos/{slug}/releases?per_page=100",
                             headers={"Accept": "application/vnd.github+json"})
if os.environ.get("GH_TOKEN"):
    req.add_header("Authorization", "Bearer " + os.environ["GH_TOKEN"])
rels = json.load(urllib.request.urlopen(req))
cands = [r for r in rels if (r.get("tag_name", "") + "/").startswith(f"{name}-{piece}-v")]
if want:
    cands = [r for r in cands if r["tag_name"] == tag(want)]
if not cands:
    sys.exit(f"no release for {piece} {name}" + (f" version {want}" if want else ""))
rel = sorted(cands, key=lambda r: r.get("created_at", ""))[-1]
zst = [a["browser_download_url"] for a in rel.get("assets", []) if a["name"].endswith(".tar.zst")]
if not zst:
    sys.exit(f"release {rel['tag_name']} holds no .tar.zst asset")
print(zst[0])
EOF
)
    out="$root/profiles/.fetch.$name.tar.zst"
    curl -fsSL ${GH_TOKEN:+-H "Authorization: Bearer $GH_TOKEN"} -o "$out" "$url"
    mv "$out" "$root/profiles/$PIECE/$name.tar.zst"
    "${BASH_SOURCE[0]}" verify "$name" --piece "$PIECE"
    ;;
  unpack)
    bundle="$root/profiles/$PIECE/$name.tar.zst"
    [ -f "$bundle" ] || { echo "no such release (fetch it first: profile.sh fetch $name --piece $PIECE)" >&2; exit 1; }
    stage=$(mktemp -d "${TMPDIR:-/tmp}/bm-profile.XXXXXX")
    trap 'rm -rf "$stage"' EXIT
    tar --use-compress-program=unzstd -xf "$bundle" -C "$stage"
    verify_stage "$stage"
    member=$(piece_members "$PIECE" "$name")
    rm -rf "$root/$member"
    mkdir -p "$root/$(dirname "$member")"
    mv "$stage/$member" "$root/$(dirname "$member")/"
    stamp_piece "$stage/manifest.json" "$root/.bm/profile"
    trap - EXIT; rm -rf "$stage"
    ;;
  list)
    shopt -s nullglob
    # Per piece first, then a pre-split bundle at the top level if one is left.
    for b in "$root"/profiles/*/*.tar.zst "$root"/profiles/*.tar.zst; do
      man=$(tar --use-compress-program=unzstd -xOf "$b" manifest.json 2>/dev/null || echo '{}')
      name_v=$(echo "$man" | python3 -c 'import json,sys; m=json.load(sys.stdin); print(m.get("piece","pack")+" "+m.get("name","?")+" v"+m.get("version","?"))')
      printf '  %-40s %s  %s\n' "${b#"$root"/}" "$name_v" "$(du -h "$b" | cut -f1)"
    done
    ;;
  verify)
    bundle="$root/profiles/$PIECE/$name.tar.zst"
    [ -f "$bundle" ] || { echo "no such release: $bundle" >&2; exit 1; }
    stage=$(mktemp -d "${TMPDIR:-/tmp}/bm-profile.XXXXXX")
    trap 'rm -rf "$stage"' EXIT
    tar --use-compress-program=unzstd -xf "$bundle" -C "$stage"
    verify_stage "$stage" && echo "OK $PIECE $name"
    ;;
  *) echo "unknown command $cmd (pack|fetch|unpack|list|verify)" >&2; exit 1;;
esac
