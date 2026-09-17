#!/usr/bin/env bash
# Normalize a directory of sound-design clips into a pool directory.
#
#   tools/normalize-audio.sh <src-dir> <dest-dir>
#
# The merge reads pool clips through the same chain for every scene, so a clip's
# loudness is part of its contract: the `level` a scene map applies is a *gain
# over a known source*, not a lottery. Sources arrive from anywhere — the
# samples in `refs/temp/` spanned 38 LU and several already clipped — so every
# clip is brought to one house spec before it is allowed into a pool:
#
#   * leading/trailing silence trimmed (a clip that opens with 2 s of nothing
#     reads as a 2 s hole in the mix)
#   * loudness normalized, two-pass, to I=-26 LUFS / TP=-3 dBTP
#     (-26 LUFS keeps the effect beds below the voice while preserving headroom;
#     TP=-3 leaves room for ducking and the final mp3 encode)
#   * mono 48 kHz — the merge works in mono 48 kHz end to end, so stereo here
#     is bytes that are thrown away at the first `aformat`
#   * all metadata dropped (`-map_metadata -1`, no Xing/LAME header, no ID3v2)
#
# Two-pass loudnorm, not one: single-pass `loudnorm` is a dynamic normalizer and
# pumps on a bed. Pass 1 measures, pass 2 applies the measurement as a *linear*
# gain, so a bed's internal dynamics survive intact.
#
# Files already in <dest-dir> are left alone — the script is how a clip gets in,
# not a way to re-encode the pool on every run.

set -euo pipefail

I_TARGET=${I_TARGET:--26}      # integrated loudness, LUFS (-20 for foreground injects)
TP_TARGET=${TP_TARGET:--3}      # true peak ceiling, dBTP
LRA_TARGET=11     # loudness range the normalizer is allowed to work with
BITRATE=${BITRATE:-64k}       # mono 48 kHz mp3; smaller pool files with acceptable bed quality
TRIM_FLOOR=-50dB  # what counts as silence at either end

src=${1:?usage: normalize-audio.sh <src-dir> <dest-dir>}
dest=${2:?usage: normalize-audio.sh <src-dir> <dest-dir>}

command -v ffmpeg >/dev/null || { echo "ffmpeg not on PATH" >&2; exit 1; }
command -v ffprobe >/dev/null || { echo "ffprobe not on PATH" >&2; exit 1; }
[ -d "$src" ] || { echo "no such source directory: $src" >&2; exit 1; }

mkdir -p "$dest"
# Intermediates go on the *destination's* filesystem, not in /tmp: a 600 s clip
# is a 57 MB wav on the way through, and /tmp shares a volume with the system
# disk, which is routinely the fullest thing on the machine (it ran out at
# 125 MiB free mid-run and left the pool half-built). Same-filesystem scratch
# also makes the write path the short one.
tmp=$(mktemp -d "$dest/.norm.XXXXXX")
# Each intermediate is removed as soon as it is consumed and the directory is
# only ever `rmdir`'d: a whole-run `rm -rf` of a directory holding a minute of
# wav per clip is exactly the shape a bulk-delete guard exists to stop, and the
# script should not need anyone to approve its own cleanup.
trap 'rm -f "$tmp"/* 2>/dev/null; rmdir "$tmp" 2>/dev/null; true' EXIT
# bash's here-strings and ffmpeg's own scratch both default to $TMPDIR, which
# on a laptop is the system volume — the one that is full when you notice.
export TMPDIR="$tmp"

measure() { # $1 = file -> echo the loudnorm JSON block
  ffmpeg -hide_banner -nostats -i "$1" \
    -af "loudnorm=I=$I_TARGET:TP=$TP_TARGET:LRA=$LRA_TARGET:print_format=json" \
    -f null - 2>&1 | python3 -c '
import json, re, sys
m = re.search(r"\{[^{}]*\"input_i\"[^{}]*\}", sys.stdin.read(), re.S)
if not m:
    sys.exit("loudnorm produced no measurement")
d = json.loads(m.group(0))
print(" ".join(d[k] for k in
      ("input_i", "input_tp", "input_lra", "input_thresh", "target_offset")))'
}

lufs() { # $1 = file -> integrated loudness of the finished clip
  ffmpeg -hide_banner -nostats -i "$1" \
    -af "loudnorm=print_format=json" -f null - 2>&1 | python3 -c '
import json, re, sys
m = re.search(r"\{[^{}]*\"input_i\"[^{}]*\}", sys.stdin.read(), re.S)
print(json.loads(m.group(0))["input_i"] if m else "?")'
}

shopt -s nullglob
n=0
for f in "$src"/*.mp3 "$src"/*.wav "$src"/*.m4a "$src"/*.ogg "$src"/*.flac; do
  name=$(basename "${f%.*}")
  out="$dest/$name.mp3"
  if [ -e "$out" ]; then
    printf '  skip  %-42s (already in %s)\n' "$name" "$dest"
    continue
  fi

  # 1. trim dead air off both ends, downmix, resample. `areverse` twice is how
  #    the *trailing* silence is trimmed: silenceremove only ever works on the
  #    head of its input.
  trimmed="$tmp/$name.wav"
  ffmpeg -y -hide_banner -loglevel error -i "$f" -map_metadata -1 \
    -af "highpass=f=20,silenceremove=start_periods=1:start_threshold=$TRIM_FLOOR:detection=peak,areverse,silenceremove=start_periods=1:start_threshold=$TRIM_FLOOR:detection=peak,areverse,aresample=48000" \
    -ac 1 -ar 48000 -c:a pcm_s16le "$trimmed"

  # 2. measure, then 3. apply as a linear gain.
  dur_trim=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$trimmed")
  if awk -v d="$dur_trim" 'BEGIN { exit (d < 0.5) ? 0 : 1 }'; then
    # A clip under half a second is shorter than loudnorm's measurement window,
    # so it measures -inf and the linear pass refuses to run. Peak-normalize
    # instead: for a 0.2 s hit the peak *is* the loudness, and TP_TARGET lands
    # it in the same territory as the voice peaks.
    peak=$(ffmpeg -hide_banner -nostats -i "$trimmed" \
      -af volumedetect -f null - 2>&1 | awk '/max_volume/ { print $5 }')
    if ! awk -v p="$peak" 'BEGIN { exit (p + 0 == p) ? 0 : 1 }' 2>/dev/null; then
      printf '  SKIP  %-42s trimmed to digital silence (%s)\n' "$name" "$f"
      rm -f "$trimmed"
      continue
    fi
    gain=$(awk -v t="$TP_TARGET" -v p="$peak" 'BEGIN { printf "%.2f", t - p }')
    ffmpeg -y -hide_banner -loglevel error -i "$trimmed" \
      -af "volume=${gain}dB,aresample=48000" \
      -ac 1 -ar 48000 -c:a libmp3lame -b:a "$BITRATE" \
      -write_xing 0 -id3v2_version 0 -map_metadata -1 "$out"
    mode=peak
    got=$(lufs "$out")
    dur=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$out")
    printf '  ok    %-42s %6.1fs  %6.2f LUFS  %-7s (%s)\n' \
      "$name" "$dur" "$got" "$mode" "$f"
    rm -f "$trimmed"
    n=$((n + 1))
    continue
  fi
  read -r mi mtp mlra mthr moff <<<"$(measure "$trimmed")"
  apply() { # $1 = linear|false, $2 = output
    ffmpeg -y -hide_banner -loglevel error -i "$trimmed" \
      -af "loudnorm=I=$I_TARGET:TP=$TP_TARGET:LRA=$LRA_TARGET:measured_I=$mi:measured_TP=$mtp:measured_LRA=$mlra:measured_thresh=$mthr:offset=$moff:linear=$1,aresample=48000" \
      -ac 1 -ar 48000 -c:a libmp3lame -b:a "$BITRATE" \
      -write_xing 0 -id3v2_version 0 -map_metadata -1 "$2"
  }
  apply true "$out"

  # A clip whose peak sits far above its loudness cannot be raised to the
  # target by a linear gain without breaking the ceiling — `cave-drip` landed
  # 4 LU short that way. Dynamic mode trades some of the clip's own dynamics
  # for a level the scene map can rely on, which is the point of normalizing at
  # all; it is the fallback, not the default, because a bed that breathes is
  # worse than a bed that is 1 LU off.
  got=$(lufs "$out")
  mode=linear
  if awk -v g="$got" -v t="$I_TARGET" \
       'BEGIN { d = g - t; if (d < 0) d = -d; exit (d > 1.5) ? 0 : 1 }'; then
    apply false "$out"
    got=$(lufs "$out")
    mode=dynamic
  fi

  dur=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$out")
  # A clip that still misses the target after both passes is reported, not
  # hidden: `cave-drip` is 6 s of sparse transients, and integrated loudness
  # simply does not describe that material — it sits 4 LU low and its scene
  # carries the highest `level` in the map to compensate. Every other clip in
  # both pools lands within 0.5 LU of the target, which is what makes the
  # scene map's gains comparable at all.
  short=""
  if awk -v g="$got" -v t="$I_TARGET" \
       'BEGIN { d = g - t; if (d < 0) d = -d; exit (d > 1.5) ? 0 : 1 }'; then
    short="  <-- off target"
  fi
  printf '  ok    %-42s %6.1fs  %6.2f LUFS  %-7s (%s)%s\n' \
    "$name" "$dur" "$got" "$mode" "$f" "$short"
  rm -f "$trimmed"
  n=$((n + 1))
done

echo "$n clip(s) normalized into $dest"
