# Troubleshooting — when a chapter will not come out

Quick symptom → fix list, ordered the way you will actually hit them. The TUI
(`make tui`) is the fastest diagnostic: the **Events** pane records every
failure with the worker's own error text, and **K** shows it per task.

## Nothing is happening at all

| Check | Fix |
|---|---|
| Is the inductor up? `curl http://127.0.0.1:8901/api/state` | `make serve` (or press `B` in the TUI, which starts it) |
| Is a worker connected? `tui --once` → "workers (N)" | `make agent` locally; on a remote box, `:prov` in the TUI (`p`) — it provisions **and then launches that box's worker**. `make provision BOX=…` provisions and registers the box but starts **no** worker: it finishes with the box joined to the registry and idle |
| Is any work enqueued? Tasks pane says "no tasks queued" | press `t` and enter a range like `1 10` |
| The footer says **disconnected** | the inductor is down or the `--api` URL is wrong (`make tui API=http://box:8901`) |

## A task keeps failing

1. Press **K**, find the red row, press **Enter** — `task.detail` is the
   worker's actual error (the full text, not a summary).
2. Fix the cause. Then either `u` (retry: strikes reset) or `F`
   (force: also deletes the stage's partial output, e.g. a half-written
   `script-NN.json`, so it truly runs again). A forced **merge** also forces
   its render when the chapter has no published mp3 — a merge makes none of its
   own input, so re-offering it alone fails again on the same box for the same
   reason. The command line reaches the same three scopes: `:retry` is every
   shelved task in the ledger, `:retry 24` is every shelved stage of one
   chapter, `:retry render 24` is one task by name. Only the ledger's `F`
   deletes anything; the `:retry` forms requeue.
3. A task that failed 3 times is **shelved** on purpose — it stops starving the
   healthy chapters until you retry it. It is not broken forever. A shelved
   chapter strands *every* stage of it, so a merge that keeps failing on input
   it cannot get will shelve the whole chapter.

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
    `gemini-3.5-flash-lite`) — a provisioned worker has **no settings file at
    all**: provisioning copies `prompts/`, `python/`, `assets/`, `refs/` and the
    cast, and never the inductor's own state. It therefore used
    to run on `Settings::default()` — the *compiled-in* chain — and
    ignore the operator's chain completely. The inductor now sends its analyzer
    block (`analyze_models`, the per-backend model names,
    `ollama_url`) with every digest offer and the worker overlays it, so the
    model named in the event is the model in the inductor's own settings
    (`workspaces/<name>/settings.json`). With Gemini credentials configured, an
    empty
    `analyze_models` list skips Gemini and falls back to `opencode`. If a
    stale name still shows up, that box is running an agent from before the fix:
    re-run `make provision BOX=…`. The agent is re-pushed **only when the
    workspace version in `rust/Cargo.toml` changed** — an unchanged version
    reports "already configured" and pushes nothing, so bump it first, or force
    the push with `P` / `:reprov` (`provision … --force`). Either way the **push
    does not replace the running worker**: nothing kills `bm-agent`, and
    `start_remote_workers` reports `worker already running (pid …)`. Drain first
    (`:drain` — workers exit once the queue empties) or `X`, then `B` to relaunch
    them onto the new binary. The local node is never provisioned at all, so there
    the relaunch is the whole job.
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
* **merge fails** — usually `N segments missing in <seg dir> (e.g. <name>.wav)`.
  A merge reads its segments **from the box that runs it** (pulling any piece
  it lacks from the inductor's store over the tunnel first), and it makes none
  of its own — so the question is what the **inductor's store** holds, not
  whether the cluster rendered the chapter. Two things produce the message:
  * **the store never got them.** Every render report ships its wavs and the
    inductor stores them before the row turns `Done`; a box on an older agent
    reports without shipping, and `collect_units` then has to pull each unit
    from that box after the fact — if the box is gone by then, the file is
    gone. This is also what a **voice swap** looks like: the swap invalidates
    the store's copies only, and until the re-render's completion lands (with
    its wavs) the heal requeues the takes the store lacks. The message naming
    a `title_<voice>.wav` is the tell.
  * **the render did not finish the chapter.** A render offer names every unit
    and the worker skips what it already has, which makes any box that
    finishes a render hold the whole chapter — but `Done` rows from before
    that fix (or a failed collection) can still claim files that were never
    written. A box on an older agent can still do this.
  Fix with `F` on the merge row — it cascades to the render, which is the stage
  that can actually make the wavs. The ledger's `u`/`F` work on a task in any
  state; the `:retry` forms move **shelved** tasks only, so `:retry 24` on a
  chapter whose merge merely failed says `no shelved tasks on ch24` and does
  nothing. Once the chapter *is* shelved, `:retry render 24` then
  `:retry merge 24` is the same repair two steps at a time.
* **one character speaks with two voices** — the bible forked: title/case/
  description variants (`Huyền Vũ lão tổ`, `Sở Cuồng sư`) became separate
  entries. Press `:m` (reconcile): certain folds apply immediately, ambiguous
  pairs go to the analyzer once, the cast is rewritten and only the losers'
  chapters re-render. Refused mid-play — `:X` first, like a voice swap.

## Uplink blips and the completion hook

The inductor drives, so a worker only ever *answers* — and a mid-task uplink
blip used to swallow the answer, costing a finished stage its report and the
chapter a full re-render. The reverse tunnel (`ssh -N -R` per box, held by the
inductor) plus the worker's completion hook (`bm-agent/src/hook.rs`) close
that hole. What each symptom means:

| Symptom (worker log) | What it is |
|---|---|
| `no word from the inductor for 30s — reporting render:7:3 through the tunnel` | **not an error.** The primary channel went quiet mid-task; the hook is delivering the finished stage through the reverse tunnel. `hook accepted — the completion is home` closes it |
| `hook: <task> still unreported — no tunnel answers on 127.0.0.1:18901` | the tunnel is down: the inductor is dead, predates the feature, or was restarted. Harmless to the box — the lease reaper requeues the task — but the work re-renders. Start the inductor; the supervisor respawns the tunnel on its own clock |
| `hook: the inductor answered 401/409/… — not accepted` | the tunnel is up but the API refused: a 401 is a stale cluster token on the box (re-provision), anything else read the inductor's own log |

| Symptom (inductor log) | What it is |
|---|---|
| `stale report for render:7:3 ignored` right after a tunnel recovery | the hook and the lease reaper raced: the task was already requeued when the completion landed. Discarded by design — the worker's word is not evidence — and the chapter renders once more. Frequent races mean the link drops often; see the next row |
| `tunnel: <addr> client exited (255) — respawning` | the ssh client died (NAT timeout, network flap). Respawning is automatic; the only cost is the seconds it is down. A client that exits *immediately* in a loop usually means `ExitOnForwardFailure` tripped — a stale sshd session on the box still holds the forwarded port, and it clears when that session dies |
| `tunnel: <addr> left the registry — tunnel closed` | the box was unlinked/dropped; the channel went with it, as it should |

## Provisioning problems

| Symptom | Meaning / fix |
|---|---|
| `unreachable: ssh exit 255` | wrong address/user/key, or ssh asks for a password. The provisioner uses `BatchMode=yes`, so **keys must work non-interactively**; test `ssh -o BatchMode=yes user@box true`. `make link KEY=~/.ssh/your-key` then provision again |
| `ssh exit 255: Host key verification failed` | **cannot happen any more.** The transport declines host-key verification and never reads or writes `known_hosts`, because a launched instance always presents a key nobody has seen and AWS hands the same public IP to a different box later (`StrictHostKeyChecking=no` alone still refuses a *changed* key). If you see it, that inductor predates the fix — rebuild it |
| a box just launched says **still booting** and `:prov` keeps doing nothing | **that is the correct answer, not a failure.** A fresh instance is `running` in EC2 seconds before `sshd` listens, and a probe cannot tell a booting box from a dead one. The state stays `initializing` (with the `state age` line counting up) rather than flipping to `error`, and the boot deadline gives up after five minutes. Wait, then `:prov` again. If it goes to `error` instead, ssh *did* answer and the note names the step that failed — that one is real |
| provision uses the wrong key | the machine overlay's `ssh key` line names the winning source (`machines.json` / `settings.json` / ssh default) — fix it where it wins: re-bind with `:a`, `:sshkey` for the default, or `make link KEY=..` |
| `bm-tts would not run — missing libonnxruntime.so.1 beside it?` | the sidecar is pushed together with its shared ONNX Runtime; if the library did not travel the loader fails by SONAME. `make runtime` stages it locally, then re-provision with `P` |
| `voice enrollment failed for: <names>` | a clip in `voices.json` is missing or unreadable on this machine; fix `refs/`, then provision again |
| Provisioning runs the slow path every time | the stamp changed — check *what* changed: any edit under `prompts/`, `python/requirements.txt`, the cast files, or the agent version resets the sources stamp; any change to `voices.json` or `refs/` resets the voices stamp. `P` forces the slow path deliberately |
| Re-provisioning a **healthy** box is slow | it should not be. On a box that already passed a full provision, `ensure_opencode`/`ensure_ffmpeg` are gated off and only run a `command -v` check — so no `npm i` (600 s bound) and no `apt install -y`. If it is still slow, the stamp changed: see the row above |
| `OPENCODE-SKIP (already configured — not reinstalling; force a re-provision to try again)` | not an error — the gate above declining to reinstall on a box it has already configured. It only appears if the tool is genuinely missing, and then the message names the remedy: `P` |
| `FFMPEG-SKIP (…)` / `ffmpeg is not on PATH` | same gate. Merge is disabled on that box until ffmpeg is present; crawl/digest/render still work. `apt install ffmpeg` (or `dnf`), then `P` |
| Machine added but workers never start | the inductor must be reachable **from the worker boxes** — start it bound to the LAN: `make serve` already uses `--bind 0.0.0.0` |
| a box **keeps OOMing** during renders | the model is ~2.85 GB and the box is 8 GiB, so one sidecar fits and two do not. Two is what a race used to produce: provisioning started one detached and the worker — unable to tell "not up yet" from "not there" — started its own. That race is closed (`bm-tts` binds before loading and answers 503 until ready; the worker waits instead of spawning), so a box doing it now is running an older agent/inductor, or holding an orphan. Check the Workers pane's `tts` column (or `pgrep -c bm-tts` on the box): `2×` raises an error event and the scheduler stops feeding that box. `X` sweeps it |
| a stage **frozen at a percentage with no error anywhere** | the signature of a **child process with no deadline** — the worker is waiting on something that will never return, so it prints nothing and the only thing that happens is the lease expiring, silently. It happened on 2026-09-22: a Gemini 503 sent the digest to its `opencode` fallback, which hung for 26 minutes (it hangs on `opencode run … "say hi"` too, so the fallback itself was broken). Read the worker's log tail — the last line names the step — then `pgrep -fl opencode` for the child. The events pane now says `expired while <worker> was still beating` on the first expiry and escalates on the second; the analyzer's own logs name the backend that actually ran, which is not always the one in the activity column. Fixed by deadlines on both the child and the Gemini client |
| the log is **full of** `tunnel: <addr> client exited (255) — respawning` | one unreachable box, and on an older build one line per five seconds for ever — 1339 lines was 16% of the inductor log and 198 of its last 200, which buries every real event. The supervisor is now edge-triggered like the duplicate-sidecar alarm: the first failure, then one line every ~5 minutes, plus a line when the box comes back. Seeing it in bulk means the build predates that, or the box has been down a long time — `ssh <addr>` to check |
| `TTS-STARTING (not ready after 240s — check …/tts.log)` | the sidecar started but never answered ready inside provisioning's budget. It is **not** treated as a failure and the box is still registered: the worker waits for a loading server rather than starting a second one. Read the log it names; a missing `libonnxruntime.so.1` is the usual cause (`make runtime`, then `P`) |
| a box's memory **creeps up over a long render run** — one sidecar, not two | a *different* cause from the row above, and the one the `tts` column's GiB figure is for. A single sidecar's resident set grows across a long run of inferences, and the old guard could not see it: the idle reaper is keyed on *not working*, so a box rendering continuously never reached it. The worker now recycles the model at a task boundary once it reaches half the box's RAM or has served 200 takes (whichever first, at most once per 5 minutes), logging `sidecar holds N MiB … — recycling it`. Watch for that line; its absence on a climbing box means the agent predates the guard |
| **tuning** the recycle budget, or measuring the growth | the numbers ship as a judgement and are meant to be replaced by a measurement. The worker prints what it will apply at startup (`TTS sidecar budget: recycle at …`), and both thresholds take a per-box override, so no rebuild is needed: `BM_TTS_MAX_RSS_MB=<MiB>` replaces the half-RAM cap with an absolute one (the log line then says `BM_TTS_MAX_RSS_MB` instead of `half this box's RAM`), `BM_TTS_MAX_RENDERS=<n>` and `BM_TTS_MIN_LIFETIME_SECS=<s>` move the other two. **The experiment:** one box, `:batch 32`, a long chapter — growth then shows up within one sitting in the `tts` column and in the recycle lines, and tells you whether the cap or the count is doing the work. Set the value in the worker's environment permanently (the launch line in `lifecycle.rs`) once you know it |
| a digest-only box **still holds the model** | its work policy has render off, so it needs no ~2.85 GB sidecar — the dispatcher re-tells it on every poll until the worker answers (`dispatch: <addr> told to drop its TTS sidecar`). If it never happens: an older agent answers 404, the inductor gives up after five tries (`sidecar instruction not delivered yet … giving up until its policy changes`) and the box keeps the old warm behaviour — re-provision or rebuild the agent. A box that *briefly* kept the model after an edit is the reboot case: it came back with the default before the instruction landed, and the next beat re-converges it |
| a render **fails with `refused (render off by policy)`** | exactly what it says: that box's policy has render off, the worker declined instead of re-warming the model the operator turned off, and the take was released back to the pool **strike-free** — it renders on another box. Persistent across the fleet means every box's policy has render off and the takes have nowhere to go; press `P` and turn render on somewhere |

## AWS boxes

A box that is in the cloud fails in ways a LAN box cannot, and the first
question is always *"is this the tool or is this the network"*. There is one
deliberate inversion to keep in mind: **the inductor dials the box, and nothing
ever dials the inductor.**

| Symptom | What it is |
|---|---|
| `no IAM user for this app yet` | expected until you run `bm-inductor aws login --csv <the console's download>`. The app does not fall back to your own AWS identity — see [AWS-CREDENTIALS.md](AWS-CREDENTIALS.md) |
| `aws ec2 failed: InvalidKeyPair.NotFound` | the keypair is in another region. EC2 keypairs are region-scoped, and the console's region selector is nowhere near the Create button |
| `aws ec2 failed: InvalidGroup.NotFound` | same, for the security group — one region *and* one VPC |
| `UnauthorizedOperation … iam:PassRole` | the launch named an instance profile the policy does not allow. `aws-policy.json` scopes `PassRole` to `storycast-worker` alone, which is the point |
| `InvalidParameterCombination … not eligible for Free Tier` | the account is on an AWS **Free plan**, which caps the instance type. Nothing the tool can read can see a plan, so `aws show` said "ready" and meant it. **Fix: set `instance_type` to `m7i-flex.large`** — 2 vCPU / **8 GiB**, on the free-tier list. Not a reason to pay more — but not a reason to go smaller either: `c7i-flex.large` sits on the same list at 4 GiB and **does not fit**, because the sidecar is ~2.85 GB resident the moment the weights load and that plus the agent and the OS will not go into ~3.9 GB usable. The field is `instance_type` in `.bm/aws.json` |
| `AuthFailure.ServiceLinkedRoleCreationNotPermitted` | the account has never used spot, so `AWSServiceRoleForEC2Spot` does not exist and the operator user cannot create it. Request one spot instance once in the console, or set `"spot": false` |
| A box runs, `aws ls` shows it, but the dashboard says **Offline** for ever | nothing admits the **task port (8917)** inbound. The box is healthy and simply unreachable; a closed port is swallowed, not refused, so there is no error anywhere |
| `ssh` **hangs and times out** | nothing admits **port 22**. Different failure, different cause |
| `ssh` connects then says `Permission denied (publickey)` | the firewall is fine; the box was linked without its key. `link --key .bm/aws/<region>.pem` |
| A freshly launched box shows **initializing** | correct, not a fault. `:up` links it the moment the account returns an address, and an instance is not reachable until sshd is listening — typically 30–60 s. `initializing` is deliberately not `offline`, so nothing calls the box dead while it boots. Still initializing after 5 minutes means it never answered ssh at all: it becomes **error** and the note says so (terminated, or in a subnet this machine cannot dial). `i` on the box shows `state age` — how long it has been that way |
| A box is up and driven, but every crawl fails | the box has no **outbound** access to the internet. Crawl is the one stage that fetches chapter URLs; every other stage works on files the inductor already pushed |

`bm-inductor aws discover` is the diagnostic for the two firewall rows: it reads
the group's own rules and names the ports that are missing. `bm-inductor aws
show` says what is missing from the pool. Setup and the reasoning behind each
rule are in [AWS-IAM-USER.md](AWS-IAM-USER.md); the order to do things in is
[AWS-WORKERS.md](AWS-WORKERS.md).

## TUI problems

| Symptom | Fix |
|---|---|
| "terminal too small" panel | resize to at least 76×20; 100×32 for the full dashboard. `C` cycles the theme (default → dim → mono) |
| Events pane floods after connecting | that is the inductor's recent history (up to 100 events) being shown once — by design |
| A key does nothing | `?` shows the full list; capital vs lowercase matters (`u` retry vs nothing, `K` ledger vs `k` move up, `P` force vs `p` provision) |
| A job sits at `queued · needs box 10.0.0.5` | **that is the answer, not a fault.** Another job holds that box. Jobs run together unless they need the same thing, so this row is waiting on the named resource and on nothing else. Jobs that name no resource share the command lane and queue among themselves by design |
| `start backend` finished, but boxes are still catching up | intended. `B` brings the backend up in seconds and hands every box that is **not already working** its own job, so the press returns immediately instead of holding the cluster for the slowest push. A failing box lands in `Error` with its reason and does not veto the rest |
| `B` re-provisions a box that was already working | it should not — an `online` box is skipped, and the log says so. If you see it, that box was not `online` when `B` ran (its first beat had not arrived). `p` is the deliberate re-provision, `B` is the catch-up |
| Everything froze | it should not — polling is backgrounded. If it truly hangs, `Ctrl-C` loses nothing: restart with `make tui` and the ledger is intact |

## Nuclear options (all safe, in order of severity)

1. **Restart the TUI** — pure viewer, loses nothing but local log lines.
2. **Restart the inductor** (`X`, then `B`) — the ledger is on disk; workers
   ride through and re-register. In-flight tasks are re-queued automatically.
3. **Restart a worker** — its running task is reaped (~90 s) and re-offered.
4. **Force a stage** — in the K ledger, `F` deletes that stage's artifact so it
   re-runs end to end (a re-render speaks only the units the box running it does
   not already hold). A forced merge whose chapter has no published mp3 forces
   its render too, because the merge makes none of its own input.
5. **`requeue` op** — frees everything stranded on dead workers without
   waiting out leases. Attempts are kept (it unsticks, it does not forgive).
