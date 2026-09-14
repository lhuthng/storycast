"""Remote job runner: one stage of one chapter. Run from ~/tts-worker/job.

Usage: ../.venv/bin/python runjob.py {prep|render|merge} NN [--url-template T]
  prep   crawl chapter NN + digest -> script-NN.json (bible: local copy, merged back by orchestrator)
  render TTS script-NN.json -> segs-NN/ + marker done-render-NN
  merge  assemble segs-NN/ -> Ch.NN.mp3 (fetched by orchestrator)
Markers (checked by orchestrator over ssh): done-render-NN, done-merge-NN.
"""
import sys
from pathlib import Path

BASE = Path(__file__).parent
sys.path.insert(0, str(BASE.parent))  # venv's tts_vieneu (custom voices live there)
sys.path.insert(0, str(BASE))  # job-local pipeline files win

STAGE, NN = sys.argv[1], int(sys.argv[2])
PAD = f"{NN:02d}"
URL = "https://storya.click/truyen/nguoi-tren-van-nguoi/chuong-{n}"
if "--url-template" in sys.argv:
    URL = sys.argv[sys.argv.index("--url-template") + 1]

SCRIPT = BASE / f"script-{PAD}.json"
CHAPTER = BASE / f"ch{PAD}.txt"
SEGS = BASE / f"segs-{PAD}"
FINAL = BASE / f"Ch.{NN}.mp3"

import os

ANALYZER = os.environ.get("ANALYZER", "opencode")

if STAGE == "prep":
    from ingest import ingest
    from analyze import analyze

    ingest(url=URL.format(n=NN), out=str(CHAPTER))
    analyze(str(CHAPTER), str(SCRIPT), analyzer=ANALYZER)
    print(f"PREP-OK {SCRIPT}", flush=True)
elif STAGE == "render":
    from synthesize import render_segments

    render_segments(str(SCRIPT), str(BASE / "cast.json"), str(SEGS), engine="vieneu")
    (BASE / f"done-render-{PAD}").touch()
    print(f"RENDER-OK {SEGS}", flush=True)
elif STAGE == "merge":
    from synthesize import assemble

    mp3 = assemble(str(SCRIPT), str(BASE / "cast.json"), str(SEGS), str(BASE / "chxx.wav"),
                   gap_ms=300, ambience=True, speed=1.25,
                   engine="vieneu").with_suffix(".mp3")
    mp3.rename(FINAL)
    for p in BASE.glob("chxx*"):
        p.unlink(missing_ok=True)
    (BASE / f"done-merge-{PAD}").touch()
    print(f"MERGE-OK {FINAL} {FINAL.stat().st_size}", flush=True)
else:
    raise SystemExit(f"unknown stage {STAGE}")
