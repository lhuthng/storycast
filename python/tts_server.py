"""Remote TTS worker: run on the machine with the model/GPU, point clients at it.

    python tts_server.py [--port 8818]

Clients set TTS_HOST=http://<this-host>:8818 (or --tts-host) and every
synth/preview/clone call runs here instead of locally. Stdlib only, no auth —
LAN use or SSH tunnel only.

NOTE: custom enrolled voices (Suneo, Nobita, ...) live in THIS machine's voice
store. Copy refs/*.wav over and enroll once per name:
    python -c "import tts_vieneu as vn; tts=vn.engine(); tts.add_voice('Suneo','refs/suneo.wav'); tts.save_voices()"
"""
from __future__ import annotations

import argparse
import io
import json
import wave
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import tts_vieneu as vn

# Auditioning line for /preview. Fixed on purpose: a voice sample is only
# comparable to another voice sample if both say the same thing.
PREVIEW_TEXT = "Xin chào, đây là giọng đọc thử của bộ truyện."


def _split_label(label: str) -> "tuple[str, list[str]]":
    """`"Thái Sơn — Nam · Trung · Kể chuyện"` -> name + fields.

    Enrolled clones carry a bare label (no separator); that is exactly how the
    SDK distinguishes them from shipped presets.
    """
    for sep in ("—", "–", " - "):
        if sep in label:
            name, _, rest = label.partition(sep)
            return name.strip(), [f.strip() for f in rest.split("·") if f.strip()]
    return label.strip(), []


def _gender(field: str) -> str:
    # Female first: "female" contains "male". Bare "nu" is deliberately not
    # matched — it would make "neutral" report as female.
    f = field.lower()
    if "female" in f or "nữ" in f:
        return "female"
    if "male" in f or "nam" in f:
        return "male"
    if "neutral" in f or "trung tính" in f:
        return "neutral"
    return "unknown"


def _accent(field: str) -> str:
    # Fields are positional, which is the only way to read "Nam": it means
    # *male* in the gender slot and *South* in the accent slot.
    f = field.lower()
    if "bắc" in f or "bac" in f:
        return "Northern"
    if "trung" in f:
        return "Central"
    if "nam" in f:
        return "South"
    return "unknown"


def roster() -> "list[dict]":
    """Structured roster: every voice with the metadata an operator picks by."""
    out = []
    for label, vid in vn.engine().list_preset_voices():
        name, fields = _split_label(label)
        name = name or vid
        enrolled = label == vid
        gender = _gender(fields[0]) if fields else "unknown"
        if len(fields) > 1:
            accent = _accent(fields[1])
        else:
            # No label to read: fall back to the policy guarantee, which is a
            # true statement about every preset this engine may use.
            accent = "unknown" if enrolled else "Central/South"
        out.append({
            "name": name,
            "gender": gender,
            "accent": accent,
            "language": "vi-VN",
            "style": " · ".join(fields[2:]) if len(fields) > 2 else "",
            "enrolled": enrolled,
            "allowed": enrolled or name in vn.ALLOWED_VOICES,
        })
    return out


def _wav_bytes(pcm: "object") -> bytes:
    import numpy as np

    buf = io.BytesIO()
    with wave.open(buf, "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(vn.SAMPLE_RATE)
        w.writeframes((np.clip(np.asarray(pcm, dtype=np.float32), -1, 1) * 32767).astype("<i2").tobytes())
    return buf.getvalue()


class Handler(BaseHTTPRequestHandler):
    server_version = "TTSWorker/1.0"

    def _json(self, obj: object, code: int = 200) -> None:
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:  # noqa: N802
        if self.path == "/health":
            return self._json({"ok": True})
        if self.path == "/voices":
            return self._json(vn.engine().list_preset_voices())
        if self.path == "/roster":
            # Structured form of /voices: name + gender + accent + style +
            # language, so a client never has to parse SDK label strings.
            try:
                return self._json(roster())
            except Exception as e:  # noqa: BLE001 — report, don't kill the worker
                return self._json({"error": str(e)[:300]}, 500)
        if self.path == "/policy":
            # Offline authority for accent policy + roster so Rust never
            # duplicates TTS knowledge (fallback lives in bm-core/voices.rs).
            return self._json({
                "engine": "vieneu",
                "sample_rate": vn.SAMPLE_RATE,
                "male_voices": vn.MALE_VOICES,
                "female_voices": vn.FEMALE_VOICES,
                "allowed_voices": sorted(set(vn.MALE_VOICES) | set(vn.FEMALE_VOICES)),
                "default_cast": vn.DEFAULT_CAST,
            })
        return self._json({"error": "unknown path"}, 404)

    def _read_json(self) -> dict:
        length = int(self.headers.get("Content-Length", 0))
        return json.loads(self.rfile.read(length) or b"{}")

    def _send_wav(self, body: bytes, voice: str) -> None:
        self.send_response(200)
        self.send_header("Content-Type", "audio/wav")
        self.send_header("Content-Length", str(len(body)))
        # Lets a client name the file without guessing from the request.
        self.send_header("X-Voice", voice)
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self) -> None:  # noqa: N802
        if self.path not in ("/infer", "/preview"):
            return self._json({"error": "unknown path"}, 404)
        try:
            req = self._read_json()
            voice = req.get("voice")
            # /preview always speaks the same line, so two samples are
            # actually comparable. /infer takes the caller's text.
            text = req.get("text") or (PREVIEW_TEXT if self.path == "/preview" else None)
            if not text:
                return self._json({"error": "text is required"}, 400)
            tts = vn.engine()
            audio = tts.infer(text, voice=voice,
                              temperature=float(req.get("temperature", 0.8)),
                              silence_p=float(req.get("silence_p", 0.15)))
            body = _wav_bytes(audio)
        except Exception as e:  # noqa: BLE001 — report, don't kill the worker
            return self._json({"error": str(e)[:300]}, 500)
        self._send_wav(body, voice or "")

    def log_message(self, *a) -> None:
        pass  # quiet; use --verbose here if you ever need access logs


def main() -> None:
    ap = argparse.ArgumentParser(description="VieNeu TTS worker (LAN only, no auth)")
    ap.add_argument("--port", type=int, default=8818)
    ap.add_argument("--bind", default="0.0.0.0")
    args = ap.parse_args()
    vn.engine()  # load model up front so first request isn't cold
    print(f"TTS worker on {args.bind}:{args.port} (no auth — LAN/SSH-tunnel only)")
    ThreadingHTTPServer((args.bind, args.port), Handler).serve_forever()


if __name__ == "__main__":
    main()
