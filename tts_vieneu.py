"""Local TTS via VieNeu-TTS v3 Turbo (48 kHz, torch-free ONNX on CPU).

Accent policy: Central/South presets ONLY — Northern voices are excluded.
Roster source: tts.list_preset_voices() labels "Giới tính · Vùng · Phong cách".

NOTE: Xuân Vĩnh is deliberately omitted — the SDK labels it Bắc while the
model card and forks call it Southern. Unanimous voices only below.
"""
from __future__ import annotations

import json
import os
from pathlib import Path

def tts_host() -> str:
    """Remote worker URL, e.g. http://gpu-box:8818. Empty = render locally."""
    return os.environ.get("TTS_HOST", "").rstrip("/")

SAMPLE_RATE = 48000

MALE_VOICES = ["Thái Sơn", "Đức Trí", "Adam", "Minh Triết", "Quang Sơn"]
FEMALE_VOICES = ["Thục Đoan", "Mỹ Duyên", "Thùy Dung", "Kim Thanh", "Ngọc Trân"]
ALLOWED_VOICES = set(MALE_VOICES + FEMALE_VOICES)

DEFAULT_CAST = {
    "Narrator": "Đức Trí",        # Nam, đọc truyện — audiobook voice
    "Dịch Phong": "Thái Sơn",      # Nam, kể chuyện — male lead
    "Lạc Lan Tuyết": "Thục Đoan",  # Nam, kể chuyện — cold female lead
    "Doãn Lạc Ly": "Mỹ Duyên",     # Nam, đọc truyện — youngest-sounding, verify by ear
    "Chủ hàng sát vách": "Quang Sơn",  # Trung — distinct central accent for the neighbor
}

_tts = None


def engine():
    global _tts
    if _tts is None:
        from vieneu import Vieneu

        _tts = Vieneu()  # int8 CPU default; auto-downloads weights on first run
    return _tts


def assert_allowed(cast: dict) -> None:
    # Built-in presets carry "Name — Giới tính · Vùng · ..." labels; user-enrolled
    # clones have bare labels (voice==label), so accept those while still blocking
    # Northern presets.
    customs = {v for label, v in engine().list_preset_voices() if label == v}
    bad = {k: v for k, v in cast.items() if v not in ALLOWED_VOICES and v not in customs}
    if bad:
        raise SystemExit(f"non Central/South voice in cast (policy): {bad}")


def synth_to_wav(text: str, voice: str, dest: Path, temperature: float = 0.8,
                 silence_p: float = 0.15) -> None:
    """temperature: higher = more expressive variance, less stable (0.7 calm .. 0.95 hot).
    silence_p: pause-insertion rate — higher breathes more drama into pacing.
    Inline tags [cười]/[thở dài]/[hắng giọng] in text render as chuckle/sigh/throat-clear.
    Set TTS_HOST=http://gpu-box:8818 to render on a remote worker instead."""
    dest.parent.mkdir(parents=True, exist_ok=True)
    if tts_host():
        _synth_remote(text, voice, dest, temperature, silence_p)
        return
    tts = engine()
    tts.save(tts.infer(text, voice=voice, temperature=temperature, silence_p=silence_p), str(dest))


def _synth_remote(text: str, voice: str, dest: Path, temperature: float, silence_p: float) -> None:
    import urllib.request

    body = json.dumps({"text": text, "voice": voice,
                        "temperature": temperature, "silence_p": silence_p}).encode()
    req = urllib.request.Request(f"{tts_host()}/infer", data=body,
                                 headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=900) as r:
            if r.status != 200:
                raise SystemExit(f"TTS worker error: {r.read()[:200]}")
            dest.write_bytes(r.read())
    except OSError as e:
        raise SystemExit(f"cannot reach TTS worker at {tts_host()} ({e})") from e


def list_voices() -> list[tuple[str, str]]:
    if tts_host():
        import urllib.request

        with urllib.request.urlopen(f"{tts_host()}/voices", timeout=30) as r:
            return [tuple(v) for v in json.loads(r.read().decode())]
    return engine().list_preset_voices()


if __name__ == "__main__":  # ponytail: roster self-check, no framework
    voices = list_voices()
    assert len(voices) >= 20, voices
    assert any("Trung" in label for label, _ in voices), voices
    print(f"vieneu voices OK ({len(voices)} presets)")
