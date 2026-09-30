#!/usr/bin/env python3
"""What a pack's pools name, and what is actually on disk.

    tools/inspect-pool.py assets/_extends/craft
    tools/inspect-pool.py assets/_extends/court-mystery music inject
    tools/inspect-pool.py assets

A pack's registries are authored before its audio: the sound is designed, then
recorded or generated, then normalized into place. That order is what lets the
two halves be done by different hands on different days, and it means the
question "where is this pack now?" has to be asked of the filesystem rather than
of memory — and it is a question with a number in the answer.

For every sound a registry names this prints the sound, the file, whether the
file exists, and for the ones that exist their duration, sample rate, channel
count and **measured** mean volume. The measurement is the point: a clip can be
on disk, monophonic, 48 kHz and at the wrong loudness, and each of those is a
different fault with a different fix, so a row that says only "on disk" is not
enough to sign anything off.

Nothing is written and nothing is assumed. A file that is named but absent is
`PENDING` — a normal state for a pack mid-build, not an error — and a file that
is present but unprobeable is called out rather than passed over, because that
is what a zero-byte placeholder looks like.

Reads the pool registries the way the mix does: a key is a sound, `files` are
its takes, and the paths are relative to the pack's own directory.
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

# The three registries, and the clip directory each one's files live in. This
# mirrors `audio_pool::PoolKind`; the check is here because the pools are the
# authority for what exists and this script must not be the second answer.
REGISTRIES = ("effect-pool.json", "music-pool.json", "inject-pool.json")

WANT = {"effect-pool.json": "effects", "music-pool.json": "music", "inject-pool.json": "injects"}


def probe(path: Path) -> tuple[str, str, str] | None:
    """(duration, rate/channels, mean volume) for one clip, or None."""
    dur = subprocess.run(
        [
            "ffprobe", "-v", "error", "-show_entries", "format=duration",
            "-of", "csv=p=0", str(path),
        ],
        capture_output=True, text=True,
    ).stdout.strip()
    fmt = subprocess.run(
        [
            "ffprobe", "-v", "error", "-select_streams", "a:0",
            "-show_entries", "stream=sample_rate,channels", "-of", "csv=p=0", str(path),
        ],
        capture_output=True, text=True,
    ).stdout.strip()
    if not dur:
        return None
    try:
        dur = f"{float(dur):.1f}s"
    except ValueError:
        pass
    rate = fmt.replace(",", "/") if fmt else "?"
    vol = subprocess.run(
        ["ffmpeg", "-v", "info", "-i", str(path), "-af", "volumedetect", "-f", "null", "-"],
        capture_output=True, text=True,
    ).stderr
    mean = next(
        (line.split("mean_volume:")[1].strip() for line in vol.splitlines() if "mean_volume:" in line),
        "?",
    )
    return dur, rate, mean


def rows(pack: Path, registry: str) -> list[tuple[str, str, Path]]:
    path = pack / registry
    if not path.is_file():
        return []
    try:
        doc = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as e:
        print(f"  !! {registry} does not parse: {e}", file=sys.stderr)
        return []
    out = []
    for sound, entry in sorted(doc.items()):
        if sound.startswith("_") or not isinstance(entry, dict):
            continue
        for f in entry.get("files", []):
            out.append((sound, f, pack / f))
    return out


def main(argv: list[str]) -> int:
    if len(argv) < 2:
        print(__doc__)
        return 2
    pack = Path(argv[1])
    if not pack.is_dir():
        print(f"not a directory: {pack}", file=sys.stderr)
        return 2
    only = argv[2:]

    total = pending = present = unreadable = 0
    for registry in REGISTRIES:
        if only and WANT[registry] not in only:
            continue
        found = rows(pack, registry)
        if not found:
            continue
        print(f"=== {WANT[registry]} ===")
        for sound, f, path in found:
            total += 1
            if not path.is_file():
                pending += 1
                print(f"  {sound:<24} {f:<36} PENDING")
                continue
            got = probe(path)
            if got is None:
                present += 1
                unreadable += 1
                print(f"  {sound:<24} {f:<36} UNREADABLE — named but not a usable clip")
                continue
            present += 1
            dur, rate, mean = got
            print(f"  {sound:<24} {f:<36} {dur:>8}  {rate:<8} {mean}")
    print()
    print(
        f"{present}/{total} on disk, {pending} pending"
        + (f", {unreadable} unreadable" if unreadable else "")
        + f"  ({pack})"
    )
    # A named-but-absent clip is progress, not failure. An unreadable one is
    # not: it is either a truncated copy or a placeholder, and it will degrade a
    # merge to silence with a warning nobody reads.
    return 1 if unreadable else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
