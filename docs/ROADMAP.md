# Roadmap — where Storycast is going

Plans in your author's words, restated in mine. If I misread anything, correct
the wording here — this file is the contract the work gets built against.

Status legend: `planned` (written down, not started) · `in progress` ·
`done`.

## 1. Any key, any endpoint, any models — de-hardcode the analyzer (`planned`)

**Your words:** "change hardcoded gemini key to any key with api link + model
names."

**My reading, to confirm:** today the analyzer backend is chosen by the
`analyzer` setting (`opencode | openrouter | gemini | local`), but inside each
backend the *endpoint* and *credential* are baked into the code: the Gemini
path always calls
`https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent?key=`
with `GEMINI_API_KEY`, the OpenRouter path always calls
`https://openrouter.ai/api/v1/chat/completions` with `OPENROUTER_API_KEY`, and
only Ollama takes a configurable URL (`ollama_url` + `local_model`). The goal
is one generic HTTP provider driven entirely by settings and `.env`:

- `LLM_BASE_URL` ("the api link") — endpoint root, e.g. a Gemini-compatible
  gateway, an OpenAI-compatible proxy, a self-hosted vLLM, anything that
  speaks the agreed request/response schema.
- `LLM_API_KEY` ("any key") — the bearer credential, replacing the per-vendor
  `*_API_KEY` names.
- `LLM_MODELS` ("model names") — the chain, comma-separated, keeping today's
  behavior: try each in order, skip on 404/quota, abort on 401/403, then fall
  back to the local option.

Concretely:

1. Add a provider to `bm-core/src/digest/llm.rs` next to
   `generate_gemini`/`generate_openrouter`/`generate_ollama`/`generate_opencode`,
   posting the OpenAI-compatible `chat/completions` body (`response_format:
   json_object`, `max_tokens`, temperature 0) and reading
   `choices[0].message.content`. (While in there: the OpenRouter path also
   hardcodes this repo's *old* name in its `Referer`/`X-Title` headers — that
   gets the generic treatment too.)
2. Read endpoint + key + model chain from `Settings` (`.bm/settings.json`)
   with `.env` fallbacks, one knob per value — no more vendor names compiled
   in. Keep the existing `GEMINI_API_KEY` / `OPENROUTER_API_KEY` names working
   as deprecated aliases for one release so nobody's setup breaks silently.
3. Keep the fallback semantics the code already has: per-model retries, skip
   on 404/day-quota, fast abort on 401/403/400, opencode as the last resort.
4. Test it the way the repo tests everything: a test that pins the request
   shape against a local fixture server (no real API keys in tests).

Files likely touched: `rust/crates/bm-core/src/digest/llm.rs`,
`rust/crates/bm-core/src/config.rs`, `.env.example`.

## 2. AWS: workers on EC2, artifacts and output in S3

**Your words:** "connect to AWS" — confirmed: **EC2 for compute, S3 for
storage** (artifacts + output), *not* Bedrock and *not* Polly for now.

**Status, split because the two halves are not in the same place:**

| | |
|---|---|
| **2b. EC2 workers** | **`done`** — launched, linked, provisioned, driven and terminated from the TUI |
| **2a. S3 as the shared artifact store** | **`planned`** — still the gap |

**My reading, to confirm:** today the cluster assumes one LAN: the inductor
pushes sources, the sidecar's weights and the voices over `ssh`+`rsync`, a
chapter's segment store lives on the box that rendered it (merge *affinity* pins
each merge to that box, since a merge reads segments it cannot be handed), and
`output/` lands on the inductor's disk. The goal is a cluster that survives the
internet:

### 2a. S3 as the shared artifact store

- Every artifact the scheduler currently moves through reports
  (`script-NN.json`, chapter text, bible deltas, mp3 payloads, and the cached
  segments) becomes addressable in S3, so workers never depend on a direct
  connection to the inductor to hand things back.
- The segment store already sits behind the `SegmentStore` trait
  (`bm-core/src/segments.rs`, `LocalStore` the only implementation): `S3Store`
  becomes an implementation instead of a rewrite.
- `output/Ch.N - Title.mp3` lands in the bucket (and optionally still syncs
  home).
- Open question to settle: AWS CLI via shell (matches the repo's "shell out
  to ssh/rsync" philosophy — zero new dependencies) vs. an SDK crate. CLI is
  the smaller, more consistent change; start there.

### 2b. EC2 workers — `done`

Shipped, and the shape is close to what was written here — with three places
where the plan was wrong and the code went another way. Those are worth keeping,
because each one was a real correction:

- **A provisioned instance is not a special kind of box.** The plan said "the
  provisioner learns an AWS target alongside the `ssh` target". It did not need
  to: an instance is linked with the pool's `.pem` and the `ubuntu` login and
  then provisioned by exactly the same `ssh`/`rsync` path as a LAN box, stamp
  logic included. `:up` is the only AWS-specific step, and all it does is launch
  and link. Guides: [AWS-WORKERS.md](AWS-WORKERS.md),
  [AWS-IAM-USER.md](AWS-IAM-USER.md).
- **A GPU instance for the render lane was the wrong idea, and it is not
  planned.** The plan assumed render was the expensive stage in a
  GPU-accelerable sense. It is not: the TTS path is a hand-written SIMD matvec
  with no GPU code, so the instance choice is decided by **RAM, not CPU** — and
  the answer is that the sidecar is ~2.85 GB resident the moment the weights
  load, so 8 GiB is the size and 4 GiB does not fit. See
  [AWS-WORKERS.md](AWS-WORKERS.md) §4 for the measurement.
- **The instance id lives in the machine's note, not in a new field.** The plan
  said `.bm/machines.json` "needs an `instance-id` next to addr/user/key". The
  registry still keys by **address**, and the id is stamped in the note instead —
  which is what lets `state/relink.rs` repair an address that rotated (every
  stop/start, every spot relaunch) by matching the stable id against one account
  listing. A field would have been the second source of truth this repo keeps
  avoiding.

The cost guardrail is in and it is not one mechanism but three, because they
fail differently: `Settings.idle_mins` (default 5) shuts the cluster down when
there is genuinely nothing to do, `X` stops everything on command, and `:down`
terminates the boxes — asking first, refusing while a render is in flight, and
scoped to the `storycast-worker` tag **and** explicit instance ids. `ttl_hours`
(6) is the backstop for a box that outlives its work.

Credentials are exactly where the plan said they must be: the IAM user's key in
the ignored `.bm/aws/credentials` (0600), never in `machines.json`, and with **no
fallback to this machine's own AWS identity**.

Files 2a will touch when it is picked up: `bm-core/src/segments.rs` (an
`S3Store` behind the existing trait), the artifact paths in `bm-agent/src/push.rs`
and `bm-inductor/src/api.rs`, and `output/` in `bm-core/src/paths.rs`.

Files 2b actually touched — kept as a map of where the cloud plane lives, since
that is the useful half of a plan once it is done:

| | |
|---|---|
| `bm-core/src/provision/aws.rs` | the EC2 calls, the AMI lookup through SSM, the firewall check |
| `bm-core/src/provision/aws_credentials.rs` | the credential store, and the verify-then-write order |
| `bm-core/src/provision/ssh.rs` | `HOST_KEY_OPTS` — one constant, both transports |
| `bm-core/src/provision/steps.rs` | `may_install`, and the stamp the second run skips on |
| `bm-inductor/src/aws_ops.rs` | one implementation per verb, shared by CLI and TUI |
| `bm-inductor/src/dispatch.rs` | the inductor-drives loop |
| `bm-inductor/src/state/{observe,relink}.rs` | one entry point for liveness; address drift repair |
| `bm-inductor/src/tui/{draw,input}/{cloud,policy}.rs` | the Cloud view and the policy view |
| `aws.default.json`, `aws-policy.json` | the tracked pool shape, and the policy to paste |

## 3. … (more to add)

Reserved. Tell me the next item and it goes here with the same treatment:
your words first, my reading to confirm, then the concrete steps.
