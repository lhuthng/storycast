#!/usr/bin/env bash
# Add music to the background-music pool from an audio OR video file.
#
#   tools/add-music.sh --tags sad,sorrow [--sound sad] [--as sad-bg-1] <input...>
#
# Video lands audio-only (`-vn` drops the picture; only the soundtrack travels).
# Conversion, metadata stripping and leveling are not reimplemented here — each
# input is staged to wav, then tools/normalize-audio.sh brings it to the house
# spec (silence trimmed, two-pass loudnorm, mono 48 kHz, no metadata), and the
# registry entry is appended to assets/music-pool.json.
#
# Takes are indexed from 1 (`<sound>-bg-1.mp3`), matching the pool convention —
# `--as` names the output stem for a single input (the usual case for music).
#
# Music house spec (what the pool already holds — measure one before changing):
#   I_TARGET=-23 BITRATE=96k. Beds need the extra bits over the script default.
#
# Tags must intersect a music_palette mood in assets/scene-map.json or the
# clip never plays (a mood with no matching tags gets silence, and pool tags
# matching no mood are dead weight). The script warns when that happens; adding
# the mood itself stays a manual edit — it is an editorial decision, and the
# digest prompt is rendered from the palette, so it takes effect immediately.

set -euo pipefail

tags=""; sound=""; as_name=""; pool="assets/music-pool.json"; dest="assets/music"
inputs=()
while [ $# -gt 0 ]; do
  case "$1" in
    --tags) tags=${2:?--tags needs a value}; shift 2;;
    --sound) sound=${2:?--sound needs a value}; shift 2;;
    --as) as_name=${2:?--as needs a value}; shift 2;;
    --pool) pool=${2:?--pool needs a value}; shift 2;;
    --dest) dest=${2:?--dest needs a value}; shift 2;;
    --) shift; break;;
    -*) echo "unknown flag $1 (usage: add-music.sh --tags a,b [--sound name] [--as stem] <input...>)" >&2; exit 1;;
    *) inputs+=("$1"); shift;;
  esac
done
[ -n "$tags" ] || { echo "--tags is required (comma-separated, e.g. sad,sorrow)" >&2; exit 1; }
[ ${#inputs[@]} -gt 0 ] || { echo "no input files" >&2; exit 1; }
command -v ffmpeg >/dev/null || { echo "ffmpeg not on PATH" >&2; exit 1; }
here=$(dirname "$0")
[ -x "$here/normalize-audio.sh" ] || { echo "normalize-audio.sh missing beside $0" >&2; exit 1; }
[ -f "$pool" ] || { echo "no such pool file: $pool" >&2; exit 1; }

# Default sound key: the first input's stem (sad.mp4 -> sad). The registry —
# never the filename — decides what answers a mood, so stems stay human.
if [ -z "$sound" ]; then
  sound=$(basename "${inputs[0]%.*}")
fi

stage=$(mktemp -d "$dest/.add.XXXXXX")
trap 'rm -f "$stage"/* 2>/dev/null; rmdir "$stage" 2>/dev/null; true' EXIT
if [ -n "$as_name" ] && [ ${#inputs[@]} -ne 1 ]; then
  echo "--as names one output, so it takes exactly one input" >&2; exit 1
fi
i=0
for f in "${inputs[@]}"; do
  [ -f "$f" ] || { echo "no such file: $f" >&2; exit 1; }
  # Audio-only on purpose: `-vn` drops the picture from video inputs, and the
  # wav stage is what normalize-audio.sh already accepts, so every format
  # funnels through one measured chain instead of two.
  if [ -n "$as_name" ]; then
    stem="$as_name"
  else
    stem=$(basename "${f%.*}")
  fi
  ffmpeg -y -hide_banner -loglevel error -i "$f" -vn -map_metadata -1 \
    -ac 1 -ar 48000 -c:a pcm_s16le "$stage/$stem.wav"
  i=$((i + 1))
done

# The pool's own spec, not the script default (64k/-26 is for effects).
I_TARGET=${I_TARGET:--23} BITRATE=${BITRATE:-96k} \
  "$here/normalize-audio.sh" "$stage" "$dest"

# Register: append this run's outputs to the sound's takes (a second take of
# an existing sound joins its `files`, it never becomes its own key).
produced=()
for w in "$stage"/*.wav; do
  stem=$(basename "${w%.wav}")
  [ -f "$dest/$stem.mp3" ] && produced+=("$stem.mp3")
done
[ ${#produced[@]} -gt 0 ] || { echo "nothing normalized (already in $dest?)" >&2; exit 1; }
files_json=$(printf '%s\n' "${produced[@]}" | python3 -c 'import json,sys; print(json.dumps([l.strip() for l in sys.stdin if l.strip()]))')

TAGS="$tags" SOUND="$sound" FILES="$files_json" POOL="$pool" python3 - <<'EOF'
import json, os
pool_path = os.environ["POOL"]
tags = [t.strip() for t in os.environ["TAGS"].split(",") if t.strip()]
sound, files = os.environ["SOUND"], json.loads(os.environ["FILES"])
with open(pool_path) as fh:
    pool = json.load(fh)
entry = pool.get(sound)
if entry is None:
    pool[sound] = {"tags": tags, "files": ["music/" + f for f in files]}
else:
    if sorted(entry.get("tags", [])) != sorted(tags):
        print(f"note: {sound} keeps its registered tags {entry.get('tags')} (given {tags})")
    have = set(entry.setdefault("files", []))
    entry["files"].extend("music/" + f for f in files if "music/" + f not in have)
# Canonical form: `_`-keys first, the rest sorted, one field per line with
# inline arrays — the fixed point of audio_pool::save_pool, so an untouched
# save rewrites nothing and diffs stay surgical. (Plain json.dump spreads
# arrays over lines and appends keys at the end, which breaks both.)
members = []
for k in pool:
    if k.startswith("_"):
        members.append((0, k))
for k in sorted(pool):
    if not k.startswith("_"):
        members.append((1, k))
lines = ["{"]
for i, (_, k) in enumerate(members):
    if k.startswith("_"):
        body = json.dumps(pool[k], ensure_ascii=False)
    else:
        e = pool[k]
        order = ["tags", "files"] + [fk for fk in e if fk not in ("tags", "files")]
        rendered = []
        for fk in order:
            if fk not in e or (fk == "files" and not e["files"]):
                continue
            inline = json.dumps(e[fk], ensure_ascii=False)
            # The repo writer's 80-column budget: long lists expand one per
            # line rather than collapsing the file's hand formatting.
            if 4 + len(json.dumps(fk)) + 2 + len(inline) <= 80:
                rendered.append(f'{json.dumps(fk)}: {inline}')
            else:
                items = ",\n".join("      " + json.dumps(x, ensure_ascii=False) for x in e[fk])
                rendered.append(f'{json.dumps(fk)}: [\n{items}\n    ]')
        body = "{\n" + ",\n".join("    " + f for f in rendered) + "\n  }"
    lines.append(f'  {json.dumps(k)}: {body}' + ("," if i < len(members) - 1 else ""))
lines.append("}")
with open(pool_path, "w") as fh:
    fh.write("\n".join(lines) + "\n")
print(f"registered {sound}: {pool[sound]}")

# Warn, don't refuse: a clip whose tags match no palette mood is silent
# everywhere, which is exactly the mistake worth noticing now.
import pathlib
root = pathlib.Path(pool_path).parent
try:
    scene = json.load(open(root / "scene-map.json"))
    pal = scene.get("music_palette", {})
    moods = [m for m, v in pal.items()
             if m != "_note" and set(tags) & set(v.get("tags", []))]
    print("moods answering these tags:", moods if moods else "NONE — add one to music_palette or this clip never plays")
except FileNotFoundError:
    pass
EOF
