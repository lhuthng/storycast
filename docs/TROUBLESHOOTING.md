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
* **digest fails** — almost always the analyzer: missing `GEMINI_API_KEY`,
  quota exhausted (free tier: 3 RPM / 10 RPD), or the model name changed. The
  analyzer chain falls through `analyze_models` in order; check which model the
  event names. `ANALYZER=opencode|openrouter|local` in `.env` swaps the backend.
* **render fails / voice missing** — the TTS sidecar is down (`tts=down` in the
  Machines pane) or a speaker has no voice. `v` re-reads the roster and refills
  gaps from the pool; `s` assigns one by hand; `S` shows every speaker's
  verdict. If a clone voice is missing on a worker, re-run `make provision
  BOX=…` — enrollment is stamped, so it only re-enrolls what changed.
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
