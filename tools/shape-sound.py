#!/usr/bin/env python3
"""SoX authoring front door: judge a take, make another one, or shape one.

    tools/shape-sound.py doctor
    tools/shape-sound.py recipes
    tools/shape-sound.py triage  <clip> [clip...] [--json] [--sort KEY]
    tools/shape-sound.py variants <clip> --count N
        [--pitch CENTS] [--tone DB] [--gain DB] [--seed S] [--as STEM]
        [--out DIR] [--force]
    tools/shape-sound.py mutate <clip> --recipe NAME [--amount F]
        [--seed S] [--as STEM] [--out DIR] [--force]
    tools/shape-sound.py shape <clip> --effect "<sox effects>" [--as STEM]
    tools/shape-sound.py synth --effect "synth 3s sine 200 tremolo 5 60"
        [--as STEM] [--seconds S]
    tools/shape-sound.py self-test

WHY SOX, AND WHY ONLY HERE. SoX is a fine *authoring* tool and a poor pipeline
dependency. `tools/normalize-audio.sh` and the mix in `bm-core/src/ambience.rs`
are ffmpeg **on purpose**, and the reason is one thing SoX does not have: EBU
R128 integrated-loudness normalization. A pool clip's `level` means something
only because every clip sits at a known -26 / -20 LUFS rung, and `loudnorm` is
what puts it there. So SoX never replaces `normalize-audio.sh` — it runs
*before* it, on the way in, and every file this tool writes is meant to be
handed to `tools/add-sound.py` (or `add-music.sh`), which does the spec check,
the normalize pass, the registry edit and the aliases.

What SoX is actually better at, and what this tool wraps:

  * **`triage`** — the take-ranking problem `docs/SOUND.md` §9 names. A
    whole-clip average cannot see *when* a sound happens inside the clip, so a
    knock and a stone breaking apart can share a spectral centroid and be
    nothing alike. SoX `stat` gives crest factor, the deltas and DC offset
    cheaply, and (when numpy is present) the raw stream gives attack time and
    the count of onsets — the measurements that separate those takes.
  * **`variants`** — the "two takes a spot sound, not one" rule. `pick` rolls
    a second, decorrelated take; a one-room foley session often yields only
    one, and a take nobody can re-record is where a *subtle* SoX variation is
    better than nothing. These are variations, not new recordings: judge them
    with `triage` and your ears (`tools/sound-lab.py` plays a clip).
  * **`mutate`** — the *crazy* half. SoX does things ffmpeg phrases awkwardly or
    not at all: `pitch` that shifts formants, `reverse`-into-reverb-into-reverse
    for a ghost, `trim`+`repeat` for a stutter, aliasing resample for lo-fi.
    Named recipes are fine *here* — they make a clip, and a clip's character is
    baked in before it enters a pool, so nothing in the scene map can hide
    behind a name. Run `recipes` to see the palette.
  * **`shape`** — one explicit SoX chain, for a clip with a specific fault.
    The escape hatch under everything else.
  * **`synth`** — generate a source clip from nothing (`sox -n`): drones,
    rumblers, risers, sci-fi tones. SoX's `synth`/`tremolo`/`bend` vocabulary is
    richer than ffmpeg's `sine`/`anoisesrc`, and a generated bed is a legitimate
    starting point for a place the model or a microphone could not reach.

WHY ffmpeg STILL DECODES. SoX reads what its build supports; libsndfile on a
stock Homebrew install is not guaranteed to read mp3. So every input is first
decoded to 48 kHz mono f32 wav by ffmpeg — which is always present — and SoX is
used only for the shaping, never the decoding. One decoder, one resampler, and
the lossy pool formats are read exactly the way the rest of the pipeline reads
them.

NOTHING IS WRITTEN INTO A POOL. Output lands in `refs/temp/clips/shape-sound/`
(`refs/` is gitignored), and the tool prints the `add-sound.py` line that
registers it. It is the same split as `add-music.sh`: this tool makes a file a
pool could hold; the ingest tools decide whether it may.
"""

from __future__ import annotations

import argparse
import json
import math
import pathlib
import random
import re
import shutil
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
# Staging under `refs/` for the same reason `add-sound.py`'s staging is: anything
# under `assets/` ships in the release, and a scratch wav is not a pack.
STAGING = ROOT / "refs" / "temp" / "clips" / "shape-sound"
SR = 48_000
# SoX's own hard bounds: pitch and tempo ranges the manual defines.
MAX_PITCH_CENTS = 1200.0

SOX_HINT = (
    "sox is an *authoring* dependency only — the merge pipeline stays "
    "ffmpeg-only. Install it with `brew install sox` (macOS) or "
    "`apt install sox` (Debian/Ubuntu), then re-run."
)


def die(msg: str) -> None:
    print(f"REFUSED: {msg}")
    sys.exit(1)


def run(cmd: list[str]) -> subprocess.CompletedProcess:
    return subprocess.run(cmd, capture_output=True, text=True)


def find_sox(explicit: str | None) -> str:
    if explicit:
        p = pathlib.Path(explicit)
        if not p.is_file():
            die(f"no sox binary at {p}")
        return str(p)
    exe = shutil.which("sox")
    if not exe:
        die(f"sox not on PATH — {SOX_HINT}")
    return exe


def need_ffmpeg() -> None:
    for tool in ("ffmpeg", "ffprobe"):
        if not shutil.which(tool):
            die(f"{tool} not on PATH — decoding runs through ffmpeg, see "
                f"docs/SOUND.md §3")


# ---------------------------------------------------------------------------
# measurement — pure parsers first, so `self-test` can exercise them with no sox
# ---------------------------------------------------------------------------

# `sox <f> -n stat` prints one `Key : value` per line, keys padded to a column,
# and some keys are two words (`RMS     amplitude`, `Length (seconds)`).
_STAT_LINE = re.compile(r"^([A-Za-z][A-Za-z ()]*?):\s+(-?[0-9.]+)\s*$")


def parse_stat(text: str) -> dict[str, float]:
    """The `sox ... stat` block as `{normalized key: value}`.

    Key whitespace is collapsed so `RMS     amplitude` and `RMS amplitude` are
    one key; that is the whole reason this is a parser and not four regexes
    inline.
    """
    out: dict[str, float] = {}
    for line in text.splitlines():
        m = _STAT_LINE.match(line.strip())
        if not m:
            continue
        key = " ".join(m.group(1).split()).lower()
        out[key] = float(m.group(2))
    return out


def crest_factor(stats: dict[str, float]) -> float | None:
    """Peak / RMS — the transient-versus-sustain discriminator.

    A hit has a high crest (a spike over a mostly quiet body); a bed has a low
    one (energy spread across the whole clip). `docs/SOUND.md` §9 names this as
    one of the three things a whole-clip average misses, and it is the one
    `stat` already computes the inputs for.
    """
    peak = abs(stats.get("maximum amplitude", 0.0))
    rms = stats.get("rms amplitude", 0.0)
    if rms <= 0.0:
        return None
    return peak / rms


def envelope_metrics(samples, rate: int) -> dict[str, float]:
    """Attack time and onset count from a mono f32 sample array.

    `samples` is any 1-D sequence of floats in [-1, 1] (a numpy array when
    numpy is installed). Kept duck-typed so `self-test` can pass a plain list
    and never import numpy. The smoothing window is ~1 ms: short enough not to
    blur a click, long enough that a single sample of dither is not an onset.
    """
    n = len(samples)
    if n == 0:
        return {}
    win = max(1, rate // 1000)
    # |x| then a box average, computed with a running sum: an O(n) pass, no
    # numpy required for the arithmetic itself.
    env = [0.0] * n
    acc = 0.0
    for i in range(n):
        acc += abs(float(samples[i]))
        if i >= win:
            acc -= abs(float(samples[i - win]))
        env[i] = acc / min(i + 1, win)
    peak = max(env)
    if peak <= 0.0:
        return {"peak_env": 0.0}
    # Onset: the first place the envelope leaves the noise floor, at 5% of the
    # clip's own peak. Attack: how long it then takes to reach 90% — the rise,
    # which is what separates a click from a swell.
    onset = next((i for i, v in enumerate(env) if v >= 0.05 * peak), 0)
    ninety = next((i for i in range(onset, n) if env[i] >= 0.9 * peak), n - 1)
    attack_ms = (ninety - onset) / rate * 1000.0
    # Onsets: each time the envelope crosses 25% of peak *from below*. A
    # crossing only counts once per excursion — after counting, skip the 40 ms
    # refractory and then everything still above the threshold — or a single
    # transient counts once per sample of its rise.
    refractory = int(0.040 * rate)
    thresh = 0.25 * peak
    onsets = 0
    i = 0
    while i < n:
        if env[i] < thresh:
            i += 1
            continue
        onsets += 1
        i += refractory
        while i < n and env[i] >= thresh:
            i += 1
    return {
        "peak_env": peak,
        "attack_ms": attack_ms,
        "onsets": float(onsets),
    }


def sox_effects(params: dict[str, float]) -> list[str]:
    """The SoX effect chain for one variation, in a fixed order.

    Pitch first, then the tone tilt, then gain, then the format tail — the
    order the effects run, so the command reads top to bottom. A zero parameter
    emits nothing rather than `pitch 0`, which SoX rejects.
    """
    fx: list[str] = []
    if params.get("pitch_cents"):
        fx += ["pitch", f"{int(round(params['pitch_cents']))}"]
    tone = params.get("tone_db", 0.0)
    if tone:
        # A *tilt*, not a boost: bass down as treble comes up, so the variant
        # is a different shade of the same sound rather than a louder top end.
        fx += ["bass", f"{-tone:.2f}", "treble", f"{tone:.2f}"]
    if params.get("gain_db"):
        fx += ["gain", f"{params['gain_db']:.2f}"]
    return fx


def variant_params(
    seed: int, count: int, pitch: float, tone: float, gain: float
) -> list[dict[str, float]]:
    """`count` deterministic parameter sets for one source clip.

    Deterministic from `seed` on purpose: the same command makes the same
    takes, which is how a variation worth keeping can be re-made — the same
    reason `gen-sound.sh` pins its seed. Variant *1* is never the source: a
    pitch of zero would be a byte-for-byte copy the picker would roll as a
    distinct take, which is worse than not having it.
    """
    rng = random.Random(seed)
    out: list[dict[str, float]] = []
    for i in range(count):
        p = rng.uniform(-pitch, pitch) if pitch else 0.0
        # Nudge off zero: a variation nobody can hear is not a second take.
        if pitch and abs(p) < 1.0:
            p = math.copysign(1.0, p or 1.0)
        if abs(p) > MAX_PITCH_CENTS:
            p = math.copysign(MAX_PITCH_CENTS, p)
        out.append({
            "index": i + 1,
            "pitch_cents": round(p, 1),
            "tone_db": round(rng.uniform(-tone, tone), 2) if tone else 0.0,
            "gain_db": round(rng.uniform(-gain, gain), 2) if gain else 0.0,
        })
    return out


# ---------------------------------------------------------------------------
# the creative palette — SoX chains that make a clip *different*
# ---------------------------------------------------------------------------

def _amt(a: float) -> float:
    """Intensity, floored so a zero never emits `pitch 0` (which SoX rejects)
    and capped so an operator cannot drive a pitch past an octave by accident."""
    return max(0.1, min(3.0, a))


# Each entry is `(description, chain builder)`. The builder takes the intensity
# and returns SoX argv. Only effects whose syntax is stable across builds are
# used; `bend` and `fir` are deliberately absent — their profiles are easy to get
# subtly wrong, and a wrong profile is a clip that sounds broken, not creative.
#
# These make a *clip*. None of them is a mix knob: the result is normalized and
# registered like any recording, so it carries no hidden decision the scene map
# cannot see.
RECIPES: dict[str, tuple[str, object]] = {
    "reverse": (
        "the clip backwards — a swell reads as a stab",
        lambda a: ["reverse"],
    ),
    "ghost": (
        "reverse, drench in reverb, reverse again — a tail that arrives",
        lambda a: ["reverse", "reverb", f"{60 * _amt(a):.0f}", "60", "100", "reverse"],
    ),
    "demon": (
        "pitch down with a growl and a room",
        lambda a: ["pitch", f"{-700 * _amt(a):.0f}",
                   "reverb", f"{40 * _amt(a):.0f}", "50", "100",
                   "overdrive", f"{8 * _amt(a):.1f}"],
    ),
    "giant": (
        "an octave down and heavy",
        lambda a: ["pitch", f"{-1200 * _amt(a):.0f}",
                   "bass", f"{4 * _amt(a):.1f}",
                   "reverb", f"{50 * _amt(a):.0f}", "40", "100"],
    ),
    "sprite": (
        "pitch up, bright and small",
        lambda a: ["pitch", f"{900 * _amt(a):.0f}", "treble", f"{6 * _amt(a):.1f}"],
    ),
    "alien": (
        "pitch up through a chorus — glassy and wrong",
        lambda a: ["pitch", f"{500 * _amt(a):.0f}",
                   "chorus", "0.6", "0.9", "50", "0.4", "0.25", "2", "-t"],
    ),
    "underwater": (
        "lowpassed into a phaser with a slow tremolo",
        lambda a: ["lowpass", f"{max(200, 600 / (1 + _amt(a))):.0f}",
                   "phaser", "0.6", "0.66", "3", "0.6", "0.5",
                   "tremolo", f"{2 + _amt(a):.1f}", f"{min(80, 30 * _amt(a)):.0f}"],
    ),
    "radio": (
        "telephone band with saturation",
        lambda a: ["highpass", "300", "lowpass", "3000",
                   "overdrive", f"{15 * _amt(a):.1f}", "gain", f"{-2 * _amt(a):.1f}"],
    ),
    "megaphone": (
        "narrower and harder than radio — a shout down a corridor",
        lambda a: ["highpass", "500", "lowpass", "2500",
                   "overdrive", f"{25 * _amt(a):.1f}", "tremolo", "8", "10"],
    ),
    "lofi": (
        "resample down and back for aliasing grit",
        lambda a: ["rate", f"{max(2000, int(48_000 / (2 + 8 * _amt(a))))}",
                   "rate", "48000", "gain", f"{-_amt(a):.1f}"],
    ),
    "muffle": (
        "heard through a wall",
        lambda a: ["lowpass", f"{max(300, 1200 / (1 + _amt(a))):.0f}", "gain", "2"],
    ),
    "cave": (
        "a big wet room, scale with intensity",
        lambda a: ["reverb", f"{min(95, 70 * _amt(a) + 20):.0f}", "40",
                   f"{min(100, 60 + 40 * _amt(a)):.0f}"],
    ),
    "shimmer": (
        "pitched up into a multi-tap echo",
        lambda a: ["pitch", f"{300 * _amt(a):.0f}",
                   "echos", "0.7", "0.4", "80", "0.5", "160", "0.25", "320", "0.12"],
    ),
    "pulse": (
        "tremolo, for a breathing bed",
        lambda a: ["tremolo", f"{3 + 5 * _amt(a):.1f}", f"{min(95, 40 + 40 * _amt(a)):.0f}"],
    ),
    "drone": (
        "slowed and smeared — length grows, expected",
        lambda a: ["tempo", f"{max(0.4, 1 / (1 + 0.6 * _amt(a))):.3f}",
                   "reverb", f"{60 * _amt(a):.0f}", "50", "100",
                   "tremolo", f"{1.5 + _amt(a):.1f}", "20"],
    ),
    "stutter": (
        "the first slice repeated — length changes, expected",
        lambda a: ["trim", "0", f"{max(0.03, 0.10 / (1 + _amt(a))):.3f}",
                   "repeat", f"{int(3 + 6 * _amt(a))}"],
    ),
    "robot": (
        "saturated through a flanger",
        lambda a: ["overdrive", f"{12 * _amt(a):.1f}", "flanger",
                   "tremolo", "10", "15"],
    ),
    "grit": (
        "drive, top-end and contrast",
        lambda a: ["overdrive", f"{10 * _amt(a):.1f}",
                   "treble", f"{4 * _amt(a):.1f}",
                   "contrast", f"{min(100, 50 + 30 * _amt(a)):.0f}"],
    ),
}

# Recipes whose whole point is a different duration. Everything else is meant to
# keep the source's length, so a drift there is worth saying out loud.
LENGTH_CHANGING = {"drone", "stutter"}


# ---------------------------------------------------------------------------
# io helpers
# ---------------------------------------------------------------------------

def scratch_dir(name: str) -> pathlib.Path:
    d = STAGING / "src"
    d.mkdir(parents=True, exist_ok=True)
    return d / name


def stage_wav(src: pathlib.Path, tag: str) -> pathlib.Path:
    """Decode any input to 48 kHz mono f32 wav with ffmpeg.

    ffmpeg is the *decoder* because it reads every lossy format the pool holds
    and is already a hard dependency; SoX is used only for what it is good at.
    """
    if not src.is_file():
        die(f"no such file: {src}")
    dst = STAGING / "src" / f"{src.stem}.{tag}.wav"
    r = run(["ffmpeg", "-nostdin", "-y", "-hide_banner", "-loglevel", "error",
             "-i", str(src), "-map_metadata", "-1",
             "-ac", "1", "-ar", str(SR), "-c:a", "pcm_f32le", str(dst)])
    if r.returncode != 0 or not dst.exists():
        die(f"could not decode {src}:\n{r.stderr.strip()}")
    return dst


def sox_stat(exe: str, wav: pathlib.Path) -> dict[str, float]:
    r = run([exe, str(wav), "-n", "stat"])
    if r.returncode != 0:
        die(f"sox stat failed on {wav.name}:\n{r.stderr.strip()}")
    return parse_stat(r.stderr)


def read_mono_f32(exe: str, wav: pathlib.Path):
    """The staged wav's samples as a float array, or `None` if numpy is absent.

    numpy is an *optional* convenience here: without it the tool still reports
    everything `stat` knows (crest, deltas, DC, peak), and simply omits attack
    time and onsets. That degradation is deliberate — triage must never be
    unusable because of a missing analysis library.
    """
    try:
        import numpy as np
    except ImportError:
        return None
    r = subprocess.run([exe, str(wav), "-t", "f32", "-"],
                       capture_output=True)
    if r.returncode != 0 or not r.stdout:
        return None
    return np.frombuffer(r.stdout, dtype="<f4")


def fmt(v, spec: str, dash: str = "—") -> str:
    return "—" if v is None else format(v, spec)


# ---------------------------------------------------------------------------
# commands
# ---------------------------------------------------------------------------

def cmd_doctor(exe: str | None) -> None:
    """Report what the authoring tools can and cannot do right now."""
    for tool in ("ffmpeg", "ffprobe", "sox"):
        found = shutil.which(tool)
        print(f"{tool:8} {'ok  ' + found if found else 'MISSING'}")
    if not shutil.which("sox"):
        print(f"\n{SOX_HINT}")
    else:
        r = run([shutil.which("sox"), "--version"])
        print(f"\n{(r.stdout or r.stderr).strip()}")
    try:
        import numpy  # noqa: F401
        print("numpy    ok   (attack time and onset count available)")
    except ImportError:
        print("numpy    MISSING (triage reports stat metrics only)")


def cmd_triage(exe: str, clips: list[str], as_json: bool, sort_key: str) -> None:
    rows = []
    for c in clips:
        src = pathlib.Path(c)
        wav = stage_wav(src, "triage")
        st = sox_stat(exe, wav)
        peak = abs(st.get("maximum amplitude", 0.0))
        dc = st.get("mean amplitude")
        row = {
            "clip": src.name,
            "seconds": st.get("length (seconds)"),
            "peak_dbfs": 20 * math.log10(peak) if peak > 0 else None,
            "rms_dbfs": (20 * math.log10(st["rms amplitude"])
                         if st.get("rms amplitude", 0.0) > 0 else None),
            "crest_db": (20 * math.log10(crest_factor(st))
                         if crest_factor(st) else None),
            "max_delta": st.get("maximum delta"),
            "dc_offset": dc,
        }
        for k, v in envelope_metrics(read_mono_f32(exe, wav), SR).items():
            if k == "peak_env":
                continue
            row[k] = v
        row["flags"] = triage_flags(row)
        rows.append(row)

    order = {"name": "clip", "crest": "crest_db", "attack": "attack_ms",
             "onsets": "onsets", "peak": "peak_dbfs"}
    key = order.get(sort_key, "clip")
    rows.sort(key=lambda r: (r.get(key) is None, r.get(key)))
    if sort_key in ("crest", "attack", "peak"):
        rows.reverse()

    if as_json:
        print(json.dumps(rows, indent=2))
        return
    print(f"{'clip':38} {'sec':>6} {'peak':>7} {'crest':>7} {'attack':>8} "
          f"{'onsets':>6} {'dc':>9}  flags")
    for r in rows:
        print(f"{r['clip'][:38]:38} "
              f"{fmt(r['seconds'], '.2f'):>6} "
              f"{fmt(r['peak_dbfs'], '.1f'):>7} "
              f"{fmt(r['crest_db'], '.1f'):>7} "
              f"{fmt(r.get('attack_ms'), '.1f'):>8} "
              f"{fmt(r.get('onsets'), '.0f'):>6} "
              f"{fmt(r['dc_offset'], '+.4f'):>9}  {r['flags']}")
    print("\ncrest = peak/RMS: high is a hit, low is a bed. attack ms is the "
          "rise to 90% of the peak.\nThe pick wants two takes that differ; "
          "take the pair whose crest/attack differ most.")


def triage_flags(row: dict) -> str:
    flags = []
    if (row.get("peak_dbfs") or -99) > -0.1:
        flags.append("near clipping")
    if abs(row.get("dc_offset") or 0.0) > 0.002:
        flags.append("DC offset")
    if row.get("onsets") is not None and row["onsets"] == 0:
        flags.append("no clear onset")
    return ", ".join(flags) if flags else ""


def cmd_variants(exe: str, args: argparse.Namespace) -> None:
    src = pathlib.Path(args.clip)
    wav = stage_wav(src, "variants")
    outdir = (pathlib.Path(args.out) if args.out
              else STAGING / f"{src.stem}-variants-{args.seed}")
    if outdir.exists():
        if not args.force:
            die(f"{outdir} already exists — pass --force to overwrite, or "
                f"--seed S for a different set")
        shutil.rmtree(outdir)
    outdir.mkdir(parents=True, exist_ok=True)

    base = sox_stat(exe, wav)
    params = variant_params(args.seed, args.count, args.pitch, args.tone,
                            args.gain)
    made = []
    for p in params:
        dst = outdir / f"{args.as_name or src.stem}-v{p['index']}.wav"
        cmd = [exe, str(wav), str(dst), *sox_effects(p),
               "channels", "1", "rate", str(SR)]
        r = run(cmd)
        if r.returncode != 0 or not dst.exists():
            die(f"sox failed for variant {p['index']}:\n{r.stderr.strip()}")
        st = sox_stat(exe, dst)
        got = st.get("length (seconds)")
        want = base.get("length (seconds)")
        drift = (abs(got - want) / want * 100.0
                 if got and want else None)
        if drift is not None and drift > args.max_drift:
            print(f"  note  variant {p['index']}: length drifted "
                  f"{drift:.1f}% ({want:.3f}->{got:.3f}s) — SoX `pitch` is "
                  f"not always tempo-preserving on this build; re-check with "
                  f"`triage`")
        made.append((p, dst, got, crest_factor(st)))
        print(f"  ok    {dst.name:32} pitch {p['pitch_cents']:+.0f}c "
              f"tone {p['tone_db']:+.2f}dB gain {p['gain_db']:+.2f}dB  "
              f"{fmt(got, '.3f')}s")

    print(f"\n{len(made)} variant(s) in {outdir.relative_to(ROOT)}")
    print("these are variations, not recordings — judge them (\n"
          "  python3 tools/sound-lab.py, or `triage` above) before keeping any.")
    print("\nnext — spec-check, normalize and register them as takes:")
    print(f"  tools/add-sound.py <pack> <place|moment> <key> "
          f"'{outdir.relative_to(ROOT)}'/*.wav --tags <a,b>")


def cmd_shape(exe: str, args: argparse.Namespace) -> None:
    src = pathlib.Path(args.clip)
    wav = stage_wav(src, "shape")
    outdir = STAGING / "shaped"
    outdir.mkdir(parents=True, exist_ok=True)
    dst = outdir / f"{args.as_name or src.stem}-shaped.wav"
    # The user's chain, split on whitespace exactly as SoX would read argv: no
    # shell, so no quoting surprises, and a typo is SoX's own error, verbatim.
    fx = args.effect.split()
    if not fx:
        die("--effect is empty — pass a SoX chain, e.g. "
            '"highpass 80 equalizer 300 1q -4"')
    cmd = [exe, str(wav), str(dst), *fx, "channels", "1", "rate", str(SR)]
    r = run(cmd)
    if r.returncode != 0 or not dst.exists():
        die(f"sox failed:\n{r.stderr.strip()}")
    print(f"  ok    {dst.relative_to(ROOT)}")
    print(f"  chain {' '.join(fx)}")
    print("\nnext — spec-check, normalize and register:")
    print(f"  tools/add-sound.py <pack> <place|moment> <key> "
          f"'{dst.relative_to(ROOT)}' --tags <a,b>")


def cmd_recipes() -> None:
    print(f"{len(RECIPES)} SoX recipes — "
          f"`mutate <clip> --recipe NAME [--amount F]`\n")
    for name in sorted(RECIPES):
        desc, _ = RECIPES[name]
        print(f"  {name:11} {desc}")
    print("\n--amount scales intensity, 0.1–3.0 (default 1.0). A recipe makes a "
          "clip;\nnormalize-audio.sh still brings it to the layer's rung.")


def cmd_mutate(exe: str, args: argparse.Namespace) -> None:
    if args.recipe not in RECIPES:
        die(f"unknown recipe {args.recipe!r} — run `recipes` for the palette")
    desc, build = RECIPES[args.recipe]
    src = pathlib.Path(args.clip)
    wav = stage_wav(src, "mutate")
    fx = build(args.amount)
    outdir = (pathlib.Path(args.out) if args.out
              else STAGING / f"{src.stem}-{args.recipe}")
    if outdir.exists():
        if not args.force:
            die(f"{outdir} already exists — pass --force to overwrite")
        shutil.rmtree(outdir)
    outdir.mkdir(parents=True, exist_ok=True)
    dst = outdir / f"{args.as_name or src.stem}-{args.recipe}.wav"

    r = run([exe, str(wav), str(dst), *fx, "channels", "1", "rate", str(SR)])
    if r.returncode != 0 or not dst.exists():
        die(f"sox failed:\n{r.stderr.strip()}")

    base = sox_stat(exe, wav)
    st = sox_stat(exe, dst)
    got = st.get("length (seconds)")
    want = base.get("length (seconds)")
    print(f"  {args.recipe}: {desc}")
    print(f"  chain  {' '.join(fx)}")
    if got and want:
        pct = (got - want) / want * 100.0
        tag = "" if args.recipe in LENGTH_CHANGING else \
            ("  (length changed — check the placement)" if abs(pct) > 5 else "")
        print(f"  {want:.3f}s -> {got:.3f}s  ({pct:+.1f}%){tag}")
    print(f"  ok     {dst.relative_to(ROOT)}")
    print("\nnext — spec-check, normalize and register:")
    print(f"  tools/add-sound.py <pack> <place|moment> <key> "
          f"'{dst.relative_to(ROOT)}' --tags <a,b>")


def cmd_synth(exe: str, args: argparse.Namespace) -> None:
    fx = args.effect.split()
    if not fx:
        die("empty chain — pass a SoX synth chain, e.g. "
            '"synth 3 sine 200 tremolo 5 60"')
    outdir = STAGING / "synth"
    outdir.mkdir(parents=True, exist_ok=True)
    dst = outdir / f"{args.as_name or 'synth'}.wav"
    # `-n` is SoX's null input: the chain *is* the sound, so this is the one
    # path with no source file and no ffmpeg decode.
    cmd = [exe, "-n", str(dst), *fx, "channels", "1", "rate", str(SR)]
    if args.seconds:
        cmd += ["trim", "0", str(args.seconds)]
    r = run(cmd)
    if r.returncode != 0 or not dst.exists():
        die(f"sox failed:\n{r.stderr.strip()}")
    st = sox_stat(exe, dst)
    print(f"  chain  {' '.join(fx)}")
    print(f"  ok     {dst.relative_to(ROOT)}  {fmt(st.get('length (seconds)'), '.2f')}s")
    print("\nnext — spec-check, normalize and register:")
    print(f"  tools/add-sound.py <pack> <place|moment> <key> "
          f"'{dst.relative_to(ROOT)}' --tags <a,b>")


def cmd_self_test() -> None:
    """Pure-function checks, so the tool has a test that needs no sox."""
    st = parse_stat(
        "Samples read:          48000\n"
        "Length (seconds):      0.500000\n"
        "Maximum amplitude:     0.500000\n"
        "RMS     amplitude:     0.250000\n"
        "Mean    amplitude:     0.001000\n"
        "Maximum delta:         0.400000\n"
    )
    assert st["length (seconds)"] == 0.5
    assert st["rms amplitude"] == 0.25, "two-word keys normalize"
    assert st["maximum delta"] == 0.4
    assert abs(crest_factor(st) - 2.0) < 1e-9
    assert crest_factor({"maximum amplitude": 1.0, "rms amplitude": 0.0}) is None

    fx = sox_effects({"pitch_cents": -25.0, "tone_db": 1.5, "gain_db": -0.5})
    assert fx[:2] == ["pitch", "-25"], fx
    assert fx[2:4] == ["bass", "-1.50"], "tone tilts, never boosts"
    assert fx[4:6] == ["treble", "1.50"]
    assert sox_effects({}) == [], "all-zero emits nothing, not `pitch 0`"

    a = variant_params(7, 3, 25.0, 1.0, 0.5)
    b = variant_params(7, 3, 25.0, 1.0, 0.5)
    assert a == b, "same seed, same takes"
    assert a != variant_params(8, 3, 25.0, 1.0, 0.5), "seed matters"
    for p in a:
        assert abs(p["pitch_cents"]) >= 1.0, "a variation must be audible"
        assert abs(p["pitch_cents"]) <= 25.0
        assert abs(p["tone_db"]) <= 1.0
        assert abs(p["gain_db"]) <= 0.5

    env = envelope_metrics([0.0] * 100 + [1.0] * 4800, 48000)
    assert env["onsets"] == 1, env
    assert 0 <= env["attack_ms"] < 5, env

    # Every recipe must build a plausible SoX argv at every intensity: no empty
    # token, no NaN/inf sneaking in from a scaled float. This is the only check
    # that needs no sox binary, so it is the one that runs in CI.
    assert LENGTH_CHANGING <= set(RECIPES)
    for name, (desc, build) in RECIPES.items():
        assert desc and isinstance(desc, str)
        for a in (0.1, 1.0, 3.0):
            fx = build(a)
            assert fx, f"{name} @ {a} is empty"
            for tok in fx:
                t = str(tok)
                assert t and not any(bad in t.lower() for bad in ("nan", "inf")), \
                    f"{name} @ {a}: bad token {t!r}"
    print(f"self-test: ok ({len(RECIPES)} recipes)")


# ---------------------------------------------------------------------------

def main() -> None:
    ap = argparse.ArgumentParser(
        description="SoX authoring: triage, vary and shape pool clips before "
                    "normalize-audio.sh.",
        formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--sox", help="path to a sox binary (default: PATH)")
    sub = ap.add_subparsers(dest="cmd", required=True)

    sub.add_parser("doctor", help="what the authoring tools can do right now")
    sub.add_parser("recipes", help="the creative SoX palette")
    sub.add_parser("self-test", help="check the pure helpers (needs no sox)")

    t = sub.add_parser("triage", help="measure candidate takes and rank them")
    t.add_argument("clips", nargs="+")
    t.add_argument("--json", action="store_true")
    t.add_argument("--sort", default="name",
                   choices=["name", "crest", "attack", "onsets", "peak"])

    v = sub.add_parser("variants", help="synthesize decorrelated takes")
    v.add_argument("clip")
    v.add_argument("--count", type=int, default=1)
    v.add_argument("--pitch", type=float, default=25.0,
                   help="max pitch variation in cents (default 25)")
    v.add_argument("--tone", type=float, default=1.0,
                   help="max tone tilt in dB (default 1.0)")
    v.add_argument("--gain", type=float, default=0.5,
                   help="max gain variation in dB (default 0.5)")
    v.add_argument("--seed", type=int, default=1)
    v.add_argument("--as", dest="as_name", help="output stem")
    v.add_argument("--out", help="output directory (default: staging)")
    v.add_argument("--force", action="store_true")
    v.add_argument("--max-drift", type=float, default=5.0,
                   help="warn when a variant's length drifts past this %% "
                        "(default 5)")

    s = sub.add_parser("shape", help="run one explicit SoX effect chain")
    s.add_argument("clip")
    s.add_argument("--effect", required=True,
                   help='a SoX chain, e.g. "highpass 80 equalizer 300 1q -4"')
    s.add_argument("--as", dest="as_name", help="output stem")

    m = sub.add_parser("mutate", help="a creative SoX recipe (see `recipes`)")
    m.add_argument("clip")
    m.add_argument("--recipe", required=True)
    m.add_argument("--amount", type=float, default=1.0,
                   help="intensity, 0.1-3.0 (default 1.0)")
    m.add_argument("--as", dest="as_name", help="output stem")
    m.add_argument("--out", help="output directory (default: staging)")
    m.add_argument("--force", action="store_true")

    y = sub.add_parser("synth", help="generate a source clip from nothing")
    y.add_argument("--effect", required=True,
                   help='a SoX synth chain, e.g. "synth 3 sine 200 tremolo 5 60"')
    y.add_argument("--as", dest="as_name", help="output stem")
    y.add_argument("--seconds", type=float,
                   help="trim the result to this length")

    args = ap.parse_args()

    if args.cmd == "self-test":
        cmd_self_test()
        return
    if args.cmd == "doctor":
        cmd_doctor(args.sox)
        return
    if args.cmd == "recipes":
        cmd_recipes()
        return

    need_ffmpeg()
    exe = find_sox(args.sox)
    STAGING.mkdir(parents=True, exist_ok=True)
    if args.cmd == "triage":
        cmd_triage(exe, args.clips, args.json, args.sort)
    elif args.cmd == "variants":
        if args.count < 1:
            die("--count must be at least 1")
        cmd_variants(exe, args)
    elif args.cmd == "mutate":
        cmd_mutate(exe, args)
    elif args.cmd == "shape":
        cmd_shape(exe, args)
    elif args.cmd == "synth":
        cmd_synth(exe, args)


if __name__ == "__main__":
    main()
