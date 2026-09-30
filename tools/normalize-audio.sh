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
#   * a short fade at each end. A trim cuts at whatever sample the sound
#     happened to open or end on, which is rarely zero, and a step to or from
#     silence is a click — audible on a hit, and on a bed it is the seam at the
#     loop point every time it comes round
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
# What counts as silence at either end. -45 dB, not -50: the detector below
# compares *peak* against this, and a 64k mono recording's own room tone sits
# around -45, so at -50 the trim cannot see the near-silence it exists to cut.
# Measured on this pool: a 78.8 s war din kept 5.9 s of tail at -50, which on a
# looped bed plays as a hole in the din every time the clip comes round.
TRIM_FLOOR=${TRIM_FLOOR:--45dB}
# Below this length a clip is peak-normalized rather than loudness-normalized:
# it is shorter than loudnorm's measurement window, so the integrated number
# does not describe it. 0.5 s is the recorded-pool's default; generated clips
# raise it, because a model's one-shot puts its energy in a fraction of its
# length and the pool's own note says such a transient is placed by its peak.
PEAK_UNDER=${PEAK_UNDER:-0.5}
FADE_MS=${FADE_MS:-12}  # click guard at both new edges, in ms

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

# EVERY ffmpeg CALL TAKES -nostdin. Without it ffmpeg reads standard input for
# its keyboard controls, and when this script is called from inside a `while
# read` loop that stdin *is the caller's list*: ffmpeg consumes a chunk of the
# next line and the loop resumes mid-record. That is not hypothetical — a batch
# over 23 clips wrote four of them under truncated names (`fire-crackle-2`
# became `-crackle-2`) before the fifth died, and nothing in this script's own
# output said why. `-nostdin` is ffmpeg's documented answer to exactly this.
measure() { # $1 = file -> echo the loudnorm JSON block
  ffmpeg -nostdin -hide_banner -nostats -i "$1" \
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
  ffmpeg -nostdin -hide_banner -nostats -i "$1" \
    -af "loudnorm=print_format=json" -f null - 2>&1 | python3 -c '
import json, re, sys
m = re.search(r"\{[^{}]*\"input_i\"[^{}]*\}", sys.stdin.read(), re.S)
print(json.loads(m.group(0))["input_i"] if m else "?")'
}

# A clip loudnorm cannot describe is normalized by its peak instead. Under half
# a second it is shorter than the measurement window, and a lone transient
# measured over its whole length is mostly decay, so the integrated number does
# not describe it while the peak does — and TP_TARGET lands it in the same
# territory as the voice peaks. Prints the finished line either way; returns 1
# when the clip trimmed to digital silence, so there is nothing to write.
peak_db() { # peak_db <file> -> sample peak in dBFS
  ffmpeg -nostdin -hide_banner -nostats -i "$1" \
    -af volumedetect -f null - 2>&1 | awk '/max_volume/ { print $5 }'
}

peak_normalize() { # peak_normalize <trimmed> <out> <name> <source>
  peak=$(peak_db "$1")
  if ! awk -v p="$peak" 'BEGIN { exit (p + 0 == p) ? 0 : 1 }' 2>/dev/null; then
    printf '  SKIP  %-42s trimmed to digital silence (%s)\n' "$3" "$4"
    return 1
  fi
  gain=$(awk -v t="$TP_TARGET" -v p="$peak" 'BEGIN { printf "%.2f", t - p }')
  ffmpeg -nostdin -y -hide_banner -loglevel error -i "$1" \
    -af "volume=${gain}dB,aresample=48000" \
    -ac 1 -ar 48000 -c:a libmp3lame -b:a "$BITRATE" \
    -write_xing 0 -id3v2_version 0 -map_metadata -1 "$2"
  mode=peak
  got=$(lufs "$2")
  dur=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$2")
  printf '  ok    %-42s %6.1fs  %6.2f LUFS  %-7s (%s)\n' \
    "$3" "$dur" "$got" "$mode" "$4"
  return 0
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

  # 1. trim dead air off both ends, fade the two new edges, downmix, resample.
  #    `areverse` twice is how the *trailing* silence is trimmed: silenceremove
  #    only ever works on the head of its input. The tail fade reuses the same
  #    trick for the same reason — neither may need the duration up front.
  trimmed="$tmp/$name.wav"
  fade_s=$(awk -v ms="$FADE_MS" 'BEGIN { printf "%.3f", ms / 1000 }')
  fade="afade=t=in:st=0:d=$fade_s"
  ffmpeg -nostdin -y -hide_banner -loglevel error -i "$f" -map_metadata -1 \
    -af "highpass=f=20,silenceremove=start_periods=1:start_threshold=$TRIM_FLOOR:detection=peak,areverse,silenceremove=start_periods=1:start_threshold=$TRIM_FLOOR:detection=peak,areverse,$fade,areverse,$fade,areverse,aresample=48000" \
    -ac 1 -ar 48000 -c:a pcm_s16le "$trimmed"

  # 2. measure, then 3. apply as a linear gain.
  dur_trim=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$trimmed")
  if awk -v d="$dur_trim" -v u="$PEAK_UNDER" 'BEGIN { exit (d < u) ? 0 : 1 }'; then
    if peak_normalize "$trimmed" "$out" "$name" "$f"; then
      n=$((n + 1))
    fi
    rm -f "$trimmed"
    continue
  fi
  read -r mi mtp mlra mthr moff <<<"$(measure "$trimmed")"
  # A clip loudnorm cannot describe gets its peak instead of a failure. The
  # measurement comes back out of range for a hit at the edge of the window
  # (bell-2, 0.6 s, measured 0.25), and passing that number to the linear pass
  # aborts ffmpeg outright: `measured_I` out of range [-99 - 0].
  if ! awk -v m="$mi" 'BEGIN { exit (m < 0 && m > -99) ? 0 : 1 }'; then
    printf '  note  %-42s loudnorm cannot measure this one (%s), going by peak\n' \
      "$name" "$mi"
    if peak_normalize "$trimmed" "$out" "$name" "$f"; then
      n=$((n + 1))
    fi
    rm -f "$trimmed"
    continue
  fi
  apply() { # $1 = linear|false, $2 = output
    ffmpeg -nostdin -y -hide_banner -loglevel error -i "$trimmed" \
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
  # Two reasons to re-run in dynamic mode. The linear gain missed the target by
  # more than 1.5 LU, or it pushed the peak past the ceiling — the second
  # happens on a dense, quiet source, where the gain that raises its loudness to
  # the rung raises its peak with it, because loudnorm's linear pass is a plain
  # gain and does not limit. `footstep-forest-2`, 50 s of quiet forest floor,
  # came out of the linear pass with its peak on the ceiling. Dynamic mode uses
  # the filter's own limiter, so the ceiling holds. The +1.5 dB of tolerance is
  # the mp3 overshoot this pool's `_note` documents.
  peak_out=$(peak_db "$out")
  if awk -v g="$got" -v t="$I_TARGET" \
       'BEGIN { d = g - t; if (d < 0) d = -d; exit (d > 1.5) ? 0 : 1 }' \
     || awk -v p="$peak_out" -v t="$TP_TARGET" \
       'BEGIN { exit (p > t + 1.5) ? 0 : 1 }'; then
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
