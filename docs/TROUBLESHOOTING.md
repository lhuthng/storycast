# Troubleshooting — when a chapter will not come out

Quick symptom → fix list, ordered the way you will actually hit them. The TUI
(`make tui`) is the fastest diagnostic: the **Events** pane records every
failure with the worker's own error text, and **K** shows it per task.

## Nothing is happening at all

| Check | Fix |
|---|---|
| Is the inductor up? `curl http://127.0.0.1:8901/api/state` | `make serve` (or press `B` in the TUI, which starts it) |
| Is a worker connected? `tui --once` → "workers (N)" | `make agent` locally; on other boxes, re-run `make provision BOX=…` which starts workers |
| Is any work enqueued? Tasks pane says "no tasks queued" | press `t` and enter a range like `1 10` |
| The footer says **disconnected** | the inductor is down or the `--api` URL is wrong (`make tui API=http://box:8901`) |

## A task keeps failing

1. Press **K**, find the red row, press **Enter** — `task.detail` is the
   worker's actual error (the full text, not a summary).
2. Fix the cause. Then either `u` (retry: strikes reset) or `F`
   (force: also deletes the stage's partial output, e.g. a half-written
   `script-NN.json`, so it truly runs again).
3. A task that failed 3 times is **shelved** on purpose — it stops starving the
   healthy chapters until you retry it. It is not broken forever.

Common causes per stage:

* **crawl fails / empty text** — the URL template is wrong or the site changed.
  `c` in the TUI re-saves the template and probe-crawls one chapter; check the
  response in the events pane. Some sites rate-limit: lower `START`/`COUNT` and
  add pauses.
* **digest fails** — almost always the analyzer: a missing key, quota
  exhausted (free tier: 3 RPM / 10 RPD), or the model name changed. The
  analyzer chain falls through `analyze_models` in order; check which model the
  event names. `ANALYZER=opencode|openrouter|local` in `.env` swaps the backend.
  * **the event names a model you stopped using** (e.g. a `503` for
    `gemini-3.5-flash` when `analyze_models` holds only
    `gemini-3.5-flash-lite`) — a provisioned worker has **no
    `.bm/settings.json`**: provisioning copies `prompts/`, `python/`, `assets/`,
    `refs/` and the cast, and never the inductor's own state. It therefore used
    to run on `Settings::default()` — the *compiled-in* chain — and
    ignore the operator's chain completely. The inductor now sends its analyzer
    block (`analyze_models`, the per-backend model names,
    `ollama_url`) with every digest offer and the worker overlays it, so the
    model named in the event is the model in the inductor's
    `.bm/settings.json`. With Gemini credentials configured, an empty
    `analyze_models` list skips Gemini and falls back to `opencode`. If a
    stale name still shows up, that box is running an agent from before the fix:
    re-run `make provision BOX=…`. The agent is re-pushed **only when the
    workspace version in `rust/Cargo.toml` changed** — an unchanged version
    reports "already configured" and pushes nothing, so bump it first.
  * **`…; opencode fallback failed: opencode CLI not found`** — the gemini chain
    ran out *and* the fallback behind it is not installed on the box that took
    the task. `opencode` is a **binary**, not a pip package, and provisioning
    does not install it, so a remote worker can have gemini configured and no
    fallback at all. Two ways out: put `opencode` on `PATH` for the worker's
    user on that box, or accept that an exhausted gemini chain is terminal
    there. Worth checking *before* blaming the model — a 503/429 that shelves a
    task is often this pair, not one failure.
  * **`GEMINI_API_KEY missing` (or `OPENROUTER_API_KEY`)** — the key is set in
    the **inductor's** `.env`, and only there. It rides the task offer to
    whichever worker runs the digest, so a box with no `.env` of its own is
    normal and expected; there is nothing to copy onto it. If this fires, the
    inductor itself has no key for the analyzer it is configured with: fix
    `.env` next to the inductor, **restart the inductor** (it reads `.env` at
    boot), then `u` to retry. To check which side is short, the worker logs
    `credentials from inductor: GEMINI_API_KEY` for every task that received
    one — no such line means the offer carried nothing.
* **render fails / voice missing** — the TTS sidecar is down (`tts=down` in the
  Machines pane) or a speaker has no voice. `v` re-reads the roster and refills
  gaps from the pool; `s` assigns one by hand; `S` shows every speaker's
  verdict. If a clone voice is missing on a worker, re-run `make provision
  BOX=…` — enrollment is stamped, so it only re-enrolls what changed.
  * **`GEMINI_API_KEY missing` from the sidecar (`TTS_ENGINE=gemini`)** — the
    key is installed into the worker's environment per task, and the sidecar is
    a child process, so it inherits whatever was installed when the worker
    *spawned* it. A sidecar that was already running before the task — started
    by `make provision`, or left behind by an earlier run — keeps the
    environment it started with and will not see it. Stop it and let the worker
    start a fresh one: `pkill -f tts_server.py` on that box. The `vieneu`
    engine reads no key and is unaffected.
* **merge fails** — usually a missing segment (the render was interrupted and
  the segment cache incomplete). Retry the render task first, then merge.
* **one character speaks with two voices** — the bible forked: title/case/
  description variants (`Huyền Vũ lão tổ`, `Sở Cuồng sư`) became separate
  entries. Press `:m` (reconcile): certain folds apply immediately, ambiguous
  pairs go to the analyzer once, the cast is rewritten and only the losers'
  chapters re-render. Refused mid-play — `:X` first, like a voice swap.

## Provisioning problems

| Symptom | Meaning / fix |
|---|---|
| `unreachable: ssh exit 255` | wrong address/user/key, or ssh asks for a password. The provisioner uses `BatchMode=yes`, so **keys must work non-interactively**; test `ssh -o BatchMode=yes user@box true`. `make link KEY=~/.ssh/your-key` then provision again |
| provision uses the wrong key | the machine overlay's `ssh key` line names the winning source (`machines.json` / `settings.json` / ssh default) — fix it where it wins: re-bind with `:a`, `:sshkey` for the default, or `make link KEY=..` |
| `python provisioning failed` | the box ran out of disk or has no `python3`; the venv needs ~2 GB free |
| `voice enrollment failed for: <names>` | a clip in `voices.json` is missing or unreadable on this machine; fix `refs/`, then provision again |
| Provisioning runs the slow path every time | the stamp changed — check *what* changed: any edit under `prompts/`, `python/requirements.txt`, the cast files, or the agent version resets the sources stamp; any change to `voices.json` or `refs/` resets the voices stamp. `P` forces the slow path deliberately |
| Machine added but workers never start | the inductor must be reachable **from the worker boxes** — start it bound to the LAN: `make serve` already uses `--bind 0.0.0.0` |

## TUI problems

| Symptom | Fix |
|---|---|
| "terminal too small" panel | resize to at least 76×20; 100×32 for the full dashboard. `C` toggles colour |
| Events pane floods after connecting | that is the inductor's recent history (up to 100 events) being shown once — by design |
| A key does nothing | `?` shows the full list; capital vs lowercase matters (`u` retry vs nothing, `K` ledger vs `k` move up, `P` force vs `p` provision) |
| Everything froze | it should not — polling is backgrounded. If it truly hangs, `Ctrl-C` loses nothing: restart with `make tui` and the ledger is intact |

## Nuclear options (all safe, in order of severity)

1. **Restart the TUI** — pure viewer, loses nothing but local log lines.
2. **Restart the inductor** (`X`, then `B`) — the ledger is on disk; workers
   ride through and re-register. In-flight tasks are re-queued automatically.
3. **Restart a worker** — its running task is reaped (~90 s) and re-offered.
4. **Force a stage** — in the K ledger, `F` deletes that stage's artifact so it
   re-runs end to end (a re-render only re-speaks segments missing from the
   cache).
5. **`requeue` op** — frees everything stranded on dead workers without
   waiting out leases. Attempts are kept (it unsticks, it does not forgive).
