#!/usr/bin/env bash
# Generate a sound locally, with Stable Audio 3 — effects *and* music.
#
#   tools/gen-sound.sh setup [--repin]      one-time: the engine, its venv, the pin
#
#   tools/gen-sound.sh one --prompt "…" --as <stem> --into <pool dir>
#                            --level place|moment [--seconds N] [--seed N]
#                            [--cfg N] [--negative "…"]
#
#   tools/gen-sound.sh one --prompt "…" --as <stem> --into <music dir>
#                            --level track --sound <key> --tags a,b [--pool <json>]
#
#   tools/gen-sound.sh one --from <wav> --as <stem> --into <pool dir>
#                            --level place|moment [--seconds N]
#                            install a raw you already picked, instead of generating
#
#   tools/gen-sound.sh batch --list <tsv>   one line a clip: see `batch` below
#   tools/gen-sound.sh list                 what is installed, and where
#
# WHY THIS MODEL. The local half of the audio work is **places** (a bed under a
# scene), **moments** (a one-shot on a beat) and, in a music pack, **tracks** —
# and those are two different models, which is why this script has both.
# Stable Audio **2.0 has no downloadable weights**: it runs on Stability's
# platform only, so it cannot be the local path at all. The open-weight line is
# **3.0**, tiered by what the audio *is*:
#
#   small-sfx    433 M  CPU  120 s  places, moments
#   small-music  433 M  CPU  120 s  tracks
#   medium       1.4 B  CUDA 380 s  not this laptop
#
# Small-SFX and Small-Music share the SAME-Small autoencoder and the same repo,
# venv and CLI, differing only in the checkpoint `--model` names — so one engine
# tree serves both and the tier is decided by `--level`. Medium is deliberately
# not wired: this is an Apple silicon laptop, and a model that needs a GPU to be
# usable is not a local tool.
#
# WHAT LIVES WHERE. The engine is `engines/stable-audio/` — a checkout of
# Stability-AI/stable-audio-3, its `.venv`, and its revision pin. `engines/` is
# gitignored, like `engines/vieneu/`: an engine tree is fetched or built, never
# authored. The weights land in the Hugging Face cache on first use (`HF_HOME`
# moves it) — one download per tier, and only for the tier you actually run.
# There is no bake step and no release bundle, unlike `tools/models.sh`: VieNeu
# is 16 fixed ONNX files that a box has to receive byte-identical, while these
# are safetensors the HF client already names and verifies by hash.
#
# THE PIN. `setup` records the checkout's commit in
# `engines/stable-audio/revision`, and `one` and `batch` refuse to run once the
# checkout has moved. The same prompt at two revisions is two different clips,
# and nothing downstream can tell them apart — so the pin is the same rule as
# the pools' hash, made cheap. `setup --repin` accepts the move and rewrites it.
#
# EVERY CLIP GOES THROUGH normalize-audio.sh. Generation lands a wav at whatever
# level the model felt like; the pool's contract is that `level` in a scene map
# is gain over a *known* source. So this script generates into a staging
# directory inside the destination and then hands the pair to
# `tools/normalize-audio.sh` with the rung its layer needs: `-26` for a place,
# under the voice, and `-20` for a moment, in front of it. Existing files in the
# destination are never touched — normalize-audio.sh skips a name that is
# already there, which is also what makes `batch` resumable.
#
# A TRACK IS REGISTERED, NOT JUST WRITTEN. Music is not a directory of files,
# it is a sound in a pool: `tags` are what the palette matches and `files` is
# what the picker rolls between. So a track is handed to
# `tools/add-music.sh`, which brings it to the music house spec (`-23` LUFS,
# 96k — beds need the extra bits over an effect's 64k), appends it to the
# registry as another take of `--sound`, and warns when its tags match no mood.
# `--pool` defaults to `music-pool.json` beside the destination directory,
# which is the layout both the root score and every pack already use.
#
# LENGTH. The model's ceiling is 120 s, and 120 s is also the useful default for
# a place: the effect layer still loops with `-stream_loop -1` and a window is at
# most `max_window_s` (75 s), so a bed of 90 s or more plays once per window and
# has no loop seam at all. A shorter bed splices at every window boundary until
# that layer crossfades. A moment wants its own length from the registry's
# `dur_s` — pass it; a generated one-shot is trimmed to 0.4–2.2 s anyway. A
# track defaults to the 120 s ceiling, and music always loops through a
# crossfade, so a track longer than that has to come from a service or a
# recording (normalize-audio.sh takes either).
# `--seconds` wins over any "N seconds" clause in the prompt text: that clause is
# for a service that reads the prompt as a request, and this model takes the
# duration as a parameter.
#
# THE WEIGHTS ARE GATED, THE OPTIMIZED ONES ARE NOT. `stable-audio-3-small-sfx`
# and `-small-music` are *gated* repos on Hugging Face: access is auto-approved
# the moment you click agree on the model page, but you do have to be logged in.
# A 401 from the HF client is that gate and nothing else, so run_model catches it
# and prints the two URLs instead of a traceback. No account is needed for the
# `optimized/` runtimes in the same checkout — they pull the same two tiers from
# `stabilityai/stable-audio-3-optimized`, which is public (MLX on Apple silicon,
# TFLite on CPU anywhere); see §9 of docs/SOUND.md.
#
# PROMPTS ARE NOT STORED HERE. They live with the reason for them, in
# docs/AUDIO-NEEDS.md — one row a sound, with its tags and its prompt. Copy the
# prompt text out of that table into `--prompt`, or into the tsv that `batch`
# reads:
#
#   <stem> <TAB> <level> <TAB> <seconds> <TAB> <dest dir> <TAB> <prompt>
#   <stem> <TAB> track   <TAB> <seconds> <TAB> <music dir> <TAB> <prompt> <TAB> <tags>
#
#   storm-2<TAB>place<TAB>90<TAB>assets/_extends/common/effects<TAB>heavy rain on tiled roofs…
#   market-bg-2<TAB>track<TAB>120<TAB>assets/music<TAB>busy market street, pipa and clappers…<TAB>market,busy
#
# The tags column is the one a track adds, and the only one that may be absent.
# There are no empty columns on purpose: a repeated tab collapses in an IFS
# read (bash 3.2, which is what macOS ships), so a blank field would shift every
# column after it. A track's sound key comes from its take name instead —
# `market-bg-2` is another take of `market` — and `one --sound` overrides that
# when a take name says something else.
#
# `batch` also reads `BM_SA_NEGATIVE` and `BM_SA_CFG` from the environment, and
# passes both to every row. A negative prompt is a property of the *run*, not of
# a row: "nothing in this list is music" is one sentence about a whole pack, and
# a column would invite 40 near-identical copies of it that drift apart. That is
# how a pack is kept off music or off voices without touching a single prompt.
#
# `#` starts a comment line. Blank lines are skipped. Batch prints what it made
# and what it skipped, and stops at the first failure — a half-generated pool is
# fine, a silently wrong one is not.

set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
ENGINE=${BM_SA_ENGINE:-$ROOT/engines/stable-audio}
SRC="$ENGINE/src"
PIN="$ENGINE/revision"
SFX_MODEL=${BM_SFX_MODEL:-small-sfx}      # places and moments
MUSIC_MODEL=${BM_MUSIC_MODEL:-small-music} # tracks
REPO=${BM_SA_REPO:-https://github.com/Stability-AI/stable-audio-3}
# A drop-in for the engine's own CLI when the optimized runtimes are in use.
CLI=${BM_SA_CLI:-${BM_SFX_CLI:-}}
CLI_FLAVOR=${BM_SA_CLI_FLAVOR:-sa3}
# Auto-detected, not configured: if the MLX runtime has been installed it is
# what runs, because it is the same two tiers from a public repo, at roughly
# double the speed, and the gated PyTorch weights are then never needed. An
# explicit BM_SA_CLI always wins, and `list` always says which one it picked.
if [ -z "$CLI" ] && [ -x "$SRC/optimized/mlx/sa3" ] \
   && [ -x "$SRC/optimized/mlx/.venv/bin/python" ]; then
  CLI="$SRC/optimized/mlx/sa3"
  CLI_FLAVOR=optimized
fi

command -v uv >/dev/null || {
  echo "uv is not on PATH — Stable Audio 3 installs with it (https://docs.astral.sh/uv)" >&2
  exit 1
}

# The model runs through the engine's own CLI, in its own venv, unless
# BM_SA_CLI points at one of the optimized runtimes instead (below).
#
# The model repos are gated, and the traceback buries the one line that says so.
show_gate() { # show_gate <model>
  case "$1" in
    small-music) repo=stable-audio-3-small-music;;
    medium)      repo=stable-audio-3-medium;;
    *)           repo=stable-audio-3-small-sfx;;
  esac
  {
    echo
    echo "the weights for $1 are gated on Hugging Face, and this machine is not"
    echo "logged in. Click agree once (it is auto-approved), then log in once:"
    echo
    echo "    https://huggingface.co/stabilityai/$repo"
    echo
    echo "    export HF_TOKEN=hf_...        # or, in the engine: uv run hf auth login"
    echo
    echo "no account is needed for the ungated optimized runtimes, which serve the"
    echo "same two tiers from stabilityai/stable-audio-3-optimized — see §9 of"
    echo "docs/SOUND.md for that route (Apple silicon MLX, or TFLite on CPU)."
  } >&2
}

# The optimized runtimes are not flag-compatible with the engine's own CLI:
# both take `--dit {sm-sfx,sm-music,medium} --prompt … --seconds N --out f.wav`.
# `BM_SA_CLI_FLAVOR=optimized` translates, so the ungated route is the same
# command line as the gated one.
# The codec tier follows the DiT: both small tiers carry SAME-S, medium SAME-L.
# It has to be passed explicitly — the optimized CLIs otherwise ask, and a prompt
# on stdin is a hang when this runs from a script or a batch.
decoder_for() {
  case "$1" in medium) printf 'same-l\n';; *) printf 'same-s\n';; esac
}

model_for() { # model_for <model> -> the name the configured CLI knows it by
  if [ "$CLI_FLAVOR" = optimized ]; then
    case "$1" in
      small-sfx)   printf 'sm-sfx\n';;
      small-music) printf 'sm-music\n';;
      *)           printf '%s\n' "$1";;
    esac
  else
    printf '%s\n' "$1"
  fi
}

run_model() { # run_model <model> <prompt> <seconds> <out.wav> [seed] [cfg] [negative]
  mkdir -p "$(dirname "$4")" "$ENGINE"
  log="$ENGINE/.gen.log"
  rc=0
  # A seed pins the take. The model answers the same draw for the same seed, and
  # it does *not* otherwise: the requested length decides whether a prompt comes
  # back as an event or as a continuous block, and re-running the same command
  # rolls that again. So a take worth keeping is worth passing --seed for.
  #
  # Written as `${extra[@]+...}` on purpose: bash 3.2, which is what macOS
  # ships, treats `"${empty[@]}"` as an unbound variable under `set -u`.
  extra=()
  [ -n "$5" ] && extra+=(--seed "$5")
  [ -n "$6" ] && extra+=(--cfg "$6")
  [ -n "$7" ] && extra+=(--negative-prompt "$7")
  # stdin is /dev/null in every branch: a CLI that decides to ask a question
  # gets end-of-file instead of a person, which fails fast instead of hanging.
  if [ -n "$CLI" ] && [ "$CLI_FLAVOR" = optimized ]; then
    "$CLI" --dit "$(model_for "$1")" --decoder "$(decoder_for "$1")" \
      --prompt "$2" --seconds "$3" ${extra[@]+"${extra[@]}"} --out "$4" </dev/null >"$log" 2>&1 || rc=$?
  elif [ -n "$CLI" ]; then
    "$CLI" --model "$1" -p "$2" --duration "$3" -o "$4" </dev/null >"$log" 2>&1 || rc=$?
  else
    ( cd "$SRC" && uv run --quiet stable-audio \
        --model "$1" -p "$2" --duration "$3" -o "$4" </dev/null ) >"$log" 2>&1 || rc=$?
  fi
  if [ "$rc" -ne 0 ]; then
    grep -q "GatedRepoError\|Cannot access gated repo\|401 Client Error" "$log" && show_gate "$1"
    tail -5 "$log" >&2
    rm -f "$log"
    exit "$rc"
  fi
  rm -f "$log"
}

# THE MODEL'S OUTPUT IS NOT A FINISHED CLIP. Two things have to be true of it
# before the pool's own normalizer sees it, and neither is:
#
#   * its peak has to sit below full scale. The optimized runtime writes float
#     wavs and can put its peak ABOVE 0 dBFS — a 0.6 s bell came back at
#     +6.7 dBFS — and converting that to the int16 the normalizer works in
#     *clips* it. Attenuate only, never boost: the normalizer sets the level.
#   * a one-shot has to be asked for on a longer canvas than it is. Asked for
#     0.6 s of "a small temple bell struck once", the model returns a
#     full-scale block with no event in it; asked for 4 s on the same prompt it
#     returns a strike and a ring. So a moment is generated on a canvas of at
#     least `BM_SA_CANVAS` seconds (3 by default) and cut back to the length in
#     its row, after its leading silence is dropped — which is what puts the
#     event at the head of the file.
#
# A bed needs none of the cutting: it is 90 s because it loops, and its length is
# not part of any contract.
CANVAS=${BM_SA_CANVAS:-3}
prepare() { # prepare <in.wav> <out.wav> <seconds> <place|moment>
  local in=$1 out=$2 secs=$3 level=$4 peak gain chain
  peak=$(ffmpeg -nostdin -hide_banner -nostats -i "$in" \
    -af volumedetect -f null - 2>&1 | awk '/max_volume/ { print $5 }')
  # Ceiling of -3 dBFS, not -1: the normalizer downstream applies its own gain
  # (a dense, quiet source needs several dB to reach its rung), and that gain
  # moves the peak up with it. Three dB of headroom keeps it under full scale.
  gain=$(awk -v p="${peak:-0}" 'BEGIN { g = -3 - p; if (g > 0) g = 0; printf "%.2f", g }')
  chain="volume=${gain}dB"
  if [ "$level" = moment ]; then
    # Drop the model's own run-up so the event starts the file, then cut to the
    # length its row asks for. NOTHING trims the tail here: the model leaves a
    # low-level floor under its event (a slap came back with a −57 dB bed around
    # it), and trimming that as silence collapsed the finished clip to the 0.1 s
    # transient — a click instead of a slap. The tail is the decay and the room.
    chain="$chain,silenceremove=start_periods=1:start_threshold=-50dB:detection=peak"
    chain="$chain,atrim=0:$secs"
  fi
  ffmpeg -nostdin -y -hide_banner -loglevel error -i "$in" -af "$chain" \
    -ac 1 -ar 48000 -c:a pcm_s16le "$out"
}

check_engine() {
  [ -x "$SRC/.venv/bin/python" ] || {
    echo "no engine at $SRC — run: tools/gen-sound.sh setup" >&2
    exit 1
  }
  [ -f "$PIN" ] || {
    echo "no revision pin at $PIN — run: tools/gen-sound.sh setup" >&2
    exit 1
  }
  local now; now=$(git -C "$SRC" rev-parse HEAD)
  if [ "$now" != "$(cat "$PIN")" ]; then
    printf 'the engine moved since it was pinned:\n  pinned  %s\n  now     %s\n' \
      "$(cat "$PIN")" "$now" >&2
    echo "the same prompt at two revisions is two different clips." >&2
    echo "accept it with 'tools/gen-sound.sh setup --repin', or check it out back." >&2
    exit 1
  fi
}

cmd=${1:?usage: gen-sound.sh 'setup|one|batch|list' ...}; shift || true

case "$cmd" in
  setup)
    repin=""
    while [ $# -gt 0 ]; do
      case "$1" in
        --repin) repin=1; shift;;
        *) echo "unknown flag $1" >&2; exit 1;;
      esac
    done
    mkdir -p "$ENGINE"
    if [ ! -d "$SRC/.git" ]; then
      git clone --depth 1 "$REPO" "$SRC"
    fi
    ( cd "$SRC" && uv sync )
    rev=$(git -C "$SRC" rev-parse HEAD)
    if [ -f "$PIN" ] && [ "$(cat "$PIN")" != "$rev" ] && [ -z "$repin" ]; then
      printf 'the checkout has moved:\n  pinned  %s\n  now     %s\n' "$(cat "$PIN")" "$rev" >&2
      echo "re-run with --repin to accept it" >&2
      exit 1
    fi
    printf '%s\n' "$rev" > "$PIN"
    printf 'engine  %s\nsfx     %s\ntracks  %s\nrevision %s\n' \
      "$SRC" "$SFX_MODEL" "$MUSIC_MODEL" "$rev"
    if [ "$CLI_FLAVOR" = optimized ]; then
      printf 'runtime %s  (ungated weights)\n' "$CLI"
    else
      echo 'runtime the engine CLI — the weights of small-sfx and small-music are'
      echo '        gated, so accept the terms and log in once (see §9 of SOUND.md),'
      echo '        or install optimized/mlx and this picks it up on its own.'
    fi
    echo
    echo "the weights download on first generation, into the HF cache."
    echo "licence: Stability AI Community Licence — read it before this audio is"
    echo "distributed, and put the line in the pack's LICENSES.json."
    ;;

  one)
    prompt=""; as=""; into=""; level=place; secs=""
    sound=""; tags=""; pool=""; from=""; seed=""; cfg=""; neg=""
    while [ $# -gt 0 ]; do
      case "$1" in
        --prompt) prompt=${2:?--prompt needs a value}; shift 2;;
        --from)   from=${2:?--from needs a value}; shift 2;;
        --seed)   seed=${2:?--seed needs a value}; shift 2;;
        --cfg)    cfg=${2:?--cfg needs a value}; shift 2;;
        --negative) neg=${2:?--negative needs a value}; shift 2;;
        --as)     as=${2:?--as needs a value}; shift 2;;
        --into)   into=${2:?--into needs a value}; shift 2;;
        --level)  level=${2:?--level needs a value}; shift 2;;
        --seconds) secs=${2:?--seconds needs a value}; shift 2;;
        --sound)  sound=${2:?--sound needs a value}; shift 2;;
        --tags)   tags=${2:?--tags needs a value}; shift 2;;
        --pool)   pool=${2:?--pool needs a value}; shift 2;;
        *) echo "unknown flag $1" >&2; exit 1;;
      esac
    done
    if [ -z "$prompt" ] && [ -z "$from" ]; then
      echo "one: --prompt is required" >&2
      echo "     (or --from <wav> to install a raw the model already made)" >&2
      exit 1
    fi
    if [ -n "$from" ] && [ ! -f "$from" ]; then
      echo "one: no such --from file: $from" >&2; exit 1
    fi
    if [ -n "$seed" ] && [ -n "$from" ]; then
      echo "one: --seed and --from are alternatives — a raw already has its seed" >&2
      exit 1
    fi
    if { [ -n "$cfg" ] || [ -n "$neg" ]; } && [ -n "$from" ]; then
      echo "one: --cfg and --negative steer a generation, not an installed raw" >&2
      exit 1
    fi
    if { [ -n "$cfg" ] || [ -n "$neg" ]; } && [ "$CLI_FLAVOR" != optimized ]; then
      echo "one: --cfg and --negative are the optimized runtime's flags;" >&2
      echo "     the engine CLI has no equivalent here" >&2
      exit 1
    fi
    [ -n "$as" ]     || { echo "one: --as is required" >&2; exit 1; }
    [ -n "$into" ]   || { echo "one: --into is required" >&2; exit 1; }
    # `--into` is resolved against the repo root, the way `batch` already does
    # it — and the doc's own examples are relative. The optimized runtime writes
    # a relative `--out` under its *own* `output/` directory, so a relative
    # destination puts the raw inside the engine tree and `prepare` then fails on
    # a file that is not where it was told to look.
    case "$into" in
      /*) ;;
      *)  into="$ROOT/$into";;
    esac
    case "$level" in
      place)  model=$SFX_MODEL;   itarget=${I_TARGET:--26};;   # under the voice
      moment) model=$SFX_MODEL;   itarget=${I_TARGET:--20};;   # in front of it
      track)  model=$MUSIC_MODEL; itarget=;;                   # add-music.sh owns the spec
      *) echo "--level is 'place', 'moment' or 'track', not '$level'" >&2; exit 1;;
    esac
    if [ -z "$secs" ]; then
      case "$level" in place) secs=90;; moment) secs=3;; track) secs=120;; esac
    fi

    # A track's bookkeeping is checked BEFORE the model runs: an untagged take
    # is silent everywhere, and a minute of music is a slow way to find out.
    if [ "$level" = track ]; then
      [ -n "$tags" ] || {
        echo "one: --tags is required for a track (comma-separated, e.g. battle,intense)" >&2
        echo "     the palette matches on tags — an untagged track never plays." >&2
        exit 1
      }
      # The pool convention is `<sound>-bg-<n>.mp3`, so the take's name already
      # says which sound it belongs to; take it from there when not given.
      [ -n "$sound" ] || sound=$(printf '%s\n' "$as" | sed -E 's/-bg-[0-9]+$//')
      [ -n "$pool" ] || pool="$(dirname "$into")/music-pool.json"
      [ -f "$pool" ] || { echo "one: no such music pool: $pool" >&2; exit 1; }
    fi

    check_engine
    stage="$into/.gen"
    mkdir -p "$into"
    rm -rf "$stage"; mkdir -p "$stage"
    canvas=$secs
    if [ "$level" = moment ] \
       && awk -v s="$secs" -v c="$CANVAS" 'BEGIN { exit (s < c) ? 0 : 1 }'; then
      canvas=$CANVAS
    fi
    # The raw goes in its own subdirectory: normalize-audio.sh takes *every*
    # audio file in the staging directory, so a raw left beside the finished clip
    # is another clip to it, and lands in the pool as `<name>.raw.mp3`.
    if [ -n "$from" ]; then
      # A raw the model already made, picked by ear or by measurement. It is
      # still run through `prepare` and the normalizer, so it meets the pool on
      # the same terms as a fresh generation — this is the way back in for a take
      # re-rolled outside this script, and the reason --seed exists at all.
      printf 'install   %s  (%s cut to %s s, %s)\n' \
        "$as" "$(basename "$from")" "$secs" "$level"
      mkdir -p "$stage/.raw"
      cp "$from" "$stage/.raw/$as.wav"
    else
      if [ "$canvas" = "$secs" ]; then
        printf 'generate  %s  (%s s, %s, %s%s)\n' "$as" "$secs" "$model" "$level" \
          "$([ -n "$CLI" ] && printf ', %s' "$CLI_FLAVOR")"
      else
        printf 'generate  %s  (%s s asked on a %s s canvas, %s, %s%s)\n' \
          "$as" "$secs" "$canvas" "$model" "$level" \
          "$([ -n "$CLI" ] && printf ', %s' "$CLI_FLAVOR")"
      fi
      run_model "$model" "$prompt" "$canvas" "$stage/.raw/$as.wav" "$seed" "$cfg" "$neg"
    fi
    prepare "$stage/.raw/$as.wav" "$stage/$as.wav" "$secs" "$level"

    if [ "$level" = track ]; then
      "$ROOT/tools/add-music.sh" --tags "$tags" --sound "$sound" --as "$as" \
        --pool "$pool" --dest "$into" "$stage/$as.wav"
    else
      # A moment is normalized by its peak, not by a loudness measurement: it
      # is 0.3–2 s of event, and the pool's own note is that such a transient
      # "reads several LU low on integrated loudness — that number does not
      # describe them, their peaks sit with the voice peaks". The pool's short
      # takes confirm it: they sit at −3 to −5 dBFS peak, not at a LUFS rung.
      # Its trim floor is lowered too, because the model's floor under the event
      # is around −57 dB and the pool's −45 dB default reads that as silence.
      if [ "$level" = moment ]; then
        I_TARGET=$itarget TP_TARGET=${TP_TARGET:--3} \
          PEAK_UNDER=${BM_SA_PEAK_UNDER:-2} TRIM_FLOOR=${BM_SA_TRIM_FLOOR:--70dB} \
          "$ROOT/tools/normalize-audio.sh" "$stage" "$into"
      else
        I_TARGET=$itarget TP_TARGET=${TP_TARGET:--3} \
          "$ROOT/tools/normalize-audio.sh" "$stage" "$into"
      fi
    fi
    rm -rf "$stage"
    if [ -f "$into/$as.mp3" ]; then
      if [ "$level" = track ]; then
        printf '\nnext  the track is in %s — check its tags against the palette in\n' \
          "${pool#"$ROOT"/}"
        echo "      the pack's scene-map.json, or it is silent everywhere"
      else
        printf '\nnext  add "%s" to the sound it belongs to in the pack registry\n' \
          "${into#"$ROOT"/}/$as.mp3"
      fi
    fi
    ;;

  batch)
    list=""
    while [ $# -gt 0 ]; do
      case "$1" in
        --list) list=${2:?--list needs a value}; shift 2;;
        *) echo "unknown flag $1" >&2; exit 1;;
      esac
    done
    [ -f "$list" ] || { echo "batch: no such list: $list" >&2; exit 1; }
    check_engine
    made=0
    while IFS=$'\t' read -r as level secs into prompt tags; do
      case "$as" in ''|'#'*) continue;; esac
      [ -n "$prompt" ] || { echo "batch: $as has no prompt" >&2; exit 1; }
      dest="$ROOT/$into"
      if [ -e "$dest/$as.mp3" ]; then
        printf '  have  %-34s %s\n' "$as" "$into"
        continue
      fi
      args=(one --prompt "$prompt" --as "$as" --into "$dest" --level "$level")
      if [ -n "$secs" ]; then args+=(--seconds "$secs"); fi
      if [ "$level" = track ]; then
        [ -n "$tags" ] || {
          echo "batch: $as is a track and its line has no tags column" >&2
          exit 1
        }
        args+=(--tags "$tags")   # and `one` takes the sound key off the take name
      fi
      # The run's steering, not the row's: see the header. `one` refuses these
      # on the engine CLI, so a batch that asks for them on a runtime that has
      # no equivalent fails on its first row instead of quietly ignoring them.
      [ -n "${BM_SA_NEGATIVE:-}" ] && args+=(--negative "$BM_SA_NEGATIVE")
      [ -n "${BM_SA_CFG:-}" ]      && args+=(--cfg "$BM_SA_CFG")
      # </dev/null: this loop's stdin is the list, and a child that reads stdin
      # (ffmpeg, before it learned -nostdin) eats the next line's opening bytes.
      "$ROOT/tools/gen-sound.sh" "${args[@]}" </dev/null
      made=$((made + 1))
    done < "$list"
    printf '\n%d clips generated\n' "$made"
    ;;

  list)
    printf 'engine    %s\n' "$ENGINE"
    printf 'sfx       %s   (places, moments)\n' "$SFX_MODEL"
    printf 'tracks    %s   (a music pack)\n' "$MUSIC_MODEL"
    if [ -n "$CLI" ]; then
      printf 'cli       %s   (flavor %s)\n' "$CLI" "$CLI_FLAVOR"
    else
      printf 'cli       the engine default   (flavor %s)\n' "$CLI_FLAVOR"
    fi
    if [ -d "$SRC/.git" ]; then
      now=$(git -C "$SRC" rev-parse HEAD)
      printf 'checkout  %s\n' "$now"
      [ -f "$PIN" ] && printf 'pinned    %s%s\n' "$(cat "$PIN")" \
        "$([ "$now" = "$(cat "$PIN")" ] && echo '  (match)' || echo '  (MOVED)')"
    else
      echo 'checkout  not installed — tools/gen-sound.sh setup'
    fi
    printf 'venv      %s%s\n' "$SRC/.venv" \
      "$([ -x "$SRC/.venv/bin/python" ] && echo '  (present)' || echo '  (missing)')"
    printf 'weights   %s\n' "${HF_HOME:-$HOME/.cache/huggingface}/hub  (downloaded on first use)"
    ;;

  *)
    echo "unknown command $cmd" >&2
    exit 1
    ;;
esac
