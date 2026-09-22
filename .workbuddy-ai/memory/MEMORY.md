# beyond-myriads-converter — durable project notes

**Index, not the record.** Long form: `docs/ARCHITECTURE.md` (§7 transport, §8
scheduler), `docs/AWS-*.md`, the README, and the daily logs beside this file. Only
what the docs do not carry, and what has cost time twice, is here.

## Rules that outrank everything

- **Destructive anything: explicit command, never an automatic side effect.**
- `session-*.md` and `.workbuddy-ai/` are **not ignored and must not be** — "do not
  add them", not "hide them". They stay `??`, so a bare `git add -A` is unsafe:
  `git add -A -- . ':!session-*.md' ':!.workbuddy-ai'`.
- **TUI-first.** Anything an operator must do belongs on a `:` command or a screen —
  never a shell line, never a raw `aws` CLI call. The AWS console stays browser work
  because AWS offers nothing else. `main.rs::aws_cmd` *delegates* to `aws_ops`.
- Discipline rules (committing is an instruction; a resumed-session prompt is never
  an instruction to act; "plan X" goes in the reply) live in the user-level
  `MEMORY.md`.

## Layout

- `{root, work}`: per book → `work` (ledger, settings, stats, `data/`, `output/`);
  machine-global → `root` (`assets/`, `prompts/`, `models/`, `profiles/`, `tools/`,
  `.bm/*`). `Layout::new` is tests/legacy; `resolve` refuses a stale pointer;
  `resolve_or_root` must open on the broken one it exists to repair. A *worker* root
  is flat. `roster()` is `.bm/voices.json` — `voices.json` **at the root** is the
  clone manifest. `prompts/` + `assets/` are gitignored live content; tests use
  `rust/fixtures/profile/`.
- `output/` is the deliverable; `data/audio/segments-*` is **provenance, not a
  cache** — TTS is stochastic, re-rendering does not reproduce the mp3.
  `assets/*-pool.json` and the scene map are hand-formatted with a `_note`: never
  re-serialise one.
- **rustfmt: never `cargo fmt --all`** — it walks `rust/vendor/sea-g2p/` (470 hunks,
  412 vendored, against 58 real). Use `rustfmt --config skip_children=true`, and read
  the hunk, not the exit code.

## Traps

- **A `MutexGuard` held across an `await` is the whole family of hangs here.**
  `std::sync`'s makes the future non-`Send`, and axum reports an opaque
  `Handler<_, _>` bound error that never mentions `Send`; `tokio`'s gives no warning,
  it wedges. `if let Some(x) = lock().await.thing()` holds the guard for the whole
  block — bind the result first.
- **No `\|` in a grep here.** BSD grep has no BRE alternation, so the pattern matches
  *nothing* and an empty result reads as "no hits". Use `grep -E`.
- **A poll loop that awaits the task stops being a poll** — `/status` stops, the beat
  ages past the reaper's 90 s window, and the orphan pass requeues the task the worker
  is executing.
- **A failed stage is an answer, not an error status**: both directions report
  `Complete { ok: false, .. }`. A 500 makes the inductor decode a text body as a
  `Complete` and drop the task.
- **Grep `reqwest::Client` before trusting a loopback path** — with `HTTP_PROXY` the
  proxy answers instead. `HTTP_PROXY` catches curl too: `curl --noproxy '*'`.
- Smaller: `pgrep -fl "bm-inductor|bm-agent"` (no path) finds the live processes; a
  0-byte `.git/index.lock` is a mutex nobody holds (`ps` is not permitted);
  `spot: true` fails until a spot instance has been requested once in the console;
  provisioning must push `bm-tts` **and** its shared ONNX Runtime under every name
  the linker/loader ask for (`make runtime`); a *worker* no longer receives
  `python/` while the inductor-side `Layout::venv_python()` path is still live.

## The profile gate (2026-09-21) — a live foot-gun

`profile::verify` hashes the live `assets/` + `prompts/` against `.bm/profile`.
**`:sound` writes `assets/inject-pool.json`**, so every sound edit trips the gate and
the daemon refuses to start. **The repair the error names is the wrong one**:
`profile load` / `unpack` does `rm -rf assets prompts` and re-extracts the bundle,
reverting the very edit that caused the drift; `pack` alone re-bundles the live tree
but does **not** write the pointer. Correct, cluster stopped: **`:profile pack <n>`
then `:profile <n>`**. Diagnose by unpacking the bundle to a temp dir and diffing
manifests — pointer, bundle and `manifest.json` agree, and only the edited files differ.

## The mix is applied at merge, not render

`assemble()` calls `ambience::injects_of`, so effect/inject/music land when segments
are assembled — the Merge stage. A mix or pool change needs merges only (`:remerge` =
`op_remerge_all`, render cache kept); **never** `invalidate_render`, which deletes the
stochastic `segments-*/`.

**A stamp requeue can be far wider than the edit.** `invalidate_stale_design` with
`adopt=false` (what `Op::SoundChanged` passes) requeues every `Done` merge whose stamp
is `None`. A one-clip edit reaching 4 chapters requeued all 100, because the library
predated the stamp field.

## Transport, scheduler, AWS — only the non-doc facts

- **Host keys are never verified, and `known_hosts` is never touched**
  (`HOST_KEY_OPTS`). `StrictHostKeyChecking=no` alone still refuses a *changed* host,
  which is what a recycled AWS IP looks like. **The same const must feed both
  transports** — `rsync` spawns its own ssh via `-e`.
- `state_since == 0` = "predates the field" → adopt, never expire.
  `machine_from_instance` is born `Initializing` for both `pending` and `running`;
  after a good `:prov` the state is **`Configured`**.
- **The scheduler queues on a resource, not a lane** (`Res::{Command, Cluster, Box,
  Aws}`, `Job::resources()`). Two guards were durations, and shortening
  `start backend` broke both.
- **Merge `affinity` is spread over the boxes, and a remote merge fails "N segments
  missing"** because the segment cache is local; the local box succeeds on retry.
- **One AWS identity, console-only setup**, two app commands (`aws login --csv`,
  `aws discover --region --pem`); never `aws` CLI. `.bm/aws/credentials` (0600,
  ignored, profile `storycast`) is the only source, and `SHADOWING_ENV` is enforced
  by construction so no call site can skip it. **Names from outside the account**
  (`--instance-profile`, `--pem`, `--security-group`) **are verified before writing
  because those three fail *silently***; `--instance-profile` is needed on most real
  accounts, and `discover` refusing to choose is correct. Sizing is `m7i-flex.large`
  (8 GiB); `c7i-flex.large` is 4 GiB and does not fit. **Never size a box from `du`.**
