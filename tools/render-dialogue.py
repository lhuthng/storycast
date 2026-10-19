#!/usr/bin/env python3
"""Render every non-narrator take of a workspace's render plans, in place.

Reads `data/render/<nn>.json`, skips takes whose speaker is the Narrator, and
writes each remaining take to the file the plan already names in
`data/audio/segments-<adapter>-<engine>-<nn>`. Plans are never rewritten, so a
later full render only fills the narrator's takes.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path

NARRATOR = {"narrator", "người dẫn chuyện", "người dẫn", "dẫn chuyện"}
MP3_KBPS = {"raw": None, "small": 64, "balanced": 96, "high": 128}
MIN_TAKE_BYTES = 1000


def repo_root() -> Path:
    here = Path(__file__).resolve().parent
    for cand in (here.parent, Path.cwd()):
        if (cand / "backend" / "Cargo.toml").is_file():
            return cand
    raise SystemExit("cannot find the repo root (backend/Cargo.toml)")


def infer(base: str, take: dict, engine: str) -> bytes:
    body = json.dumps(
        {
            "text": take["text"],
            "voice": take["voice"],
            "temperature": take.get("temperature", 0.9),
            "silence_p": take.get("silence_p", 0.1),
            "engine": engine,
        }
    ).encode()
    req = urllib.request.Request(
        f"{base}/infer", data=body, headers={"content-type": "application/json"}
    )
    with urllib.request.urlopen(req, timeout=900) as r:
        data = r.read()
    if len(data) < MIN_TAKE_BYTES:
        raise RuntimeError(f"sidecar answered {len(data)} bytes — not a take")
    return data


def encode(wav: bytes, dest: Path, kbps: int) -> None:
    dest.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=dest.parent) as tmp:
        src = Path(tmp) / "take.wav"
        dst = Path(tmp) / "take.mp3"
        src.write_bytes(wav)
        subprocess.run(
            [
                "ffmpeg", "-nostdin", "-y", "-loglevel", "error", "-i", str(src),
                "-ac", "1", "-ar", "48000", "-c:a", "libmp3lame", "-b:a", f"{kbps}k",
                "-write_xing", "0", "-id3v2_version", "0", "-map_metadata", "-1",
                str(dst),
            ],
            check=True,
        )
        os.replace(dst, dest)


def take_targets(ws: Path, engine: str) -> list[tuple[int, dict, Path]]:
    out = []
    for plan_path in sorted((ws / "data" / "render").glob("*.json")):
        chapter = int(plan_path.stem)
        seg = ws / "data" / "audio" / f"segments-vi-VN-{engine}-{chapter:02}"
        plan = json.loads(plan_path.read_text(encoding="utf-8"))
        for take in plan.get("takes") or []:
            if take.get("speaker", "").strip().lower() in NARRATOR:
                continue
            out.append((chapter, take, seg / take["file"]))
    return out


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--root", type=Path, default=None)
    ap.add_argument("--workspace", default="beyond-myriads")
    ap.add_argument("--engine", default="vieneu")
    ap.add_argument("--tts-url", default="http://127.0.0.1:8832")
    ap.add_argument("--chapter", type=int, default=None, help="one chapter only")
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args()

    root = args.root or repo_root()
    ws = root / "workspaces" / args.workspace
    settings = json.loads((ws / "settings.json").read_text())
    kbps = MP3_KBPS.get(str(settings.get("take_quality", "balanced")).lower(), 96)
    if kbps is None:
        raise SystemExit("take_quality raw has no mp3 name; run the pipeline instead")

    targets = take_targets(ws, args.engine)
    if args.chapter is not None:
        targets = [t for t in targets if t[0] == args.chapter]
    todo = [t for t in targets if not (t[2].is_file() and t[2].stat().st_size > MIN_TAKE_BYTES)]
    done = len(targets) - len(todo)
    print(
        f"{len(targets)} dialogue take(s) in {args.workspace}, {done} already on disk, "
        f"{len(todo)} to render",
        flush=True,
    )
    if args.dry_run or not todo:
        return 0

    started = time.monotonic()
    failed: list[str] = []
    streak = 0
    for i, (chapter, take, dest) in enumerate(todo, 1):
        try:
            try:
                wav = infer(args.tts_url, take, args.engine)
            except urllib.error.HTTPError as e:
                detail = e.read().decode("utf-8", "replace")[:200]
                raise RuntimeError(f"HTTP {e.code}: {detail}") from e
            encode(wav, dest, kbps)
        except Exception as e:  # noqa: BLE001 - one bad take must not stop the run
            failed.append(f"ch{chapter:02} {take['tag']} {take['speaker']}: {e}")
            print(f"FAILED {failed[-1]}", flush=True)
            streak += 1
            if streak >= 5:
                print("5 failures in a row — the sidecar is gone; stopping", flush=True)
                return 2
            continue
        streak = 0
        if i % 20 == 0 or i == len(todo):
            rate = i / max(time.monotonic() - started, 1e-6)
            eta = (len(todo) - i) / rate / 60
            print(
                f"ch{chapter:02} {i}/{len(todo)} ({100 * i / len(todo):.1f}%) "
                f"{rate:.2f} take/s, eta {eta:.0f} min",
                flush=True,
            )

    elapsed = time.monotonic() - started
    print(f"rendered {len(todo) - len(failed)}/{len(todo)} in {elapsed / 60:.1f} min", flush=True)
    if failed:
        print(f"{len(failed)} failed:", flush=True)
        for line in failed:
            print(f"  {line}", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
