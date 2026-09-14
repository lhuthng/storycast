"""Ordered Gemini TTS fallback chain with quota books.

Chain (newest first): 3.1 Flash -> 2.5 Pro -> 2.5 Flash -> local (caller-side).
Free-tier limits per model: RPM=3, TPM=10k, RPD=10.

429s are classified by level: day-exhaustion skips the model for the day
(persisted in data/quota.json); minute/token exhaustion sleeps RetryInfo
and retries the SAME model; anything else moves to the next model.
"""
from __future__ import annotations

import base64
import json
import os
import re
import time
from collections import deque
from datetime import datetime, timezone
from pathlib import Path

DEFAULT_ORDER = [
    "gemini-3.1-flash-tts-preview",
    "gemini-2.5-pro-preview-tts",
    "gemini-2.5-flash-preview-tts",
]
SHORT = {"gemini-3.1-flash-tts-preview": "3.1-flash",
         "gemini-2.5-pro-preview-tts": "2.5-pro",
         "gemini-2.5-flash-preview-tts": "2.5-flash"}

RPM, TPM, RPD = 3, 10_000, 10
QUOTA_PATH = Path("data/quota.json")

_calls: dict[str, deque] = {}    # model -> request timestamps (RPM window)
_tokens: dict[str, deque] = {}   # model -> (timestamp, est_tokens) (TPM window)


class AllExhausted(Exception):
    def __init__(self, events: list[str]):
        super().__init__("; ".join(events))
        self.events = events


def resolve_order(explicit: list[str] | None = None) -> list[str]:
    if explicit:
        return explicit
    env = os.environ.get("GEMINI_MODEL_ORDER", "").strip()
    return env.replace(",", " ").split() if env else list(DEFAULT_ORDER)


def est_tokens(text: str) -> int:
    return max(1, len(text) // 3)


def _today() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%d")


def _state() -> dict:
    try:
        st = json.loads(QUOTA_PATH.read_text(encoding="utf-8"))
        if st.get("date") == _today():
            return st
    except (OSError, ValueError):
        pass
    return {"date": _today(), "models": {}}


def _save(st: dict) -> None:
    QUOTA_PATH.parent.mkdir(parents=True, exist_ok=True)
    QUOTA_PATH.write_text(json.dumps(st, indent=1), encoding="utf-8")


def day_used(model: str) -> int:
    return int(_state().get("models", {}).get(model, {}).get("day", 0))


def _bump_day(model: str, exhausted: bool = False) -> None:
    st = _state()
    cur = int(st.get("models", {}).get(model, {}).get("day", 0))
    st.setdefault("models", {})[model] = {"day": RPD if exhausted else cur + 1}
    _save(st)


def _retry_delay(exc_str: str, default: float = 60.0) -> float:
    m = re.search(r"retry in ([\d.]+)s", exc_str) or re.search(r'"retryDelay":\s*"(\d+)s"', exc_str)
    return float(m.group(1)) + 2 if m else default


def _classify(exc: Exception) -> str:
    """429 RESOURCE_EXHAUSTED -> 'day' | 'minute' | None (not a quota error)."""
    s = str(exc)
    if "429" not in s and "RESOURCE_EXHAUSTED" not in s:
        return None  # type: ignore[return-value]
    if "PerDay" in s:
        return "day"
    return "minute"  # PerMinute / TPM / unknown-429: all minute-scale, wait-and-retry


def _genai_call(model: str, direction: str, voice: str) -> bytes:
    from google import genai
    from google.genai import types

    key = os.environ.get("GEMINI_API_KEY")
    if not key:
        raise SystemExit("GEMINI_API_KEY missing — copy .env.example to .env")
    client = genai.Client(api_key=key, http_options={"retry_options": {"attempts": 1}})
    try:  # keep a ref: inline temporary gets GC'd mid-request (SDK 2.x)
        resp = client.models.generate_content(
            model=model,
            contents=[{"parts": [{"text": direction}]}],
            config=types.GenerateContentConfig(
                response_modalities=["AUDIO"],
                speech_config=types.SpeechConfig(
                    voice_config=types.VoiceConfig(
                        prebuilt_voice_config=types.PrebuiltVoiceConfig(voice_name=voice)
                    )
                ),
            ),
        )
    finally:
        del client
    part = resp.candidates[0].content.parts[0]
    data = part.inline_data.data or ""
    return base64.b64decode(data) if isinstance(data, str) else bytes(data)


def _window_wait(model: str, tokens: int) -> float:
    """Seconds to wait for RPM/TPM windows, 0 if clear."""
    now = time.time()
    dq = _calls.setdefault(model, deque())
    while dq and now - dq[0] > 60:
        dq.popleft()
    if len(dq) >= RPM:
        return max(0.0, 60 - (now - dq[0]) + 1)
    tq = _tokens.setdefault(model, deque())
    while tq and now - tq[0][0] > 60:
        tq.popleft()
    if sum(t for _, t in tq) + tokens > TPM:
        return max(0.0, 60 - (now - tq[0][0]) + 1)
    return 0.0


def request(direction: str, voice: str, order: list[str] | None = None) -> tuple[bytes, str, list[str]]:
    """One TTS call through the fallback chain. Returns (pcm, model_id, events).

    Raises AllExhausted if every model fails or is day-capped.
    """
    order = resolve_order(order)
    tokens = est_tokens(direction)
    events: list[str] = []
    for model in order:
        tag = SHORT.get(model, model)
        if day_used(model) >= RPD:
            events.append(f"{tag}: skipped, RPD {RPD}/day used")
            continue
        wait = _window_wait(model, tokens)
        if wait > 0:
            events.append(f"{tag}: RPM/TPM window, sleeping {wait:.0f}s")
            time.sleep(wait)
        for attempt in range(3):  # fresh client per try (GC bug, see _genai_call)
            try:
                _calls.setdefault(model, deque()).append(time.time())
                _tokens.setdefault(model, deque()).append((time.time(), tokens))
                pcm = _genai_call(model, direction, voice)
                _bump_day(model)
                events.append(f"{tag}: OK ({tokens} tok)")
                return pcm, model, events
            except Exception as e:  # noqa: BLE001 — classified below
                level = _classify(e)
                if level == "day":
                    _bump_day(model, exhausted=True)
                    events.append(f"{tag}: RPD exhausted, marked for today")
                    break
                if level == "minute" and attempt < 2:
                    delay = _retry_delay(str(e))
                    events.append(f"{tag}: minute-limited, sleeping {delay:.0f}s (try {attempt + 1}/3)")
                    time.sleep(delay)
                    continue
                events.append(f"{tag}: failed ({str(e)[:100]}), next model")
                break
    raise AllExhausted(events)


if __name__ == "__main__":  # ponytail: quota-classification self-check, no framework
    assert _classify(Exception("400 nope")) is None
    assert _classify(Exception("429 RESOURCE_EXHAUSTED ... GenerateRequestsPerDayPerProjectPerModel-FreeTier ...")) == "day"
    assert _classify(Exception("429 RESOURCE_EXHAUSTED ... GenerateRequestsPerMinutePerProjectPerModel-FreeTier ... retry in 53.2s")) == "minute"
    assert _classify(Exception("429 RESOURCE_EXHAUSTED ... please retry in 10.9s. ... PerDay ...")) == "day"
    assert _retry_delay("retry in 53.262507263s") == 55.262507263
    assert est_tokens("abc" * 100) == 100
    print("router demo OK")
