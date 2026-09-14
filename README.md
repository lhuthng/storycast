# beyond-myriads-converter

Vietnamese web-novel chapters → multi-voice audiobooks, distributed across a
cluster of machines. One orchestrator ("inductor") schedules work; worker
agents pull tasks, report progress, and push results back. TTS runs in a
Python sidecar; everything else is Rust + Tokio.

```
inductor (Rust) ──assign (workers pull)──▶ bm-agent × N (Rust)
     ▲                      │ heartbeat: progress · activity · ETA
     │                      ▼
     │              Python TTS sidecar ──/infer── (VieNeu voices)
     └──────── complete (ok/fail + artifacts: scripts, mp3s, bible deltas)
data/ · output/ · refs/          (per-machine work dirs; mp3s come home)
```

## Prereqs

- Rust toolchain (`rustup`), `ffmpeg`, `ssh`, `rsync`, `curl`
- Python 3.12 + a venv with the sidecar deps (`python/requirements.txt`)
- `opencode` CLI with access to a free model (digest lane; auth is per-machine)
- Voice reference clips in `refs/` + their names in `voices.json`

## Quickstart (solo, one machine)

```bash
cargo build --workspace --manifest-path rust/Cargo.toml
# 1. sidecar (the agent manages this itself per render task; manual form:)
./.venv/bin/python python/tts_server.py --port 8818
# 2. orchestrator
./rust/target/debug/bm-inductor serve --start 1 --count 100
# 3. worker (another shell)
./rust/target/debug/bm-agent worker --inductor http://127.0.0.1:8901
# 4. enqueue + watch
curl -X POST localhost:8901/api/op -H 'Content-Type: application/json' \
  -d '{"op":"translate","start":1,"count":100}'
./rust/target/debug/bm-inductor tui
```

Output lands in `output/Ch.N - Title.mp3`. Every chapter opens with its spoken
headline (`Chương N, <title>`) followed by the standard inter-turn pause.

## Adding a machine by IP

```bash
# cross-compile the agent once (ring needs a C cross-compiler: zig)
cargo install cargo-zigbuild && uv tool install ziglang
cargo zigbuild --target x86_64-unknown-linux-gnu -p bm-agent
# onboard: probe → push what's missing → verify (skips configured boxes)
./rust/target/debug/bm-inductor provision --addr 192.168.2.2 --user thang --key ~/.ssh/key
```

Provisioning is idempotent: configured machines get a sources sync + voice
check only. The venv build (~1.7 GB of weights) runs only when missing.
Enrolled clone voices live in the venv — `voices.json` re-enrolls whatever is
missing on every provision, so a venv rebuild never silently loses the cast.
`opencode` is installed best-effort; its login stays manual (browser).

Then start an agent there pointing at the inductor:

```bash
./bm-agent worker --inductor http://<inductor-lan-ip>:8901 --addr <its-ip>
```

## The five operations (TUI keys or `POST /api/op`)

| Key | Op | What it does |
|---|---|---|
| `t` | translate | Enqueue crawl+digest for a range (`start count`). Idempotent. |
| `c` | crawl-setup | Persist the URL template; probe-crawl one chapter, report selector health. |
| `v` | voices | Read the sidecar roster, enforce the Central/South accent policy on the cast (enrolled clones always pass), refill gaps. Cast ships to workers on next provision. |
| `s` | swap-voice | Repoint one character (`character voice`); deletes **only** that speaker's cached segment files, drops stale mp3s, requeues render+merge. Everyone else keeps cache. |
| `e` | eta | Remaining work per stage from measured throughput ÷ live workers (`(guess)` = fallback, no data yet). |

TUI: `a` add machine · `p` provision selected · `r` refresh · `q` quit.
`bm-inductor tui --api http://127.0.0.1:8901`.

## How scheduling works

- Workers pull; the inductor is the only decider (eligibility, leases, strikes).
- Leases expire back to the pool with **no strike** — silence is not failure.
- 3 reported failures shelve a chapter; the rest flow around it.
- Merge runs where the segments are (affinity) — segment caches never cross
  the network. Only scripts (~30 KB) go out, mp3s (~5 MB) come home.
- Ledger (`.bm/ledger.json`) persists assignments + strikes; startup
  reconciles from artifacts on disk, so restarts resume.
- Digest workers return bible deltas; the inductor merges as the single
  writer. Scripts hold content only — headlines are never segments.

## Troubleshooting (earned the hard way)

- **Render fails `non Central/South voice` for a valid voice** — stale
  sidecar: an old server answers `/health` but lacks `/policy`. The agent's
  currency check restarts it automatically; `curl localhost:8818/policy`
  tells the truth.
- **Tasks shelved after infra trouble** (dead box, stale binary): shelving
  counts *reported* failures. Fix the cause, reset the task to pending in
  `.bm/ledger.json` (or delete the entry — reconcile recreates it), restart.
- **Merge stuck `assigned` + worker idle** — affinity points at a machine the
  scheduler can't map (workers map was empty). Heartbeats now reheal the map;
  check `affinity` vs `workers` in `/api/state`.
- **mp3 exists but render re-queues** — the segment cache no longer matches
  the script (re-digest shifted run boundaries). By design: content changed,
  audio rebuilds. Never hand-edit scripts without purging that chapter's
  `data/audio/segments-*/` dir.
- **Reports vanish for big mp3s** — axum's default 2 MB body cap 413s them;
  this repo disables the limit (LAN-only API). If you re-enable auth/limits,
  raise it past 10 MB.
- **Nested `assets/assets` (or `refs/refs`) on a worker** — rsync directory
  semantics: sources must sync *contents* (trailing slash). Fixed in
  `rsync_push`; blow away the nesting if you see it.
- **Fresh box digests fail on auth** — `opencode auth login` needs a browser
  on that machine. Provisioning installs the CLI; login stays yours.
- **Sidecar RSS climbs across chapters** — by design it can't: agents start
  the sidecar per render task and stop it after. A single chapter peaks
  ~3–5 GB transient (model + buffers), then the OS reclaims all of it.
