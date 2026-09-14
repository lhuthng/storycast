"""Post-process: per-scene ambience beds + room reverb under the voice mix.

Offline, deterministic, no API. Inputs: chapter voice wav (with gaps), script
segments (with scene tags), run wav list. Missing bed files degrade gracefully
to dry voice for that span — never an error.
"""
from __future__ import annotations

import json
import subprocess
import wave
from collections import Counter
from pathlib import Path

ASSETS = Path("assets")
SCENE_MAP = ASSETS / "scene-map.json"
BEDS = ASSETS / "ambience"


def load_map(path: str | Path = SCENE_MAP) -> dict:
    return json.loads(Path(path).read_text(encoding="utf-8"))


def match_scene(scene: str, cfg: dict) -> dict:
    s = (scene or "").lower()
    for rule in cfg.get("rules", []):
        if any(k in s for k in rule["match"]):
            return rule
    return cfg.get("default", {"bed": None, "level": 0.0, "reverb": None})


def _wav_dur(path: Path) -> float:
    with wave.open(str(path), "rb") as w:
        return w.getnframes() / w.getframerate()


def run_scenes(segments: list[dict], runs: list[dict]) -> list[str]:
    """Majority scene tag per run (runs are consecutive same-speaker lines)."""
    out = []
    for run in runs:
        tags = [(segments[i].get("scene") or "").strip() for i in run["idx"]]
        top = Counter(t for t in tags if t).most_common(1)
        out.append(top[0][0] if top else "")
    return out


def build_spans(wavs: list[Path], scenes: list[str], gap_ms: int, cfg: dict) -> list[dict]:
    """Tile the mix timeline into spans of identical (bed, level, reverb)."""
    spans, t = [], 0.0
    for wav, scene in zip(wavs, scenes):
        dur = _wav_dur(wav)
        rule = match_scene(scene, cfg)
        key = (rule.get("bed"), rule.get("level", 0), rule.get("reverb"))
        if spans and spans[-1]["key"] == key:
            spans[-1]["end"] = t + dur
        else:
            spans.append({"key": key, "bed": rule.get("bed"), "level": rule.get("level", 0),
                          "reverb": rule.get("reverb"), "scene": scene, "start": t, "end": t + dur})
        t += dur + gap_ms / 1000
    return spans


def apply_ambience(voice_wav: Path, scenes: list[str], wavs: list[Path],
                   gap_ms: int, out: Path, assets: Path = ASSETS) -> Path:
    """Mix per-scene beds + room reverb under the voice track. `scenes` aligns with `wavs`."""
    cfg = load_map(assets / "scene-map.json")
    spans = build_spans(wavs, scenes, gap_ms, cfg)
    presets = cfg.get("reverb_presets", {})
    duck = cfg.get("duck", {})

    missing = {s["bed"] for s in spans if s["bed"] and not (assets / "ambience" / s["bed"]).exists()}
    for bed in sorted(missing):
        print(f"ambience: bed missing ({bed}) -> dry voice for those spans")
    for s in spans:
        if s["bed"] in missing:
            s["bed"], s["level"] = None, 0.0

    if all(not s["bed"] and not s["reverb"] for s in spans):
        print("ambience: everything dry, skipped")
        return voice_wav

    work = out.parent / ".amb_tmp"
    work.mkdir(parents=True, exist_ok=True)
    # 1. voice track with per-scene reverb
    voice_fx = voice_wav
    if any(s["reverb"] for s in spans):
        parts = []
        for n, s in enumerate(spans):
            p = work / f"v{n}.wav"
            fx = presets.get(s["reverb"] or "", "")
            af = f"-af {fx}" if fx else ""
            subprocess.run(f"ffmpeg -y -loglevel error -i {voice_wav} -ss {s['start']:.3f} "
                           f"-to {s['end']:.3f} {af} {p}".split(), check=True)
            parts.append(p)
        voice_fx = work / "voice_fx.wav"
        _concat_files(parts, voice_fx)
    # 2. bed track, looped per span, crossfaded at boundaries
    bed_slices = []
    for n, s in enumerate(spans):
        if not s["bed"]:
            continue
        p = work / f"b{n}.wav"
        subprocess.run(["ffmpeg", "-y", "-loglevel", "error", "-stream_loop", "-1",
                        "-i", str(assets / "ambience" / s["bed"]),
                        "-t", f"{s['end'] - s['start']:.3f}",
                        "-af", f"volume={s['level']},aformat=sample_rates=48000:channel_layouts=mono",
                        str(p)], check=True)
        bed_slices.append((p, s))
    bed_mix = work / "bed.wav"
    _join_beds(bed_slices, bed_mix, _wav_dur(voice_fx),
               starts=[s["start"] for _, s in bed_slices])
    # 3. duck bed under voice, mix
    subprocess.run(["ffmpeg", "-y", "-loglevel", "error", "-i", str(voice_fx), "-i", str(bed_mix),
                    "-filter_complex",
                    f"[1:a][0:a]sidechaincompress=threshold={duck.get('threshold', 0.02)}"
                    f":ratio={duck.get('ratio', 6)}:attack={duck.get('attack', 20)}"
                    f":release={duck.get('release', 400)}[duck];[0:a][duck]amix=inputs=2:normalize=0[a]",
                    "-map", "[a]", str(out)], check=True)
    for s in spans:
        tag = s["bed"] or "dry"
        print(f"ambience [{s['start']:.0f}-{s['end']:.0f}s] {s['scene'] or '?'} -> {tag}"
              + (f" + {s['reverb']}" if s["reverb"] else ""))
    for f in work.glob("*.wav"):
        f.unlink(missing_ok=True)
    for f in work.glob("*.txt"):
        f.unlink(missing_ok=True)
    return out


def _concat_files(parts: list[Path], out: Path) -> None:
    lst = out.parent / "parts.txt"
    # absolute paths: concat demuxer resolves relative ones against the playlist dir
    lst.write_text("".join(f"file '{p.resolve()}'\n" for p in parts))
    subprocess.run(["ffmpeg", "-y", "-loglevel", "error", "-f", "concat", "-safe", "0",
                    "-i", str(lst), "-c:a", "pcm_s16le", str(out)], check=True)
    lst.unlink(missing_ok=True)


def _join_beds(slices: list[tuple[Path, dict]], out: Path, total: float,
               starts: list[float]) -> None:
    """Place bed slices at exact scene offsets (adelay+amix, no drift) with tiny
    edge fades to avoid clicks."""
    if not slices:
        subprocess.run(["ffmpeg", "-y", "-loglevel", "error", "-f", "lavfi",
                        "-i", f"anullsrc=r=48000:cl=mono:d={total:.3f}", str(out)], check=True)
        return
    cmd = ["ffmpeg", "-y", "-loglevel", "error"]
    fc = []
    for n, ((p, s), st) in enumerate(zip(slices, starts)):
        cmd += ["-i", str(p)]
        dur = s["end"] - s["start"]
        ms = int(st * 1000)
        fc.append(f"[{n}:a]afade=t=in:st=0:d=0.3,afade=t=out:st={max(0.0, dur - 0.3):.3f}:d=0.3,"
                  f"adelay={ms}|{ms}[s{n}]")
    labels = "".join(f"[s{n}]" for n in range(len(slices)))
    fc.append(f"{labels}amix=inputs={len(slices)}:normalize=0,"
              f"apad=whole_dur={total:.3f},aformat=sample_rates=48000:channel_layouts=mono[out]")
    subprocess.run(cmd + ["-filter_complex", ";".join(fc), "-map", "[out]",
                          "-t", f"{total:.3f}", str(out)], check=True)


if __name__ == "__main__":  # ponytail: scene-match + span tiling self-checks, no framework
    import tempfile

    cfg = load_map()
    assert match_scene("street-day-book-discovery", cfg)["bed"] == "market-crowd.mp3"
    assert match_scene("courtyard-evening", cfg)["bed"] == "night-crickets.mp3"
    assert match_scene("courtyard-rain-day", cfg)["bed"] == "rain-light.mp3"
    assert match_scene("qingshan-sect-peak", cfg)["bed"] == "mountain-wind.mp3"
    assert match_scene("great-hall-day", cfg)["reverb"] == "hall"
    assert match_scene("something-unknown-xyz", cfg)["bed"] is None
    with tempfile.TemporaryDirectory() as d:
        a, b = Path(d) / "a.wav", Path(d) / "b.wav"
        for p in (a, b):  # 1s 48k mono silence stand-ins
            import wave as _w
            with _w.open(str(p), "wb") as w:
                w.setnchannels(1)
                w.setsampwidth(2)
                w.setframerate(48000)
                w.writeframes(b"\x00" * 48000 * 2)
        spans = build_spans([a, b], ["street-day", "night-x"], 300, cfg)
        assert len(spans) == 2 and abs(spans[1]["start"] - 1.3) < 0.01, spans
        spans2 = build_spans([a, b], ["street-day", "street-day"], 0, cfg)
        assert len(spans2) == 1 and abs(spans2[0]["end"] - 2.0) < 0.01, spans2
    print("ambience demo OK")
