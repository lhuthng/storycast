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

    def do_POST(self) -> None:  # noqa: N802
        if self.path != "/infer":
            return self._json({"error": "unknown path"}, 404)
        try:
            req = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))))
            tts = vn.engine()
            audio = tts.infer(req["text"], voice=req.get("voice"),
                              temperature=float(req.get("temperature", 0.8)),
                              silence_p=float(req.get("silence_p", 0.15)))
            body = _wav_bytes(audio)
        except Exception as e:  # noqa: BLE001 — report, don't kill the worker
            return self._json({"error": str(e)[:300]}, 500)
        self.send_response(200)
        self.send_header("Content-Type", "audio/wav")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

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
