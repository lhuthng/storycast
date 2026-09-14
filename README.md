# beyond-myriads-converter

Vietnamese web-novel chapter → multi-voice audio via Gemini (AI Studio key).

## Setup

```sh
cp .env.example .env   # add GEMINI_API_KEY
uv sync
```

## Use

```sh
# one chapter, end to end
uv run python main.py run --url "https://storya.click/truyen/.../chuong-1"
# or from a file
uv run python main.py run --file my-chapter.txt
# cheap voice test: first 5 lines only
uv run python main.py run --url ... --limit 5
# hear every cast voice before a full render (check gender/fit by ear)
uv run python main.py preview   # -> output/voice-preview/*.mp3
# sub-steps: ingest | analyze | synth | check | voices
uv run python main.py synth --dry-run   # no API: tests concat/plumbing
```

## Batch: 10 chapters at once

```sh
uv run python main.py batch --start 11 --count 10 \
  --url-template "https://storya.click/truyen/nguoi-tren-van-nguoi/chuong-{n}" \
  --engine vieneu --speed 1.5
```

One shared `bible.json` (auto-grows) and one shared cast per engine; per-chapter
scripts (`script-11.json`), segment caches, and outputs (`ch11-vieneu.mp3`).
Failures log per chapter and the batch continues; `--skip-ingest` / `--skip-analyze`
resume partial runs; `--dry-run` rehearses paths for free.

## Data model: bible + chapters

- `data/bible.json` — global character identity (EN personality/voice_hint,
  proper-name aliases only). The digest reads it as context and auto-merges
  new proper names with a log line.
- `data/script-NN.json` — per chapter: EN atmosphere, `roster`, chapter-local
  `mentions` (pronouns like hắn/ta stay here, never global), and per-speech
  `segments` (Vietnamese text, EN mood).
- `data/cast[-vieneu].json` — voice assignment per engine, shared across chapters.

## Remote TTS worker (run the model on another machine)

On the GPU box (same repo checkout, model auto-downloads on first run):

```sh
uv run python tts_server.py --port 8818   # LAN only, no auth — or SSH-tunnel it
```

On this machine, add `--tts-host http://gpu-box:8818` (or `TTS_HOST=...`) to any
`synth`/`preview`/`batch` command — every TTS call renders remotely, everything
else (digest, concat, ambience, cache) stays local. Verify with
`TTS_HOST=... uv run python main.py voices`.

Custom enrolled voices (Suneo, Nobita…) live in the **worker's** voice store:
copy `refs/*.wav` over and enroll once per name there (see `tts_server.py` header).
`clone` always enrolls locally by design.

## Local TTS (VieNeu, no quota, Central/South voices only)

```sh
uv pip install vieneu   # torch-free CPU build; ~1.7GB weights on first run
uv run python main.py voices                       # roster vs accent policy
uv run python main.py preview --engine vieneu      # 5 samples, verify by ear
uv run python main.py synth --engine vieneu --out output/ch01-vieneu.wav
```

`--engine` (or `TTS_ENGINE`) switches voices. The chapter digest defaults to
OpenCode's free model (`--analyzer opencode`, 1 call/chapter, no API key needed);
alternatives: `--analyzer openrouter` (needs `OPENROUTER_API_KEY`),
`--analyzer local` (needs `ollama serve` + model), `--analyzer gemini`.
Cast files and segment caches are per-engine
(`cast-vieneu.json`, `segments-vieneu/`), so swapping voices never poisons cache.
Edit `data/cast-vieneu.json` to recast — only changed voices re-render.

## Gemini cloud voices (quota-aware chain)

```sh
uv run python main.py synth --limit 5   # grouped calls via 3.1-flash -> 2.5-pro -> 2.5-flash
```

Free tier = 3 req/min, 10 req/day **per model**. `tts_router.py` tracks spend in
`data/quota.json`, paces requests, and falls over on 429s (day-exhaustion skips
the model; minute-limits sleep and retry). To stretch quota, lines are grouped
by (voice, mood) into single calls and cut back apart on silence
(`output/render-*.jsonl` logs every call: engine, model, split result, fallback
reason). If all cloud models are capped, remaining lines render locally and the
log says so.

Edit `data/cast.json` to change voices. Output: `output/ch01.wav` (+`.mp3` if ffmpeg installed).
