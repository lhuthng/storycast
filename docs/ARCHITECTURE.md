# Architecture — how Storycast works under the hood

Companion to the [README](../README.md), which is the "how do I run it" guide.
This one is the "why is it built this way" guide. Everything here describes
code that exists in this repo — five Rust crates plus a Python enrollment
tool — not aspirations.

```mermaid
flowchart TB
    PROTO["bm-proto<br/>wire types shared by everyone<br/>Task · Stage · Op · Machine · Roster"]
    CORE["bm-core<br/>the pipeline: crawl · digest · cast · voices/pool<br/>assemble · ambience · ETA · provisioning<br/>(library, no binaries)"]
    AGENT["bm-agent<br/>the worker: runs one stage, reports back<br/>(bin: bm-agent)"]
    IND["bm-inductor<br/>the orchestrator: scheduler + control API<br/>+ provisioner + AWS + TUI<br/>(bin: bm-inductor)"]
    TTS["bm-tts<br/>the TTS sidecar: Vieneu voices over HTTP<br/>(bin: bm-tts, cross-built with zig)"]
    PY["python/<br/>voice enrollment + the retired sidecar<br/>(kept for reference)"]

    PROTO --- CORE
    PROTO --- AGENT
    PROTO --- IND
    CORE --- IND
    CORE --- AGENT
    AGENT -.->|"serves it"| TTS
    IND -.->|"pushes it, per machine"| TTS
    PY -.->|"enrollment only"| CORE
```

`bm-tts` is the fifth crate and the one that used to be Python: serving is a
Rust binary now, cross-built with `zig` and pushed to each worker with its ONNX
runtime. The `python/` tree that remains is the enrollment tooling — the last
thing still needing an interpreter, until that is ported too. It is not on the
serving path, so a worker needs no Python at all.

## 1. One idea: work is a ledger of tasks, not a loop

Every unit of work is a `(stage, chapter)` pair held in one place — the
**ledger** (`.bm/ledger.json`) — with one of six states:

```mermaid
stateDiagram-v2
    direction LR
    state "Pending" as P
    state "Assigned" as A
    state "Running" as R
    state "Done" as D
    state "Shelved" as S
    [*] --> P
    P --> A: offer
    A --> R: the worker starts beating
    R --> D: report ok
    R --> P: report fail — attempts + 1
    A --> P: lease expired, or the worker went silent
    R --> P: lease expired, or the worker went silent
    R --> S: 3 strikes
    S --> P: "u" — retry, strikes forgiven
```

The two edges back to `Pending` are **not** the same edge, and the difference is
the whole failure policy: a report that fails costs a strike, silence costs
nothing.

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
worker is enough for it to be picked up again, because the inductor is the one
asking — there is no registration it has to get back in on (§7).

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

### Two scopes: the book, and the machine

`Layout` is a **pair** of paths, not one, and almost every bug in this area is
somebody using the wrong half:

```mermaid
flowchart TB
    ROOT["root — the checkout<br/>machine-global, shared by every book"]
    WORK["work — the active workspace<br/>one book's state"]
    ROOT --> R1["machines.json · roster · voice refs + samples"]
    ROOT --> R2["assets/ · prompts/ · models/ · profiles/ · tools/"]
    ROOT --> R3[".bm/profile · .bm/aws.json · .bm/aws/"]
    WORK --> W1["settings.json · ledger.json"]
    WORK --> W2["data/ · output/ · scratch/"]
```

* **`work`** — the book: `ledger()`, `settings()`, `data()`, `script(n)`,
  `cast(e)`, `seg_dir(e, n)`, `bible()`, `output()`, `scratch()`. These are
  `workspaces/<name>/…` when a workspace is selected, and `root` itself when one
  is not.
* **`root`** — the machine: `machines()`, `roster()`, `voice_refs()`,
  `voice_samples()`, `assets()`, `pools()`, `scene_map()`, `prompts`, `models/`,
  `profiles/`, `tools/`, and the `.bm/` pointers (`.bm/profile`,
  `.bm/active-workspace`, `.bm/aws*`).

Three constructors, and **picking the wrong one is silent**:

| | |
|---|---|
| `Layout::new(root)` | `work = root`. **Tests and legacy mode only.** |
| `Layout::resolve(root)` | Follows `.bm/active-workspace`; **refuses** a stale pointer. Every *runner* — `serve`, `provision`, `bm-agent`, `roster`, `segments`, `digest`. |
| `Layout::resolve_or_root(root)` | Falls back to the root and hands the error back. **The management plane only** — the dashboard and `workspace`, which must open on the broken pointer they exist to repair. |

No pointer file means this root *is* the workspace: a fresh clone just works and
state appears under it on demand. Only a *stale* pointer is an error, because
silently running at the root would scatter one book's state where another was
expected. A **worker** root (`$HOME/bm-worker`) is flat — no pointer, so
`work == root` there too, which is what makes the same binary work on both ends.

### The profile is the other pointer, and it is verified

`assets/` and `prompts/` are the *profile*: the live, git-ignored tree that
decides how a book sounds and how it is dramatized. `.bm/profile` names which
profile the tree claims to be, plus the **hash of its contents**.

`bm_core::profile::verify` is the load gate: the pointer must exist **and** the
live tree must still hash to what it claims. Anything that *runs* calls it
first; the TUI, which loads and switches profiles, does not. That asymmetry is
deliberate — a dashboard that refused to open because the tree drifted could not
be used to fix the drift.

The hash is computed as sha256 over `path + NUL + content-hash` lines in sorted
order, and the parallel version **must stay byte-identical** to the sequential
one: every box's pointer was computed that way, so a different order would read
as false "profile drift" on every machine at once.

This is why every box's marker tag records the profile hash it was launched for
— and why `:profile` → `load` is a **required step before `:up`**. A box is a
faithful mirror of one profile; launching one into a pool that has since changed
profiles is the mismatch the tag exists to catch.

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
  which removes any read-modify-write race between machines. A digest that
  lands a *changed* script invalidates the chapter's render+merge (segments
  and mp3 go, both tasks requeue fresh) — otherwise the kept render would
  speak the old dramatization under the new one. The "analyzer" is
  pluggable (`opencode | openrouter | gemini | local`) with a fallback chain
  over models.
* **render** (`bm-agent/src/tts.rs` + `python/`) — speaks each script segment
  through the engine. Vieneu runs as an HTTP sidecar on `127.0.0.1:8818` per
  machine. The inductor owns `data/audio/segments-<engine>-NN/` — it is the
  only copy of any segment; a worker's copy is scratch. A render offer names
  **every** unit of the chapter (`render_units`, planned with the same
  `expected_wavs` the merger uses), never the difference against the
  inductor's store: the offer goes to whichever box asks next, and the
  inductor cannot read that box's disk. The worker skips the units it already
  holds (`pending_units` in `bm-agent`, the same present-and-non-trivial test
  `assemble` applies) and speaks the rest. A partial offer is what used to
  leave a box holding a strict subset of a chapter, which the merge then
  pinned to that box failed on as `N segments missing`. Non-local workers
  upload each wav via `POST /api/segment` and discard their copy once the
  report is accepted, while the local node writes straight into the store. A
  render report is gated on the files being present — the worker's word is not
  evidence. The report's `units` counts real TTS calls (cache hits excluded),
  which feeds the ETA model. Only workers advertising the `render-segments`
  capability are offered renders.
* **merge** (`assemble/`, `ambience.rs`) — concatenates segments with
  `gap_ms` pauses and optional ambience beds keyed by the script's `scene`
  labels, and writes `output/Ch.N - Title.mp3`. A merge task carries
  **affinity** for the box that rendered its chapter, because a box's seg dir
  is where those wavs were written. Affinity is an optimisation for remote
  boxes and never a gate for the local node, which shares the inductor's
  store and may take any merge; that exemption is also why a chapter whose pin
  is dead still merges. A local merge ships no mp3 — the file itself is the
  evidence. A remote merge (pre-migration affinity, or no local worker alive)
  ships its mp3 home, base64, inside the report.

## 3. The control API (bm-inductor, axum, default :8901)

| Endpoint | What it does |
|---|---|
| `GET /api/state` | The whole world for the TUI: `tasks`, `machines`, `beats`, `counts`, `settings`, `events` |
| `POST /api/offer` | A worker asks for work; answers with a task offer (or nothing) |
| `POST /api/complete` | A worker reports done/failed (+ artifacts: script, text, bible delta, mp3) |
| `POST /api/segment` | A non-local worker uploads one rendered wav (name validated against the expected set) |
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
       voices unchanged: skip `ensure_voices`; the sidecar and its ONNX runtime
       already staged: skip the ~700 MB push of `models/`. Local copies compare
       size+mtime per file, exactly like rsync.
4. **write the stamp**, start the TTS sidecar if it is not answering, and
   re-probe so the TUI shows the post-provision truth.

**On a box that is already configured, the installers are not run at all.**
`may_install(configured, force)` gates `ensure_opencode` and `ensure_ffmpeg`:
the first provision on a fresh box installs, and every later one only asks
`command -v` whether the tool is there. Without that, a re-provision of a
perfectly healthy box re-ran `npm i` (bounded at 600 s) and `apt install -y`
for no reason — which is most of what made `B` feel slow on a cluster that was
already working. The check-only path says "force a re-provision to try again"
when a tool is missing, so the remedy is named rather than silently skipped.

### What `B` does now, and what it used to do

It used to provision every registered machine inside the start job (a `JoinSet`),
and only start the backend once every machine reported ready — a *provision-gated
start*, so "started" always meant "ready". That is gone, and the reason is the
one this repo keeps rediscovering: **a job's duration was being used as a
guard.**

The start is now **degraded**: `B` brings the backend up in seconds, then hands
each box that still needs work to the dashboard as a **job of its own** and
ends. So:

* The press returns immediately, and a five-minute push to one box no longer
  holds the cluster lane — nor queues everything behind it.
* Boxes catch up **concurrently**, because each catch-up job holds
  `Res::Box(addr)` and nothing else.
* A failing box lands in `Error` with its reason and **never vetoes the rest**.
* A box already `online` is **skipped and said out loud** — `online` is the state
  this path exists to reach, and it is *working*. Re-provisioning it anyway is
  what made `B` on a healthy cluster take minutes. `p` is the deliberate
  re-provision.

The trade is real and worth naming: "started" no longer implies "every box
ready". The footer says which boxes are still catching up instead.

### Host keys are not verified, on purpose

Every box here is either an instance launched minutes ago or a worker linked by
hand, and every call is scripted (`BatchMode=yes`), so the "continue
connecting?" prompt becomes a hard `exit 255 — Host key verification failed`.
A fresh instance always presents a key nobody has seen.

So the transport declines verification and never reads or writes `known_hosts`.
That is not only about first contact: AWS hands the same public IP to a
different box later, and `StrictHostKeyChecking=no` alone still refuses a
*changed* key. `~/.ssh/config` is still read — an `-o` overrides one option, it
does not replace the file — so Host aliases, `ProxyJump` and `IdentityFile`
keep working. The policy is one constant (`HOST_KEY_OPTS`) used by **both** the
direct `ssh` and the `ssh` that `rsync` spawns through `-e`; setting it on one
transport only would fix the probe and leave every push failing identically.

### Machine states, and the gates that read them

A machine's state is a *decision*, not a label: `unknown` (never contacted),
`initializing` (created, not yet answering), `probing`, `provisioning`,
`configured` (has everything it needs, no worker beating yet), `online` (a
worker is answering), `offline` (was answering, now silent), `error`. Two gates
read it:

* **`accepts_work`** — only `online` is handed tasks. Every other state is a
  deliberate "not yet", and offering work into one is how a task lands on a box
  that cannot run it. `unknown` passes separately, as "no opinion formed": a
  hand-written ledger or the legacy pull worker asking before its first beat,
  neither of which may be stranded.
* **`coming_up`** — `initializing`/`probing`/`provisioning`/`configured`. The
  dispatcher stamps `offline` on any box that fails to answer `/status`, and
  must not do that while the box is still on its way up: a box twenty seconds
  into its first boot is not gone, and calling it gone is how a freshly launched
  pool looks broken.

`initializing` is the only state with a deadline (`BOOT_DEADLINE_SECS`, five
minutes). A state with no exit condition is a lie — a box terminated before it
booted, or launched into a subnet this machine cannot dial, would sit there for
ever — so it becomes `error`, with the reason in the note. `state_since` is what
makes both the deadline and the overlay's `state age` line possible.

The third reader is the *failed provision*. `verdict_after_failed_provision`
(`tui/jobs.rs`) decides what a failed run leaves behind, and it separates two
outcomes that look identical from the operator's chair. If ssh never answered
**and** the box was already `initializing`, nothing was learned — a probe cannot
tell a booting box from a dead one — so it stays `initializing` and the boot
deadline is what gives up. If ssh *did* answer, the failure is real (missing
python, full disk, failed push) and it is `error` whatever the clock says. Both
the `:prov` retry and the catch-up job `B` dispatches for each box ask it,
because `B` is the automated path and runs seconds after `:up` — the likeliest of
all to meet a box mid-boot.
Restoring `initializing` re-stamps the clock, which is the right rule: while the
operator is retrying, somebody is watching the box.

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
tui/jobs.rs   background jobs: the `Job` enum, the resource scheduler
              (`run_jobs_with`), and one function per job
tui/audio.rs  the speaker: one reused temp file, played by afplay
tui/audition.rs the line index and the chooser behind "hear a real line"
tui/sound.rs  the three clip pools, what each entry is used for, and the
              prompt/registry edits — pure, so the guard is testable
tui/input.rs  the modal key chain, in order, then normal::normal_key
tui/input/    one file per modal block; `audition.rs` is the shared
              four-key audition decision both voice screens call, and
              `cloud.rs` / `policy.rs` are the AWS ones
tui/draw.rs   the tier dispatch and the overlay match
tui/draw/     one file per pane or overlay — `cloud.rs` is the pool view,
              `policy.rs` the IAM policy view
tui/tests.rs  every test
```

Outside the TUI, the modules the cloud work added:

```
dispatch.rs        the inductor-drives loop — asks every worker, hands out work
aws_ops.rs         one implementation per AWS verb, shared by CLI and TUI
state/observe.rs   what the inductor records when a worker speaks to it
state/relink.rs    keep EC2 boxes pointed at the address they carry now
provision/aws_credentials.rs  the credential store and the verify-then-write order
```

To follow a key press: `input.rs` → `input/<screen>.rs` → `jobs.rs` →
`app.rs` → `draw.rs` → `draw/<pane>.rs`.

### The job scheduler: queue on a resource, not on a lane

**What it replaced.** `Job::lifecycle() -> bool` classified a job as
"lifecycle" and pushed it into one serial lane shared with every other lifecycle
job. That is a *classification*, used as if it were a *dependency*, and the two
are not the same thing: `aws discover` queued behind a five-minute box push, and
a provision of box A queued behind one of box B, because all three had been
filed under the same word.

`Job::resources() -> Vec<Res>` names what a job actually touches:

| `Res` | held by | means |
|---|---|---|
| `Command` | everything else | the default lane — the old command lane, still serial among its own members |
| `Cluster` | `StartBackend`, `StopBackend` | the backend and the fleet as a whole; `B` and `X` must never interleave |
| `Box(addr)` | `Provision { machine }` | **one box's** ssh/rsync channel |
| `Aws` | `AwsUp`, `AwsDown`, `AwsLogin`, `AwsDiscover` | the account, and the `.bm/aws/` document it is written into |

`Res` is `Ord` so a job naming several takes them in a stable order — two jobs
with overlapping sets then queue rather than deadlock.

```mermaid
flowchart TB
    Q["pending — every queued job,<br/>with the resources it needs"] --> SCAN{"anything it needs<br/>already busy?"}
    SCAN -->|no| RUN["start it<br/>busy takes its resources"]
    SCAN -->|yes| WAIT["leave it queued,<br/>scan the next one"]
    RUN --> FIN["the job finishes"]
    FIN --> REL["release its resources —<br/>the scan runs again"]
    REL --> SCAN
```

The scan re-runs **from the head after each launch**, and that is not a detail: a
later job may fit where an earlier one did not, so skipping past it would be
exactly the unnecessary queueing this exists to remove. FIFO is still preserved
among jobs that genuinely contend.

A job that names nothing conflicting is `Command`, and that lane stays serial on
purpose — two model-loading previews at once is not a thing anyone asked for.

**The queue is visible.** `Job::resource_label()` drops `Command` (true of most
jobs, worth nothing on a row) and the jobs screen renders the rest, so a blocked
row says `queued · needs box 10.0.0.5`. A job that shows a resource is a job that
can be *blocked*, and the row answers "why is this not running" without a log.

**Two guards were durations, and the job got shorter.** Both were attached to how
long `start backend` took, and both broke silently when it stopped taking
minutes:

* `App::backend_start_outstanding` spanned the whole catch-up loop, and that is
  what refused a second `B`. With the start job ending in seconds it released
  immediately, so a second `B` would have queued a duplicate push at **every**
  box. `App::catchup_jobs` — the ids of the catch-up provisions still running —
  now holds it open until the last one finishes.
* `DoneKind::StartDone` used to clear the `X` cancel flag. But `StartDone` now
  arrives *while* the catch-up provisions are still running, so clearing it would
  leave `X` with nothing to set and a mid-push provision would launch its worker
  anyway. It is deliberately **not** cleared.

Same lesson twice: **a guard scoped to a job's duration is not a boolean, and
shortening the job breaks it.** `Ev::JobFinished` releases the flag, and
`StartDone` *reads* `!catchup_jobs.is_empty()` rather than assigning, so the two
events can arrive in either order without losing it.

**The TUI is the operator's only interface.** Anything an operator has to do to
run the cluster — launch, link, provision, start a worker, terminate, store the
AWS key, read the account into the pool, load a profile — belongs on a `:`
command or a screen. A shell command in a guide is a gap to close, not a
workflow; the only step outside the dashboard is the AWS **console** (creating
the IAM user, its access key, the keypair and the security group), because AWS
offers no other way to create them.

One implementation per verb still holds: every `:` command calls the same
`aws_ops` function and `Job` the CLI does, never a second copy — `aws_ops::login`
and `aws_ops::discover` are what both `aws login` and `:login`, `aws discover` and
`:discover` run. The flags for those two are defined **once**, as clap `Args`
structs (`aws_ops::LoginArgs`/`DiscoverArgs`): the CLI derives its subcommand
from them and the TUI parses the prompt through the same definition, so a flag
cannot mean two things depending on which end it was typed at.

**The sound-design editor (`:sound`) is a screen because its guard needs a
load.** Three registries, the scene map and every `data/script-*.json` decide
what may be removed, so it is a `Job` like the audition index rather than a
keypress handler — and it is re-run after every save, because the guard is read
off it. The rule it enforces is the one this pipeline keeps having to relearn:
a scene names *tags*, not sounds, so dropping a sound a rule can still reach
makes the scene score zero and go *quiet* instead of failing. "In use" is
therefore a reference — a rule, a palette value, or a chapter's script — and
never "a merge happens to be running", which is a different question with a
different answer. `save_pool` rewrites only the entry that changed: the
registries are hand-formatted and their `_note` is the only written record of
why a pool is shaped the way it is, so an untouched pool round-trips byte for
byte.

**Auditioning a voice is the one place the TUI makes a sound.** The split is
deliberate and worth keeping: the *inductor* renders (`Op::PreviewVoice` calls the
sidecar and ships the wav back in `OpResult::audio_b64`), and the *TUI* plays,
because the speaker is on the operator's desk and the inductor may be on another
box. Four consequences that are easy to undo by accident:

* The wire carries **bytes, not a path**. A path is only meaningful to a client
  that shares the inductor's filesystem, and it puts the sample on the wrong
  machine — so the client writes it, next to the speaker.
* **One file, reused.** `audio::Player` overwrites a single temp path and removes
  it on drop, so auditioning twenty voices leaves one clip behind rather than
  twenty. The inductor writes nothing at all: an audition is not a pipeline
  artifact and has no business in `data/`.
* Playback is **not** a `Job`. `audio::Player` spawns and returns. Two reasons,
  and the second is the one that survives the scheduler change: a five-second
  sample is immediate feedback the operator is *waiting on*, and the lane it
  would land in (`Res::Command`) is deliberately serial — so parking a sound
  there queues it behind whatever else is in that lane for no benefit.
* The in-flight marker (`App::audition`) lives on the `App`, not on a screen —
  "one render at a time" is a property of the process, since both the picker and
  the cast overview can start one.
* `dispatch_op` returns whether it actually dispatched, and the marker is set
  **only** on success. A refused dispatch sends no `Done`, so a marker set
  regardless would never be cleared and the screen would wedge behind a render
  that never started.

Because `App::pending` is incremented by every dispatch and decremented only by
`Ev::Done`, **every arm of `run_job` owes exactly one `Done`** — a job that
reports its payload without one leaves the footer claiming work is running for
the rest of the session.

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

## 7. The transport: the inductor drives, nothing dials it

Every worker is a small HTTP server that answers questions. `dispatch.rs` is the
other half — the loop that asks. **A worker is never told where the inductor is**,
and `lifecycle.rs` asserts the launch script never contains the argument that
would tell it.

```mermaid
sequenceDiagram
    autonumber
    participant I as inductor · dispatch.rs
    participant W as worker · bm-agent
    I->>W: GET /status — every 2 s, 5 s deadline
    W-->>I: Heartbeat: worker_id · task_id · capabilities
    Note over I: observe() — one entry point,<br/>shared with the pull protocol
    I->>W: POST /task — the offer, and NO deadline
    Note over W: the response IS the stage
    W-->>I: Complete: artifacts, unit count
    I->>W: GET /unit?chapter&engine&name — only the wavs it is missing
    W-->>I: the bytes
    Note over I: units are collected BEFORE the completion is applied —<br/>the render gate reads the filesystem
    I->>W: POST /shutdown — on the idle timeout
```

Four consequences, each of which is easy to undo by accident:

* **`POST /task` has no timeout on purpose.** The response *is* the stage's
  outcome, and a render takes as long as it takes. Timing out would abandon a
  task the worker is still running, and the lease would then strike it — a
  self-inflicted failure.
* **The task runs on its own task, not awaited inside the poll loop.** Awaiting
  it would stop `/status` for the whole stage — twenty minutes for a render — and
  a beat older than 90 s is exactly what the reaper's orphan pass reads as
  "worker gone". That is not hypothetical: it requeued a digest the worker was
  45% through, then rejected the report as stale.
* **One entry point for liveness.** `Inner::observe(&Heartbeat)` sits behind
  `/status`, `/api/register` *and* `/api/heartbeat`. Two copies would let the
  machine state, the worker map and the capability list disagree depending on
  which way the report travelled, and the panes would show whichever arrived
  last. The counterpart is `Inner::note_silence`, named and separate because
  *nobody answered* has a case that is easy to get wrong: a box that is coming
  up cannot answer, and silence about it is not news.
* **`.no_proxy()` on the client.** Worker addresses are loopback, LAN, or a
  cloud private network. An ambient `HTTP_PROXY` answering in their place is a
  trap this repo has already paid for three times.

**Why the direction was inverted.** The pull protocol required the inductor to
be reachable *from* every worker — which a box on the public internet cannot do
to a laptop behind NAT, and which forced a local/remote fork through the
launcher, the offer *and* the artifact path. Inverting it removes the
requirement instead of working around it: the inductor already has a route to
every worker, because it launched them. The pull protocol still exists for a
worker given `--inductor`; it is the transition path, not the design.

**Idle auto-off.** `Settings.idle_mins` (default 5, `0` disables) shuts the
cluster down when there is nothing to do. `Inner::idle()` is deliberately not
`busy()`: a shelved crawl leaves digest/render/merge `Pending` for ever, so
`busy()` stays true in exactly the case the timer exists for.

## 8. The cloud plane: EC2 boxes as ordinary machines

The design goal is that **a cloud box is not a special kind of worker.** It is
linked, provisioned and driven by the same code as a LAN box; the AWS half only
creates it and gives it an address.

```mermaid
flowchart LR
    LOGIN[":login<br/>the IAM user's key"] --> DISC[":discover<br/>read the account into .bm/aws.json"]
    DISC --> PROF[":profile load<br/>REQUIRED — the tag records this hash"]
    PROF --> UP[":up 3<br/>launch + link, the one command that spends money"]
    UP --> B[":B<br/>catch-up: provision + start, one job per box"]
    B --> WORK["boxes render chapters"]
    WORK --> DOWN[":down<br/>terminate by explicit instance id"]
    DOWN -->|"or idle_mins elapses"| OFF["stopped"]
```

* **One identity, and it is not yours.** The app runs as an IAM user created for
  it, with the key in the ignored `.bm/aws/credentials` (0600, AWS's own INI
  format). There is **no fallback to the ambient AWS identity**: no stored user,
  no AWS call. `aws show` names the user it will act as. `.env` and the shell
  cannot override it — see [AWS-CREDENTIALS.md](AWS-CREDENTIALS.md).
* **The tag is the safety boundary, in two places at once.** Every box carries
  `storycast-worker` = the profile hash it was launched for. `aws-policy.json`
  scopes `ec2:TerminateInstances` to `aws:ResourceTag/storycast-worker` **and**
  `:down` terminates by **explicit instance id**, never a filter. Either alone
  would be enough to be careful; both together mean the tool cannot touch a box
  that is not yours even if one of them is wrong.
* **`PassRole` is scoped too** — to the one worker role, and only when passed to
  `ec2.amazonaws.com`. The instance profile is what lets a box pull its own
  asset plane instead of receiving a 668 MB upload from your connection.
* **Addresses rotate, so the registry is repaired, not trusted.** An EC2 public
  address changes on every stop/start and every spot relaunch, while the
  instance id is stable for the box's whole life. `state/relink.rs` reconciles
  the registry against one account listing: a box whose address moved is re-keyed
  to the address it carries now, and an agent-reported *private*-address ghost of
  the same instance is folded into the real entry. Every repair is returned as a
  log line, so the events pane explains what changed and why. **No operator
  selection is involved** — that is the point.
* **The instance type is chosen for RAM, not CPU.** The TTS path is a
  hand-written SIMD matvec with no GPU code, so 2 vCPU is the floor — but the
  sidecar is **~2.85 GB resident the moment the weights load**, so a 4 GiB box
  does not fit. 8 GiB is the size to use. Two cautions from that measurement: a
  macOS/arm64 build of the same binary idles at ~1.0 GB, so **measuring on the
  wrong platform understates it by ~2.8×**; and `models/` is 668 MB on disk, so
  **never size a box from `du`**.
* **Spot is a good fit.** Render and merge are idempotent and the lease reaper
  requeues an interrupted task, so a reclaimed box costs a retry rather than a
  lost chapter. It needs `AWSServiceRoleForEC2Spot`, which the operator user
  cannot create — request one spot instance once in the console, or set
  `"spot": false`.
* **Not built: S3.** `output/` and the segment store are still local. The
  `SegmentStore` trait (`bm-core/src/segments.rs`, `LocalStore` the only
  implementation) exists so `S3Store` is an implementation rather than a
  rewrite; see [ROADMAP.md](ROADMAP.md).
