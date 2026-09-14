"""Chapter -> script-NN.json via a Gemini text model, against the global bible."""
from __future__ import annotations

import json
import os
import re
from pathlib import Path

PROMPT_PATH = Path("prompts/analyze.txt")
BIBLE_PATH = Path("data/bible.json")
ANALYZE_MODEL = os.environ.get("ANALYZE_MODEL", "gemini-3.5-flash")
ANALYZER = os.environ.get("ANALYZER", "opencode")  # opencode | openrouter | local | gemini
LOCAL_MODEL = os.environ.get("LOCAL_MODEL", "gemma-4-12b")
OLLAMA_URL = os.environ.get("OLLAMA_URL", "http://localhost:11434")
OPENROUTER_MODEL = os.environ.get("OPENROUTER_MODEL", "google/gemma-4-31b-it:free")
OPENCODE_MODEL = os.environ.get("OPENCODE_MODEL", "opencode/muse-spark-1.3-contributor-free")

# Surface forms that may NEVER join the bible: pronouns, generic nouns, verb phrases.
ALIAS_STOP = {
    "hắn", "nàng", "ta", "ngươi", "y", "huynh", "đệ", "tỷ", "muội",
    "phàm nhân", "con", "người", "tên", "tiểu", "lão", "tiểu tử",
    "narrator", "người dẫn chuyện",
}

_VI_RE = re.compile(r"[àáạảãâầấậẩẫăằắặẳẵèéẹẻẽêềếệểễìíịỉĩòóọỏõôồốộổỗơờớợởỡùúụủũưừứựửữỳýỵỷỹđ]")


def _client():
    from google import genai  # lazy import so `check` works without deps

    key = os.environ.get("GEMINI_API_KEY")
    if not key:
        raise SystemExit("GEMINI_API_KEY missing — copy .env.example to .env")
    # attempts=1: SDK 2.x reuses a closed http client on internal retry; we retry below with a fresh client instead.
    return genai.Client(api_key=key, http_options={"retry_options": {"attempts": 1}})


def _generate(prompt: str, analyzer: str = ANALYZER):
    if analyzer == "local":
        return _generate_local(prompt)
    if analyzer == "openrouter":
        return _generate_openrouter(prompt)
    if analyzer == "opencode":
        return _generate_opencode(prompt)
    from google.genai import types

    last: Exception | None = None
    for i in range(6):
        try:
            client = _client()  # keep a ref: inline temporary gets GC'd mid-request (SDK 2.x)
            return client.models.generate_content(
                model=ANALYZE_MODEL,
                contents=prompt,
                config=types.GenerateContentConfig(
                    response_mime_type="application/json",
                    max_output_tokens=16384,
                ),
            )
        except Exception as e:  # noqa: BLE001
            last = e
            s = str(e)
            if "PerDay" in s:
                raise SystemExit("text-model day quota exhausted — resume remaining chapters tomorrow") from e
            import re as _re
            import time as _time

            m = _re.search(r"retry in ([\d.]+)s", s) or _re.search(r'"retryDelay":\s*"(\d+)s"', s)
            wait = float(m.group(1)) + 2 if m else 30.0
            print(f"analyze attempt {i + 1}/6 rate-limited, sleeping {wait:.0f}s")
            _time.sleep(wait)
    raise last  # type: ignore[misc]


def _generate_opencode(prompt: str):
    """Headless `opencode run` with a free model. Same .text contract as other backends."""
    import subprocess

    full = ("Do not use any tools. Answer with the requested output and nothing else.\n\n" + prompt)
    try:
        r = subprocess.run(["opencode", "run", "-m", OPENCODE_MODEL, full],
                           capture_output=True, text=True, timeout=600)
    except FileNotFoundError as e:
        raise SystemExit("opencode CLI not found — install it first") from e
    if r.returncode != 0:
        raise SystemExit(f"opencode run failed: {r.stderr[-500:]}")
    out = r.stdout
    start, end = out.find("{"), out.rfind("}")
    if start < 0 or end <= start:
        raise ValueError(f"no JSON object in opencode output: {out[:200]!r}")

    class _Resp:
        text = out[start:end + 1]

    return _Resp()


def _generate_local(prompt: str):
    """Ollama chat with JSON mode. Returns an object with a .text attr like the Gemini path."""
    import urllib.request

    body = json.dumps({
        "model": LOCAL_MODEL,
        "messages": [{"role": "user", "content": prompt}],
        "stream": False,
        "format": "json",
        "options": {"temperature": 0, "num_ctx": 16384},
    }).encode()
    req = urllib.request.Request(f"{OLLAMA_URL}/api/chat", data=body,
                                 headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=1800) as r:
            content = json.loads(r.read().decode())["message"]["content"]
    except OSError as e:
        raise SystemExit(f"cannot reach Ollama at {OLLAMA_URL} ({e}); run: ollama serve") from e

    class _Resp:
        text = content

    return _Resp()


def _generate_openrouter(prompt: str):
    """OpenRouter chat completions with JSON mode. Same .text contract as other backends."""
    import urllib.request

    key = os.environ.get("OPENROUTER_API_KEY")
    if not key:
        raise SystemExit("OPENROUTER_API_KEY missing — add it to .env")
    body = json.dumps({
        "model": OPENROUTER_MODEL,
        "messages": [{"role": "user", "content": prompt}],
        "temperature": 0,
        "max_tokens": 16384,
        "response_format": {"type": "json_object"},
    }).encode()
    req = urllib.request.Request(
        "https://openrouter.ai/api/v1/chat/completions", data=body,
        headers={"Content-Type": "application/json",
                 "Authorization": f"Bearer {key}",
                 "HTTP-Referer": "https://github.com/beyond-myriads-converter",
                 "X-Title": "beyond-myriads-converter"})
    try:
        with urllib.request.urlopen(req, timeout=600) as r:
            content = json.loads(r.read().decode())["choices"][0]["message"]["content"]
    except urllib.error.HTTPError as e:
        if e.code != 429:
            raise SystemExit(f"OpenRouter error {e.code}: {e.read()[:200]}") from e
        raise _OpenRouterRateLimited(e) from e
    except OSError as e:
        raise SystemExit(f"cannot reach OpenRouter ({e})") from e

    class _Resp:
        text = content

    return _Resp()


class _OpenRouterRateLimited(Exception):
    def __init__(self, http_error):
        self.http_error = http_error
        super().__init__(f"OpenRouter 429: {http_error}")


def _openrouter_wait(exc: _OpenRouterRateLimited, attempt: int) -> float:
    try:
        return float(exc.http_error.headers.get("Retry-After", "")) + 2
    except (TypeError, ValueError):
        return min(30 * 2 ** attempt, 300)


def load_bible(path: str | Path = BIBLE_PATH) -> dict:
    p = Path(path)
    if p.exists():
        return json.loads(p.read_text(encoding="utf-8"))
    return {"characters": []}


def save_bible(bible: dict, path: str | Path = BIBLE_PATH) -> None:
    from synthesize import atomic_write  # local import: synthesize never imports analyze (no cycle)

    p = Path(path)
    atomic_write(p, json.dumps(bible, ensure_ascii=False, indent=1) + "\n")


def _bible_context(bible: dict) -> str:
    """Lean context for the prompt: identity only, no chapter baggage."""
    lean = [{k: c.get(k) for k in ("name", "personality", "voice_hint", "proper_aliases")}
            for c in bible.get("characters", [])]
    return json.dumps(lean, ensure_ascii=False)


def _promotable(form: str, owner: str, bible: dict) -> str | None:
    """Proper-name forms may join the bible; pronouns/generics stay chapter-local."""
    f = (form or "").strip()
    if not f or f.lower() in ALIAS_STOP or len(f) < 2:
        return None
    for c in bible.get("characters", []):
        if c["name"] != owner and f in c.get("proper_aliases", []):
            print(f"   bible: reject alias {f!r} for {owner} (owned by {c['name']})")
            return None
    return f


def merge_bible(bible: dict, data: dict, chapter: str) -> dict:
    by_name = {c["name"]: c for c in bible.get("characters", [])}
    for nc in data.get("new_characters", []):
        name = (nc.get("name") or "").strip()
        if not name or name in by_name:
            continue
        entry = {"name": name, "personality": nc.get("personality", ""),
                 "voice_hint": nc.get("voice_hint", ""), "proper_aliases": [],
                 "first_seen": chapter, "chapters_seen": []}
        for form in [name, *(nc.get("proper_aliases") or [])]:
            if _promotable(form, name, bible) and form not in entry["proper_aliases"]:
                entry["proper_aliases"].append(form)
        bible["characters"].append(entry)
        by_name[name] = entry
        print(f"   bible +{name} ({entry['voice_hint']}) [{chapter}]")
    for owner, forms in (data.get("new_aliases") or {}).items():
        if owner not in by_name:
            continue
        for form in forms or []:
            if _promotable(form, owner, bible) and form not in by_name[owner]["proper_aliases"]:
                by_name[owner]["proper_aliases"].append(form)
                print(f"   bible alias {form!r} -> {owner} [{chapter}]")
    for c in bible.get("characters", []):
        if c["name"] in data.get("roster", []) + [s.get("speaker") for s in data.get("segments", [])]:
            if chapter not in c.setdefault("chapters_seen", []):
                c["chapters_seen"].append(chapter)
    return bible


def _warn_vietnamese(data: dict, bible: dict) -> None:
    """EN policy is trust-based; flag obvious violations for the review gate."""
    skip = {"dich", "lac", "doan", "thanh", "nguyen", "tran", "ngo", "phong", "tuyet", "ly"}
    for c in bible.get("characters", []):
        skip.update(c["name"].lower().split())
        skip.update(a.lower() for a in c.get("proper_aliases", []))

    def looks_vi(s: str) -> bool:
        words = re.findall(r"[A-Za-zÀ-ỹ]+", s or "")
        return any(_VI_RE.search(w) and w.lower() not in skip for w in words)

    for c in data.get("new_characters", []):
        for k in ("personality", "voice_hint"):
            if looks_vi(c.get(k, "")):
                print(f"   WARN: {c.get('name')}.{k} looks Vietnamese, expected English")
    if looks_vi(data.get("atmosphere", "")):
        print("   WARN: atmosphere looks Vietnamese, expected English")


def analyze(chapter_path: str = "data/chapters/ch01.txt", out: str = "data/script-01.json",
            bible_path: str | Path = BIBLE_PATH, analyzer: str | None = None) -> Path:
    chapter = Path(chapter_path).stem.replace("ch", "")
    analyzer = analyzer or ANALYZER
    bible = load_bible(bible_path)
    text = Path(chapter_path).read_text(encoding="utf-8")
    prompt = (PROMPT_PATH.read_text(encoding="utf-8")
              .replace("{bible_json}", _bible_context(bible))
              .replace("{chapter_text}", text))
    print(f"digest via {analyzer} ({ {'local': LOCAL_MODEL, 'openrouter': OPENROUTER_MODEL, 'opencode': OPENCODE_MODEL}.get(analyzer, ANALYZE_MODEL) })")
    last_rl = None
    for attempt in range(6):  # free-tier 429s: back off and retry, quota resets quickly
        try:
            resp = _generate(prompt, analyzer)
            break
        except _OpenRouterRateLimited as e:
            last_rl = e
            wait = _openrouter_wait(e, attempt)
            print(f"openrouter rate-limited, sleeping {wait:.0f}s (try {attempt + 1}/6)")
            import time as _time

            _time.sleep(wait)
    else:
        raise SystemExit(f"openrouter still rate-limited after retries: {last_rl}") from last_rl
    raw = (resp.text or "").strip().removeprefix("```json").removesuffix("```").strip()
    try:
        data = json.loads(raw)
        _validate(data, bible)
    except Exception as e:  # noqa: BLE001 — one repair retry, then keep raw for debugging
        print(f"digest invalid ({e}); asking model to repair once")
        resp = _generate(prompt + f"\n\nYour last output was invalid: {e}. Return ONLY the corrected JSON object.", analyzer)
        raw = (resp.text or "").strip().removeprefix("```json").removesuffix("```").strip()
        try:
            data = json.loads(raw)
            _validate(data, bible)
        except Exception as e2:  # noqa: BLE001
            Path("data/.last-analyze-raw.json").write_text(raw, encoding="utf-8")
            raise SystemExit(f"digest invalid ({e2}); raw saved to data/.last-analyze-raw.json") from e2
    _warn_vietnamese(data, bible)
    fixes = data.get("fixes", []) or []
    for fx in fixes:
        assert fx.get("before") and fx.get("after"), f"fix needs before+after: {fx}"
        if fx["before"] not in text:
            print(f"   WARN: fix source not found in chapter: {fx['before'][:60]!r}")
    if fixes:
        print(f"   grammar fixes: {len(fixes)}")
    bible = merge_bible(bible, data, chapter)
    save_bible(bible, bible_path)
    script = {"atmosphere": data["atmosphere"], "roster": data["roster"],
              "mentions": data.get("mentions", {}), "segments": data["segments"],
              "fixes": data.get("fixes", [])}
    dest = Path(out)
    dest.parent.mkdir(parents=True, exist_ok=True)
    from synthesize import atomic_write  # local import: synthesize never imports analyze (no cycle)

    atomic_write(dest, json.dumps(script, ensure_ascii=False, indent=1) + "\n")
    print(f"segments={len(script['segments'])} roster={script['roster']} -> {dest}")
    return dest


def _validate(data: dict, bible: dict | None = None) -> None:
    assert isinstance(data, dict), "top-level must be a JSON object"
    assert isinstance(data.get("segments"), list) and data["segments"], "no segments"
    names = set(data.get("roster", [])) | {"Narrator"}
    known = names | {c["name"] for c in (bible or {}).get("characters", [])}
    for i, s in enumerate(data["segments"]):
        assert s.get("speaker") in names, f"segment {i}: unknown speaker {s.get('speaker')!r}"
        assert s.get("text"), f"segment {i}: empty text"
        assert str(s.get("direction", "")).startswith("Say "), f"segment {i}: direction must start with 'Say '"
    for form, owner in (data.get("mentions") or {}).items():
        # mentions may point at bible-known characters who are only talked ABOUT this chapter
        assert owner in known, f"mention {form!r} -> unknown {owner!r}"
    for nc in data.get("new_characters", []):
        assert nc.get("name"), "new_character without name"
        head = re.split(r"[,:\-–]", nc.get("voice_hint", ""))[0].strip().lower()
        assert head in (
            "adult male", "adult female", "boy", "girl", "elderly male", "elderly female"), \
            f"new_character {nc.get('name')}: voice_hint must start with gender/age"


if __name__ == "__main__":
    import sys

    analyze(*sys.argv[1:])
