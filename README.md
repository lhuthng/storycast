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
- Voice reference clips in `refs/` + their names in `voices.json` (personal —
  both are gitignored; copy them from a machine that has them)

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
| `v` | voices | Read the sidecar roster, enforce your accent policy on the cast (enrolled clones always pass), refill gaps. Cast ships to workers on next provision. |
| `s` | swap-voice | Repoint one character (`character voice`); deletes **only** that speaker's cached segment files, drops stale mp3s, requeues render+merge. Everyone else keeps cache. |
| `e` | eta | Remaining work per stage from measured throughput ÷ live workers (`(guess)` = fallback, no data yet). |

TUI keys: `a` add machine · `p` provision selected · `P` force re-provision ·
`d` drop · `i` inspect · `r` refresh · `?` help · `C` colour · `q` quit.
`bm-inductor tui --api http://127.0.0.1:8901`.

`s` (swap voice) opens a two-step picker instead of a blind prompt: pick the
character from the speakers the inductor actually knows about, then pick the
voice from the full roster with gender, accent, language and style shown
alongside whether it is already in use and whether the accent policy permits
it. Filtering ignores diacritics, so `thai son` finds `Thái Sơn`, and `Tab`
auditions the highlighted voice into `data/previews/<voice>.wav` before you
commit to it.

`S` opens the **cast overview**: one table of every speaker, the voice they will
be rendered with, that voice's gender and accent, and a verdict per row —
`ok`, `shared with N others`, `blocked by the accent policy`, `unknown voice —
stale cast?`, or `unassigned`. `Enter` on a row jumps straight to the picker's
voice step for that speaker, so the overview is where a reassignment starts
rather than just a report.

In both list screens every printable character goes to the filter — movement is
arrow keys, PgUp/PgDn and Home/End only. (They used to also bind `j`/`k`, which
made those letters impossible to type into a filter over Vietnamese names.)

`bm-inductor tui --once --api …` prints one plain-text snapshot and exits — no
alternate screen, no colour, no keyboard, so it works with screen readers,
`watch(1)` and shell pipelines.

### Terminal size

The dashboard has three tiers, chosen from the window size:

| Tier | Needs | Layout |
|---|---|---|
| Full | 100×32 | all five panes |
| Compact | 76×20 | the Tasks pane folds into the footer roll-up; `tts` drops from Machines and `machine` from Workers, so the remaining columns keep full width instead of all clipping together |
| Too small | below 76×20 | a notice saying the size needed and the size found, and nothing else |

The floor is 76×20 rather than 100×32 on purpose: an 80×24 window is the default
on most setups, and blanking the dashboard there would help nobody. Below the
floor the dashboard is replaced rather than squeezed — a clipped table is not
just ugly, it can make a reversed-video row look like a different row than the
one selected. Keys stay live at every size, so a dialog opened before a resize
still says `Esc` cancels it.

## Voices: the shipped catalogue and yours

`voices.default.json` is the roster the repo ships: both engines, **every**
preset with its gender/accent/style. It is committed on purpose — a scheduled
render must not depend on the TTS sidecar being up, and a fresh clone has to
render with no local config.

`bm-core` embeds it at compile time (`voices::CATALOGUE_JSON`), so a worker
started from a bare checkout still has the full roster. The tests in
`bm-core::voices` assert the file and the compiled-in tables agree
voice-for-voice, so editing the JSON and forgetting the code fails the build
rather than silently changing which voice speaks.

**The catalogue states no preference.** All 23 VieNeu presets are declared —
the 13 Northern ones included — no accent is excluded, and no character is
pre-cast. It used to carry only the Central/South presets and allow-list exactly
those, which made one operator's regional taste look like a property of the
engine. A restriction is yours, not the engine's:

```json
// .bm/voices.json — this machine's roster, ignored by git
{
  "engines": {
    "vieneu": {
      "policy": { "excluded_accents": ["Northern"] },
      "default_cast": { "Narrator": "duc-tri" }
    }
  }
}
```

A field you leave out inherits the catalogue; a field you fill replaces it. With
no local roster, every declared preset is assignable. The inductor reads this
file wherever a voice is chosen — the picker, the cast refill, the swap gate —
and a malformed file is refused loudly rather than silently falling back to the
catalogue, which would re-admit everything you excluded.

Each voice also has an ASCII **key** (`duc-tri`), and the cast file stores keys
rather than display names — so renaming a voice's `name` no longer orphans every
assignment that referenced it. The reader accepts **either** form, so a migrated,
half-migrated and untouched cast all render:

```bash
./rust/target/debug/bm-inductor roster migrate-cast --dry-run   # show the diff
./rust/target/debug/bm-inductor roster migrate-cast             # rewrite, keeping a .bak
```

The cast is keyed on its next save anyway, so the command is a convenience for
doing it now — and for seeing what changed before it changes. Voices the
catalogue does not declare (enrolled clones) keep their names, which still
resolve. `roster` is local-only: it touches no worker and needs neither ffmpeg
nor ssh.

**One thing the split does not reach yet:** the Python sidecar
(`python/tts_vieneu.py`) still hardcodes its own Central/South allow-list, and
the render gate enforces *that* on workers. On this machine the two agree; on a
fresh clone the picker offers Northern presets the sidecar would refuse. Moving
that gate onto the operator's roster is the remaining piece — it needs the
roster to reach workers, which is the sync stage of
`.docs/VOICE_CONFIG_PROPOSAL.md`.

## What git deliberately does not carry

Audits, proposals, plans and notes describe a moment in the code's life, not the
code itself, and are regenerated on demand rather than read from history. They
live in `.docs/`, which is ignored as a directory — so a fresh clone simply does
not have them, and nothing has to be pattern-matched by filename. That is
intended, not an accident.

The personal voice data is ignored too:

| Path | What it is |
|---|---|
| `voices.default.json` | **tracked** — the shipped catalogue |
| `voices.json` | clones enrolled on this machine, and the clips they came from |
| `refs/*.wav` | the reference clips — the *input* to enrolment |

The last two are the trade: a fresh clone has no clones, and cannot rebuild them
from git alone, because the reference clips are the input rather than a build
artifact. Copy them from a machine that has them. Provisioning treats their
absence as a valid state (`VOICES-OK (no voices.json — no clones to enroll)`)
rather than a failure.

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
