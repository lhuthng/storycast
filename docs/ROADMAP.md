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

## 2. AWS: workers on EC2, artifacts and output in S3 (`planned`)

**Your words:** "connect to AWS" — confirmed: **EC2 for compute, S3 for
storage** (artifacts + output), *not* Bedrock and *not* Polly for now.

**My reading, to confirm:** today the cluster assumes one LAN: the inductor
pushes sources/venv/voices over `ssh`+`rsync`, the segment store lives on the
inductor's disk (merge *affinity* pins merges to the local node), and `output/`
lands on the inductor's disk. The goal is a cluster that survives the internet:

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

### 2b. EC2 workers

- Provision an instance like any other box: the provisioner learns an "AWS
  target" alongside the `ssh` target — same stamp logic (skip unchanged
  sources/voices), installing into `~/.bm-worker/` on the instance and
  starting the worker + TTS sidecar.
- A GPU instance for the render lane (the expensive one) and cheap CPU boxes
  for crawl/digest/merge, following the stage affinities the scheduler
  already understands.
- Cost guardrail from day one: idle instances must be stoppable (the existing
  `X` "stop everything everywhere" is the natural hook), otherwise the render
  farm bills you while you sleep.
- `.bm/machines.json` needs an `instance-id` next to addr/user/key — and must
  stay git-ignored, since credentials live there too.

Files likely touched: `rust/crates/bm-core/src/provision/` (the `Ssh` transport
in `ssh.rs`, the new AWS target in `steps.rs`),
`rust/crates/bm-inductor/src/{backend,state,api}.rs`, `bm-agent` report paths,
`.bm/machines.json` schema.

## 3. … (more to add)

Reserved. Tell me the next item and it goes here with the same treatment:
your words first, my reading to confirm, then the concrete steps.
