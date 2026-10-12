#!/usr/bin/env bash
# Publish one preset pack version to the R2 bucket, then point latest.json at
# it. Layout per docs/R2-PRESETS.md:
#
#   preset/<kind>/<name>/<version>/<file>
#   preset/<kind>/<name>/latest.json
#
# Required env (never commit these):
#   R2_ACCOUNT_ID       Cloudflare account id (the dashboard URL shows it)
#   R2_ACCESS_KEY_ID    from an R2 API token, Object Read and Write, this bucket
#   R2_SECRET_ACCESS_KEY  same token
#
# Optional env:
#   R2_BUCKET       default: storycast-presets
#   R2_PUBLIC_BASE  default: https://storycast-presets.huuthangle.site
#
# Usage:
#   tools/preset-publish.sh <kind> <name> <version|auto> <file...>
#
# The version is a date tag like 2026-10-11, part of the key and never
# overwritten: a bad upload is fixed by publishing a new version, and a
# rollback is re-pointing latest.json. `auto` stamps today.
#
# Example (the models bundle first):
#   R2_ACCOUNT_ID=... R2_ACCESS_KEY_ID=... R2_SECRET_ACCESS_KEY=... \
#     tools/preset-publish.sh models vieneu auto \
#       engines/vieneu/models/models.tar.zst
set -euo pipefail

KIND="${1:?usage: $0 <kind> <name> <version|auto> <file...>}"
NAME="${2:?usage: $0 <kind> <name> <version|auto> <file...>}"
VERSION="${3:?usage: $0 <kind> <name> <version|auto> <file...>}"
shift 3
[ "$#" -ge 1 ] || { echo "usage: $0 <kind> <name> <version|auto> <file...>" >&2; exit 1; }
if [ "$VERSION" = "auto" ]; then
    VERSION="$(date +%F)"
fi

: "${R2_ACCOUNT_ID:?set R2_ACCOUNT_ID}"
: "${R2_ACCESS_KEY_ID:?set R2_ACCESS_KEY_ID}"
: "${R2_SECRET_ACCESS_KEY:?set R2_SECRET_ACCESS_KEY}"
BUCKET="${R2_BUCKET:-storycast-presets}"
BASE="${R2_PUBLIC_BASE:-https://storycast-presets.huuthangle.site}"
ENDPOINT="https://$R2_ACCOUNT_ID.r2.cloudflarestorage.com"

export AWS_ACCESS_KEY_ID="$R2_ACCESS_KEY_ID"
export AWS_SECRET_ACCESS_KEY="$R2_SECRET_ACCESS_KEY"
# R2 speaks its own regions; your ambient AWS_REGION is rejected otherwise.
export AWS_REGION=auto
export AWS_DEFAULT_REGION=auto

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

FILES_JSON="$(mktemp)"
echo '{}' > "$FILES_JSON"
cleanup() { rm -f "$FILES_JSON" "$FILES_JSON.latest"; }
trap cleanup EXIT

for f in "$@"; do
    [ -f "$f" ] || { echo "no such file: $f" >&2; exit 1; }
    base="$(basename "$f")"
    key="preset/$KIND/$NAME/$VERSION/$base"
    sum="$(sha256 "$f")"
    bytes="$(wc -c < "$f" | tr -d ' ')"
    echo "uploading $f ($bytes bytes) -> $key"
    aws --endpoint-url "$ENDPOINT" s3 cp "$f" "s3://$BUCKET/$key" --only-show-errors
    python3 - "$FILES_JSON" "$base" "$key" "$sum" "$bytes" <<'EOF'
import json, sys
path, base, key, sum, nbytes = sys.argv[1:6]
doc = json.load(open(path))
doc[base] = {"key": key, "sha256": sum, "bytes": int(nbytes)}
json.dump(doc, open(path, "w"), indent=2)
EOF
done

# The tree hash the boxes verify against (not the archive hash): computed
# over the bundle's own manifest.json with the same rule both manifest
# shapes share — sorted `path + NUL + content-sha + NUL` lines, where a
# pack entry *is* the sha and a models entry carries it under `sha256`.
MANIFEST_HASH="$(tar --use-compress-program=unzstd -xOf "$1" manifest.json 2>/dev/null | python3 -c '
import hashlib, json, sys
m = json.load(sys.stdin)
h = hashlib.sha256()
for path in sorted(m["files"]):
    entry = m["files"][path]
    sha = entry if isinstance(entry, str) else entry["sha256"]
    h.update(path.encode()); h.update(b"\0")
    h.update(sha.encode()); h.update(b"\0")
print(h.hexdigest())')"
[ -n "$MANIFEST_HASH" ] || { echo "no manifest.json inside the bundle, cannot pin the tree" >&2; exit 1; }
echo "tree manifest hash: $MANIFEST_HASH"

python3 - "$FILES_JSON" "$VERSION" "$MANIFEST_HASH" <<'EOF'
import json, sys
path, version, mhash = sys.argv[1:4]
files = json.load(open(path))
latest = {"version": version, "manifest_hash": mhash, "files": files}
open(path + ".latest", "w").write(json.dumps(latest, indent=2) + "\n")
EOF

echo "uploading latest.json"
aws --endpoint-url "$ENDPOINT" s3 cp "$FILES_JSON.latest" \
  "s3://$BUCKET/preset/$KIND/$NAME/latest.json" \
  --content-type application/json --only-show-errors

echo "verifying"
curl -fsS -A 'storycast-preset-fetch/1.0' "$BASE/preset/$KIND/$NAME/latest.json" -o "$FILES_JSON.verify"
python3 - "$FILES_JSON.verify" "$BASE" <<'EOF'
import json, sys, urllib.request
path, base = sys.argv[1:3]
latest = json.load(open(path))
for name, entry in latest["files"].items():
    url = base + "/" + entry["key"]
    # A one-byte ranged GET: R2 answers HEAD with 403 on some setups, and a
    # full GET would download the whole pack just to check it is there.
    req = urllib.request.Request(url, headers={"Range": "bytes=0-0", "User-Agent": "storycast-preset-fetch/1.0"})
    try:
        with urllib.request.urlopen(req) as r:
            assert r.status in (200, 206), (name, r.status)
            if r.status == 206:
                total = int(r.headers.get("Content-Range", "/-1").split("/")[-1])
                assert total == entry["bytes"], (name, total, entry["bytes"])
                size = total
            else:
                size = entry["bytes"]
    except urllib.error.HTTPError as e:
        raise SystemExit(f"verify failed for {name}: HTTP {e.code} at {url}")
    print(f"ok {name} ({size} bytes)");
print("latest.json points at", latest["version"])
EOF
echo "published $KIND/$NAME@$VERSION"
