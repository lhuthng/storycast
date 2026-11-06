# Cloud workers

Provider-neutral launch of disposable workers: AWS and InstaBox today,
Azure/Oracle later. One `:up`, one smoke test, one capacity plan, one deadline,
one reclaim path.

## Status

In: `bm_core::cloud` (provider, reason, shortfall, plan, launch, `ProviderView`
+ `fate`), the AWS provider (`smoke` → `plan` → one call per box → shortfall
list, `list`, `down`), `:up [provider] [count] [--ttl 7h|90m|0] [--strict]
[--from-machine]`, `:down [provider] [force]`, `:cloud reap`, the same flags on
`aws up`, `expires_at` on every launched box, the box-side deadline watchdog,
the minute poll that reclaims what the provider has lost, the delete queue, and
the packs gate `:up` refuses on, and InstaBox as a second provider (protocol,
capacity, create, port publish, list, delete, `:up instabox`, `:down instabox`),
with its setup: `bm-inductor ibx show|init|login` and `:ibx` in the dashboard.

InstaBox boxes are provisioned too: `bm_core::provision::Exec` drives a machine
over the provider's own exec and files API, so `:up instabox` → `:prov` is the
same flow as EC2 with ssh swapped out.

Not in yet: a provider listing in the Cloud view (it is the EC2 account), a
snapshot built through exec, and any merge on InstaBox.

## InstaBox setup

```
bm-inductor ibx init      # write .bm/instabox.json (mode 600)
bm-inductor ibx login     # paste the workspace key; not echoed
bm-inductor ibx show      # the definition, what is missing, whether the key works
```

In the dashboard, `:ibx` is the same login: the key is typed, proved against
`GET /v1/orgs` and `GET /v1/machines`, and only then saved to
`.bm/instabox.json`. A key the API refuses is **not** left on disk — the
previous one is put back — so a launch never fails on a stale typo. An empty
line clears the key, which turns the provider off. `INSTABOX_API_KEY` still
wins for a CI run that has no file.

## Non-goals

Keeping or resuming a paused box; merging on InstaBox (v1 is crawl/digest/render
only); a web UI; any change to the worker task protocol.

## Where we are

- `:up` is `Command::AwsUp{count}` → `Job::AwsUp` → `aws_ops::launch` →
  `run-instances --count N`; `:down` terminates ids picked in the Cloud view.
- `ttl_hours` exists in `AwsConfig` and is enforced nowhere.
- The inductor *dials* workers (`dispatch::targets` → `http://{addr}:{task_port}`);
  provision, moves and sweeps are ssh/rsync on EC2 and the provider's exec API
  on InstaBox; the hook tunnel is ssh only.
- `run-instances --count N` is all-or-nothing: a quota or capacity refusal fails
  the whole call, so no partial launch exists today.
- Packs have a publish plane (`packs_release` R2 base, `models_release` GitHub
  repo) and `PackRelease::resolve` / `ModelsRelease::resolve` already compare a
  release against the checkout.
- InstaBox has no public IP (`exec` over their API, `ssh` through their
  gateway, `port publish` for HTTPS, `port-forward` for localhost). Native
  limits: `deleteAfterSeconds`
  60–86400, `maxRunMinutes`, idle pause (free: 30 min, and a pause loses
  processes), plan caps (free: 1 machine, 4 vCPU / 8 GiB).

## Provider boundary

`bm-core/src/cloud/` owns the shared types; each provider is a module that
implements:

```rust
trait Cloud {
    fn smoke(&self) -> Result<Smoke>;             // creds + account + limits, no spend
    fn list(&self) -> Result<Vec<ProviderView>>;  // normalised, keeps the raw state
    fn plan(&self, want: u32) -> Result<Plan>;    // capacity math, no spend
    fn up(&self, plan: &Plan) -> Result<Launch>;  // one machine at a time
    fn down(&self, ids: &[String]) -> Result<Vec<Down>>;
}
```

`Smoke` carries the facts later steps interpolate (region, tier, `allowedSpecs`,
quota code and headroom, live count, cap). `Launch` is
`{created: Vec<CloudBox>, skipped: Vec<Shortfall>}`; `CloudBox` carries
`provider`, `provider_id`, `name`, `addr`, `base_url`, `transport`, `expires_at`.

A provider is the only place that knows a cloud's words:

```rust
struct Shortfall { want: u32, reason: Reason, code: String, message: String,
                   fix: Option<String>, docs: Option<String>,
                   retryable: bool, retry_after_s: Option<u64> }
enum Reason { Stop, Clamp, Capacity, Unknown }
```

`Reason` is the only shared vocabulary and shared code never renders it: the TUI
and CLI print `fix` verbatim and fall back to `code` + `message` + `docs` when a
code is unmapped. Fixes are built from live facts — a quota code and region from
`get-service-quota`, `allowedSpecs` and tier from `GET /v1/orgs`, `max_workers`
from config — never from literals. AWS codes are parsed out of the CLI's
`An error occurred (Code) when calling …` line; InstaBox's
`code`/`message`/`retryable`/`retryAfterSeconds`/`docs_url` are copied through.

### Normalised liveness

| Native | `ProviderView` |
|---|---|
| instabox `running` | `Running` |
| instabox `paused` / `stopped` / `stopping` / `failed` | `Stopped` |
| instabox 404 | `Gone` |
| EC2 `running` | `Running` |
| EC2 `stopping` / `stopped` | `Stopped` |
| EC2 `shutting-down` / `terminated` | `Dead` / `Gone` |
| EC2 spot interruption notice | `Stopping` + `reclaim_at` |
| EC2 scheduled retirement / maintenance | `Stopping` + `reclaim_at` |
| EC2 status check `impaired` | `Running` (alive) — suspect, never killed |
| a `list()` that failed | `Unknown` |

### Registry v2

`machines.json` rows gain `provider`, `provider_id`, `base_url`, `transport`,
`expires_at: Option<u64>`, all `#[serde(default)]` so today's file loads. A row
without a provider (hand-added, LAN) is never reclaimed destructively.
`dispatch::targets`, `stop_everything` and the segment sweep prefer `base_url`
over `addr:task_port`.

## Command surface

```
:up                          configured default provider, its default ttl
:up 3                        three boxes
:up aws 4 --ttl 0            four AWS boxes, no deadline
:up instabox 1 --ttl 1h      one InstaBox box, deleted at the hour
:up aws 10 --strict          refuse rather than launch fewer
:up aws 4 --from-machine     allow the uplink, skip the packs gate
:cloud reap                  retry the cloud deletes that kept failing
:down [provider] [force]
```

TTL resolution is per launch → provider config → 7 h, and `0` means no deadline
(`expires_at: None`). The chosen value is written at launch, so a later config
edit cannot re-time a running box. Provider limits are stated, never silently
applied: InstaBox above 24 h has no cloud-side deadline; a free-tier box pauses
after 30 idle minutes, which counts as killed.

One job (`Job::CloudUp`) with steps packs → creds → plan → launch → register.
A failure stops at its step: the packs gate prints one line per gap with the
command that fixes it, and the provider step prints its own `fix`.

## Capacity

`plan()` computes `fit = min(want, our_cap − live, quota_remaining / vcpus_per_box)`
from live facts, then launches one machine per call, so a refusal mid-flight is
attributable and leaves real boxes behind.

```
:up aws 10 — quota allows 6 (32 vCPU, 20 in use, 4 each). Launch 6?
   [Enter] launch 6   [s] strict: launch none   [Esc] cancel
```

Only created boxes are registered. The status line reads
`launched 3 of 10 — 7 not created: quota (fix: …)`, the Jobs pane keeps the full
list, the event log records it once. `--strict` launches none.

`aws-policy.json` `ReadOnlyLookups` gains `ec2:DescribeInstanceTypes` and
`service-quotas:GetServiceQuota` / `ListServiceQuotas`.

## Expiry

1. **Inductor** — warns at T−15 m and at T (once each, marked in the box's note:
   "deadline in 14m", "deadline reached"). The deadline is in `machines.json`, so
   a restart remembers it.
2. **The box itself** — the launch script starts a watchdog beside the worker:
   `sleep <deadline−launch>; pkill -f 'bm-agent.*worke[r]'; sync; sudo -n
   shutdown -h now`. On EC2 `--instance-initiated-shutdown-behavior terminate`
   turns that shutdown into a termination, so a box cannot outlive its work even
   if this inductor is gone. The inductor never starts a worker on a box past
   its deadline.
3. **`--ttl 0`** sets nothing anywhere; only `:down` or a death verdict ends it.
4. **InstaBox** (later) sets `deleteAfterSeconds` as well, so the provider's own
   reaper is the second backstop there.

## Reclaim

`Killed` means "its processes are gone and it will not come back". `Offline`
keeps meaning "was answering, now silent". A reclaimed box with a provider half
leaves the registry entirely (its row names nothing that exists); `Killed` is
what a box with **no** provider half is marked, because it has to stay visible.
The persisted ledger keeps `killed: <why>` in the event log either way.

```rust
enum Fate { Keep, Suspect{since}, Dead{why} }
fn fate(view, last_beat, expires_at, now) -> Fate   // pure, table-tested
```

| view | beat | verdict |
|---|---|---|
| `Running` | fresh (<90 s) | `Keep` |
| `Running` | stale | `Suspect` → `Offline`; never deleted |
| `Stopped` / `Dead` / `Gone` / spot-or-retirement `reclaim_at` reached | any | `Dead` |
| any, past `expires_at` | any | `Dead` |
| `Unknown` (the poll failed) | any | `Suspect` only — a failed poll never deletes |

`Inner::reclaim(addr, why)`:

1. take the work back (below) and refuse the address from here on,
2. `remove_box` + the registry row, so nothing is offered to a dead box,
3. enqueue a delete intent `{provider, provider_id, addr, why, attempts,
   last_error, created}`, persisted with the ledger.

`Killed` is written instead of removal for a box with **no** provider half: it is
kept, dimed in the pane, and said to be dead — an operator's to `:drop`, never a
cloud delete.

There is no separate worker shutdown on this path: every verdict means the
box's processes are already gone (paused, stopped, deleted, or past a deadline
its own watchdog saw), so the delete *is* the shutdown.

One sweep a minute polls each provider that owns a box (`describe-instances` on
the exact ids, no state filter), builds `fate` per box, and then settles **one**
owed delete — InstaBox `DELETE` + `X-Confirm-Delete`, AWS `terminate-instances`
— treating `InvalidInstanceID.NotFound` as success, which is the "or will be
killed" case the provider backstop also covers. Intents survive a restart; past
five failures the line reads `delete failed (5): <code> — not retrying` and
`:cloud reap` gives them a fresh set and sweeps immediately. `:up` refuses while
anything is owed: the provider has not released the capacity yet, so a launch
would be asking for a quota refusal.

## Taking the task back

1. **Only this box's share.** `release_held_rows` keeps racing rows with their
   other holders; a batch offer returns all N rows it covered.
2. **Strike-free.** `requeued: box killed (idle-paused)`, no strike, no shelve.
   Strikes stay for "attempted and failed", or a flaky cloud shelves a chapter.
3. **A report that landed wins.** The report inbox is processed before the
   verdict; a row already `Done`/`Failed` is left alone.
4. **Requeue before delete.** shutdown → take-back → `Killed` → registry drop →
   intent. A row never sits leased to a box being deleted.
5. **Box-local artifacts are assumed gone.** Nothing in the ledger points at a
   take on the box; the `.incoming` copy makes a partial unable to look
   complete, and a requeued merge republishes and sweeps the older pair.
6. **No double release.** Reclaim clears the beat/caps/queue mapping and marks
   the address dropped; release is idempotent anyway.
7. **Live box, dead process** is the lease reaper's job, with its normal strike
   counting. Reclaim fires only on `Stopped`/`Dead`/`Gone`/past-TTL.
8. **Cold start.** The verdict is rebuilt from the registry's `provider_id` on
   the first poll; the worker→addr map resolves holders without a live beat.

## Credential smoke test

- **AWS**: `sts get-caller-identity`, one tagged `describe-instances`,
  `AwsConfig::missing()`, security-group ingress for 22/8917, subnet route, AMI
  for the region, `describe-instance-types` for vCPU math.
- **InstaBox**: `GET /v1/orgs` (tier, `machineLimit`, `allowedSpecs`,
  `maxRunMinutes`, `idlePauseMinutes`), then `GET /v1/machines`; the key is the
  definition's `api_key` or `INSTABOX_API_KEY`.

A failure rejects the command with that provider's own `fix`: no creds →
`aws login`; expired key → new CSV; no region → `discover --region`; closed SG →
add inbound; `invalid_key` → `ibx login`; 402 → the dashboard page;
`spec_not_allowed` → the workspace's own `allowedSpecs`.

One refusal is ours, not the provider's: **a box under 8192 MiB of RAM never
launches.** The TTS sidecar holds ~2918 MiB resident as soon as its weights
load (measured on linux/x86_64, in `aws.default.json`'s note), so a 4 GiB
machine — ~3.9 GiB usable — has no room for it plus the agent and the OS. AWS
reads `MemoryInfo.SizeInMiB` together with the vCPU count in one
`describe-instance-types` call; InstaBox compares the `spec` it resolved before
the create. The fix names a size that actually fits: the workspace's own
`allowedSpecs` for InstaBox, `instance_type` for AWS. A type the role cannot
describe is not a refusal — it launches unguarded rather than guessing.

## Packs preflight

`cloud::pack_gaps(layout, settings) -> Vec<Gap>` is the `packs` step, and it is
the same two resolutions provisioning makes before it decides to push:

1. `models_release`, resolved exactly as provisioning resolves it: a preset
   bucket (`https://…`) answers `preset/models/<engine>/latest.json` and its
   `manifest_hash` must be this checkout's bake; an `owner/name` repo must have
   `ModelsRelease::resolve` find this bake's hash. Either way a foreign bundle
   would land and fail its own checksum;
2. the profile pack is published: an `owner/name` repo with the pointer's
   `<name>-pack-v<version>`, or a preset bucket whose
   `PackRelease::in_bucket` finds `preset/sounds/<name>/latest.json` (covers
   `assets/` — clip registries, scene map, prompts, `voices.json`); a workspace
   composing its own `assets/` is not a gap, its tree ships in the sources
   bundle;
3. every `voices/packs.json` pin is `published` and carries a `sha256` — a
   checkout-only pack is a local path no other machine has.

An unreachable bucket is a gap too, and it says so — a network failure is not
"nothing is published". A gap fails `:up` with the one command that fixes it
(`tools/profile.sh pack`, `tools/preset-publish.sh`, `:packrelease`, `:release`)
and the boxes are not launched. `--from-machine` is the explicit escape hatch:
it says so in the log and skips the gate. Provisioning's own log still reports a
clip a registry names but the disk lacks; that is per-box, and it is not a
publication gap.

## InstaBox bring-up

In:

- **`.bm/instabox.json`**: `api_key` (or `INSTABOX_API_KEY`), `api_url`, `spec`
  (`<vcpus>/<mem GiB>`, e.g. `4/8`; empty is the workspace's smallest),
  `template`, `max_workers`, `ttl_hours`, `publish_worker_port`.
- **Protocol half in `bm_core::cloud::instabox`** — pure, fixture-tested: the
  machine record (`id`, `name`, `state`, `spec` as a vCPU/MiB object,
  `runtime.ready`, `deleteAt`), the workspace (`/v1/orgs` → `allowedSpecs`),
  the account's caps (`/v1/usage` → `limits.maxMachines`, never a tier literal),
  the create body (`templateId`, `vcpus`, `memMiB`, `deleteAfterSeconds`), the
  published-port record, and the error catalog (`invalid_key`, `quota_exceeded`,
  `spec_not_allowed`, `template_not_allowed`, `no_capacity`, `machine_cooldown`,
  `operation_conflict`, 5xx → retryable, unknown → passthrough).
- **Provider**: `smoke` reads `/v1/orgs`, `/v1/usage` and `/v1/machines`; `plan`
  takes the smaller of `max_workers − live` and `limits.maxMachines − live`;
  `up` creates one machine at a time, waits (≤180 s) for `runtime.ready`, then
  publishes the worker port and records its URL as the box's address; `list`
  maps every state word; `down` deletes with `X-Confirm-Delete: <id>`, and every
  lifecycle call carries an `Idempotency-Key` so a lost answer is never applied
  twice.
- **Honoured in code**: the account's machine cap, the 24 h ceiling on
  `deleteAfterSeconds` (beyond it nothing is set and the line says so), and the
  idle pause / max-run numbers, printed by `smoke` from the account's own caps.
- **Provisioning over exec, not ssh** — see below. There is no bake gate: a box
  created from the stock `agent-browser` template is provisioned like any other.

### The exec transport

A machine has no address of its own, so `bm_core::provision::Exec` speaks
`POST /v1/machines/{id}/exec` and the files API, and `Ssh` dispatches to it when
the box's provider is `instabox` (`provision::transport_for`), so every step —
probe, install, models, packs, receipts — runs unchanged over HTTP:

- `run` → `exec` (`bash -lc`). The API kills a call after **600 s**, so a step
  with a longer timeout is written to the box, started under `nohup`, and its
  exit status polled; the caller sees one `(code, stdout, stderr)` either way.
- `rsync_push*` → a local `tar` uploaded with `PUT /files` and unpacked on the
  box (`delete` clears the target first); `rsync_pull` → `tar` on the box,
  `GET /files/content`, unpack locally. Incremental rsync is not emulated: a
  push sends the tree.
- `write_remote_file`, `probe`, `ensure_zstd` and the rest are scripts, so they
  need no transport of their own.

Verified live against the API: create → ready → publish → exec → push → pull →
delete (the ignored `a_live_box_comes_up_answers_exec_and_is_deleted` in
`cloud/instabox/tests.rs`, run by hand with `--ignored`; it deletes even when it
fails).

Not in yet:

- **A `:pool`-style listing for the provider** (the Cloud view is the EC2
  account).
- **Baking a snapshot**: the exec API can build one (create → install →
  snapshot), which would make a fresh box cheaper to provision, but nothing
  needs it while exec provisioning works.
- **Merge data plane**: the reverse tunnel is ssh today, so InstaBox v1 is
  crawl/digest/render, as planned.

## Future providers

Azure and Oracle are one module each: implement `Cloud`, map native states into
`ProviderView`, ship an error catalog, reuse `Exec`. No TUI, CLI, reaper or
registry change.

## Phases

| # | Scope | State |
|---|---|---|
| 1 | `Cloud` trait, `Shortfall`/`Reason`/`Launch`, plan + one-at-a-time + shortfall, `:up`/`:down [provider] [count] [--ttl] [--strict]`, smoke test, AWS catalog, stepped job | done |
| 2 | Registry v2 + `base_url` in dispatch, expiry for every provider, both backstops | done |
| 3 | `ProviderView` + `fate()` + reclaim + tombstones + `Killed` + take-back rules | done |
| 4 | Packs gate | done |
| 5 | InstaBox provider (protocol, capacity, create, list, delete) | done |
| 5b | Exec transport (`Exec`), exec-API provisioning | done |
| 6 | Docs, Azure/Oracle shape | next |

Phase 1 and 3 pay off on AWS alone.

## Risks

- InstaBox free is 1 machine, 8 h run cap, 30 min idle pause — evaluation, not a
  pool. `smoke` prints those numbers from the workspace and the account so
  `:up`'s line says so.
- An exec call dies with the client: a disconnect kills the command's process
  group, so a provision step that times out leaves nothing half-done on the box
  — and the next run re-does it.
- InstaBox egress ~50 Mbit/s: never ship takes out of the box.
- A private-subnet or closed-SG EC2 box still looks `running` to the provider;
  only the beat makes it suspect, which is why silence never deletes.
- `RunInstances` atomicity is why partial launches come from single calls; never
  reintroduce `--count N`.

## Decisions

- InstaBox v1 is crawl/digest/render; merge stays on AWS or `--from-machine`.
- Death means delete for every provider (AWS `stop` only when an operator parks).
- Bare `:up` uses the configured default provider in `.bm/cloud.json`.
