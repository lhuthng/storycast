"""Multi-voice TTS + concat. Gemini path groups lines to save quota; local path is per-run."""
from __future__ import annotations

import json
import os
import shutil
import struct
import subprocess
import wave
from datetime import datetime, timezone
from pathlib import Path

SAMPLE_RATE = 24000
SEP = "\n\n\n"  # paragraph breaks -> pause the model reliably leaves for silence-splitting
GROUP_CHAR_CAP = 6000  # far under the 8192-token TTS input limit
GROUP_LINE_CAP = 8  # long groups split unreliably; sub-groups stay cuttable

# Gendered by ear (Google's labels like "firm"/"breezy" don't encode gender —
# Kore sounds female despite "firm"). When unsure, run `preview` and listen.
MALE_VOICES = ["Orus", "Charon", "Fenrir", "Algenib", "Gacrux", "Alnilam"]
FEMALE_VOICES = ["Vindemiatrix", "Leda", "Aoede", "Callirrhoe", "Despina", "Sulafat", "Achernar", "Kore"]
NEUTRAL_VOICES = ["Schedar", "Puck", "Erinome", "Rasalgethi"]

# Sensible Vietnamese audiobook defaults; cast.json overrides everything.
DEFAULT_CAST = {
    "Narrator": "Charon",       # male, informative
    "Dịch Phong": "Orus",        # adult male lead, firm but not stern
    "Lạc Lan Tuyết": "Vindemiatrix",  # cold young female
    "Doãn Lạc Ly": "Leda",       # youthful little girl
}

_MALE_HINTS = ("male", "nam", "ông", "anh", "trai", "đàn ông", "boy", "man", "lão")
_FEMALE_HINTS = ("female", "nữ", "cô", "chị", "tỷ", "muội", "gái", "girl", "woman", "lady", "bà", "muội tử")


def atomic_write(path: str | Path, text: str) -> None:
    """Crash-safe write (tmp + rename) so queue pollers never see partial files."""
    p = Path(path)
    p.parent.mkdir(parents=True, exist_ok=True)
    tmp = p.with_name(f".{p.name}.tmp")
    tmp.write_text(text, encoding="utf-8")
    tmp.replace(p)


def load_cast(script_path: str = "data/script.json", cast_path: str = "data/cast.json",
               defaults: dict | None = None, male_pool: list | None = None,
               female_pool: list | None = None, neutral_pool: list | None = None,
               save: bool = True) -> dict:
    defaults = defaults if defaults is not None else DEFAULT_CAST
    male_pool = male_pool if male_pool is not None else MALE_VOICES
    female_pool = female_pool if female_pool is not None else FEMALE_VOICES
    neutral_pool = neutral_pool if neutral_pool is not None else NEUTRAL_VOICES
    hints: dict[str, str] = {}
    speakers = ["Narrator"]
    if Path(script_path).exists():
        data = json.loads(Path(script_path).read_text(encoding="utf-8"))
        for c in data.get("characters", []):  # legacy shape (pre-bible scripts)
            hints[c["name"]] = c.get("voice_hint", "").lower()
            if c["name"] not in speakers:
                speakers.append(c["name"])
        for name in data.get("roster", []):
            if name not in speakers:
                speakers.append(name)
        if Path("data/bible.json").exists():  # voice hints live in the bible now
            bible = json.loads(Path("data/bible.json").read_text(encoding="utf-8"))
            for c in bible.get("characters", []):
                hints.setdefault(c["name"], c.get("voice_hint", "").lower())
        for s in data.get("segments", []):
            if s["speaker"] not in speakers:
                speakers.append(s["speaker"])
    cast = dict(defaults)
    if Path(cast_path).exists():
        cast.update(json.loads(Path(cast_path).read_text(encoding="utf-8")))
    used = list(cast.values())
    chapter_voices = {cast[n] for n in speakers if n in cast}
    for name in speakers:
        if name not in cast:
            hint = hints.get(name, "")
            # female first: "female" contains substring "male" — order matters
            pool = (
                female_pool if any(k in hint for k in _FEMALE_HINTS)
                else male_pool if any(k in hint for k in _MALE_HINTS)
                else neutral_pool
            )
            # least-used globally, deprioritizing voices already speaking this chapter
            cast[name] = min(pool, key=lambda v: (v in chapter_voices, used.count(v), pool.index(v)))
            used.append(cast[name])
            chapter_voices.add(cast[name])
    Path(cast_path).parent.mkdir(parents=True, exist_ok=True)
    if save:
        atomic_write(cast_path, json.dumps(cast, ensure_ascii=False, indent=1))
    return cast


def write_wav(pcm: bytes, path: Path, rate: int = SAMPLE_RATE) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with wave.open(str(path), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(rate)
        w.writeframes(pcm)


def silent_wav(path: Path, seconds: float = 0.5, rate: int = SAMPLE_RATE) -> None:
    write_wav(struct.pack(f"<{int(rate * seconds)}h", *([0] * int(rate * seconds))), path, rate)


def _expected_wavs(segments: list[dict], cast: dict, seg_dir: str, local: bool) -> list[Path]:
    """Cache filenames for a segment list (shared by assemble + segments_complete)."""
    if local:
        wavs = []
        for run in _runs(segments):
            a, b = run["idx"][0], run["idx"][-1]
            tag = f"{a:04d}" if a == b else f"{a:04d}-{b:04d}"
            wavs.append(Path(seg_dir) / f"{tag}_{cast[run['speaker']]}.wav")
        return wavs
    return [Path(seg_dir) / f"{i:04d}_{cast[s['speaker']]}.wav" for i, s in enumerate(segments)]


def segments_complete(script_path: str, cast_path: str, seg_dir: str,
                      engine: str = "gemini", limit: int = 0) -> bool:
    """True if every expected segment wav exists — the renderer's skip check and
    the merger's ready check. Read-only (never assigns or saves cast voices)."""
    try:
        data = json.loads(Path(script_path).read_text(encoding="utf-8"))
        segments = data["segments"][:limit] if limit else data["segments"]
        if not segments:
            return False
    except (OSError, ValueError, KeyError):
        return False
    local = engine == "vieneu"
    if local:
        import tts_vieneu as vn

        cast = load_cast(script_path, cast_path, vn.DEFAULT_CAST, vn.MALE_VOICES, vn.FEMALE_VOICES, vn.MALE_VOICES,
                         save=False)
    else:
        cast = load_cast(script_path, cast_path, save=False)
    try:
        wavs = _expected_wavs(segments, cast, seg_dir, local)
    except KeyError:
        return False
    return all(w.exists() and w.stat().st_size > 1000 for w in wavs)


def render_segments(
    script_path: str = "data/script.json",
    cast_path: str = "data/cast.json",
    seg_dir: str = "data/audio/segments",
    limit: int = 0,
    dry_run: bool = False,
    engine: str = "gemini",
    model_order: list[str] | None = None,
) -> list[Path]:
    """Queue stage 2: TTS every segment into seg_dir cache. No merging — see assemble()."""
    local = engine == "vieneu"
    if local:
        import tts_vieneu as vn

        cast = load_cast(script_path, cast_path, vn.DEFAULT_CAST, vn.MALE_VOICES, vn.FEMALE_VOICES, vn.MALE_VOICES)
        vn.assert_allowed(cast)
        seg_rate = vn.SAMPLE_RATE
    else:
        cast = load_cast(script_path, cast_path)
        seg_rate = SAMPLE_RATE
    data = json.loads(Path(script_path).read_text(encoding="utf-8"))
    segments = data["segments"][:limit] if limit else data["segments"]
    manifest = _Manifest()
    for f in Path(seg_dir).glob(".*.wav"):  # orphaned group/fallback temps from killed runs
        f.unlink(missing_ok=True)
    if dry_run:
        wavs = _dry_runs(segments, cast, seg_dir, seg_rate)
    elif local:
        wavs = _synth_runs(segments, cast, seg_dir, manifest)
    else:
        wavs = _synth_groups(segments, cast, seg_dir, manifest, model_order)
    _clean_stale_segments(seg_dir, {w.name for w in wavs}, {f"{i:04d}" for i in range(len(segments))})
    manifest.summarize()
    return wavs


def assemble(
    script_path: str = "data/script.json",
    cast_path: str = "data/cast.json",
    seg_dir: str = "data/audio/segments",
    out: str = "output/ch01.wav",
    limit: int = 0,
    gap_ms: int = 300,
    ambience: bool = False,
    speed: float = 1.0,
    engine: str = "gemini",
) -> Path:
    """Queue stage 3: concat cached segments -> chapter file. Rerunnable, no TTS calls."""
    local = engine == "vieneu"
    if local:
        import tts_vieneu as vn

        cast = load_cast(script_path, cast_path, vn.DEFAULT_CAST, vn.MALE_VOICES, vn.FEMALE_VOICES, vn.MALE_VOICES,
                         save=False)
    else:
        cast = load_cast(script_path, cast_path, save=False)
    data = json.loads(Path(script_path).read_text(encoding="utf-8"))
    segments = data["segments"][:limit] if limit else data["segments"]
    try:
        wavs = _expected_wavs(segments, cast, seg_dir, local)
    except KeyError as e:
        raise SystemExit(f"cast has no voice for {e}: run the render stage first")
    missing = [w.name for w in wavs if not (w.exists() and w.stat().st_size > 1000)]
    if missing:
        raise SystemExit(f"{len(missing)} segments missing in {seg_dir} "
                         f"(e.g. {missing[0]}): run the voices stage first")
    out_path = Path(out)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    _concat_wavs(wavs, out_path, gap_ms=gap_ms)
    if gap_ms:
        print(f"gap {gap_ms}ms x{len(wavs)} speech turns")
    if ambience:
        from ambience import apply_ambience, run_scenes

        scenes = (run_scenes(segments, _runs(segments)) if local
                  else [(s.get("scene") or "") for s in segments])
        amb_out = out_path.with_name(out_path.stem + "-amb" + out_path.suffix)
        out_path = apply_ambience(out_path, scenes, wavs, gap_ms, amb_out)
    if speed != 1.0:  # tempo on the mix only; segment cache stays 1.0x for re-renders
        sped = out_path.with_name(out_path.stem + f"x{speed:g}" + out_path.suffix)
        subprocess.run(["ffmpeg", "-y", "-loglevel", "error", "-i", str(out_path),
                        "-filter:a", f"atempo={speed:g}", str(sped)], check=True)
        print(f"speed {speed:g}x -> {sped}")
        out_path = sped
    mp3 = out_path.with_suffix(".mp3")
    if shutil.which("ffmpeg"):
        subprocess.run(["ffmpeg", "-y", "-loglevel", "error", "-i", str(out_path), str(mp3)], check=True)
        print(f"done -> {out_path} + {mp3}")
    else:
        print(f"done -> {out_path} (install ffmpeg for mp3)")
    return out_path


def synthesize(
    script_path: str = "data/script.json",
    cast_path: str = "data/cast.json",
    seg_dir: str = "data/audio/segments",
    out: str = "output/ch01.wav",
    limit: int = 0,
    dry_run: bool = False,
    engine: str = "gemini",
    model_order: list[str] | None = None,
    speed: float = 1.0,
    gap_ms: int = 300,
    ambience: bool = False,
) -> Path:
    """One-shot: render_segments + assemble (same output as the old synthesize)."""
    render_segments(script_path, cast_path, seg_dir, limit, dry_run, engine, model_order)
    return assemble(script_path, cast_path, seg_dir, out, limit, gap_ms, ambience, speed, engine)


def _dry_runs(segments: list[dict], cast: dict, seg_dir: str, rate: int) -> list[Path]:
    print(f"{len(segments)} segments -> dry-run (silent, no API)")
    wavs = []
    for run in _runs(segments):
        a, b = run["idx"][0], run["idx"][-1]
        tag = f"{a:04d}" if a == b else f"{a:04d}-{b:04d}"
        dest = Path(seg_dir) / f"{tag}_{cast[run['speaker']]}.wav"
        silent_wav(dest, rate=rate)
        wavs.append(dest)
    return wavs


def _synth_runs(segments: list[dict], cast: dict, seg_dir: str, manifest: _Manifest) -> list[Path]:
    """Local engine: one free call per consecutive same-speaker run, acted by mood."""
    import tts_vieneu as vn

    runs = _runs(segments)
    print(f"{len(segments)} segments -> {len(runs)} local calls (runs batched)")
    wavs: list[Path] = []
    for n, run in enumerate(runs):
        voice = cast[run["speaker"]]
        a, b = run["idx"][0], run["idx"][-1]
        tag = f"{a:04d}" if a == b else f"{a:04d}-{b:04d}"
        dest = Path(seg_dir) / f"{tag}_{voice}.wav"
        wavs.append(dest)
        if dest.exists() and dest.stat().st_size > 1000:
            print(f"[{tag}] skip cached {run['speaker']} ({voice})")
            continue
        temp, sil = _mood_take(segments, run["idx"])
        print(f"[{tag}] {run['speaker']} ({voice}, t={temp} s={sil}): {segments[a]['text'][:60]}…")
        t0 = datetime.now(timezone.utc)
        vn.synth_to_wav(_run_text(segments, run["idx"]), voice, dest,
                        temperature=temp, silence_p=sil)
        manifest.add("vieneu-local", voice, run["idx"], t0, split=f"{len(run['idx'])}/{len(run['idx'])}",
                     note=f"t={temp} s={sil}")
    return wavs


# Mood -> (temperature, silence_p). Calm reads steady, hot moods swing wider and pause harder.
# ponytail: one table, no per-mood engine — retune values here, filenames unchanged.
MOOD_TAKE = {
    "neutral": (0.80, 0.15), "calm": (0.72, 0.12), "reflective": (0.75, 0.18),
    "grand": (0.85, 0.20), "sarcastic": (0.85, 0.12), "ironic": (0.85, 0.12),
    "amused": (0.90, 0.12), "excited": (0.92, 0.10), "smug": (0.88, 0.12),
    "happy": (0.90, 0.10), "sad": (0.85, 0.22), "angry": (0.90, 0.18),
    "cold": (0.70, 0.15), "stern": (0.72, 0.18), "urgent": (0.92, 0.08),
    "surprised": (0.90, 0.10), "shocked": (0.90, 0.20), "gossipy": (0.88, 0.10),
}


def _mood_take(segments: list[dict], idx: list[int]) -> tuple[float, float]:
    """Hottest mood in the run wins (one expressive line should lift the whole breath)."""
    best: tuple[float, float] = MOOD_TAKE["neutral"]
    for i in idx:
        cand = MOOD_TAKE.get(_mood_cluster(segments[i].get("mood") or "neutral"), MOOD_TAKE["neutral"])
        if cand[0] > best[0]:
            best = cand
    return best


class _Manifest:
    """JSONL audit trail: what rendered what, on which engine/model, and why it fell back."""

    def __init__(self) -> None:
        self.records: list[dict] = []
        self.run_id = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
        self.path = Path("output") / f"render-{self.run_id[:8]}.jsonl"
        self.path.parent.mkdir(parents=True, exist_ok=True)

    def add(self, engine: str, voice: str, idx: list[int], t0: datetime,
            split: str = "", note: str = "") -> None:
        rec = {
            "run": self.run_id, "ts": t0.isoformat(), "engine": engine, "voice": voice,
            "segments": [idx[0], idx[-1]], "n": len(idx), "split": split,
            "latency_s": round((datetime.now(timezone.utc) - t0).total_seconds(), 1), "note": note,
        }
        self.records.append(rec)
        with open(self.path, "a", encoding="utf-8") as f:  # write-through: survives kills
            f.write(json.dumps(rec, ensure_ascii=False) + "\n")

    def summarize(self) -> None:
        if not self.records:
            return
        by_engine: dict[str, int] = {}
        for r in self.records:
            by_engine[r["engine"]] = by_engine.get(r["engine"], 0) + 1
        print(f"render log -> {self.path} | calls: " +
              ", ".join(f"{e}×{n}" for e, n in sorted(by_engine.items())))


def _mood_cluster(mood: str) -> str:
    """Normalize free-form digest moods to a small acting vocabulary (saves quota)."""
    first = (mood or "neutral").split()[0].lower().strip(",.")
    return MOOD_NORM.get(first, first)


MOOD_NORM = {
    # flat narration -> neutral
    "flat": "neutral", "monotone": "neutral", "expository": "neutral",
    "narrative": "neutral", "matter-of-fact": "neutral", "mildly": "neutral",
    "observant": "neutral", "descriptive": "neutral",
    "indifferent": "neutral", "awkward": "neutral", "pragmatic": "neutral",
    "casual": "neutral",
    # calm family
    "serene": "calm", "unconcerned": "calm", "reflective": "calm",
    # amused family (dry humor beats)
    "ironic": "amused", "sarcastic": "amused",
    # low-energy -> sad
    "helpless": "sad", "resigned": "sad", "subdued": "sad", "pouting": "sad",
    # bright facets -> excited
    "amazed": "excited", "earnest": "excited", "playful": "excited",
    "pleading": "excited", "happy": "excited", "warm": "excited", "grand": "excited",
    "admiring": "excited",
    # smug / urgent / stern
    "satisfied": "smug", "self-satisfied": "smug", "hasty": "urgent",
    # icy authority -> cold
    "aloof": "cold", "arrogant": "cold", "stern": "cold", "strict": "cold",
    "annoyed": "angry",
}

SMALL_GROUP_CHARS = 200  # tiny groups join the speaker's largest group (interjections borrow tone)


def _groups(segments: list[dict]) -> list[dict]:
    """Group by (speaker, mood-cluster): one cloud call per group to save quota.

    Mood-cluster = first word of the digest mood ("lazy self-mocking" -> "lazy"),
    so one call never averages wildly different acting directions. Capped by chars.
    """
    groups = []
    for i, seg in enumerate(segments):
        key = (seg["speaker"], (seg.get("scene") or "").strip().lower(), _mood_cluster(seg.get("mood") or "neutral"))
        target = next((g for g in groups
                       if g["key"] == key and len(g["idx"]) < GROUP_LINE_CAP
                       and g["chars"] + len(seg["text"]) < GROUP_CHAR_CAP), None)
        if target is None:
            groups.append({"key": key, "speaker": seg["speaker"], "idx": [], "chars": 0})
            target = groups[-1]
        target["idx"].append(i)
        target["chars"] += len(seg["text"])
    # merge tiny groups (one-line interjections) into the speaker's largest group
    big: dict[str, dict] = {}
    for g in groups:
        if g["chars"] >= SMALL_GROUP_CHARS and g["chars"] > big.get(g["speaker"], {}).get("chars", 0):
            big[g["speaker"]] = g
    kept = []
    for g in groups:
        target = big.get(g["speaker"])
        if g["chars"] < SMALL_GROUP_CHARS and target is not None and target is not g:
            target["idx"].extend(g["idx"])
            target["chars"] += g["chars"]
        else:
            kept.append(g)
    for g in kept:
        g["idx"].sort()
    # re-split same-key groups into near-equal parts (avoids 1-line tails: 25->7+6+6+6, not 8+8+8+1)
    import math

    by_key: dict[tuple, list[int]] = {}
    order: list[tuple] = []
    for g in kept:
        if g["key"] not in by_key:
            by_key[g["key"]] = []
            order.append(g["key"])
        by_key[g["key"]].extend(g["idx"])
    speakers = {g["key"]: g["speaker"] for g in kept}
    final = []
    for key in order:
        idx = sorted(by_key[key])
        k = math.ceil(len(idx) / GROUP_LINE_CAP)
        size = math.ceil(len(idx) / k)
        for c in _chunks(idx, size):
            final.append({"key": key, "speaker": speakers[key], "idx": c,
                          "chars": sum(len(segments[i]["text"]) for i in c)})
    return final


def _group_direction(segments: list[dict], idx: list[int]) -> str:
    mood = segments[idx[0]].get("mood", "neutral")
    joined = SEP.join(segments[i]["text"] for i in idx)
    return f"Say {mood} in Vietnamese, pausing briefly between paragraphs:\n\n{joined}"


def _synth_groups(segments: list[dict], cast: dict, seg_dir: str,
                  manifest: _Manifest, model_order: list[str] | None) -> list[Path]:
    """Cloud path: grouped requests through the quota-aware router, cut back to lines."""
    from tts_router import AllExhausted
    from tts_router import request as _request

    groups = _groups(segments)
    print(f"{len(segments)} segments -> {len(groups)} cloud calls (voice+mood grouped)")
    wavs: list[Path] = [Path(seg_dir) / f"{i:04d}_{cast[s['speaker']]}.wav" for i, s in enumerate(segments)]
    cloud_off = False
    for n, grp in enumerate(groups):
        idx, voice = grp["idx"], cast[grp["speaker"]]
        if all(wavs[i].exists() and wavs[i].stat().st_size > 1000 for i in idx):
            print(f"[{n + 1}/{len(groups)}] skip cached {grp['speaker']} ×{len(idx)} ({voice})")
            continue
        if cloud_off:
            _local_fallback_lines(segments, idx, voice, wavs, seg_dir, manifest)
            continue
        try:
            _cloud_lines(segments, idx, voice, wavs, seg_dir, manifest, model_order)
        except AllExhausted as e:
            print(f"   cloud exhausted ({e}); remaining lines go local")
            cloud_off = True
            _local_fallback_lines(segments, idx, voice, wavs, seg_dir, manifest)
    return wavs


def _chunks(idx: list[int], n: int) -> list[list[int]]:
    return [idx[i:i + n] for i in range(0, len(idx), n)]


def _cloud_lines(segments: list[dict], idx: list[int], voice: str, wavs: list[Path],
                 seg_dir: str, manifest: _Manifest, model_order: list[str] | None) -> None:
    """Cascade: whole group -> chunks of 4 -> individual lines. Raises AllExhausted."""
    from tts_router import SHORT, AllExhausted
    from tts_router import request as _request

    t0 = datetime.now(timezone.utc)
    direction = _group_direction(segments, idx) if len(idx) > 1 else _line_direction(segments[idx[0]])
    pcm, model, events = _request(direction, voice, model_order)
    tag = SHORT.get(model, model)
    for ev in events:
        if "OK" not in ev:
            print(f"   router: {ev}")
    tmp = Path(seg_dir) / f".group_{idx[0]:04d}.wav"
    write_wav(pcm, tmp)
    try:
        if _cut_pieces(tmp, segments, idx, wavs):
            print(f"   {tag} · {segments[idx[0]]['speaker']} ×{len(idx)} · split OK")
            manifest.add(tag, voice, idx, t0, split=f"{len(idx)}/{len(idx)}")
            return
    finally:
        tmp.unlink(missing_ok=True)
    if len(idx) == 1:
        write_wav(pcm, wavs[idx[0]])  # nothing to cut on: keep whole audio as the line
        print(f"   {tag} · single line, kept whole")
        manifest.add(tag, voice, idx, t0, split="1/1")
    elif len(idx) > 4:
        print(f"   split ×{len(idx)} failed -> sub-groups of 4")
        for c in _chunks(idx, 4):
            _cloud_lines(segments, c, voice, wavs, seg_dir, manifest, model_order)
    else:
        print(f"   split ×{len(idx)} failed -> individual lines")
        for i in idx:
            _cloud_lines(segments, [i], voice, wavs, seg_dir, manifest, model_order)


def _cut_pieces(group_wav: Path, segments: list[dict], idx: list[int], wavs: list[Path]) -> bool:
    bounds = _split_group(group_wav, segments, idx)
    if bounds is None:
        return False
    for (s, e), i in zip(bounds, idx):
        _cut(group_wav, s, e, wavs[i])
    return True


def _line_direction(seg: dict) -> str:
    return f"Say {seg.get('mood', 'neutral')} in Vietnamese: {seg['text']}"


def _local_fallback_lines(segments: list[dict], idx: list[int], voice: str,
                          wavs: list[Path], seg_dir: str, manifest: _Manifest) -> None:
    """Cloud unavailable: render locally, resample 48k->24k so the chapter stays uniform."""
    import tts_vieneu as vn

    local_voice = _local_voice_for(segments[idx[0]]["speaker"])
    for i in idx:
        if wavs[i].exists() and wavs[i].stat().st_size > 1000:
            continue
        t0 = datetime.now(timezone.utc)
        tmp48 = Path(seg_dir) / f".fb_{i:04d}.wav"
        vn.synth_to_wav(segments[i]["text"], local_voice, tmp48)
        subprocess.run(["ffmpeg", "-y", "-loglevel", "error", "-i", str(tmp48),
                        "-ar", "24000", "-ac", "1", "-c:a", "pcm_s16le", str(wavs[i])], check=True)
        tmp48.unlink(missing_ok=True)
        print(f"   local-fallback [{i:04d}] ({local_voice}, resampled 24k)")
        manifest.add("vieneu-local-fallback", voice, [i], t0, split="1/1",
                     note=f"cloud exhausted; spoken by {local_voice}")


def _local_voice_for(speaker: str) -> str:
    import tts_vieneu as vn

    try:
        return vn.DEFAULT_CAST[speaker]
    except KeyError:
        return vn.MALE_VOICES[0]


def _split_group(group_wav: Path, segments: list[dict], idx: list[int]) -> list[tuple[float, float]] | None:
    for noise, mind in (("-35dB", 0.35), ("-30dB", 0.30)):
        bounds = _cut_bounds(group_wav, noise, mind, len(idx))
        if bounds and _plausible(bounds, segments, idx):
            return bounds
    return None


def _cut_bounds(group_wav: Path, noise: str, mind: float, n: int) -> list[tuple[float, float]] | None:
    r = subprocess.run(["ffmpeg", "-hide_banner", "-i", str(group_wav),
                        "-af", f"silencedetect=noise={noise}:d={mind}",
                        "-f", "null", "-"], capture_output=True, text=True)
    ivals: list[tuple[float, float]] = []
    start = None
    for line in r.stderr.splitlines():
        if "silence_start:" in line:
            start = float(line.split("silence_start:")[1].split()[0])
        elif "silence_end:" in line and start is not None:
            ivals.append((start, float(line.split("silence_end:")[1].split()[0])))
            start = None
    if len(ivals) < n - 1:
        return None
    with wave.open(str(group_wav), "rb") as w:
        dur = w.getnframes() / w.getframerate()
    cuts = sorted((a + b) / 2 for a, b in
                  sorted(ivals, key=lambda iv: iv[1] - iv[0], reverse=True)[:n - 1])
    return list(zip([0.0] + cuts, cuts + [dur]))


def _plausible(bounds: list[tuple[float, float]], segments: list[dict], idx: list[int]) -> bool:
    """Each piece's duration must roughly match its text length (catches mid-line cuts)."""
    for (s, e), i in zip(bounds, idx):
        chars = max(1, len(segments[i]["text"]))
        d = e - s
        if not (chars / 60 < d < chars / 2 + 4):
            return False
    return True


def _cut(src: Path, start: float, end: float, dest: Path) -> None:
    dest.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run(["ffmpeg", "-y", "-loglevel", "error", "-i", str(src),
                    "-ss", f"{start:.3f}", "-to", f"{end:.3f}",
                    "-c:a", "pcm_s16le", "-ar", "24000", "-ac", "1", str(dest)], check=True)
def _runs(segments: list[dict]) -> list[dict]:
    """Group consecutive same-speaker segments: one TTS call per run."""
    runs = []
    for i, seg in enumerate(segments):
        if runs and runs[-1]["speaker"] == seg["speaker"]:
            runs[-1]["idx"].append(i)
        else:
            runs.append({"speaker": seg["speaker"], "idx": [i]})
    return runs


def _run_direction(segments: list[dict], idx: list[int]) -> str:
    mood = segments[idx[0]].get("mood", "neutral")
    joined = " ".join(segments[i]["text"] for i in idx)
    return f"Say {mood} in Vietnamese: {joined}"


def _run_text(segments: list[dict], idx: list[int]) -> str:
    """Plain joined text for local engines (v3 style is baked into the voice, not the prompt)."""
    return " ".join(segments[i]["text"] for i in idx)


def _clean_stale_segments(seg_dir: str, keep: set[str], run_idx: set[str]) -> None:
    d = Path(seg_dir)
    if not d.exists():
        return
    for f in d.glob("*.wav"):
        idx = f.name.split("-")[0].split("_")[0]
        # only same-index leftovers from THIS run (old voice or old naming); never touch other indices
        # (a --limit run must not wipe cached segments beyond its range)
        if idx in run_idx and f.name not in keep:
            f.unlink()


def preview_sample(name: str, voice: str, out_dir: str = "output/voice-preview", dry_run: bool = False,
                   engine: str = "gemini") -> Path:
    """One short sample per cast voice so gender/fit can be checked by ear."""
    d = Path(out_dir)
    d.mkdir(parents=True, exist_ok=True)
    dest = d / f"{name}.{voice}.wav"
    if not (dest.exists() and dest.stat().st_size > 1000):
        if dry_run:
            silent_wav(dest, rate=48000 if engine == "vieneu" else SAMPLE_RATE)
        elif engine == "vieneu":
            import tts_vieneu as vn

            vn.synth_to_wav(f"Xin chào! Tôi là {name}.", voice, dest)
        else:
            from tts_router import SHORT
            from tts_router import request as _request

            pcm, model, events = _request(f"Say warmly in Vietnamese: Xin chào! Tôi là {name}.", voice)
            for ev in events:
                if "OK" not in ev:
                    print(f"   router: {ev}")
            print(f"   via {SHORT.get(model, model)}")
            write_wav(pcm, dest)
    mp3 = dest.with_suffix(".mp3")
    if shutil.which("ffmpeg"):
        subprocess.run(["ffmpeg", "-y", "-loglevel", "error", "-i", str(dest), str(mp3)], check=True)
    print(f"{name} ({voice}) -> {mp3 if mp3.exists() else dest}")
    return dest


def preview_all(script_path: str = "data/script.json", cast_path: str = "data/cast.json",
                out_dir: str = "output/voice-preview", dry_run: bool = False,
                engine: str = "gemini") -> None:
    if engine == "vieneu":
        import tts_vieneu as vn

        cast = load_cast(script_path, cast_path, vn.DEFAULT_CAST, vn.MALE_VOICES, vn.FEMALE_VOICES, vn.MALE_VOICES)
        vn.assert_allowed(cast)
    else:
        cast = load_cast(script_path, cast_path)
    for name, voice in cast.items():
        preview_sample(name, voice, out_dir, dry_run, engine)


def _concat_wavs(files: list[Path], out: Path, gap_ms: int = 0) -> None:
    params = None
    frames = b""
    gap = b""
    for f in files:
        with wave.open(str(f), "rb") as w:
            p = (w.getnchannels(), w.getsampwidth(), w.getframerate())
            params = params or p
            assert p == params, f"{f}: {p} != {params} (mixed engines/rates — use per-engine seg dirs)"
            if gap_ms and frames:
                if not gap:
                    n = int(p[2] * gap_ms / 1000)
                    gap = struct.pack(f"<{n}h", *([0] * n)) * p[0]
                frames += gap
            frames += w.readframes(w.getnframes())
    assert params, "no segments"
    with wave.open(str(out), "wb") as w:
        w.setnchannels(params[0])
        w.setsampwidth(params[1])
        w.setframerate(params[2])
        w.writeframes(frames)
    assert out.stat().st_size > 44, "empty output"


if __name__ == "__main__":  # ponytail: concat + grouping + silence-split self-checks, no framework
    import math
    import tempfile

    with tempfile.TemporaryDirectory() as d:
        a, b = Path(d) / "a.wav", Path(d) / "b.wav"
        silent_wav(a, 0.1)
        silent_wav(b, 0.1)
        o = Path(d) / "o.wav"
        _concat_wavs([a, b], o)
        with wave.open(str(o), "rb") as w:
            assert w.getnframes() == 2 * int(SAMPLE_RATE * 0.1), w.getnframes()

        # grouping: same speaker+mood clusters, char cap splits
        segs = [
            {"speaker": "A", "text": "x" * 100, "mood": "lazy calm"},
            {"speaker": "A", "text": "y" * 100, "mood": "lazy amused"},
            {"speaker": "A", "text": "z" * 100, "mood": "cold angry"},
            {"speaker": "B", "text": "w" * 100, "mood": "cold angry"},
        ]
        gs = _groups(segs)
        # non-contiguous same (speaker, mood-cluster) lines merge into one call
        assert [(g["speaker"], [i for i in g["idx"]]) for g in gs] == [("A", [0, 1, 2]), ("B", [3])], gs

        # silence-split: tone - long pause - tone must cut into exactly 2 plausible pieces
        import array

        sr = SAMPLE_RATE

        def tone(freq: float, n: int) -> bytes:
            return array.array("h", (int(9999 * math.sin(2 * math.pi * freq * t / sr)) for t in range(n))).tobytes()

        g = Path(d) / "group.wav"
        write_wav(tone(440, sr // 2) + b"\x00" * (sr * 2) + tone(660, sr // 2), g)
        fake = [{"text": "chao ban nhe"}, {"text": "tam biet nhe"}]
        bounds = _split_group(g, fake, [0, 1])
        assert bounds is not None and len(bounds) == 2, bounds
        assert _plausible(bounds, fake, [0, 1])
        assert not _plausible([(0.0, 0.05), (0.05, 3.0)], fake, [0, 1])  # absurd cut rejected
        c0 = Path(d) / "c0.wav"
        _cut(g, *bounds[0], c0)
        with wave.open(str(c0), "rb") as w:
            assert abs(w.getnframes() / sr - (bounds[0][1] - bounds[0][0])) < 0.05
    print("synthesize demo OK")
