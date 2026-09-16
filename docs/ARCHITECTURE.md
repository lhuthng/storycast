# Architecture — how Storycast works under the hood

Companion to the [README](../README.md), which is the "how do I run it" guide.
This one is the "why is it built this way" guide. Everything here describes
code that exists in this repo — four Rust crates plus a small Python sidecar —
not aspirations.

```
rust/crates/
  bm-proto      wire types shared by everyone (Task, Stage, Op, Machine, Roster…)
  bm-core       the pipeline itself: crawl, digest, cast, voices/pool, assemble,
                ambience, ETA, provisioning               (library, no binaries)
  bm-agent      the worker: registers with an inductor, pulls tasks, runs
                stages, reports progress                  (bin: bm-agent)
  bm-inductor   the orchestrator: scheduler + control API + provisioner + TUI
                (bin: bm-inductor)

python/         the TTS sidecar: tts_server.py (HTTP :8818), tts_vieneu.py
                (engine + voice enrollment), tts_router.py (engine selection)
```

## 1. One idea: work is a ledger of tasks, not a loop

Every unit of work is a `(stage, chapter)` pair held in one place — the
**ledger** (`.bm/ledger.json`) — with one of six states:

```
Pending ──offer──▶ Assigned ──beat──▶ Running ──report ok──▶ Done
   ▲                    │                 │
   │  lease expired /   ▼                 ▼ report fail
   └──────────── requeued ◀──────────── attempts + 1
                                          └─ 3 strikes ─▶ Shelved
```

* **offer** — a worker asks `POST /api/offer`; the inductor only offers a task
  when its *upstream* stages are `Done` (crawl → digest → render → merge), so
  ordering is a property of the data, not of luck.
* **lease** — a running task has a deadline per stage (crawl 10 min, digest 20
  min, render 90 min, merge 30 min). An expired lease returns the task to the
  pool **without a strike**: silence is not failure. A background `reap` runs
  every 10 s and also re-queues tasks stranded on workers that stopped sending
  heartbeats (~90 s window).
* **strikes** — three failed attempts shelve a task so it stops being retried
  forever. The operator lifts this with `u` (blanket retry) or per task from
  the K ledger (`u` retry, `F` force — which also deletes the stage's on-disk
  artifact, so reconcile cannot mistake stale output for a finished chapter).

Because state lives only in the ledger plus artifacts on disk, any process can
die at any moment. Restarting the inductor re-reads the ledger; restarting a
worker re-registers and pulls again.

### Config lives next to the ledger, not in it

Three files, three jobs: `.bm/settings.json` (app-wide defaults, including
`ssh.{user,port,key}`), `.bm/machines.json` (per-machine connection config,
keyed by address, written when a box is bound with `:a`, `link` or
`provision`), `.bm/ledger.json` (runtime only: task states plus per-machine
liveness under `machine_state`). The API joins config with runtime and serves
the same `Machine` shape as always, so the TUI never sees the split.

One chain resolves the ssh key, highest wins: the machine's own entry, else
the app default, else ssh decides (agent / `~/.ssh/config` — no key at all is
legal, not a gap). The machine overlay prints the winner and its source, so a
mispointed key names where it was set. `.env` holds API keys only; the SSH key
is a *path*, which is config, not a secret.

## 2. The stages (bm-core)

* **crawl** (`crawl.rs`) — fetch `url_template` with `{n}` replaced, extract and
  clean the chapter body. `POST /api/op {"op":"crawl-setup"}` saves the template
  and probe-crawls one chapter, so a bad selector fails loudly *before* you
  enqueue a range.
* **digest** (`digest/`) — one LLM call per chapter: the prompt
  (`prompts/analyze.txt`) + the bible + the chapter text in; strict JSON out
  (`segments`, `roster`, `new_characters`, `aliases`, `fixes`). The inductor is
  the **single writer** of `data/bible.json` and the cast files — workers send
  the finished script and their bible delta back inside the completion report,
  which removes any read-modify-write race between machines. The "analyzer" is
  pluggable (`opencode | openrouter | gemini | local`) with a fallback chain
  over models.
* **render** (`bm-agent/src/tts.rs` + `python/`) — speaks each script segment
  through the engine. Vieneu runs as an HTTP sidecar on `127.0.0.1:8818` per
  machine; each finished segment is cached under
  `data/audio/segments-<engine>-NN/`, so a retry resumes mid-chapter instead
  of starting over. The report's `units` counts real TTS calls (cache hits
  excluded), which feeds the ETA model.
* **merge** (`assemble/`, `ambience.rs`) — concatenates segments with
  `gap_ms` pauses and optional ambience beds keyed by the script's `scene`
  labels, and writes `output/Ch.N - Title.mp3`. A merge task carries
  **affinity**: it runs on the machine that rendered the chapter, because that
  is where the segment cache already is. A remote merge ships its mp3 home,
  base64, inside the report.

## 3. The control API (bm-inductor, axum, default :8901)

| Endpoint | What it does |
|---|---|
| `GET /api/state` | The whole world for the TUI: `tasks`, `machines`, `beats`, `counts`, `settings`, `events` |
| `POST /api/offer` | A worker asks for work; answers with a task offer (or nothing) |
| `POST /api/complete` | A worker reports done/failed (+ artifacts: script, text, bible delta, mp3) |
| `POST /api/heartbeat` | Progress: stage, chapter, %, activity, ETA |
| `GET /api/roster` | The resolved voice roster (catalogue + pool + policy verdicts) |
| `POST /api/op` | Operator ops: `translate`, `crawl-setup`, `voices`, `swap-voice`, `preview-voice`, `eta`, `requeue`, `retry`, `retry-task` |

Every state change the scheduler makes also appends an **event** to an
in-memory ring buffer (`bm-inductor/src/state/`): completions with duration,
failures **with the worker's own error text**, lease expiries, orphan
requeues, and operator actions. `/api/state` exposes it and the TUI folds it
into the Events pane, deduplicating by monotonic id — which is how a digest
failing on a distant box becomes a readable line on your screen instead of a
stuck row.

## 4. Provisioning, and why second runs are fast

`bm-core/src/provision/` onboards a machine over plain `ssh`/`rsync` — no SSH
library, so your `~/.ssh/config` and keys are reused and every command is
visible in the TUI log (the machine overlay shows which key won —
`machines.json`, `settings.json`, or the ssh default):

1. **probe** — one ssh round trip: hostname, CPUs, RAM, disk, agent version,
   python present, enrolled voices, TTS up, and the **provision stamp**.
2. **decide** — compute a local stamp (`compute_provision_stamp`): two SHA-256
   digests, one over the *sources* (`prompts/`, requirements, cast files, scene
   map, agent version) and one over the *voices* (`voices.json` content plus
   `refs/` file signatures). Compare against the stamp the target stored at
   `~/.bm-worker/.provision_stamp.json` during its last provision.
3. **do only what changed** — sources unchanged: skip the whole rsync pass;
   voices unchanged: skip `ensure_voices`, the step that boots Python and
   imports PyTorch; python already present: skip the ~1.7 GB venv build. Local
   copies compare size+mtime per file, exactly like rsync.
4. **write the stamp**, start the TTS sidecar if it is not answering, and
   re-probe so the TUI shows the post-provision truth.

From the TUI, `B` provisions **all** registered machines concurrently (a
`JoinSet`), and the backend only starts if every machine reports ready — a
provision-gated start, so "started" always means "ready".

## 5. Voices: three layers

1. **Catalogue** — voices built into the engine (`voices/`), each with
   gender/accent/language/style metadata and a stable *key*.
2. **Pool** — your clips (`voice-pool.json`), tagged from filenames or by hand
   (`roster add-sample`); enrolled into the engine's voice store on every
   worker during provisioning (enrollment is keyed by name, so a renamed entry
   re-enrolls even with an identical clip).
3. **Cast** — `data/cast-<engine>.json`, speaker → voice. Assignment is
   automatic under a per-engine **accent policy** (the shipped Vieneu policy
   restricts to Central/South accents), constrained by the bible's
   `voice_hint` and the pool's tags. `s` repoints one speaker, `S` shows the
   whole cast with health verdicts, `v` re-reads the roster and refills gaps.

## 6. The TUI (bm-inductor/src/tui/)

The dashboard is a module, not a file. `tui.rs` is the module root — it holds
the entry points (`run`, `run_loop`, `snapshot`) and the `mod` declarations. The
rest splits two ways: the shared machinery by *kind* of code, then the two
per-screen concerns — key handling and drawing — one file per screen:

```
tui.rs        entry points + module wiring
tui/layout.rs tier constants, column widths, size_class — and the
              `const _: () = assert!(...)` guards that prove they fit
tui/screen.rs the modal state machine (Screen, Picker, Confirm, TextPrompt…)
tui/app.rs    App and its state transitions
tui/style.rs  colours, glyphs, cell/line formatting
tui/model.rs  pure view-model helpers (folding, filtering, sorting, rollups)
tui/jobs.rs   background jobs (Job, run_job) — one sequential worker
tui/input.rs  the modal key chain, in order, then normal::normal_key
tui/input/    one file per modal block
tui/draw.rs   the tier dispatch and the overlay match
tui/draw/     one file per pane or overlay
tui/tests.rs  every test
```

To follow a key press: `input.rs` → `input/<screen>.rs` → `jobs.rs` →
`app.rs` → `draw.rs` → `draw/<pane>.rs`.

* **Non-blocking by construction** — HTTP polling lives in a background Tokio
  task that ships `Ev::State` over an MPSC channel; the draw loop only drains a
  channel, so a slow inductor can never freeze the interface.
* **Responsive tiers** — below 76×20 a guard panel explains the problem instead
  of drawing a clipped lie; below 100×32 a compact tier folds the Tasks pane
  into the footer; above that, the full five-pane dashboard. Column widths and
  key-hint lines are checked at compile time, so they cannot silently overflow.
* **No silent defaults** — every prompt is prefilled with the value actually in
  force, and arguments are echoed before submission.
* **No blank panes** — each pane has an explicit empty/loading/error state that
  says what to do next.
* `K` opens the task ledger overlay (type to filter), `Enter` opens a task's
  page with the full `task.detail`, `u`/`F` re-queue it. `--once` renders the
  same data as plain text for scripts, `watch` and screen readers.
